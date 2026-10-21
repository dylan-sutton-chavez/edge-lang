use anyhow::{anyhow, bail, Context, Result};
use crate::docs;
use crate::host::{cdn, get};
use crate::pack::{Bundle, Entry};
use compiler::modules::{parse_integrity, scan_imports, ImportSpec};
use compiler::util::sha256::sha256;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::lock;
use crate::manifest::Manifest;

/* The project as one `.edge`, the artifact the registry holds and an actor runs. */
pub fn pack(manifest_path: &Path) -> Result<Vec<u8>> {
    Ok(collect_bundle(manifest_path)?.encode())
}

/* Reads the project scripts, its notices, its edge.json and every url module it declares into a bundle. */
fn collect_bundle(manifest_path: &Path) -> Result<Bundle> {
    let project = match manifest_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let scripts = collect_scripts(&project, Path::new(""));
    if !scripts.iter().any(|s| s.extension().and_then(|e| e.to_str()) == Some("py")) {
        return Err(anyhow!("no .py files found under {}", project.display()));
    }
    let notices = collect_notices(&project);
    let mut files = Vec::new();
    for s in scripts.iter().chain(&notices) {
        let rel = s.strip_prefix(&project).unwrap_or(s).to_string_lossy().replace('\\', "/");
        // The root manifest and its lock are the pair the command names, pushed below whatever their files are called.
        if rel == "edge.json" || rel == lock::FILE { continue; }
        files.push(Entry { path: rel, bytes: fs::read(s).with_context(|| format!("reading {}", s.display()))? });
    }
    if manifest_path.exists() {
        files.push(Entry { path: "edge.json".to_string(), bytes: fs::read(manifest_path)? });
        // The lock travels with the manifest it resolves, so the bundle reads a name the same way its project did.
        let beside = manifest_path.with_file_name(lock::FILE);
        if beside.exists() {
            files.push(Entry { path: lock::FILE.to_string(), bytes: fs::read(&beside)? });
        }
        let manifest = Manifest::load(manifest_path)?;
        if let Some(clash) = files.iter().find(|f| f.path.starts_with(docs::PREFIX)) {
            bail!("'{}' takes the prefix the docs are packed under", clash.path);
        }
        let pages = docs::collect(&project, manifest.docs.as_deref())?;
        files.extend(pages.into_iter().map(|(path, bytes)| Entry { path, bytes }));
        vendor_bundle(&lock::resolved(manifest_path, &manifest.imports)?, &mut files)?;
    }
    // A nested package answers to its own manifest, so its url modules ride along too.
    let nested: Vec<PathBuf> = files.iter().filter(|f| f.path.ends_with("/edge.json")).map(|f| project.join(&f.path)).collect();
    for path in nested {
        vendor_bundle(&lock::resolved(&path, &Manifest::load(&path)?.imports)?, &mut files)?;
    }
    let mut kept = HashSet::new();
    files.retain(|f| kept.insert(f.path.clone()));
    Ok(Bundle { entry: find_entry(&scripts, &project), files })
}

/* The readme and licenses at the project root, sorted so two builds of a tree agree. */
fn collect_notices(project: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(project) else { return Vec::new() };
    let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file() && is_notice(p)).collect();
    found.sort();
    found
}

/* `README.md` or `LICENSE` with any extension, never one the script walk already carries. */
fn is_notice(path: &Path) -> bool {
    if matches!(path.extension().and_then(|e| e.to_str()), Some("py")) {
        return false;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.eq_ignore_ascii_case("README.md") || name.split('.').next().is_some_and(|s| s.eq_ignore_ascii_case("LICENSE"))
}

/* Carries each declared url module and the files it reaches, keyed by the address it answers. */
fn vendor_bundle(imports: &BTreeMap<String, String>, files: &mut Vec<Entry>) -> Result<()> {
    refuse_javascript(imports)?;
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
            // A query belongs to the address a run asks for, so the packed copy answers it.
            files.push(Entry { path: url.to_string(), bytes: bytes.clone() });
        }
        carry_tree(base, entry, bytes, &mut seen, files).with_context(|| format!("bundling '{name}'"))?;
    }
    Ok(())
}

