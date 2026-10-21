use anyhow::{anyhow, bail, Context, Result};
use headless_chrome::{Browser, FetcherOptions, LaunchOptions};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tiny_http::{Header, Response, Server};

use crate::cmd::serve::content_type;
use crate::web::{COMPILER_WASM, JS_HOST};

const HARNESS: &str = include_str!("../templates/harness.html");

// What the page publishes, read rather than awaited so a long run streams instead of blocking one call.
const POLL_JS: &str = "window.__edge ? JSON.stringify(window.__edge) : ''";

const POLL_EVERY: Duration = Duration::from_millis(60);

// A wedged page cannot hold the CLI forever, and the engine's own budget is what bounds a hung script.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Deserialize)]
struct State {
    out: String,
    done: bool,
    ok: bool,
    #[serde(default)]
    code: i32,
    #[serde(default)]
    err: String,
}

/// A live browser over the embedded host, reused across sources since launching one costs seconds.
pub struct Host {
    // Held so the process outlives the tab, never used directly again.
    _browser: Browser,
    // One tab navigated per source. Closing tabs drops the CDP connection once the last one goes.
    tab: Arc<headless_chrome::Tab>,
    port: u16,
    // Each source is served under its own key, so a navigation never reads the page the last run left.
    pages: Arc<Mutex<HashMap<String, String>>>,
    next: AtomicUsize,
    secrets: serde_json::Map<String, serde_json::Value>,
}

impl Host {
    /// Launch a browser and serve the harness, the manifest and the embedded host on a loopback port.
    pub fn open(manifest: Option<&Path>) -> Result<Host> {
        // The declared modules keep their own addresses, resolved here so the page is never handed a version.
        let (imports, secrets) = match manifest {
            Some(path) if path.exists() => {
                let mut declared = crate::manifest::Manifest::load(path)?;
                declared.imports = crate::lock::resolved(path, &declared.imports)?;
                let declared = serde_json::to_value(&declared).with_context(|| format!("serializing {}", path.display()))?;
                (declared.to_string().into_bytes(), kept(&declared["permissions"]))
            }
            _ => (b"{}".to_vec(), serde_json::Map::new()),
        };

        // Local imports answer from the directory the manifest sits in, and from nothing outside it.
        let root = manifest.and_then(Path::parent).filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new(".")).canonicalize()?;
        let pages = Arc::new(Mutex::new(HashMap::new()));
        let port = serve(pages.clone(), imports, root)?;
        let browser = launch().context("launching headless Chromium")?;
        let tab = browser.new_tab().map_err(|e| anyhow!("opening a tab: {e}"))?;

        Ok(Host { _browser: browser, tab, port, pages, next: AtomicUsize::new(0), secrets })
    }

    /// Run one source as the script `entry` on the browser host, returning its exit code.
    pub fn run(&self, src: &str, entry: &str) -> Result<i32> {
        let key = self.next.fetch_add(1, Ordering::Relaxed).to_string();
        let page = page(src, entry, &self.secrets)?;
        self.pages.lock().map_err(|e| anyhow!("staging the page: {e}"))?.insert(key.clone(), page);

        self.tab
            .navigate_to(&format!("http://127.0.0.1:{}/{key}", self.port))
            .map_err(|e| anyhow!("navigating to the harness: {e}"))?;
        self.tab.wait_until_navigated().map_err(|e| anyhow!("waiting for page load: {e}"))?;

        let code = drain(&self.tab);
        self.pages.lock().map(|mut pages| pages.remove(&key)).ok();

        code
    }
}

/// Run one source on the browser host, for a caller with nothing else to run.
pub fn run(src: &str, entry: &str, manifest: Option<&Path>) -> Result<i32> {
    Host::open(manifest)?.run(src, entry)
}

/* The harness for one run, filled in once so no program text is read as a placeholder. */
fn page(src: &str, entry: &str, secrets: &serde_json::Map<String, serde_json::Value>) -> Result<String> {
    Ok(HARNESS.replace("__EDGE_RUN__", &embed(&serde_json::json!({ "src": src, "entry": entry, "secrets": secrets, "limits": super::RunLimits::get() }))?))
}

/* The value of `EDGE_SECRET_<name>` for each name a `secret` entry grants, so the page holds no other variable. */
fn kept(permissions: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let entries = permissions.as_object().into_iter().flat_map(|holders| holders.values()).filter_map(|listed| listed.as_array()).flatten();
    entries
        .filter_map(|entry| entry.as_str()?.strip_prefix("secret:"))
        .filter_map(|name| Some((name.to_string(), serde_json::Value::String(std::env::var(format!("EDGE_SECRET_{name}")).ok()?))))
        .collect()
}

/* A value as a JS literal. JSON leaves `<` alone, so a script carrying a closing script tag would end the harness block and run as markup, and escaping it keeps the program's own text intact. */
fn embed(value: &serde_json::Value) -> Result<String> {
    Ok(serde_json::to_string(value)?.replace('<', "\\u003c"))
}

