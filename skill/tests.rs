use std::path::Path;
use std::process::Command;

/* SKILL_INTERPRETER wins, then the local cli build, then PATH, a missing binary fails the suite. */
fn interpreter_binary() -> String {
    if let Ok(p) = std::env::var("SKILL_INTERPRETER") {
        assert!(Path::new(&p).is_file(), "SKILL_INTERPRETER points at a missing file, {p}");
        return p;
    }
    let local = concat!(env!("CARGO_MANIFEST_DIR"), "/../cli/target/debug/interpreter");
    if Path::new(local).is_file() {
        return local.to_string();
    }
    let on_path = Command::new("interpreter").arg("--version").output().is_ok_and(|o| o.status.success());
    assert!(on_path, "no interpreter binary found, run make test-skill or set SKILL_INTERPRETER");
    "interpreter".to_string()
}

#[test]
fn skill_md() {
    let interpreter = interpreter_binary();
    let doc = concat!(env!("CARGO_MANIFEST_DIR"), "/SKILL.md");
    let out = Command::new(env!("CARGO_BIN_EXE_skill"))
        .arg(doc)
        .args(["--interpreter", &interpreter])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
