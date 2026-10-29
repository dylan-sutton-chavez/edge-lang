use anyhow::{anyhow, bail, Result};
use edge::deps::lock::{locked_spec, verify_pin};
use edge::deps::perms::FIELD;
use edge::deps::{dir_of, join_relative, parse_integrity, walk_up_dirs};
use edge::util::sha256::{hex_encode, sha256};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use super::{chained, check as section_problem, unmet};
use crate::host::{fetch_cached, HostModule};
use crate::kit::Kit;
use crate::lock::{self, Lock};
use crate::pack::Bundle;

/* where package files live on disk or in a packed bundle held in memory */
enum Home {
    Disk(PathBuf),
    Packed(HashMap<String, Vec<u8>>),
}

impl Home {
    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        match self {
            Home::Disk(root) => std::fs::read(root.join(rel)).ok(),
            Home::Packed(files) => files.get(rel).cloned(),
        }
    }
}

/* a package the root imports with the key granting it and what its section lists */
pub struct Package {
    pub name: String,
    label: String,
    pub section: Value,
}

impl Package {
    // the package as a report names it with its version beside its grant key
    pub fn who(&self) -> String {
        self.label.clone()
    }
}

/* fails when the root edge.json misses what an imported package lists */
pub fn check(kit: &Kit, path: &Path, lock: &Lock) -> Result<()> {
    let Ok(bytes) = std::fs::read(path) else { return Ok(()) };
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|e| anyhow!("parsing {}: {e}", path.display()))?;
    let root = manifest.get(FIELD).cloned().unwrap_or_else(|| json!({}));
    if let Some(problem) = manifest.get(FIELD).and_then(|section| section_problem(section, kit.wired())) {
        bail!("edge.json at '{}': {problem}", path.display());
    }
    let imports = manifest.get("imports").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut short = Vec::new();
    for package in packages(kit, path, &imports, lock)?.iter().filter(|p| !p.section.is_null()) {
        let missing = unmet(&chained(&json!([[root, package.name]])), &package.section);
        if !missing.is_empty() {
            short.push((package.who(), missing.join(", ")));
        }
    }
    if short.is_empty() {
        return Ok(());
    }
    let width = short.iter().map(|(who, _)| who.len()).max().unwrap_or(0);
    let lines: Vec<String> = short.iter().map(|(who, asks)| format!("  {who:<width$}   {asks}")).collect();
    bail!("edge.json does not grant what these packages ask for\n{}\nhelp: grant them under \"{FIELD}\" in edge.json", lines.join("\n"))
}

/* every package the root imports with each subtree held to what its importers pass on */
pub fn packages(kit: &Kit, path: &Path, imports: &Map<String, Value>, lock: &Lock) -> Result<Vec<Package>> {
    let project = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    // the root grants so it is never one of the packages it checks
    let mut walk = Walk { found: Vec::new(), path: vec![key("", "")], modules: kit.wired(), seen: HashSet::new() };
    walk.imports(&Rc::new(Home::Disk(project.to_path_buf())), "", "", None, imports, lock)?;
    Ok(walk.found)
}

/* an importer below the root by its report label and the section it passes on */
struct Importer<'a> {
    label: &'a str,
    section: &'a Value,
}

struct Walk<'a> {
    found: Vec<Package>,
    // the packages from the root down to the one being read
    path: Vec<String>,
    // the host modules whose scopes a section may name
    modules: &'a [Arc<dyn HostModule>],
    // each package with the key and section it arrived under once held to them
    seen: HashSet<(String, String, String)>,
}

