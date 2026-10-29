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

use crate::kit::{Descriptor, Kit, Limits};
use crate::web::{content_type, host_of, JS_HOST};

const HARNESS: &str = include_str!("../templates/harness.html");

// polled rather than awaited so a long run streams instead of blocking one call
const POLL_JS: &str = "window.__edge ? JSON.stringify(window.__edge) : ''";

const POLL_EVERY: Duration = Duration::from_millis(60);

// a wedged page cannot hold the CLI forever and the engine budget bounds hung scripts
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

// a live browser over the embedded host reused across sources since a launch costs seconds
pub struct Host {
    // held so the process outlives the tab
    _browser: Browser,
    // one tab navigated per source since closing the last tab drops the CDP connection
    tab: Arc<headless_chrome::Tab>,
    port: u16,
    // a key per source so a navigation never reads the page of the last run
    pages: Arc<Mutex<HashMap<String, String>>>,
    next: AtomicUsize,
    // how the runtime words a run error which a page reports in the engine's text
    words: Option<crate::kit::Words>,
    // what each run holds as the native engine would hold it
    limits: Limits,
}

impl Host {
    // launches a browser and serves the run files on a loopback port
    pub fn open(kit: &Kit, manifest: Option<&Path>) -> Result<Host> {
        // declared modules resolve to their addresses here so the page never sees a version
        let imports = match manifest {
            Some(path) if path.exists() => {
                let mut declared = kit.manifest(path)?;
                declared.imports = crate::lock::resolved(path, &declared.imports)?;
                serde_json::to_vec(&declared).with_context(|| format!("serializing {}", path.display()))?
            }
            _ => b"{}".to_vec(),
        };

        // local imports answer from the manifest directory and nothing outside it
        let root = manifest.and_then(Path::parent).filter(|dir| !dir.as_os_str().is_empty()).unwrap_or(Path::new(".")).canonicalize()?;
        let pages = Arc::new(Mutex::new(HashMap::new()));
        let port = serve(Served { pages: pages.clone(), imports, root, wasm: kit.runtime().module().into(), descriptor: kit.descriptor() })?;
        let browser = launch().context("launching headless Chromium")?;
        let tab = browser.new_tab().map_err(|e| anyhow!("opening a tab: {e}"))?;

        let limits = super::RunLimits::get().over(kit.run_limits());
        Ok(Host { _browser: browser, tab, port, pages, next: AtomicUsize::new(0), words: kit.words(), limits })
    }

    // runs `src` under the script name `entry` and returns its exit code
    pub fn run(&self, src: &str, entry: &str) -> Result<i32> {
        let key = self.next.fetch_add(1, Ordering::Relaxed).to_string();
        let page = page(src, entry, self.limits)?;
        self.pages.lock().map_err(|e| anyhow!("staging the page: {e}"))?.insert(key.clone(), page);

        self.tab
            .navigate_to(&format!("http://127.0.0.1:{}/{key}", self.port))
            .map_err(|e| anyhow!("navigating to the harness: {e}"))?;
        self.tab.wait_until_navigated().map_err(|e| anyhow!("waiting for page load: {e}"))?;

        let code = drain(&self.tab, self.words, src);
        self.pages.lock().map(|mut pages| pages.remove(&key)).ok();

        code
    }
}

// a one-off run for a caller with nothing else to run
pub fn run(kit: &Kit, src: &str, entry: &str, manifest: Option<&Path>) -> Result<i32> {
    Host::open(kit, manifest)?.run(src, entry)
}

/* the harness of one run filled once so program text never reads as a placeholder */
fn page(src: &str, entry: &str, limits: Limits) -> Result<String> {
    let limits = serde_json::json!({ "memory": limits.memory >> 20, "ops": limits.ops });
    Ok(HARNESS.replace("__EDGE_RUN__", &embed(&serde_json::json!({ "src": src, "entry": entry, "limits": limits }))?))
}

/* a JS literal escaping `<` so a closing script tag cannot end the harness block */
fn embed(value: &serde_json::Value) -> Result<String> {
    Ok(serde_json::to_string(value)?.replace('<', "\\u003c"))
}

// where a browser edge downloaded lives which `edge uninstall` offers to remove
pub fn chrome_dir() -> Result<PathBuf> {
    crate::host::data_root().map(|root| root.join("chromium")).map_err(|e| anyhow!("{e}"))
}

// the browser to drive or `None` when the launcher should fetch one into `chrome_dir`
pub fn resolve_chrome() -> Result<Option<PathBuf>> {
    if let Some(path) = std::env::var_os("EDGE_CHROME_PATH") {
        return Ok(Some(PathBuf::from(path)));
    }
    if let Ok(found) = headless_chrome::browser::default_executable() {
        return Ok(Some(found));
    }
    // the launcher reads a past download from that dir before reaching for the network
    if fetched()? {
        return Ok(None);
    }
    match agreed()? {
        true => Ok(None),
        false => bail!("--web needs Chrome or Chromium, install one or set EDGE_CHROME_PATH to a binary you have")
    }
}

// whether edge already downloaded a browser
pub fn fetched() -> Result<bool> {
    let dir = chrome_dir()?;
    Ok(std::fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_some()))
}

/* refuses without a terminal so a pipeline never hangs on an answer nobody can give */
fn agreed() -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "--web needs Chrome or Chromium and found none\n\
             help: install one, set EDGE_CHROME_PATH, or run this in a terminal to be offered a download"
        );
    }

    print!("No headless Chromium found. Download one? [y/N] ");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| anyhow!("reading the answer: {e}"))?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}

/* downloads land in a dir only edge owns so removing it takes nothing else along */
fn fetching() -> Result<FetcherOptions> {
    Ok(FetcherOptions::default()
        .with_install_dir(Some(chrome_dir()?))
        .with_allow_standard_dirs(false)
        .with_allow_download(true))
}

