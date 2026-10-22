use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

mod count;
mod glue;
mod run;
mod tools;

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
pub enum Opt {
    Speed,
    Size,
}

/// What `edge build` makes and how.
#[derive(clap::Args)]
pub struct Opts {
    /// Output path, the script's name when omitted, a .o stops before linking.
    #[arg(long, short = 'o')]
    out: Option<PathBuf>,
    /// A .wasm for WASI instead of a native binary.
    #[arg(long)]
    wasm: bool,
    /// The target triple, the host's when omitted.
    #[arg(long, conflicts_with = "wasm")]
    target: Option<String>,
    /// What LLVM optimizes for.
    #[arg(long, value_enum, default_value = "speed")]
    opt: Opt,
    /// Leave the symbols out of the output.
    #[arg(long)]
    strip: bool,
    /// Collect and move every object at every safepoint, for the tests; past 1 MB a heap moves once per 4 KB more it holds, in safepoints.
    #[arg(long, hide = true)]
    gc_torture: bool,
    /// Write the module after LLVM optimized it.
    #[arg(long, hide = true)]
    emit_llvm: Option<PathBuf>,
    /// Write the linked glue before LLVM optimized it, each block function naming its opcodes.
    #[arg(long, hide = true)]
    emit_glue: Option<PathBuf>,
    /// Count the IR instructions a run executes, printed last on stderr.
    #[arg(long, hide = true)]
    count: bool,
}

/* The triple a build targets, `--target`, wasm for WASI, or the host as a static binary. */
fn target(opts: &Opts) -> String {
    match (&opts.target, opts.wasm) {
        (Some(t), _) => t.clone(),
        (None, true) => "wasm32-wasip1".to_string(),
        (None, false) if cfg!(target_os = "macos") => format!("{}-apple-darwin", std::env::consts::ARCH),
        (None, false) => format!("{}-unknown-linux-musl", std::env::consts::ARCH),
    }
}

/// Compiles `file`, or the project's entry, to one binary, the VM's own handlers inlined into the program's blocks.
pub fn build(manifest_path: &Path, manifest: Option<&str>, file: Option<&Path>, opts: &Opts) -> Result<()> {
    let target = target(opts);
    let entry = match file {
        Some(f) => f.to_path_buf(),
        None => manifest_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).join("main.py"),
    };
    let stem = entry.file_stem().ok_or_else(|| anyhow!("{} names no file", entry.display()))?.to_string_lossy().into_owned();
    let out = opts.out.clone().unwrap_or_else(|| PathBuf::from(if target.starts_with("wasm32") { format!("{stem}.wasm") } else { stem }));
    let dir = tempfile::tempdir().context("creating a build dir")?;
    // A bitcode input is a whole program already, another language's side of a benchmark.
    let (modules, object) = match entry.extension().is_some_and(|e| e == "bc") {
        true => (vec![entry.clone()], PathBuf::new()),
        false => {
            let src = std::fs::read_to_string(&entry).with_context(|| format!("reading {}", entry.display()))?;
            // Its imports resolve as edge run resolves them, its SyntaxWarnings printed as a run prints them and never by the
            // binary, and the binary carries the answers to resolve them again the same.
            let name = crate::host::driver::path_spec(&entry);
            let mut run = crate::host::driver::host()?.vm(crate::host::driver::stdout_sink(), crate::host::Project::disk(&name, manifest), None, None)?;
            let table = run.resolutions(&src)?.unwrap_or_else(|report| { crate::ui::traceback(&report); std::process::exit(1) });
            let resolver = compiler::modules::table::Table::new(&table).map_err(|e| anyhow!(e))?;
            let chunk = compiler::code::compile_with(&src, || Some(name.clone()), || Box::new(resolver)).unwrap_or_else(|report| { eprintln!("{report}"); std::process::exit(1) });
            // Rules that are not these sources', or a tree the reviewer refuses, leave every opcode to its handler.
            let rules = compiler::types::rules();
            let typed = rules.as_ref().map_err(String::clone).and_then(|r| compiler::types::typed(&chunk, r));
            match &typed { Ok(t) if opts.count => { let (p, n) = t.count(); eprintln!("types: {p}/{n}"); } Err(why) => eprintln!("edge build: no proven types, {why}"), _ => {} }
            let (vm, collect) = vm(&target, if opts.gc_torture { "-torture" } else if opts.count { "-count" } else { "" }, dir.path())?;
            let glue = dir.path().join("glue.ll");
            std::fs::write(&glue, glue::module(&src, &name, &table, &chunk, typed.as_ref().ok(), collect, &target))?;
            (vec![vm[0].clone(), glue], vm[1].clone())
        }
    };
    let sys = tools::Sysroot::open(&target)?;
    // Progress is for a person at a terminal, so a build's stderr elsewhere holds only its diagnostics and notes.
    let sp = std::io::IsTerminal::is_terminal(&std::io::stderr()).then(|| crate::ui::spinner(&format!("compiling {} for {target}", entry.display())));
    let built = run::compile(&sys, &modules, &object, &target, opts, &out);
    match (sp, &built) { (Some(sp), Ok(())) => sp.done(&format!("built {}", out.display())), (Some(sp), Err(_)) => sp.fail(&format!("failed to build {}", out.display())), _ => {} }
    built
}

/* The C entry `target`'s start code calls, wasi-libc's without arguments so a `.wasm` imports only fd_write and proc_exit. */
fn entry(target: &str) -> &'static str {
    if target.starts_with("wasm32") { "__main_void" } else { "main" }
}

// The VM per target, gzipped, from `make inline`: the handlers' bodies, the rest compiled, the handlers that may collect.
type Vm = (&'static str, [&'static [u8]; 2], &'static [&'static str]);
const VM: &[Vm] = include!(concat!(env!("OUT_DIR"), "/vm_bitcode.rs"));

/* The VM for `target` unpacked into the build dir, its inlined part and its object, and its handlers that may collect.
   A test's VM, collecting at every safepoint or counting its instructions, is `make bitcode`'s own under `EDGE_BITCODE_DIR`. */
fn vm(target: &str, variant: &str, dir: &Path) -> Result<([PathBuf; 2], &'static [&'static str])> {
    if !variant.is_empty() {
        let root = std::env::var_os("EDGE_BITCODE_DIR").ok_or_else(|| anyhow!("--gc-torture and --count build with a test VM, set EDGE_BITCODE_DIR to the target/bitcode that make bitcode TORTURE=1 or COUNT=1 wrote"))?;
        let at = Path::new(&root).join(format!("{target}{variant}"));
        let ll: &'static str = Box::leak(std::fs::read_to_string(at.join("vm.ll")).with_context(|| format!("reading {}", at.join("vm.ll").display()))?.into_boxed_str());
        return Ok(([at.join("inline.bc"), at.join("vm.o")], Box::leak(ll.lines().filter_map(|l| l.split_once(" = !{!\"op_")?.1.strip_suffix("\"}")).collect::<Vec<_>>().into_boxed_slice())));
    }
    let Some((_, gz, collect)) = VM.iter().find(|(t, ..)| *t == target) else {
        let have: Vec<&str> = VM.iter().map(|(t, ..)| *t).collect();
        bail!("this edge carries no VM for {target}, it builds for {}", if have.is_empty() { "nothing, run make bitcode".to_string() } else { have.join(", ") });
    };
    let paths = [dir.join("inline.bc"), dir.join("vm.o")];
    for (gz, path) in gz.iter().zip(&paths) { std::io::copy(&mut flate2::read::GzDecoder::new(*gz), &mut std::fs::File::create(path)?)?; }
    Ok((paths, collect))
}