/* No host loads JavaScript but its own, so a build declaring one stops before packing. */
fn refuse_javascript(imports: &BTreeMap<String, String>) -> Result<()> {
    for (name, spec) in imports {
        let path = spec.split(['?', '#']).next().unwrap_or(spec);
        if matches!(path.rsplit('.').next(), Some("js" | "mjs")) {
            bail!("module '{name}' is JavaScript, ship a .py or a .wasm");
        }
    }
    Ok(())
}

/* Walks `entry` with everything it imports, each file stored under its own url. */
fn carry_tree(base: &str, entry: &str, bytes: Vec<u8>, seen: &mut HashSet<String>, files: &mut Vec<Entry>) -> Result<()> {
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
        for (spec, needed) in file_deps(&rel, &bytes)? {
            let clean = spec.split(['?', '#']).next().unwrap_or(&spec);
            let dep = join(&rel, clean).ok_or_else(|| anyhow!("'{url}' imports '{spec}' from outside its directory"))?;
            queue.push((dep, None, needed));
        }
        files.push(Entry { path: url, bytes });
    }
    Ok(())
}

/* The bundle a raw `.edge` file carries, read whole only once its magic matches. */
pub fn file_payload(path: &Path) -> Option<Vec<u8>> {
    let mut head = [0u8; 8];
    use std::io::Read;
    fs::File::open(path).ok()?.read_exact(&mut head).ok()?;
    head.starts_with(crate::pack::MAGIC).then(|| fs::read(path).ok()).flatten()
}

use crate::web::{COMPILER_WASM, JS_HOST};

const INDEX_HTML: &str = include_str!("../templates/dist.html");

/// Pack the project as a browser dist/, vendoring the JS host, compiler and packages.
pub fn web(manifest_path: &Path, out_dir: PathBuf) -> Result<()> {
    let t0 = Instant::now();
    let mut manifest = Manifest::load(manifest_path)?;
    // `Path::parent` returns Some("") for a bare filename, so collapse that to "." explicitly.
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

    fs::write(out_dir.join("compiler.wasm"), COMPILER_WASM).context("writing compiler.wasm")?;

    let scripts = collect_scripts(&project, &out_dir);
    let sp = crate::ui::spinner("vendoring packages");
    let vendored = match vendor_packages(&lock::resolved(manifest_path, &manifest.imports)?, &out_dir) {
        Ok(v) => v,
        Err(e) => { sp.fail("failed to vendor packages"); return Err(e); }
    };
    sp.done("vendored packages");
    let script_count = copy_scripts(&scripts, &project, &out_dir)?;

    // fs reads the granted folders beside the program, and a page lists them from the index http cannot give it.
    if !crate::files::granted(manifest.permissions()).is_empty() {
        let out = out_dir.canonicalize().unwrap_or_else(|_| out_dir.clone());
        let readable: Vec<String> = crate::files::index(&project, manifest.permissions(), crate::files::INDEX_LIMIT).into_iter().filter(|path| !project.join(path).canonicalize().is_ok_and(|real| real.starts_with(&out))).collect();
        for path in &readable {
            let to = out_dir.join(path);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            }
            fs::copy(project.join(path), &to).with_context(|| format!("copying {path}"))?;
        }
        fs::write(out_dir.join("edge.files"), serde_json::to_string(&readable)?).context("writing edge.files")?;
    }

    // Vendored paths replace their urls, every other entry and key stays as written.
    let packages = vendored.len();
    manifest.imports.extend(vendored);
    manifest.save(&out_dir.join("edge.json"))?;

    let entry = find_entry(&scripts, &project);
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

/// Write the embedded JS host into `dist/js/` mirroring its CDN layout.
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

/// Walk the project for `.py`, JavaScript, `.wasm` plugin and `edge.json` files, skipping hidden dirs and the output directory itself.
fn collect_scripts(project: &Path, out_dir: &Path) -> Vec<PathBuf> {
    let mut scripts = Vec::new();
    let out_dir = fs::canonicalize(out_dir).unwrap_or_else(|_| out_dir.to_path_buf());
    walk(project, &out_dir, &mut scripts);
    scripts
}