/// Where a browser edge downloaded lives, which `edge uninstall` offers to remove.
pub fn chrome_dir() -> Result<PathBuf> {
    crate::host::data_root().map(|root| root.join("chromium")).map_err(|e| anyhow!("{e}"))
}

/// The browser to drive, `None` when the launcher should fetch one into `chrome_dir`.
pub fn resolve_chrome() -> Result<Option<PathBuf>> {
    if let Some(path) = std::env::var_os("EDGE_CHROME_PATH") {
        return Ok(Some(PathBuf::from(path)));
    }
    if let Ok(found) = headless_chrome::browser::default_executable() {
        return Ok(Some(found));
    }
    // A past download is already in that dir, and the launcher reads it before reaching for the network.
    if fetched()? {
        return Ok(None);
    }
    match agreed()? {
        true => Ok(None),
        false => bail!("--web needs Chrome or Chromium, install one or set EDGE_CHROME_PATH to a binary you have")
    }
}

/// True when edge has already downloaded a browser.
pub fn fetched() -> Result<bool> {
    let dir = chrome_dir()?;
    Ok(std::fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_some()))
}

/* Asks once before downloading a browser, and refuses outright without a terminal so a pipeline never hangs on an answer nobody is there to give. */
fn agreed() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "--web needs Chrome or Chromium and found none\n\
             help: install one, set EDGE_CHROME_PATH, or run this in a terminal to be offered a download"
        );
    }

    crate::ui::confirm("No headless Chromium found. Download one?")
}

/* Every download lands in one dir edge owns, never a shared one, so removing it takes nothing else with it. */
fn fetching() -> Result<FetcherOptions> {
    Ok(FetcherOptions::default()
        .with_install_dir(Some(chrome_dir()?))
        .with_allow_standard_dirs(false)
        .with_allow_download(true))
}

/* Headless with the sandbox off, since a container or WSL usually cannot open one and the engine's own limits are what bound the script. */
fn launch() -> Result<Browser> {
    let found = resolve_chrome()?;
    if found.is_none() && !fetched()? {
        println!("downloading a headless Chromium into {}, this happens once", chrome_dir()?.display());
    }

    let mut builder = LaunchOptions::default_builder();
    builder.sandbox(false).path(found).fetcher_options(fetching()?);
    let options = builder.build().map_err(|e| anyhow!("building launch options: {e}"))?;
    Browser::new(options).map_err(|e| anyhow!("{e}"))
}

/* Streams whatever the page has printed since the last read, then reports how the run ended. */
fn drain(tab: &headless_chrome::Tab) -> Result<i32> {
    let stdout = std::io::stdout();
    let mut written = 0usize;
    let deadline = Instant::now() + RUN_TIMEOUT;

    loop {
        if Instant::now() > deadline {
            bail!("timed out after {}s waiting for the page", RUN_TIMEOUT.as_secs());
        }

        let raw = tab.evaluate(POLL_JS, false).map_err(|e| anyhow!("reading page state: {e}"))?;
        let json = raw.value.as_ref().and_then(|v| v.as_str()).unwrap_or("");
        if json.is_empty() {
            thread::sleep(POLL_EVERY);
            continue;
        }

        let state: State = serde_json::from_str(json).context("parsing page state")?;
        if let Some(fresh) = state.out.get(written..) {
            let _ = write!(stdout.lock(), "{fresh}");
            let _ = stdout.lock().flush();
            written = state.out.len();
        }

        if state.done {
            if !state.ok {
                crate::ui::traceback(&state.err);
                return Ok(if state.code == 0 { 1 } else { state.code });
            }
            return Ok(state.code);
        }
        thread::sleep(POLL_EVERY);
    }
}