/* no sandbox since containers and WSL often lack one and engine limits bound the script */
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

/* streams what the page printed since the last read then reports how the run ended */
fn drain(tab: &headless_chrome::Tab, words: Option<crate::kit::Words>, src: &str) -> Result<i32> {
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
                crate::ui::traceback(&crate::kit::worded(words, src, &state.err));
                return Ok(if state.code == 0 { 1 } else { state.code });
            }
            return Ok(state.code);
        }
        thread::sleep(POLL_EVERY);
    }
}

/* what the loopback server answers with the runtime module the page boots */
struct Served {
    pages: Arc<Mutex<HashMap<String, String>>>,
    imports: Vec<u8>,
    root: PathBuf,
    wasm: Arc<[u8]>,
    descriptor: &'static Descriptor,
}

/* serves everything on loopback so nothing but the declared modules leaves the machine */
fn serve(served: Served) -> Result<u16> {
    let Served { pages, imports, root, wasm, descriptor } = served;
    let server = Server::http("127.0.0.1:0").map_err(|e| anyhow!("starting the local server: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("the local server has no TCP address"))?
        .port();

    thread::spawn(move || {
        for req in server.incoming_requests() {
            // a page on a rebound name reaches this port too and reads nothing
            if !super::hosted_here(host_of(&req), port) {
                let _ = req.respond(Response::from_string("misdirected request").with_status_code(421));
                continue;
            }
            let path = req.url().split('?').next().unwrap_or("/").trim_start_matches('/').to_string();
            let served = match path.as_str() {
                "edge.json" => Some((imports.clone(), content_type(Path::new("edge.json"), descriptor))),
                "edge.wasm" => Some((wasm.to_vec(), content_type(Path::new("edge.wasm"), descriptor))),
                _ => match host_file(&path, descriptor) {
                    Some(found) => Some(found),
                    None => staged(&pages, &path).or_else(|| project_file(&root, &path, descriptor)),
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

/* a run page under its own key so the browser cache never mixes two sources */
fn staged(pages: &Arc<Mutex<HashMap<String, String>>>, key: &str) -> Option<(Vec<u8>, &'static str)> {
    let page = pages.lock().ok()?.get(key)?.clone();
    Some((page.into_bytes(), "text/html"))
}

/* a project file for a local import and never one resolving outside the project */
fn project_file(root: &Path, path: &str, d: &Descriptor) -> Option<(Vec<u8>, &'static str)> {
    let file = root.join(path).canonicalize().ok().filter(|file| file.starts_with(root))?;
    Some((std::fs::read(&file).ok()?, content_type(&file, d)))
}

/* the same `js/` prefix a dist and the CDN use so harness imports read alike */
fn host_file(path: &str, d: &Descriptor) -> Option<(Vec<u8>, &'static str)> {
    let rel = path.strip_prefix("js/")?;
    let (_, bytes) = JS_HOST.iter().find(|(key, _)| *key == rel)?;
    Some((bytes.to_vec(), content_type(Path::new(rel), d)))
}

fn header(value: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], value.as_bytes()).expect("a static header is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    // the override precedes any search so a build farm can pin a browser without prompts
    #[test]
    fn an_explicit_browser_wins() {
        // SAFETY single-threaded test where nothing else reads the environment
        unsafe { std::env::set_var("EDGE_CHROME_PATH", "/tmp/not-a-browser") };
        assert_eq!(resolve_chrome().unwrap(), Some(PathBuf::from("/tmp/not-a-browser")));
        assert!(chrome_dir().unwrap().ends_with("chromium"));
        unsafe { std::env::remove_var("EDGE_CHROME_PATH") };
    }

    /* the source enters the page as a JSON string that no tag or quote escapes */
    #[test]
    fn the_harness_cannot_be_escaped_by_a_script() {
        let hostile = "print('</script><script>alert(1)</script>')";
        let page = page(hostile, "sub/main.mini", Limits::sandbox()).unwrap();

        assert!(!page.contains("<script>alert(1)"));
        assert!(page.contains("\\u003c/script"));
        // the program and its entry still read as written once the page decodes the literal
        let run = serde_json::json!({ "src": hostile, "entry": "sub/main.mini" });
        assert_eq!(serde_json::from_str::<serde_json::Value>(&embed(&run).unwrap()).unwrap(), run);
    }

    #[test]
    fn the_host_answers_under_the_js_prefix() {
        let d = &mini::DESCRIPTOR;
        assert!(host_file("js/src/index.js", d).is_some());
        assert!(host_file("js/src/nope.js", d).is_none());
        assert!(host_file("src/index.js", d).is_none());
    }

    // a shared url would let the browser serve the first page to the second run
    #[test]
    fn a_staged_page_answers_only_under_its_own_key() {
        let pages = Arc::new(Mutex::new(HashMap::from([("0".to_string(), "<html>first</html>".to_string())])));

        assert!(staged(&pages, "0").is_some());
        assert!(staged(&pages, "1").is_none());
    }

    #[test]
    fn a_project_file_answers_only_from_inside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        std::fs::write(dir.path().join("app/helper.mini"), "x = 1").unwrap();
        std::fs::write(dir.path().join("secret.mini"), "key = 2").unwrap();
        let root = dir.path().join("app").canonicalize().unwrap();

        let d = &mini::DESCRIPTOR;
        assert_eq!(project_file(&root, "helper.mini", d), Some((b"x = 1".to_vec(), "text/plain")));
        assert!(project_file(&root, "../secret.mini", d).is_none());
        assert!(project_file(&root, "missing.mini", d).is_none());
    }
}
