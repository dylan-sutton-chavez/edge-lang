use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// A `+ name   url` line for an added package.
pub fn added(name: &str, url: &str) {
    println!("  + {name:<10} {url}");
}

/// What a package just added asks the root to grant it.
pub fn asks(who: &str, entries: &[String]) {
    println!("    {who} asks {}", entries.join(", "));
}

/// A `- name` line for a removed package.
pub fn removed(name: &str) {
    println!("  - {name}");
}

/// A blank line then a trailing note.
pub fn note(msg: &str) {
    println!();
    println!("  {msg}");
}

/// The created-files tree printed by `edge init`.
pub fn scaffolded(dir: &str, items: &[&str], next: &str) {
    let label = if dir == "." { "project" } else { dir };
    println!("  created {label}/");
    for (i, item) in items.iter().enumerate() {
        let branch = if i + 1 == items.len() { "└─" } else { "├─" };
        println!("    {branch} {item}");
    }
    note(next);
}

/// The `edge serve` banner, `lan` adds a network URL.
pub fn serve_banner(port: u16, dir: &Path, lan: Option<&str>) {
    println!("  http://localhost:{port}");
    if let Some(ip) = lan {
        println!("  http://{ip}:{port}");
    }
    println!("  watching {}", dir.display());
}

/// Print a script's traceback to stderr, verbatim from the engine.
pub fn traceback(msg: &str) {
    eprintln!("{msg}");
}

/// Summary printed by `edge build` after vendoring the JS host + packages + scripts into dist/.
pub fn build_report(dir: &std::path::Path, js_files: usize, packages: usize, scripts: usize, size: u64, elapsed: std::time::Duration) {
    println!();
    println!("  bundled to {}/", dir.display());
    println!();
    println!("  {js_files} js host files + compiler.wasm");
    println!("  {packages} packages");
    println!("  {scripts} scripts");
    println!();
    println!("  {:.2} MB · {:.1}s", size as f64 / 1_000_000.0, elapsed.as_secs_f64());
}

/// Closing lines for `edge publish --out`, `hint` says what to do with it.
pub fn packed(out: &std::path::Path, files: usize, docs: usize, size: u64, hint: &str) {
    println!();
    let pages = match docs {
        0 => String::new(),
        1 => ", 1 doc".to_string(),
        n => format!(", {n} docs"),
    };
    println!("  packed {} ({files} files{pages})", out.display());
    let (scaled, unit) = if size < 1_000_000 { (size as f64 / 1_000.0, "KB") } else { (size as f64 / 1_000_000.0, "MB") };
    println!("  {scaled:.2} {unit}");
    println!();
    println!("  {hint}");
}

/// Asks `question` with a [y/N] and reads the answer, false for anything but yes.
pub fn confirm(question: &str) -> anyhow::Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| anyhow::anyhow!("reading the answer: {e}"))?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}

/// Render an error to stderr. `{:#}` joins the cause chain so the root reason isn't swallowed.
pub fn error(e: &anyhow::Error) {
    eprintln!("error: {e:#}");
}

/// Per-file verdict line for `edge test`.
pub fn test_verdict(ok: bool, file: &str, reason: Option<&str>) {
    let tag = if ok { "(successful)" } else { "(unsuccessful)" };
    match reason {
        Some(r) => println!("  {tag} {file}: {r}"),
        None => println!("  {tag} {file}"),
    }
}

/// The `edge test` closing summary.
pub fn test_summary(passed: usize, total: usize, secs: f64) {
    println!();
    println!("  {passed}/{total} files passed · {secs:.1}s");
}

// Braille frames used by the spinner, matching the rest of the UI's unicode style.
const SPIN_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Animated stderr spinner for long operations. Falls back to a static line when redirected.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

/// Start a spinner with `label`. Drop it (or call `done`) to stop.
pub fn spinner(label: &str) -> Spinner {
    let stop = Arc::new(AtomicBool::new(false));
    // No animation when stderr is redirected, just announce the step.
    if !std::io::stderr().is_terminal() {
        eprintln!("  {label}...");
        return Spinner { stop, handle: None };
    }
    let stop_thread = stop.clone();
    let label = label.to_string();
    let handle = thread::spawn(move || {
        let mut i = 0usize;
        while !stop_thread.load(Ordering::Relaxed) {
            let frame = SPIN_FRAMES[i % SPIN_FRAMES.len()];
            eprint!("\r  {frame} {label}");
            let _ = std::io::stderr().flush();
            thread::sleep(Duration::from_millis(120));
            i += 1;
        }
        // ANSI carriage return plus erase-to-end-of-line so the next print starts clean.
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    });
    Spinner { stop, handle: Some(handle) }
}

impl Spinner {
    /// Stop the spinner and print a `(successful) {msg}` line.
    pub fn done(self, msg: &str) {
        // Drop stops the thread and clears the line, the result line goes right after.
        drop(self);
        eprintln!("  (successful) {msg}");
    }

    /// Stop the spinner and print an `(unsuccessful) {msg}` line.
    pub fn fail(self, msg: &str) {
        drop(self);
        eprintln!("  (unsuccessful) {msg}");
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
