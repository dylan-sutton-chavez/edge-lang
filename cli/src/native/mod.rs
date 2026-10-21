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
    /// Collect and move every object at every safepoint, for the tests.
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
pub fn build(manifest_path: &Path, file: Option<&Path>, opts: &Opts) -> Result<()> {
    let target = target(opts);
    let entry = match file {
        Some(f) => f.to_path_buf(),
        None => manifest_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).join("main.py"),
    };
    let stem = entry.file_stem().ok_or_else(|| anyhow!("{} names no file", entry.display()))?.to_string_lossy().into_owned();
    let out = opts.out.clone().unwrap_or_else(|| PathBuf::from(if target.starts_with("wasm32") { format!("{stem}.wasm") } else { stem }));
    let dir = tempfile::tempdir().context("creating a build dir")?;
    // A bitcode input is a whole program already, another language's side of a benchmark.
    let modules = match entry.extension().is_some_and(|e| e == "bc") {
        true => vec![entry.clone()],
        false => {
            let src = std::fs::read_to_string(&entry).with_context(|| format!("reading {}", entry.display()))?;
            let (chunk, errors) = compiler::code::compile(&src);
            if !errors.is_empty() {
                bail!("{}", compiler::code::Diagnostic::render_all(&errors, &src, Some(&entry.to_string_lossy())));
            }
            let glue = dir.path().join("glue.ll");
            std::fs::write(&glue, glue::module(&chunk, &target, opts.gc_torture))?;
            vec![vm_bitcode(&target, dir.path())?, glue]
        }
    };
    let sys = tools::Sysroot::open(&target)?;
    let sp = crate::ui::spinner(&format!("compiling {} for {target}", entry.display()));
    match run::compile(&sys, &modules, &target, opts, &out) {
        Ok(()) => sp.done(&format!("built {}", out.display())),
        Err(e) => {
            sp.fail(&format!("failed to build {}", out.display()));
            return Err(e);
        }
    }
    Ok(())
}

/* The C entry `target`'s start code calls, wasi-libc's for a `main` that takes no arguments. */
fn entry(target: &str) -> &'static str {
    if target.starts_with("wasm32") { "__main_void" } else { "main" }
}

// The VM's handlers as bitcode per target, gzipped, from `make bitcode`.
const VM_BITCODE: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/vm_bitcode.rs"));

/* The VM's bitcode for `target`, unpacked into the build dir. */
fn vm_bitcode(target: &str, dir: &Path) -> Result<PathBuf> {
    let Some((_, gz)) = VM_BITCODE.iter().find(|(t, _)| *t == target) else {
        let have: Vec<&str> = VM_BITCODE.iter().map(|(t, _)| *t).collect();
        bail!("this edge carries no VM for {target}, it builds for {}", if have.is_empty() { "nothing, run make bitcode".to_string() } else { have.join(", ") });
    };
    let path = dir.join("vm.bc");
    std::io::copy(&mut flate2::read::GzDecoder::new(*gz), &mut std::fs::File::create(&path)?)?;
    Ok(path)
}
