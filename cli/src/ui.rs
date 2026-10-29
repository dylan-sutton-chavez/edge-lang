use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

pub fn added(name: &str, url: &str) {
    println!("  + {name:<10} {url}");
}

// what a package just added asks the root to grant it
pub fn asks(who: &str, entries: &[String]) {
    println!("    {who} asks {}", entries.join(", "));
}

pub fn removed(name: &str) {
    println!("  - {name}");
}

pub fn note(msg: &str) {
    println!();
    println!("  {msg}");
}

// the tree of created files `edge init` prints
pub fn scaffolded(dir: &str, items: &[&str], next: &str) {
    let label = if dir == "." { "project" } else { dir };
    println!("  created {label}/");
    for (i, item) in items.iter().enumerate() {
        let branch = if i + 1 == items.len() { "└─" } else { "├─" };
        println!("    {branch} {item}");
    }
    note(next);
}

// the `edge serve` banner where `lan` adds a network URL
pub fn serve_banner(port: u16, dir: &Path, lan: Option<&str>) {
    println!("  http://localhost:{port}");
    if let Some(ip) = lan {
        println!("  http://{ip}:{port}");
    }
    println!("  watching {}", dir.display());
}

// the traceback goes to stderr verbatim from the engine
pub fn traceback(msg: &str) {
    eprintln!("{msg}");
}

// the summary `edge build` prints after vendoring into dist
pub fn build_report(dir: &std::path::Path, js_files: usize, packages: usize, scripts: usize, size: u64, elapsed: std::time::Duration) {
    println!();
    println!("  bundled to {}/", dir.display());
    println!();
    println!("  {js_files} js host files + edge.wasm");
    println!("  {packages} packages");
    println!("  {scripts} scripts");
    println!();
    println!("  {:.2} MB · {:.1}s", size as f64 / 1_000_000.0, elapsed.as_secs_f64());
}

// closing lines of `edge build` where `hint` keeps the other modes discoverable
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

// `{:#}` joins the cause chain so the root reason is never swallowed
pub fn error(e: &anyhow::Error) {
    eprintln!("error: {e:#}");
}

// the verdict line of each file `edge test` runs
pub fn test_verdict(ok: bool, file: &str, reason: Option<&str>) {
    let tag = if ok { "(successful)" } else { "(unsuccessful)" };
    match reason {
        Some(r) => println!("  {tag} {file}: {r}"),
        None => println!("  {tag} {file}"),
    }
}

pub fn test_summary(passed: usize, total: usize, secs: f64) {
    println!();
    println!("  {passed}/{total} files passed · {secs:.1}s");
}

// braille frames matching the unicode style of the rest of the UI
const SPIN_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// a stderr spinner for long steps that falls back to a static line when redirected
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

// dropping it or calling `done` stops the spinner
pub fn spinner(label: &str) -> Spinner {
    let stop = Arc::new(AtomicBool::new(false));
    // a redirected stderr gets the step announced without animation
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
        // a carriage return and an erase to line end so the next print starts clean
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    });
    Spinner { stop, handle: Some(handle) }
}

impl Spinner {
    pub fn done(self, msg: &str) {
        // the drop stops the thread and clears the line before the result line
        drop(self);
        eprintln!("  (successful) {msg}");
    }

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
