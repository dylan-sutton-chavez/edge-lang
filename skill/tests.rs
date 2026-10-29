use std::path::Path;
use std::process::Command;

/* SKILL_EDGE wins, then the local cli build, then PATH, a missing binary fails the suite. */
fn edge_binary() -> String {
    if let Ok(p) = std::env::var("SKILL_EDGE") {
        assert!(Path::new(&p).is_file(), "SKILL_EDGE points at a missing file, {p}");
        return p;
    }
    let local = concat!(env!("CARGO_MANIFEST_DIR"), "/../cli/target/debug/edge");
    if Path::new(local).is_file() {
        return local.to_string();
    }
    let on_path = Command::new("edge").arg("--version").output().is_ok_and(|o| o.status.success());
    assert!(on_path, "no edge binary found, run make test-skill or set SKILL_EDGE");
    "edge".to_string()
}

#[test]
fn skill_md() {
    let edge = edge_binary();
    let doc = concat!(env!("CARGO_MANIFEST_DIR"), "/SKILL.md");
    let out = Command::new(env!("CARGO_BIN_EXE_skill"))
        .arg(doc)
        .args(["--edge", &edge])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
