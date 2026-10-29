
#[path = "src/host/config.rs"]
mod config;

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/* whether the composition being built turns the named feature on */
fn wanted(feature: &str) -> bool {
    std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some()
}

fn main() {
    println!("cargo:rerun-if-env-changed=EDGE_WASM");
    println!("cargo:rerun-if-env-changed=EDGE_JS_DIST");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    // a composition that embeds no runtime of this crate's own builds none of these
    if wanted("EMBED") {
        let target = std::env::var("TARGET").expect("TARGET");
        let mut cfg = config::base();
        cfg.target(&target).expect("wasmtime has no backend for the build target");
        cfg.cranelift_opt_level(wasmtime::OptLevel::Speed);
        let engine = wasmtime::Engine::new(&cfg).expect("wasmtime engine");
        // the runtime the edge composition embeds and hands the kit
        let wasm = std::env::var("EDGE_WASM")
            .map(PathBuf::from)
            .unwrap_or_else(|_| manifest.join("../target/wasm32-unknown-unknown/cli/edge.wasm"));
        precompile(&engine, &wasm, &out.join("edge.cwasm"), "run make wasm-cli first");
        // a browser runs the module so a dist carries a raw copy too
        std::fs::copy(&wasm, out.join("edge.wasm")).unwrap_or_else(|e| panic!("copying {}: {e}", wasm.display()));
    }
    if wanted("DOCS") {
        let lucide = manifest.join("../target/lucide.json");
        println!("cargo:rerun-if-changed={}", lucide.display());
        std::fs::copy(&lucide, out.join("lucide.json")).unwrap_or_else(|e| panic!("copying {}: {e}, run make lucide first", lucide.display()));
    }
    if wanted("WEB") {
        let js_dist = std::env::var("EDGE_JS_DIST").map(PathBuf::from).unwrap_or_else(|_| manifest.join("../js/dist"));
        js_host(&js_dist, &out.join("js_host.rs"));
    }
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
    std::fs::write(out, table).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
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