fn walk(dir: &Path, out_dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // read_dir yields "./dist" while out_dir is "dist", so compare canonical forms.
        if path.is_dir()
            && fs::canonicalize(&path).ok().as_deref() == Some(out_dir) {
                continue;
            }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        if path.is_dir() {
            walk(&path, out_dir, found);
        } else if name == "edge.json" || name == lock::FILE || matches!(path.extension().and_then(|e| e.to_str()), Some("py" | "wasm")) {
            found.push(path);
        }
    }
}

/// Every url the manifest declares lands in dist/vendor/<name>/ with the files it reaches beside it.
fn vendor_packages(imports: &BTreeMap<String, String>, out_dir: &Path) -> Result<BTreeMap<String, String>> {
    refuse_javascript(imports)?;
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
        vendor_tree(base, entry, bytes, out_dir, &dest)?;
        local.insert(name.clone(), format!("./{dest}/{entry}"));
    }
    Ok(local)
}

/* Copies `entry` into `dest` with every file its imports reach, each kept at its relative path. */
fn vendor_tree(base: &str, entry: &str, bytes: Vec<u8>, out_dir: &Path, dest: &str) -> Result<()> {
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
        for (spec, needed) in file_deps(&rel, &bytes)? {
            let clean = spec.split(['?', '#']).next().unwrap_or(&spec);
            let dep = join(&rel, clean).ok_or_else(|| anyhow!("'{url}' imports '{spec}' from outside its directory, which a web build cannot vendor"))?;
            queue.push((dep, None, needed));
        }
        write_under(out_dir, &format!("{dest}/{rel}"), &bytes)?;
    }
    Ok(())
}

/* Resolves `spec` against the file `from`, None when it climbs out of the tree's directory. */
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

/* What a vendored file pulls in beside it, each flagged when the build fails without it. */
fn file_deps(rel: &str, bytes: &[u8]) -> Result<Vec<(String, bool)>> {
    let text = String::from_utf8_lossy(bytes);
    let ext = Path::new(rel).extension().and_then(|e| e.to_str()).unwrap_or("");
    Ok(match ext {
        "json" => {
            let manifest: serde_json::Value = serde_json::from_str(&text).with_context(|| format!("parsing {rel}"))?;
            let imports = manifest.get("imports").and_then(|i| i.as_object()).into_iter().flatten();
            imports
                .filter_map(|(_, target)| target.as_str())
                // A version names no file here, the lock beside this manifest is what points it at one.
                .filter(|target| !target.contains("://") && !target.starts_with('/') && !compiler::modules::rules::shaped_like_version(target))
                .map(|target| (target.to_string(), true))
                .chain([(lock::FILE.to_string(), false)])
                .collect()
        }
        // A plugin and a packed package each carry all they need.
        "wasm" | "edge" => Vec::new(),
        _ if bytes.starts_with(b"\0asm") || bytes.starts_with(crate::pack::MAGIC) => Vec::new(),
        // Python source, its relative imports plus the manifest beside it when one is served.
        _ => scan_imports(&text)
            .into_iter()
            .filter_map(|imp| match imp {
                ImportSpec::Relative(path) => Some((path, true)),
                _ => None,
            })
            .chain([("./edge.json".to_string(), false)])
            .collect(),
    })
}

