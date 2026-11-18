#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};

/* whether the build turns the named feature on */
fn wanted(feature: &str) -> bool {
    std::env::var_os(format!("CARGO_FEATURE_{feature}")).is_some()
}

fn main() {
    // an engine build reads nothing here and runs this again only once the script changes
    println!("cargo:rerun-if-changed=build.rs");
    if !wanted("CLI_WEB") {
        return;
    }
    println!("cargo:rerun-if-env-changed=INTERPRETER_JS_DIST");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let js_dist = std::env::var("INTERPRETER_JS_DIST").map(PathBuf::from).unwrap_or_else(|_| manifest.join("js/dist"));
    js_host(&js_dist, &out.join("js_host.rs"));
}

/* the compiled js host the binary carries keyed by its path in a staged tree */
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

// tsc writes js/dist while a staged tree holds it under js/src so src is added
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
