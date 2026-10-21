
#[path = "src/host/config.rs"]
mod config;

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=EDGE_COMPILER_WASM");
    println!("cargo:rerun-if-env-changed=EDGE_JS_DIST");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let target = std::env::var("TARGET").expect("TARGET");
    // `edge build` fetches the LLVM tools of the very release that compiles it, read here rather than written down.
    let rustc = std::process::Command::new(std::env::var("RUSTC").unwrap_or("rustc".into())).arg("--version").output().expect("running rustc --version");
    // The tools it fetches are the ones built for the machine this edge runs on.
    println!("cargo:rustc-env=EDGE_HOST={target}");
    println!("cargo:rustc-env=EDGE_RUSTC={}", String::from_utf8_lossy(&rustc.stdout).split_whitespace().nth(1).expect("rustc --version names a release"));
    let mut cfg = config::base();
    cfg.target(&target).expect("wasmtime has no backend for the build target");
    cfg.cranelift_opt_level(wasmtime::OptLevel::Speed);
    let engine = wasmtime::Engine::new(&cfg).expect("wasmtime engine");
    let compiler = std::env::var("EDGE_COMPILER_WASM")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("../target/wasm32-unknown-unknown/cli/compiler.wasm"));
    precompile(&engine, &compiler, &out.join("compiler.cwasm"), "run make wasm-cli first");
    // A browser runs the module itself, never the precompile, so a dist and a headless run carry a raw copy.
    std::fs::copy(&compiler, out.join("compiler.wasm")).unwrap_or_else(|e| panic!("copying {}: {e}", compiler.display()));
    let lucide = manifest.join("../target/lucide.json");
    println!("cargo:rerun-if-changed={}", lucide.display());
    std::fs::copy(&lucide, out.join("lucide.json")).unwrap_or_else(|e| panic!("copying {}: {e}, run make lucide first", lucide.display()));
    vm_bitcode(&manifest.join("../target/bitcode"), &out);
    let js_dist = std::env::var("EDGE_JS_DIST").map(PathBuf::from).unwrap_or_else(|_| manifest.join("../js/dist"));
    js_host(&js_dist, &out.join("js_host.rs"));
}

/* Each target's VM `make inline` left, gzipped: the handlers' bodies `edge build` inlines, the VM compiled once that it
   links, and the handlers its `!edge.safepoints` names, an absent target being one it cannot build for. */
fn vm_bitcode(dir: &Path, out: &Path) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut table = String::from("&[");
    for target in std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()) {
        // A test's VM, `-torture` or `-count`, stays out of the binary, `edge build` reads it from `EDGE_BITCODE_DIR`.
        if !dir.join(&target).join("inline.bc").exists() || target.ends_with("-torture") || target.ends_with("-count") { continue }
        for file in ["inline.bc", "vm.o"] {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            std::io::Write::write_all(&mut gz, &std::fs::read(dir.join(&target).join(file)).expect("vm.o beside inline.bc, run make inline"))
                .and_then(|()| std::fs::write(out.join(format!("{target}-{file}.gz")), gz.finish()?)).expect("writing the VM");
        }
        let ll = std::fs::read_to_string(dir.join(&target).join("vm.ll")).expect("vm.ll beside vm.bc, run make bitcode");
        let collect: Vec<&str> = ll.lines().filter_map(|l| l.split_once(" = !{!\"op_")?.1.strip_suffix("\"}")).collect();
        let gz = |f: &str| format!("include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{target}-{f}.gz\"))");
        table += &format!("(\"{target}\", [{}, {}], &{collect:?}),", gz("inline.bc"), gz("vm.o"));
    }
    std::fs::write(out.join("vm_bitcode.rs"), table + "]").expect("writing the VM bitcode table");
}

/* The compiled JS host the binary carries, keyed by the path the CDN serves each file at, so a page's imports read the same from a dist, from a headless run, or from the CDN. */
fn js_host(dist: &Path, out: &Path) {
    println!("cargo:rerun-if-changed={}", dist.display());
    let hint = "run make js first";
    let root = std::fs::canonicalize(dist).unwrap_or_else(|e| panic!("cannot read {}: {e}, {hint}", dist.display()));

    let mut found = Vec::new();
    walk_js(&root, &root, &mut found);
    found.sort();
    assert!(!found.is_empty(), "no .js under {}, {hint}", root.display());

    let mut table = String::from("pub const JS_HOST: &[(&str, &[u8])] = &[\n");
    for (key, path) in found {
        table.push_str(&format!("    ({key:?}, include_bytes!({:?})),\n", path.display().to_string()));
    }
    table.push_str("];\n");
    table.push_str(&format!("pub const SYSTEM_MODULES: &[&str] = &{:?};\n", system_modules(&root)));
    std::fs::write(out, table).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
}

/* The system module names as the JS host lists them, so the CLI reserves the same ones without starting SpiderMonkey. */
fn system_modules(dist: &Path) -> Vec<String> {
    let file = dist.join("system/names.js");
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()));
    let list = text.split_once("MODULES = [").and_then(|(_, rest)| rest.split_once(']')).map(|(list, _)| list);
    let list = list.unwrap_or_else(|| panic!("{} lists no MODULES", file.display()));
    list.split(',').map(|name| name.trim().trim_matches(['\'', '"']).to_string()).filter(|name| !name.is_empty()).collect()
}

// tsc writes to js/dist and the CDN serves that tree under js/src, so the prefix is added back here.
fn walk_js(root: &Path, dir: &Path, found: &mut Vec<(String, PathBuf)>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_js(root, &path, found);
        } else if path.extension().is_some_and(|e| e == "js") {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            found.push((format!("src/{rel}"), path));
        }
    }
}

fn precompile(engine: &wasmtime::Engine, input: &Path, output: &Path, hint: &str) {
    println!("cargo:rerun-if-changed={}", input.display());
    let bytes = std::fs::read(input).unwrap_or_else(|e| panic!("cannot read {}: {e}, {hint}", input.display()));
    // A rerun over an unrelated file keeps the artifact while the input and the engine match.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    engine.precompile_compatibility_hash().hash(&mut hasher);
    let key = format!("{:016x}", hasher.finish());
    let stamp = output.with_extension("key");
    if output.exists() && std::fs::read_to_string(&stamp).is_ok_and(|k| k == key) {
        return;
    }
    let cwasm = engine.precompile_module(&bytes).unwrap_or_else(|e| panic!("precompiling {}: {e}", input.display()));
    std::fs::write(output, cwasm).unwrap_or_else(|e| panic!("writing {}: {e}", output.display()));
    std::fs::write(&stamp, key).unwrap_or_else(|e| panic!("writing {}: {e}", stamp.display()));
}