impl Walk<'_> {
    /* follows each import at `dir` as the lock beside its manifest resolved it */
    fn imports(&mut self, home: &Rc<Home>, id: &str, dir: &str, importer: Option<&Importer>, imports: &Map<String, Value>, lock: &Lock) -> Result<()> {
        for (name, target) in imports {
            // a target the lock cannot answer yet is an error the command reports itself
            let Some(spec) = target.as_str().and_then(|t| locked_spec(name, t, Some(lock)).ok()) else { continue };
            let (target, pin) = parse_integrity(&spec).map_err(|e| anyhow!(e))?;
            if target.contains("://") {
                if packed(target) {
                    // a bundle carries what it vendored under the url it answers at
                    let vendored = match home.as_ref() {
                        Home::Packed(files) => files.get(target).cloned(),
                        Home::Disk(_) => None,
                    };
                    let bytes = match vendored {
                        Some(bytes) => bytes,
                        None => fetch_cached(target, pin).map_err(|e| anyhow!(e))?,
                    };
                    self.bundle(target, &spec, &bytes, name, importer)?;
                }
                continue;
            }
            let rel = join_relative(dir, target).trim_start_matches("./").to_string();
            if packed(&rel) {
                let bytes = home.read(&rel).ok_or_else(|| anyhow!("reading {rel}: not found"))?;
                self.bundle(&rel, &spec, &bytes, name, importer)?;
                continue;
            }
            // the nearest manifest above a module is a new package unless it holds the importer
            let nearest = walk_up_dirs(&dir_of(&rel)).find(|d| d == dir || home.read(&format!("{d}edge.json")).is_some());
            if let Some(found) = nearest.filter(|d| !walk_up_dirs(dir).any(|around| around == *d)) {
                let place = format!("{found}edge.json");
                self.visit(home, id, &found, &place, name, importer)?;
            }
        }
        Ok(())
    }

    /* a packed package checked against its pin then read from inside like any other */
    fn bundle(&mut self, address: &str, spec: &str, bytes: &[u8], name: &str, importer: Option<&Importer>) -> Result<()> {
        verify_pin(spec, bytes).map_err(|e| anyhow!(e))?;
        let files = crate::pack::into_files(Bundle::decode(bytes).map_err(|e| anyhow!("package '{address}' is not a packed .edge, {e}"))?);
        self.visit(&Rc::new(Home::Packed(files)), &hex_encode(&sha256(bytes)), "", address, name, importer)
    }

    /* holds the package at `dir` to what its importer passes and follows its own imports */
    fn visit(&mut self, home: &Rc<Home>, id: &str, dir: &str, place: &str, name: &str, importer: Option<&Importer>) -> Result<()> {
        let at = key(id, dir);
        // a cycle reaches the same package again as the engine reads it
        if self.path.contains(&at) {
            return Ok(());
        }
        // a package met again under the same key and grants was held to them already
        let arrived = (at.clone(), name.to_string(), importer.map_or_else(String::new, |importer| importer.section.to_string()));
        if !self.seen.insert(arrived) {
            return Ok(());
        }
        let Some(bytes) = home.read(&format!("{dir}edge.json")) else { return Ok(()) };
        let manifest: Value = serde_json::from_slice(&bytes).map_err(|e| anyhow!("edge.json at '{place}': {e}"))?;
        let section = manifest.get(FIELD).cloned().unwrap_or(Value::Null);
        if !section.is_null()
            && let Some(problem) = section_problem(&section, self.modules)
        {
            bail!("edge.json at '{place}': {problem}");
        }
        let label = match manifest.get("version").and_then(Value::as_str) {
            Some(version) => format!("{name} {version}"),
            None => name.to_string(),
        };
        match importer {
            None => self.found.push(Package { name: name.to_string(), label: label.clone(), section: section.clone() }),
            Some(importer) if !section.is_null() => {
                // what the importer writes for it since the root check covers what it holds
                let missing = unmet(&chained(&json!([[importer.section, name]])), &section);
                if !missing.is_empty() {
                    bail!("edge.json at '{place}' asks for {}, which {} does not pass it", missing.join(", "), importer.label);
                }
            }
            Some(_) => {}
        }
        let lock = match home.read(&format!("{dir}{}", lock::FILE)) {
            Some(bytes) => Lock::parse(&bytes).map_err(|e| anyhow!("edge.json at '{place}': {e}"))?,
            None => Lock::default(),
        };
        let imports = manifest.get("imports").and_then(Value::as_object).cloned().unwrap_or_default();
        let given = if section.is_null() { json!({}) } else { section };
        self.path.push(at);
        let followed = self.imports(home, id, dir, Some(&Importer { label: &label, section: &given }), &imports, &lock);
        self.path.pop();
        followed
    }
}

fn key(id: &str, dir: &str) -> String {
    format!("{id}:{dir}")
}

/* whether a target is a packed .edge as the one artifact with its own manifest */
fn packed(target: &str) -> bool {
    target.split(['?', '#']).next().is_some_and(|path| path.ends_with(".edge"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // a chain importing each package under two keys holds each once per key
    #[test]
    fn a_diamond_of_packages_is_checked_once_per_package() {
        let dir = tempfile::tempdir().unwrap();
        let depth = 18;
        for n in 0..depth {
            std::fs::create_dir_all(dir.path().join(format!("p{n}"))).unwrap();
            let manifest = match n + 1 < depth {
                true => format!("{{ \"imports\": {{ \"a\": \"../p{0}/main.mini\", \"b\": \"../p{0}/main.mini\" }} }}", n + 1),
                false => String::from("{}"),
            };
            std::fs::write(dir.path().join(format!("p{n}/edge.json")), manifest).unwrap();
        }
        let imports = json!({ "a": "./p0/main.mini", "b": "./p0/main.mini" });
        let kit = crate::kit::tests::bare();
        let mut walk = Walk { found: Vec::new(), path: vec![key("", "")], modules: kit.wired(), seen: HashSet::new() };
        let home = Rc::new(Home::Disk(dir.path().to_path_buf()));
        walk.imports(&home, "", "", None, imports.as_object().unwrap(), &Lock::default()).unwrap();
        assert_eq!(walk.found.len(), 2);
        assert!(walk.seen.len() <= 2 * depth, "{} visits for {depth} packages", walk.seen.len());
    }
}