/* Serves the staged pages, the project's manifest, its files and the embedded host on a loopback port, so nothing but the declared modules leaves the machine. */
fn serve(pages: Arc<Mutex<HashMap<String, String>>>, imports: Vec<u8>, root: PathBuf) -> Result<u16> {
    let server = Server::http("127.0.0.1:0").map_err(|e| anyhow!("starting the local server: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("the local server has no TCP address"))?
        .port();

    thread::spawn(move || {
        for req in server.incoming_requests() {
            let path = req.url().split('?').next().unwrap_or("/").trim_start_matches('/').to_string();
            let served = match path.as_str() {
                "edge.json" => Some((imports.clone(), content_type(Path::new("edge.json")))),
                // What fs may list, walked on each ask as a dev server does, since http cannot list a folder.
                "edge.files" => {
                    let manifest: serde_json::Value = serde_json::from_slice(&imports).unwrap_or_default();
                    let index = crate::files::index(&root, manifest.get("permissions"), crate::files::INDEX_LIMIT);
                    Some((serde_json::json!(index).to_string().into_bytes(), "application/json"))
                }
                "compiler.wasm" => Some((COMPILER_WASM.to_vec(), content_type(Path::new("compiler.wasm")))),
                _ => match host_file(&path) {
                    Some(found) => Some(found),
                    None => staged(&pages, &path).or_else(|| project_file(&root, &path)),
                }
            };
            let _ = match served {
                Some((bytes, kind)) => req.respond(Response::from_data(bytes).with_header(header(kind))),
                None => req.respond(Response::from_string("not found").with_status_code(404)),
            };
        }
    });

    Ok(port)
}

/* The page a run staged under its own key, so two sources never share a url the browser could cache. */
fn staged(pages: &Arc<Mutex<HashMap<String, String>>>, key: &str) -> Option<(Vec<u8>, &'static str)> {
    let page = pages.lock().ok()?.get(key)?.clone();
    Some((page.into_bytes(), "text/html"))
}

/* A file of the project itself, so a local import answers, and never one that resolves outside the project. */
fn project_file(root: &Path, path: &str) -> Option<(Vec<u8>, &'static str)> {
    let file = root.join(path).canonicalize().ok().filter(|file| file.starts_with(root))?;
    Some((std::fs::read(&file).ok()?, content_type(&file)))
}

/* The embedded host answers under the same `js/` prefix a dist and the CDN use, so the harness imports read the same everywhere. */
fn host_file(path: &str) -> Option<(Vec<u8>, &'static str)> {
    let rel = path.strip_prefix("js/")?;
    let (_, bytes) = JS_HOST.iter().find(|(key, _)| *key == rel)?;
    Some((bytes.to_vec(), content_type(Path::new(rel))))
}

fn header(value: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).expect("a static header is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The override answers before anything is searched for, so a build farm can pin one browser and never be asked.
    #[test]
    fn an_explicit_browser_wins() {
        // SAFETY: single-threaded test, nothing else reads the environment here.
        unsafe { std::env::set_var("EDGE_CHROME_PATH", "/tmp/not-a-browser") };
        assert_eq!(resolve_chrome().unwrap(), Some(PathBuf::from("/tmp/not-a-browser")));
        assert!(chrome_dir().unwrap().ends_with("chromium"));
        unsafe { std::env::remove_var("EDGE_CHROME_PATH") };
    }

    /* The source crosses into the page as a JSON string, so a script carrying a closing script tag or a quote cannot break out of the harness. */
    #[test]
    fn the_harness_cannot_be_escaped_by_a_script() {
        let hostile = "print('</script><script>alert(1)</script>')";
        let page = page(hostile, "sub/main.py", &serde_json::Map::new()).unwrap();

        assert!(!page.contains("<script>alert(1)"));
        assert!(page.contains("\\u003c/script"));
        // The program and its entry still read as written once the page decodes the literal.
        let run = serde_json::json!({ "src": hostile, "entry": "sub/main.py" });
        assert_eq!(serde_json::from_str::<serde_json::Value>(&embed(&run).unwrap()).unwrap(), run);
    }

    // The page carries a secret only when a grant names it, so no other variable reaches the browser.
    #[test]
    fn the_page_holds_only_the_secrets_a_grant_names() {
        // SAFETY: names no other test reads or writes.
        unsafe {
            std::env::set_var("EDGE_SECRET_KEPT_GRANTED", "yes");
            std::env::set_var("EDGE_SECRET_KEPT_OTHER", "no");
        }
        let permissions = serde_json::json!({ "main": ["secret:KEPT_GRANTED", "secret:KEPT_UNSET", "net:a.test"], "all": ["secret"] });
        assert_eq!(serde_json::Value::Object(kept(&permissions)), serde_json::json!({ "KEPT_GRANTED": "yes" }));
        unsafe {
            std::env::remove_var("EDGE_SECRET_KEPT_GRANTED");
            std::env::remove_var("EDGE_SECRET_KEPT_OTHER");
        }
    }

    #[test]
    fn the_host_answers_under_the_js_prefix() {
        assert!(host_file("js/src/index.js").is_some());
        assert!(host_file("js/src/nope.js").is_none());
        assert!(host_file("src/index.js").is_none());
    }

    // Two sources must never share a url, or the browser serves the first page to the second run.
    #[test]
    fn a_staged_page_answers_only_under_its_own_key() {
        let pages = Arc::new(Mutex::new(HashMap::from([("0".to_string(), "<html>first</html>".to_string())])));

        assert!(staged(&pages, "0").is_some());
        assert!(staged(&pages, "1").is_none());
    }

    // A local import answers from inside the project, and a path climbing out of it answers nothing.
    #[test]
    fn a_project_file_answers_only_from_inside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        std::fs::write(dir.path().join("app/helper.py"), "x = 1").unwrap();
        std::fs::write(dir.path().join("secret.py"), "key = 2").unwrap();
        let root = dir.path().join("app").canonicalize().unwrap();

        assert_eq!(project_file(&root, "helper.py").map(|(bytes, _)| bytes), Some(b"x = 1".to_vec()));
        assert!(project_file(&root, "../secret.py").is_none());
        assert!(project_file(&root, "missing.py").is_none());
    }
}
