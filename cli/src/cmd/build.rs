use anyhow::{anyhow, bail, Context, Result};
use crate::docs;
use crate::host::driver::{self, RunOpts};
use crate::host::scan::Scan;
use crate::host::{cdn, get};
use crate::kit::{self, Command, Descriptor, Kit};
use crate::pack::{Bundle, Entry, STANDALONE_MAGIC};
use clap::{ArgMatches, Args, FromArgMatches, Parser};
use edge::deps::parse_integrity;
use edge::util::sha256::sha256;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::lock;

#[derive(Args)]
struct BuildArgs {
    /// Output path, defaults to app.edge, app, or dist/ per mode.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Vendor the JS host into dist/ instead of a packed artifact.
    #[arg(long)]
    web: bool,
    /// Emit a standalone binary that runs with nothing installed.
    #[arg(long)]
    app: bool,
}

/* `build` packing a portable .edge or a standalone binary or a browser dist */
pub struct Build;

impl Command for Build {
    fn spec(&self, _kit: &Kit) -> clap::Command {
        BuildArgs::augment_args(clap::Command::new("build").about("Pack the project, a portable .edge by default, --app for a binary, --web for the browser"))
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("build".to_string(), "Pack a portable .edge  (--app, --web)".to_string())
    }

    fn compiles(&self) -> bool {
        true
    }

    /* a standalone binary runs the project it carries instead of parsing subcommands */
    fn boot(&self, kit: &Kit) -> Option<Result<i32>> {
        let payload = crate::pack::embedded_payload()?;
        Some(run_carried(kit, &payload))
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        let args = BuildArgs::from_arg_matches(m)?;
        let manifest = kit::manifest_path(m);
        if args.web {
            run(kit, &manifest, args.out.unwrap_or_else(|| PathBuf::from("dist")))?;
        } else if args.app {
            standalone(kit, &manifest, args.out.unwrap_or_else(|| PathBuf::from("app")))?;
        } else {
            bundle(kit, &manifest, args.out.unwrap_or_else(|| PathBuf::from("app.edge")))?;
        }
        Ok(0)
    }
}

// the flags of `edge run` that a standalone binary understands
#[derive(Parser)]
#[command(name = "edge-app", disable_help_flag = true)]
struct Carried {
    #[arg(long)]
    save_state: Option<PathBuf>,
    #[arg(long)]
    restore_state: Option<PathBuf>,
    #[arg(long)]
    preempt: Option<usize>,
    #[arg(long)]
    events: Option<PathBuf>,
}

fn run_carried(kit: &Kit, payload: &[u8]) -> Result<i32> {
    let flags = Carried::parse();
    let opts = RunOpts {
        manifest: None,
        preempt: flags.preempt.unwrap_or(0),
        events: flags.events.map(|p| p.to_string_lossy().into_owned()),
        save_state: flags.save_state.map(|p| p.to_string_lossy().into_owned()),
        restore_state: flags.restore_state.map(|p| p.to_string_lossy().into_owned()),
    };
    driver::run_bundle(kit, payload, &opts)
}

/* packs the project as this exe with the bundle and a trailer appended */
fn standalone(kit: &Kit, manifest_path: &Path, out: PathBuf) -> Result<()> {
    // nobody reads docs out of an executable so an app binary leaves them behind
    let bundle = collect_bundle(kit, manifest_path, false)?;
    let files = bundle.files.len();
    let exe = std::env::current_exe().context("locating the edge binary")?;
    let mut image = fs::read(&exe).with_context(|| format!("reading {}", exe.display()))?;
    let payload = bundle.encode();
    image.extend_from_slice(&payload);
    image.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    image.extend_from_slice(STANDALONE_MAGIC);
    fs::write(&out, &image).with_context(|| format!("writing {}", out.display()))?;
    make_executable(&out)?;
    let run = out.display();
    crate::ui::packed(&out, files, 0, image.len() as u64,
        &format!("run  ./{run}   flags  --save-state --restore-state --preempt --events"));
    Ok(())
}

