/* The hash `edge.ept`'s first line must carry: sha256 over every file under src/ but src/types/ (it reads the rules, it
   does not define what the VM does), in the byte order of their paths, each as its path from the root, a zero byte, its
   length as 8 bytes little-endian, and its bytes. */

extern crate alloc;
#[path = "src/util/sha256.rs"]
#[allow(dead_code)]
mod sha256;

fn files(dir: &std::path::Path, out: &mut Vec<String>) {
    for e in std::fs::read_dir(dir).expect("src/ reads").flatten() {
        let p = e.path();
        if p.is_dir() { files(&p, out) } else { out.push(p.to_string_lossy().replace('\\', "/")) }
    }
}

fn main() {
    let mut paths = Vec::new();
    files(std::path::Path::new("src"), &mut paths);
    paths.retain(|p| !p.starts_with("src/types/"));
    paths.sort();
    let mut fed = Vec::new();
    for p in &paths {
        let bytes = std::fs::read(p).expect("a source reads");
        fed.extend(p.as_bytes());
        fed.push(0);
        fed.extend((bytes.len() as u64).to_le_bytes());
        fed.extend(bytes);
    }
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("sources.hash");
    std::fs::write(out, sha256::hex_encode(&sha256::sha256(&fed))).expect("OUT_DIR is writable");
    println!("cargo:rerun-if-changed=src");
}
