use anyhow::{anyhow, Result};
use clap::{ArgMatches, Args, FromArgMatches};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tiny_http::{Header, Response, Server};

use crate::host::{hosted_here, loopback_bind};
use crate::kit::{Command, Descriptor, Kit};
use crate::web::{content_type, host_of};

#[derive(Args)]
struct ServeArgs {
    /// Bind address, use 0.0.0.0 to expose on your LAN.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on.
    #[arg(long, default_value_t = 5173)]
    port: u16,
    /// Open the app in a browser once the server is up.
    #[arg(long)]
    open: bool,
}

/* `serve` the dev server on loopback unless a wider bind is asked for */
pub struct Serve;

impl Command for Serve {
    fn spec(&self, _kit: &Kit) -> clap::Command {
        ServeArgs::augment_args(clap::Command::new("serve").about("Dev server with live reload"))
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("serve".to_string(), "Dev server with live reload".to_string())
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        let args = ServeArgs::from_arg_matches(m)?;
        run(kit, PathBuf::from("."), &args.host, args.port, args.open)?;
        Ok(0)
    }
}

// a static dev server on tiny_http that reloads the page on any file change
fn run(kit: &Kit, dir: PathBuf, host: &str, port: u16, open: bool) -> Result<()> {
    let server = Server::http((host, port)).map_err(|e| anyhow!("could not bind {host}:{port}: {e}"))?;
    // a loopback bind answers only requests naming it so a rebound name reads nothing
    let named = server.server_addr().to_ip().map(|at| at.port()).filter(|_| loopback_bind(host));

    // the watcher bumps it and the injected client polls it to reload on change
    let version = Arc::new(AtomicU64::new(0));
    spawn_watcher(dir.clone(), version.clone());

    // a LAN bind also advertises the network URL
    let lan = match host {
        "127.0.0.1" | "localhost" => None,
        "0.0.0.0" => lan_ip(),
        h => Some(h.to_string()),
    };
    crate::ui::serve_banner(port, &dir, lan.as_deref());
    if open {
        let _ = open_url(&format!("http://localhost:{port}"));
    }

    for req in server.incoming_requests() {
        if let Some(port) = named
            && !hosted_here(host_of(&req), port)
        {
            let _ = req.respond(Response::from_string("421 misdirected request").with_status_code(421));
            continue;
        }
        let url = req.url().split('?').next().unwrap().to_string();
        if url == "/__livereload" {
            let _ = req.respond(Response::from_string(version.load(Ordering::Relaxed).to_string()));
        } else {
            serve_file(req, &dir, &url, kit.descriptor());
        }
    }
    Ok(())
}

// whether a request path walks above the project root or names a dot entry
fn escapes_root(rel: &str) -> bool {
    rel.split('/').any(|seg| seg == ".." || seg.starts_with('.'))
}

/* whether `path` lies below `dir` once every link on the way resolves */
fn below(dir: &Path, path: &Path) -> bool {
    dir.canonicalize().ok().zip(path.canonicalize().ok()).is_some_and(|(root, real)| real.starts_with(root))
}

fn serve_file(req: tiny_http::Request, dir: &Path, url: &str, d: &Descriptor) {
    let rel = url.trim_start_matches('/');
    // a LAN bind is advertised so a traversal must not leave the served directory
    if escapes_root(rel) {
        let _ = req.respond(Response::from_string("404 not found").with_status_code(404));
        return;
    }
    let mut path = dir.join(rel);
    if rel.is_empty() || path.is_dir() {
        path = path.join("index.html");
    }
    // a link may lead out of the project and its target is never served
    if !below(dir, &path) {
        let _ = req.respond(Response::from_string("404 not found").with_status_code(404));
        return;
    }
    match std::fs::read(&path) {
        Ok(bytes) => {
            let ct = content_type(&path, d);
            let resp = if ct == "text/html" {
                let html = inject_livereload(&String::from_utf8_lossy(&bytes));
                Response::from_string(html).with_header(header("Content-Type", ct))
            } else {
                Response::from_data(bytes).with_header(header("Content-Type", ct))
            };
            let _ = req.respond(resp);
        }
        Err(_) => {
            let _ = req.respond(Response::from_string("404 not found").with_status_code(404));
        }
    }
}

fn header(key: &str, value: &str) -> Header {
    Header::from_bytes(key.as_bytes(), value.as_bytes()).expect("static header is valid")
}

const LIVERELOAD: &str = r#"<script>
let __v;
setInterval(async () => {
  try {
    const n = await (await fetch("/__livereload")).text();
    if (__v === undefined) __v = n;
    else if (n !== __v) location.reload();
  } catch {}
}, 500);
</script>"#;

fn inject_livereload(html: &str) -> String {
    match html.rfind("</body>") {
        Some(i) => format!("{}{}{}", &html[..i], LIVERELOAD, &html[i..]),
        None => format!("{html}{LIVERELOAD}"),
    }
}