/* packs the project as a portable .edge for any host that already has the CLI */
fn bundle(kit: &Kit, manifest_path: &Path, out: PathBuf) -> Result<()> {
    let bundle = collect_bundle(kit, manifest_path, true)?;
    let payload = bundle.encode();
    fs::write(&out, &payload).with_context(|| format!("writing {}", out.display()))?;
    let run = out.display();
    let pages = bundle.files.iter().filter(|f| f.path.starts_with(docs::PREFIX)).count();
    crate::ui::packed(&out, bundle.files.len() - pages, pages, payload.len() as u64,
        &format!("run  {} run {run}   or send it to an actor eval group", kit.name()));
    Ok(())
}

/* bundles the project files and every url module its manifests declare */
fn collect_bundle(kit: &Kit, manifest_path: &Path, with_docs: bool) -> Result<Bundle> {
    let project = match manifest_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let scripts = collect_scripts(kit, &project, Path::new(""));
    if !scripts.iter().any(|s| kit.is_source(s)) {
        return Err(anyhow!("no .{} files found under {}", kit.descriptor().extension, project.display()));
    }
    let notices = collect_notices(kit, &project);
    let mut scan = Scan::new(kit);
    let mut files = Vec::new();
    for s in scripts.iter().chain(&notices) {
        let rel = s.strip_prefix(&project).unwrap_or(s).to_string_lossy().replace('\\', "/");
        // the root manifest and its lock go in below under fixed names
        if rel == "edge.json" || rel == lock::FILE { continue; }
        files.push(Entry { path: rel, bytes: fs::read(s).with_context(|| format!("reading {}", s.display()))? });
    }
    if manifest_path.exists() {
        files.push(Entry { path: "edge.json".to_string(), bytes: fs::read(manifest_path)? });
        // the lock travels with its manifest so the bundle resolves names as its project did
        let beside = manifest_path.with_file_name(lock::FILE);
        if beside.exists() {
            files.push(Entry { path: lock::FILE.to_string(), bytes: fs::read(&beside)? });
        }
        let manifest = kit.manifest(manifest_path)?;
        if with_docs {
            if let Some(clash) = files.iter().find(|f| f.path.starts_with(docs::PREFIX)) {
                bail!("'{}' takes the prefix the docs are packed under", clash.path);
            }
            let pages = docs::collect(&project, manifest.text("docs"), kit.descriptor())?;
            files.extend(pages.into_iter().map(|(path, bytes)| Entry { path, bytes }));
        }
        vendor_bundle(&mut scan, &lock::resolved(manifest_path, &manifest.imports)?, &mut files)?;
    }
    // a nested package answers to its own manifest so its url modules ride along too
    let nested: Vec<PathBuf> = files.iter().filter(|f| f.path.ends_with("/edge.json")).map(|f| project.join(&f.path)).collect();
    for path in nested {
        vendor_bundle(&mut scan, &lock::resolved(&path, &kit.manifest(&path)?.imports)?, &mut files)?;
    }
    let mut kept = HashSet::new();
    files.retain(|f| kept.insert(f.path.clone()));
    Ok(Bundle { entry: find_entry(kit.descriptor(), &scripts, &project), files })
}

/* the readme and licenses at the project root sorted so two builds agree */
fn collect_notices(kit: &Kit, project: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(project) else { return Vec::new() };
    let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_notice(kit, p)).collect();
    found.sort();
    found
}

