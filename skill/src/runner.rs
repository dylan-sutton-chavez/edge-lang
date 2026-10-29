use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub ok: bool,
}

fn spawn(mut cmd: Command, stdin_data: &str, timeout: Duration) -> Result<Outcome, String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_data.as_bytes());
    }
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    Ok(Outcome { stdout, stderr, ok: status.success() })
}

/* the manifest every cell runs under since no cell imports a package */
pub const MANIFEST: &str = "{}\n";

// a port nothing listens on so a cell asking any server fails at once
const NOWHERE: &str = "http://127.0.0.1:9";

// A fresh scratch dir, one per cell so runs never share files.
fn fresh() -> Result<std::path::PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("skill-cell-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).map_err(|e| format!("tempdir failed: {e}"))?;
    std::fs::write(dir.join("edge.json"), MANIFEST).map_err(|e| format!("write edge.json failed: {e}"))?;
    Ok(dir)
}

/* the edge binary with every server it could ask moved to a closed port */
fn offline(edge: &str) -> Command {
    let mut cmd = Command::new(edge);
    cmd.env("EDGE_SITE_BASE", NOWHERE).env("EDGE_CDN_BASE", NOWHERE);
    cmd
}

pub fn run_script(edge: &str, src: &str, timeout: Duration) -> Result<Outcome, String> {
    let dir = fresh()?;
    let mut cmd = offline(edge);
    cmd.arg("run").arg("--manifest").arg(dir.join("edge.json"));
    let outcome = spawn(cmd, src, timeout);
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

pub fn run_actor(edge: &str, yml: &str, timeout: Duration) -> Result<Outcome, String> {
    let dir = fresh()?;
    let path = dir.join("actor.yml");
    std::fs::write(&path, yml).map_err(|e| format!("write actor.yml failed: {e}"))?;
    let mut cmd = offline(edge);
    cmd.arg("actor").arg(&path);
    let outcome = spawn(cmd, "", timeout);
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

pub fn check(expect: &str, outcome: &Outcome, verdict_is_error: bool) -> Result<(), String> {
    if verdict_is_error {
        if outcome.ok {
            return Err("expected a failing run, exit code was 0".to_string());
        }
        if !outcome.stderr.contains(expect.trim()) {
            return Err(format!(
                "stderr mismatch\n  expected substring: {}\n  got: {}",
                expect.trim(),
                outcome.stderr.trim()
            ));
        }
        return Ok(());
    }
    if !outcome.ok {
        return Err(format!(
            "run failed\n  stderr: {}",
            outcome.stderr.trim()
        ));
    }
    if expect.trim_end() != outcome.stdout.trim_end() {
        return Err(format!(
            "stdout mismatch\n  expected: {}\n  got: {}",
            expect.trim_end(),
            outcome.stdout.trim_end()
        ));
    }
    Ok(())
}