// the outbound interface IP from a connected UDP probe that sends no packets
fn lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

// bumps `version` whenever a file under `dir` changes by polling mtimes without a watcher crate
fn spawn_watcher(dir: PathBuf, version: Arc<AtomicU64>) {
    std::thread::spawn(move || {
        let mut last = fingerprint(&dir);
        loop {
            std::thread::sleep(Duration::from_millis(400));
            let now = fingerprint(&dir);
            if now != last {
                last = now;
                version.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
}

// a cheap directory fingerprint summing file mtimes and sizes
fn fingerprint(dir: &Path) -> u64 {
    fn walk(dir: &Path, seen: &mut std::collections::HashSet<PathBuf>, acc: &mut u64) {
        if !crate::host::first_visit(seen, dir) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, seen, acc);
            } else if let Ok(meta) = entry.metadata() {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0);
                *acc = acc.wrapping_add(mtime).wrapping_add(meta.len());
            }
        }
    }
    let mut acc = 0;
    walk(dir, &mut std::collections::HashSet::new(), &mut acc);
    acc
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("open").arg(url).spawn().map(|_| ())
}

#[cfg(target_os = "windows")]
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("cmd").args(["/C", "start", url]).spawn().map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(url).spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{below, escapes_root, fingerprint};

    #[test]
    fn traversal_is_rejected_and_normal_paths_pass() {
        for rel in ["..", "../outside.txt", "../../etc/passwd", "app/../../x", "a/../../b", ".env", ".git/config", "app/.secret", "..a/b"] {
            assert!(escapes_root(rel), "should reject {rel:?}");
        }
        for rel in ["", "index.html", "app/main.mini", "a..b/c", "dist/x..y.js", "edge.json"] {
            assert!(!escapes_root(rel), "should allow {rel:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_project_is_never_served() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("app");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(dir.path().join("secret.txt"), "outside").unwrap();
        std::fs::write(project.join("index.html"), "inside").unwrap();
        std::os::unix::fs::symlink("../secret.txt", project.join("leak.txt")).unwrap();
        std::os::unix::fs::symlink("index.html", project.join("home.html")).unwrap();
        assert!(below(&project, &project.join("index.html")) && below(&project, &project.join("home.html")));
        assert!(!below(&project, &project.join("leak.txt")));
    }

    #[test]
    fn a_loopback_server_answers_only_requests_that_name_it() {
        use crate::host::{hosted_here, loopback_bind};
        assert!(loopback_bind("127.0.0.1") && loopback_bind("localhost") && loopback_bind("::1"));
        assert!(!loopback_bind("0.0.0.0") && !loopback_bind("192.168.1.4"));
        for host in [None, Some("127.0.0.1:5173"), Some("localhost:5173"), Some("LOCALHOST:5173"), Some("[::1]:5173")] {
            assert!(hosted_here(host, 5173), "{host:?}");
        }
        for host in ["attacker.example:5173", "127.0.0.1:5174", "localhost", "127.0.0.1.attacker.example:5173"] {
            assert!(!hosted_here(Some(host), 5173), "{host}");
        }
        assert!(hosted_here(Some("localhost"), 80));
    }

    const SEC: u64 = 1_700_000_000;

    fn write_at(dir: &std::path::Path, name: &str, body: &str, nanos: u32) {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::new(SEC, nanos)).unwrap();
    }

    #[test]
    fn same_second_same_size_edit_bumps_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        write_at(dir.path(), "a.mini", "xxxx", 100);
        let before = fingerprint(dir.path());
        write_at(dir.path(), "a.mini", "yyyy", 200);
        assert_ne!(before, fingerprint(dir.path()));
    }

    #[test]
    fn an_untouched_tree_keeps_its_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        write_at(dir.path(), "a.mini", "xxxx", 100);
        assert_eq!(fingerprint(dir.path()), fingerprint(dir.path()));
    }

    #[test]
    fn a_size_change_at_the_same_mtime_bumps_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        write_at(dir.path(), "a.mini", "xxxx", 100);
        let before = fingerprint(dir.path());
        write_at(dir.path(), "a.mini", "yyyyy", 100);
        assert_ne!(before, fingerprint(dir.path()));
    }

    #[test]
    fn a_new_file_bumps_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        write_at(dir.path(), "a.mini", "xxxx", 100);
        let before = fingerprint(dir.path());
        write_at(dir.path(), "b.mini", "zz", 100);
        assert_ne!(before, fingerprint(dir.path()));
    }

    #[test]
    fn a_whole_second_change_still_bumps_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        write_at(dir.path(), "a.mini", "xxxx", 100);
        let before = fingerprint(dir.path());
        let path = dir.path().join("a.mini");
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::new(SEC + 1, 100)).unwrap();
        assert_ne!(before, fingerprint(dir.path()));
    }
}