/* `README.md` or `LICENSE` with any extension unless the script walk already carries it */
fn is_notice(kit: &Kit, path: &Path) -> bool {
    if kit.is_source(path) {
        return false;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.eq_ignore_ascii_case("README.md") || name.split('.').next().is_some_and(|s| s.eq_ignore_ascii_case("LICENSE"))
}

/* each declared url module and the files it reaches keyed by the address they answer */
fn vendor_bundle(scan: &mut Scan, imports: &BTreeMap<String, String>, files: &mut Vec<Entry>) -> Result<()> {
    refuse_javascript(scan.descriptor(), imports)?;
    let mut seen = HashSet::new();
    for (name, spec) in imports.iter().filter(|(_, spec)| spec.contains("://")) {
        let (url, pin) = parse_integrity(spec).map_err(|e| anyhow!(e))?;
        let path = url.split('?').next().unwrap_or(url);
        let (base, entry) = path.rsplit_once('/').ok_or_else(|| anyhow!("'{url}' names no file"))?;
        let bytes = read_package(url)?.ok_or_else(|| anyhow!("fetching {url}: not found"))?;
        if pin.is_some_and(|want| sha256(&bytes) != want) {
            bail!("integrity check failed for '{url}'");
        }
        if url != path {
            // a run asks for the address with its query so the packed copy answers that
            files.push(Entry { path: url.to_string(), bytes: bytes.clone() });
        }
        carry_tree(scan, base, entry, bytes, &mut seen, files).with_context(|| format!("bundling '{name}'"))?;
    }
    Ok(())
}

/* no host loads JavaScript but its own so a build declaring one stops before packing */
fn refuse_javascript(d: &Descriptor, imports: &BTreeMap<String, String>) -> Result<()> {
    for (name, spec) in imports {
        if edge::deps::rules::javascript(spec) {
            bail!("module '{name}' is JavaScript, ship a .{} or a .wasm", d.extension);
        }
    }
    Ok(())
}

/* walks `entry` and everything it imports storing each file under its own url */
fn carry_tree(scan: &mut Scan, base: &str, entry: &str, bytes: Vec<u8>, seen: &mut HashSet<String>, files: &mut Vec<Entry>) -> Result<()> {
    let mut queue = vec![(entry.to_string(), Some(bytes), true)];
    while let Some((rel, bytes, required)) = queue.pop() {
        let url = format!("{base}/{rel}");
        if !seen.insert(url.clone()) {
            continue;
        }
        let bytes = match bytes.map_or_else(|| read_package(&url), |b| Ok(Some(b)))? {
            Some(bytes) => bytes,
            None if required => bail!("fetching {url}: not found"),
            None => continue,
        };
        for (dep, needed) in file_deps(scan, base, &rel, &bytes)? {
            let dep = dep.map_err(|spec| anyhow!("'{url}' imports '{spec}' from outside its directory"))?;
            queue.push((dep, None, needed));
        }
        files.push(Entry { path: url, bytes });
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).with_context(|| format!("setting mode on {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

use crate::web::JS_HOST;

const INDEX_HTML: &str = include_str!("../templates/dist.html");

// packs a browser dist with the JS host the runtime and packages vendored
fn run(kit: &Kit, manifest_path: &Path, out_dir: PathBuf) -> Result<()> {
    let t0 = Instant::now();
    let mut manifest = kit.manifest(manifest_path)?;
    // `Path::parent` gives `Some("")` for a bare filename so it falls back to `.`
    let project = match manifest_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };

    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let sp = crate::ui::spinner("writing the JS host");
    match vendor_js(&out_dir) {
        Ok(()) => sp.done("wrote the JS host"),
        Err(e) => { sp.fail("failed to write the JS host"); return Err(e); }
    }

    fs::write(out_dir.join("edge.wasm"), kit.page_module()?).context("writing edge.wasm")?;

    let scripts = collect_scripts(kit, &project, &out_dir);
    let sp = crate::ui::spinner("vendoring packages");
    let vendored = match vendor_packages(&mut Scan::new(kit), &lock::resolved(manifest_path, &manifest.imports)?, &out_dir) {
        Ok(v) => v,
        Err(e) => { sp.fail("failed to vendor packages"); return Err(e); }
    };
    sp.done("vendored packages");
    let script_count = copy_scripts(&scripts, &project, &out_dir)?;

    // vendored paths replace their urls and every other entry and key stays as written
    let packages = vendored.len();
    manifest.imports.extend(vendored);
    manifest.save(&out_dir.join("edge.json"))?;

    let entry = find_entry(kit.descriptor(), &scripts, &project);
    fs::write(out_dir.join("index.html"), index_html(&entry))?;

    crate::ui::build_report(
        &out_dir,
        JS_HOST.len(),
        packages,
        script_count,
        dir_size(&out_dir)?,
        t0.elapsed(),
    );
    Ok(())
}

// writes the embedded JS host into `dist/js/` mirroring its CDN layout
fn vendor_js(out_dir: &Path) -> Result<()> {
    for (rel, bytes) in JS_HOST {
        let path = out_dir.join("js").join(rel);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

// the source wasm and manifest files of the project skipping hidden dirs and the output
fn collect_scripts(kit: &Kit, project: &Path, out_dir: &Path) -> Vec<PathBuf> {
    let mut scripts = Vec::new();
    let out_dir = fs::canonicalize(out_dir).unwrap_or_else(|_| out_dir.to_path_buf());
    walk(kit, project, &out_dir, &mut HashSet::new(), &mut scripts);
    scripts
}

fn walk(kit: &Kit, dir: &Path, out_dir: &Path, seen: &mut HashSet<PathBuf>, found: &mut Vec<PathBuf>) {
    if !crate::host::first_visit(seen, dir) {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // `read_dir` yields `./dist` while `out_dir` is `dist` so canonical forms are compared
        if path.is_dir()
            && fs::canonicalize(&path).ok().as_deref() == Some(out_dir) {
                continue;
            }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        if path.is_dir() {
            walk(kit, &path, out_dir, seen, found);
        } else if name == "edge.json" || name == lock::FILE || kit.is_source(&path) || path.extension().is_some_and(|e| e == "wasm") {
            found.push(path);
        }
    }
}

// every declared url lands in `dist/vendor/<name>/` with the files it reaches beside it
fn vendor_packages(scan: &mut Scan, imports: &BTreeMap<String, String>, out_dir: &Path) -> Result<BTreeMap<String, String>> {
    refuse_javascript(scan.descriptor(), imports)?;
    let mut local = BTreeMap::new();
    for (name, spec) in imports.iter().filter(|(_, spec)| spec.contains("://")) {
        let (url, pin) = parse_integrity(spec).map_err(|e| anyhow!(e))?;
        let path = url.split('?').next().unwrap_or(url);
        let (base, entry) = path.rsplit_once('/').ok_or_else(|| anyhow!("'{url}' names no file"))?;
        let bytes = read_package(url)?.ok_or_else(|| anyhow!("fetching {url}: not found"))?;
        if pin.is_some_and(|want| sha256(&bytes) != want) {
            bail!("integrity check failed for '{url}'");
        }
        let dest = format!("vendor/{name}");
        vendor_tree(scan, base, entry, bytes, out_dir, &dest)?;
        local.insert(name.clone(), format!("./{dest}/{entry}"));
    }
    Ok(local)
}

/* copies `entry` into `dest` with every file its imports reach at its relative path */
fn vendor_tree(scan: &mut Scan, base: &str, entry: &str, bytes: Vec<u8>, out_dir: &Path, dest: &str) -> Result<()> {
    let mut queue = vec![(entry.to_string(), Some(bytes), true)];
    let mut seen = HashSet::new();
    while let Some((rel, bytes, required)) = queue.pop() {
        if !seen.insert(rel.clone()) {
            continue;
        }
        let url = format!("{base}/{rel}");
        let bytes = match bytes.map_or_else(|| read_package(&url), |b| Ok(Some(b)))? {
            Some(bytes) => bytes,
            None if required => bail!("fetching {url}: not found"),
            None => continue,
        };
        for (dep, needed) in file_deps(scan, base, &rel, &bytes)? {
            let dep = dep.map_err(|spec| anyhow!("'{url}' imports '{spec}' from outside its directory, which a web build cannot vendor"))?;
            queue.push((dep, None, needed));
        }
        write_under(out_dir, &format!("{dest}/{rel}"), &bytes)?;
    }
    Ok(())
}

/* resolves `spec` against the file `from` and None when it climbs out of the tree */
fn join(from: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            seg => parts.push(seg),
        }
    }
    Some(parts.join("/"))
}

/* a dependency under the package base or the spec that reaches outside it */
type Dep = Result<String, String>;

/* what a vendored file pulls in beside it each flagged when the build needs it */
fn file_deps(scan: &mut Scan, base: &str, rel: &str, bytes: &[u8]) -> Result<Vec<(Dep, bool)>> {
    let text = String::from_utf8_lossy(bytes);
    let ext = Path::new(rel).extension().and_then(|e| e.to_str()).unwrap_or("");
    let beside = |spec: &str, needed| {
        let clean = spec.split(['?', '#']).next().unwrap_or(spec);
        (join(rel, clean).ok_or_else(|| spec.to_string()), needed)
    };
    Ok(match ext {
        "json" => {
            let manifest: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("parsing {rel}"))?;
            let imports = manifest.get("imports").and_then(|i| i.as_object()).into_iter().flatten();
            imports
                .filter_map(|(_, target)| target.as_str())
                // a version names no file since the lock beside this manifest points it at one
                .filter(|target| !target.contains("://") && !target.starts_with('/') && !edge::deps::rules::shaped_like_version(target))
                .map(|target| beside(target, true))
                .chain([beside(lock::FILE, false)])
                .collect()
        }
        // a plugin and a packed package each carry all they need
        "wasm" | "edge" => Vec::new(),
        _ if bytes.starts_with(b"\0asm") || bytes.starts_with(crate::pack::MAGIC) => Vec::new(),
        // source with what the runtime reads as its relative imports plus the manifest beside it
        _ => {
            let joined = scan.relative(&format!("{base}/{rel}"), &text)?;
            let under = |spec: String| match spec.strip_prefix(base).and_then(|path| path.strip_prefix('/')) {
                Some(path) => Ok(path.to_string()),
                None => Err(spec),
            };
            joined.into_iter().map(|spec| (under(spec), true)).chain([beside("./edge.json", false)]).collect()
        }
    })
}

/* the bytes of a package file and None when the host has no such file */
fn read_package(url: &str) -> Result<Option<Vec<u8>>> {
    // errors name the address requested since a staging origin is where a failure needs looking
    let source = cdn(url);
    match get(&source) {
        Ok(mut resp) => Ok(Some(crate::host::body(&mut resp).map_err(|e| anyhow!("reading {source}: {e}"))?)),
        Err(ureq::Error::StatusCode(404)) => Ok(None),
        Err(e) => Err(anyhow!("fetching {source}: {e}")),
    }
}

fn write_under(root: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    let path = root.join(rel);
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(&path, bytes)?;
    Ok(())
}

fn copy_scripts(scripts: &[PathBuf], project: &Path, out_dir: &Path) -> Result<usize> {
    let mut count = 0usize;
    for s in scripts {
        let rel = s.strip_prefix(project).unwrap_or(s);
        // the root lock stays out since every root import is vendored to a path here
        if rel.as_os_str() == lock::FILE {
            continue;
        }
        let dest = out_dir.join(rel);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(s, &dest)?;
        count += 1;
    }
    Ok(count)
}

// an entry name the runtime lists if present and otherwise the shallowest source
fn find_entry(d: &Descriptor, scripts: &[PathBuf], project: &Path) -> String {
    let rel = |s: &PathBuf| s.strip_prefix(project).ok().map(|p| p.to_string_lossy().replace('\\', "/"));
    // the shallowest match wins so the entry of a package never outranks the root one
    let shallowest = |found: Vec<String>| found.into_iter().min_by_key(|r| (r.matches('/').count(), r.clone()));
    for &c in d.entries {
        if let Some(r) = shallowest(scripts.iter().filter(|s| s.file_name().and_then(|n| n.to_str()) == Some(c)).filter_map(rel).collect()) {
            return r;
        }
    }
    shallowest(scripts.iter().filter(|s| s.extension().is_some_and(|e| e == d.extension)).filter_map(rel).collect())
        .unwrap_or_else(|| d.entry().to_string())
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries.flatten() {
        let m = entry.metadata()?;
        if m.is_dir() {
            total = total.saturating_add(dir_size(&entry.path())?);
        } else {
            total = total.saturating_add(m.len());
        }
    }
    Ok(total)
}

fn index_html(entry: &str) -> String {
    INDEX_HTML.replace("__EDGE_ENTRY__", entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kit::tests::kit;

    fn paths(dirs: &[&str]) -> Vec<PathBuf> {
        dirs.iter().map(PathBuf::from).collect()
    }

    fn find_entry(scripts: &[PathBuf], project: &Path) -> String {
        super::find_entry(&mini::DESCRIPTOR, scripts, project)
    }

    fn collect_scripts(project: &Path, out_dir: &Path) -> Vec<PathBuf> {
        super::collect_scripts(&kit(), project, out_dir)
    }

    fn collect_bundle(manifest_path: &Path, with_docs: bool) -> Result<Bundle> {
        super::collect_bundle(&kit(), manifest_path, with_docs)
    }

    #[test]
    fn nested_main_is_the_entry_with_its_path() {
        let scripts = paths(&["./sub/main.mini"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "sub/main.mini");
    }

    #[test]
    fn root_main_wins_over_nested_candidates() {
        let scripts = paths(&["./sub/app.mini", "./main.mini"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "main.mini");
    }

    // on Linux a package folder lists before the root file and once became the entry
    #[test]
    fn root_main_wins_over_a_nested_main_listed_first() {
        let scripts = paths(&["./pkg/main.mini", "./main.mini"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "main.mini");
    }

    #[test]
    fn nested_app_is_the_entry_with_its_path() {
        let scripts = paths(&["./util.mini", "./sub/app.mini"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "sub/app.mini");
    }

    #[test]
    fn without_candidates_the_first_script_is_the_entry() {
        let scripts = paths(&["./sub/tool.mini"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "sub/tool.mini");
    }

    #[test]
    fn empty_project_falls_back_to_main() {
        assert_eq!(find_entry(&[], Path::new(".")), "main.mini");
    }

    #[test]
    fn walk_skips_the_output_dir_in_any_path_form() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.mini"), "").unwrap();
        fs::create_dir(project.join("dist")).unwrap();
        fs::write(project.join("dist/stale.mini"), "").unwrap();
        // the CLI hands over a bare relative `dist` so a non-normalized form goes in here
        fs::create_dir(project.join("sub")).unwrap();
        let out_dir = project.join("sub/..").join("dist");
        let scripts = collect_scripts(project, &out_dir);
        assert_eq!(scripts, paths(&[project.join("main.mini").to_str().unwrap()]));
    }

    #[test]
    fn a_bundle_carries_the_root_readme_and_licenses() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.mini"), "print(1)").unwrap();
        fs::write(project.join("README.md"), "# app").unwrap();
        fs::write(project.join("LICENSE"), "bare").unwrap();
        fs::write(project.join("LICENSE.txt"), "Apache").unwrap();
        // a license that is itself a script is carried once by the walk
        fs::write(project.join("LICENSE.mini"), "pass").unwrap();
        fs::write(project.join("NOTES.md"), "not a notice").unwrap();
        fs::create_dir(project.join("sub")).unwrap();
        fs::write(project.join("sub/README.md"), "nested").unwrap();
        let bundle = collect_bundle(&project.join("edge.json"), true).unwrap();
        let mut paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["LICENSE", "LICENSE.mini", "LICENSE.txt", "README.md", "main.mini"]);
        assert_eq!(bundle.entry, "main.mini");
    }

    // a packed dependency rides whole since its imports answer from inside it
    #[test]
    fn a_packed_dependency_drags_nothing_beside_it() {
        let kit = kit();
        assert!(file_deps(&mut Scan::new(&kit), "https://cdn.test/pkg/app/0.1.0", "app.edge", b"EDGEPKG\x01from .src.test import run").unwrap().is_empty());
    }

    // a vendored source pulls in what the runtime reads as its imports and its manifest
    #[test]
    fn a_vendored_source_pulls_in_its_relative_imports() {
        let kit = kit();
        let src = b"from .lib.util import shout\nfrom ..outside import x\nimport json\n";
        let deps = file_deps(&mut Scan::new(&kit), "https://cdn.test/pkg/greet/0.1.0", "main.mini", src).unwrap();
        let outside = "https://cdn.test/pkg/greet/outside.mini".to_string();
        assert_eq!(deps, [(Ok("lib/util.mini".to_string()), true), (Err(outside), true), (Ok("edge.json".to_string()), false)]);
    }

    // a relative plugin resolves inside the bundle so the walk must carry it
    #[test]
    fn a_bundle_carries_a_relative_wasm_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::create_dir(project.join("src")).unwrap();
        fs::write(project.join("src/entry.mini"), "from _json import *").unwrap();
        fs::write(project.join("src/json.wasm"), b"\0asm").unwrap();
        let bundle = collect_bundle(&project.join("edge.json"), false).unwrap();
        let mut paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["src/entry.mini", "src/json.wasm"]);
        assert_eq!(bundle.entry, "src/entry.mini");
    }

    #[test]
    fn a_bundle_carries_the_lock_beside_every_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::create_dir(project.join("pkg")).unwrap();
        fs::write(project.join("main.mini"), "print(1)").unwrap();
        fs::write(project.join("edge.json"), r#"{ "imports": {} }"#).unwrap();
        fs::write(project.join("edge.lock"), "{}").unwrap();
        fs::write(project.join("pkg/main.mini"), "print(2)").unwrap();
        fs::write(project.join("pkg/edge.json"), r#"{ "imports": {} }"#).unwrap();
        fs::write(project.join("pkg/edge.lock"), "{}").unwrap();
        let bundle = collect_bundle(&project.join("edge.json"), false).unwrap();
        let mut paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["edge.json", "edge.lock", "main.mini", "pkg/edge.json", "pkg/edge.lock", "pkg/main.mini"]);
    }

    // a version holds no address so packing one the lock never resolved would pack nothing
    #[test]
    fn a_version_no_lock_holds_stops_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.mini"), "import json").unwrap();
        fs::write(project.join("edge.json"), r#"{ "imports": { "json": "0.1.0" } }"#).unwrap();
        let Err(e) = collect_bundle(&project.join("edge.json"), false) else { panic!("an unlocked version should stop the build") };
        let err = format!("{e:#}");
        assert!(err.contains("'json' is not locked in edge.lock\nhelp: run "), "{err}");
    }

    // a package keeps its own manifest while the root one rides once by fixed name
    #[test]
    fn a_bundle_carries_a_nested_manifest_once() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::create_dir(project.join("pkg")).unwrap();
        fs::write(project.join("main.mini"), "import pkg").unwrap();
        fs::write(project.join("edge.json"), r#"{ "imports": { "pkg": "./pkg/main.mini" } }"#).unwrap();
        fs::write(project.join("pkg/main.mini"), "import _slug").unwrap();
        fs::write(project.join("pkg/edge.json"), r#"{ "imports": { "_slug": "./slug.wasm" } }"#).unwrap();
        let bundle = collect_bundle(&project.join("edge.json"), false).unwrap();
        let mut paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["edge.json", "main.mini", "pkg/edge.json", "pkg/main.mini"]);
    }

    #[test]
    fn a_declared_docs_tree_rides_along_and_an_app_leaves_it_out() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.mini"), "print(1)").unwrap();
        fs::write(project.join("edge.json"), r#"{ "docs": "./docs", "imports": {} }"#).unwrap();
        fs::create_dir(project.join("docs")).unwrap();
        fs::write(project.join("docs/01-intro.mdx"), "---\ntitle: Intro\ndescription: Where to start.\n---\n\n# Intro\n").unwrap();
        let manifest = project.join("edge.json");

        let packed = collect_bundle(&manifest, true).unwrap();
        let mut paths: Vec<&str> = packed.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["@docs/01-intro.mdx", "edge.json", "main.mini"]);

        let app = collect_bundle(&manifest, false).unwrap();
        assert!(app.files.iter().all(|f| !f.path.starts_with(docs::PREFIX)));
    }

    #[test]
    fn a_docs_tree_off_the_convention_fails_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.mini"), "print(1)").unwrap();
        fs::write(project.join("edge.json"), r#"{ "docs": "./docs" }"#).unwrap();
        fs::create_dir(project.join("docs")).unwrap();
        fs::write(project.join("docs/intro.mdx"), "# Intro\n").unwrap();
        let Err(e) = collect_bundle(&project.join("edge.json"), true) else { panic!("an unordered page should fail the build") };
        let err = format!("{e:#}");
        assert!(err.contains("needs a numeric prefix"), "{err}");
    }

    #[test]
    fn walk_keeps_a_deeper_dir_named_like_the_output_dir() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::create_dir_all(project.join("sub/dist")).unwrap();
        fs::write(project.join("sub/dist/keep.mini"), "").unwrap();
        fs::create_dir(project.join("dist")).unwrap();
        let scripts = collect_scripts(project, &project.join("dist"));
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].ends_with("sub/dist/keep.mini"));
    }
}