/* A package file's bytes, None when the host has no such file. */
fn read_package(url: &str) -> Result<Option<Vec<u8>>> {
    // Report the address actually requested, a staging origin is where a failure needs looking at.
    let source = cdn(url);
    match get(&source) {
        Ok(mut resp) => Ok(Some(resp.body_mut().read_to_vec().map_err(|e| anyhow!("reading {source}: {e}"))?)),
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

/// Copy each script preserving its path under the project root.
fn copy_scripts(scripts: &[PathBuf], project: &Path, out_dir: &Path) -> Result<usize> {
    let mut count = 0usize;
    for s in scripts {
        let rel = s.strip_prefix(project).unwrap_or(s);
        // Every root import is vendored to a path here, so only a nested package still has a name to resolve.
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

/// Pick `main.py`/`app.py`/`index.py` if present, otherwise the first script found.
fn find_entry(scripts: &[PathBuf], project: &Path) -> String {
    let rel = |s: &PathBuf| s.strip_prefix(project).ok().map(|p| p.to_string_lossy().replace('\\', "/"));
    // The shallowest match wins, so a package's main.py never outranks the project's.
    let shallowest = |found: Vec<String>| found.into_iter().min_by_key(|r| (r.matches('/').count(), r.clone()));
    for c in ["main.py", "app.py", "index.py"] {
        if let Some(r) = shallowest(scripts.iter().filter(|s| s.file_name().and_then(|n| n.to_str()) == Some(c)).filter_map(rel).collect()) {
            return r;
        }
    }
    shallowest(scripts.iter().filter(|s| s.extension().and_then(|e| e.to_str()) == Some("py")).filter_map(rel).collect())
        .unwrap_or_else(|| "main.py".to_string())
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

    fn paths(dirs: &[&str]) -> Vec<PathBuf> {
        dirs.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn nested_main_is_the_entry_with_its_path() {
        let scripts = paths(&["./sub/main.py"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "sub/main.py");
    }

    #[test]
    fn root_main_wins_over_nested_candidates() {
        let scripts = paths(&["./sub/app.py", "./main.py"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "main.py");
    }

    // Linux lists a package folder before the root file, which once made the package the entry.
    #[test]
    fn root_main_wins_over_a_nested_main_listed_first() {
        let scripts = paths(&["./pkg/main.py", "./main.py"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "main.py");
    }

    #[test]
    fn nested_app_is_the_entry_with_its_path() {
        let scripts = paths(&["./util.py", "./sub/app.py"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "sub/app.py");
    }

    #[test]
    fn without_candidates_the_first_script_is_the_entry() {
        let scripts = paths(&["./sub/tool.py"]);
        assert_eq!(find_entry(&scripts, Path::new(".")), "sub/tool.py");
    }

    #[test]
    fn empty_project_falls_back_to_main() {
        assert_eq!(find_entry(&[], Path::new(".")), "main.py");
    }

    #[test]
    fn walk_skips_the_output_dir_in_any_path_form() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.py"), "").unwrap();
        fs::create_dir(project.join("dist")).unwrap();
        fs::write(project.join("dist/stale.py"), "").unwrap();
        // The CLI hands over a bare relative "dist", so pass a non-normalized form here.
        fs::create_dir(project.join("sub")).unwrap();
        let out_dir = project.join("sub/..").join("dist");
        let scripts = collect_scripts(project, &out_dir);
        assert_eq!(scripts, paths(&[project.join("main.py").to_str().unwrap()]));
    }

    #[test]
    fn a_bundle_carries_the_root_readme_and_licenses() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.py"), "print(1)").unwrap();
        fs::write(project.join("README.md"), "# app").unwrap();
        fs::write(project.join("LICENSE"), "bare").unwrap();
        fs::write(project.join("LICENSE.txt"), "Apache").unwrap();
        // A license that is itself a script is carried once, by the walk.
        fs::write(project.join("LICENSE.py"), "pass").unwrap();
        fs::write(project.join("NOTES.md"), "not a notice").unwrap();
        fs::create_dir(project.join("sub")).unwrap();
        fs::write(project.join("sub/README.md"), "nested").unwrap();
        let bundle = collect_bundle(&project.join("edge.json")).unwrap();
        let mut paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["LICENSE", "LICENSE.py", "LICENSE.txt", "README.md", "main.py"]);
        assert_eq!(bundle.entry, "main.py");
    }

    // A packed dependency rides whole, the imports inside it answer from inside it.
    #[test]
    fn a_packed_dependency_drags_nothing_beside_it() {
        assert!(file_deps("app.edge", b"EDGEPKG\x01from .src.test import run").unwrap().is_empty());
    }

    // A relative plugin resolves inside the bundle, so the walk must carry it.
    #[test]
    fn a_bundle_carries_a_relative_wasm_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::create_dir(project.join("src")).unwrap();
        fs::write(project.join("src/entry.py"), "from _json import *").unwrap();
        fs::write(project.join("src/json.wasm"), b"\0asm").unwrap();
        let bundle = collect_bundle(&project.join("edge.json")).unwrap();
        let mut paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["src/entry.py", "src/json.wasm"]);
        assert_eq!(bundle.entry, "src/entry.py");
    }

    /* The lock rides beside the manifest it resolves, since a bundle reads a declared name the same way its project did. */
    #[test]
    fn a_bundle_carries_the_lock_beside_every_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::create_dir(project.join("pkg")).unwrap();
        fs::write(project.join("main.py"), "print(1)").unwrap();
        fs::write(project.join("edge.json"), r#"{ "imports": {} }"#).unwrap();
        fs::write(project.join("edge.lock"), "{}").unwrap();
        fs::write(project.join("pkg/main.py"), "print(2)").unwrap();
        fs::write(project.join("pkg/edge.json"), r#"{ "imports": {} }"#).unwrap();
        fs::write(project.join("pkg/edge.lock"), "{}").unwrap();
        let bundle = collect_bundle(&project.join("edge.json")).unwrap();
        let mut paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["edge.json", "edge.lock", "main.py", "pkg/edge.json", "pkg/edge.lock", "pkg/main.py"]);
    }

    // A version says nothing about where its bytes are, so packing one the lock never resolved would pack nothing.
    #[test]
    fn a_version_no_lock_holds_stops_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.py"), "import json").unwrap();
        fs::write(project.join("edge.json"), r#"{ "imports": { "json": "0.1.0" } }"#).unwrap();
        let Err(e) = collect_bundle(&project.join("edge.json")) else { panic!("an unlocked version should stop the build") };
        let err = format!("{e:#}");
        assert!(err.contains("'json' is not locked, run edge lock"), "{err}");
    }

    // A package keeps its own manifest, and the root one rides once under its fixed name.
    #[test]
    fn a_bundle_carries_a_nested_manifest_once() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::create_dir(project.join("pkg")).unwrap();
        fs::write(project.join("main.py"), "import pkg").unwrap();
        fs::write(project.join("edge.json"), r#"{ "imports": { "pkg": "./pkg/main.py" } }"#).unwrap();
        fs::write(project.join("pkg/main.py"), "import _slug").unwrap();
        fs::write(project.join("pkg/edge.json"), r#"{ "imports": { "_slug": "./slug.wasm" } }"#).unwrap();
        let bundle = collect_bundle(&project.join("edge.json")).unwrap();
        let mut paths: Vec<&str> = bundle.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["edge.json", "main.py", "pkg/edge.json", "pkg/main.py"]);
    }

    #[test]
    fn a_declared_docs_tree_rides_along() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.py"), "print(1)").unwrap();
        fs::write(project.join("edge.json"), r#"{ "docs": "./docs", "imports": {} }"#).unwrap();
        fs::create_dir(project.join("docs")).unwrap();
        fs::write(project.join("docs/01-intro.mdx"), "---\ntitle: Intro\ndescription: Where to start.\n---\n\n# Intro\n").unwrap();
        let manifest = project.join("edge.json");

        let packed = collect_bundle(&manifest).unwrap();
        let mut paths: Vec<&str> = packed.files.iter().map(|f| f.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, ["@docs/01-intro.mdx", "edge.json", "main.py"]);
    }

    #[test]
    fn a_docs_tree_off_the_convention_fails_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::write(project.join("main.py"), "print(1)").unwrap();
        fs::write(project.join("edge.json"), r#"{ "docs": "./docs" }"#).unwrap();
        fs::create_dir(project.join("docs")).unwrap();
        fs::write(project.join("docs/intro.mdx"), "# Intro\n").unwrap();
        let Err(e) = collect_bundle(&project.join("edge.json")) else { panic!("an unordered page should fail the build") };
        let err = format!("{e:#}");
        assert!(err.contains("needs a numeric prefix"), "{err}");
    }

    #[test]
    fn walk_keeps_a_deeper_dir_named_like_the_output_dir() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path();
        fs::create_dir_all(project.join("sub/dist")).unwrap();
        fs::write(project.join("sub/dist/keep.py"), "").unwrap();
        fs::create_dir(project.join("dist")).unwrap();
        let scripts = collect_scripts(project, &project.join("dist"));
        assert_eq!(scripts.len(), 1);
        assert!(scripts[0].ends_with("sub/dist/keep.py"));
    }
}

