use anyhow::{anyhow, bail, Result};
use clap::{ArgMatches, Args, FromArgMatches};
use edge::deps::rules::{javascript, shaped_like_version};
use serde_json::{Map, Value};
use std::path::Path;

use crate::host::{cdn, get, site};
use crate::kit::{self, Command, Kit};
use crate::lock::{self, Entry};
use crate::ui;

#[derive(Args)]
struct Names {
    /// Package names to add.
    pkgs: Vec<String>,
}

/* `add` declaring packages by name or version or url */
pub struct Add;

impl Command for Add {
    fn spec(&self, _kit: &Kit) -> clap::Command {
        Names::augment_args(clap::Command::new("add").about("Add packages to edge.json"))
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("add <pkgs>".to_string(), "Add packages to edge.json".to_string())
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        add(kit, &kit::manifest_path(m), &Names::from_arg_matches(m)?.pkgs)?;
        Ok(0)
    }
}

/* `remove` dropping declared packages */
pub struct Remove;

impl Command for Remove {
    fn spec(&self, _kit: &Kit) -> clap::Command {
        Names::augment_args(clap::Command::new("remove").about("Remove packages from edge.json")).mut_arg("pkgs", |arg| arg.help("Package names to remove"))
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("remove <pkgs>".to_string(), "Remove packages from edge.json".to_string())
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        remove(kit, &kit::manifest_path(m), &Names::from_arg_matches(m)?.pkgs)?;
        Ok(0)
    }
}

/* `lock` resolving every declared version into edge.lock */
pub struct Lock;

impl Command for Lock {
    fn spec(&self, _kit: &Kit) -> clap::Command {
        clap::Command::new("lock").about("Resolve every version edge.json declares into edge.lock")
    }

    fn summary(&self, _kit: &Kit) -> (String, String) {
        ("lock".to_string(), "Resolve edge.json into edge.lock".to_string())
    }

    fn run(&self, kit: &Kit, m: &ArgMatches) -> Result<i32> {
        lock(kit, &kit::manifest_path(m))?;
        Ok(0)
    }
}

/* a refresh tells the registry so since only the first take of a package counts */
fn release(name: &str, version: Option<&str>, refresh: bool) -> Result<Entry> {
    let asked = [version.map(|v| format!("v={v}")), refresh.then(|| "lock=1".to_string())].into_iter().flatten().collect::<Vec<_>>().join("&");
    let query = match asked.is_empty() {
        true => String::new(),
        false => format!("?{asked}"),
    };
    let source = site(&format!("/api/resolve/package/{name}{query}")).map_err(|e| anyhow!(e))?;

    let mut response = get(&source).map_err(|e| match (e, version) {
        (ureq::Error::StatusCode(404), Some(v)) => anyhow!("'{name}' has no version {v}"),
        (ureq::Error::StatusCode(404), None) => anyhow!("unknown package '{name}'; give a url with {name}=<url>"),
        (other, _) => anyhow!("asking the registry about '{name}': {other}"),
    })?;

    let text = response.body_mut().read_to_string().map_err(|e| anyhow!("reading {source}: {e}"))?;
    let answer: serde_json::Value = serde_json::from_str(&text).map_err(|e| anyhow!("parsing {source}: {e}"))?;
    let field = |key: &str| answer.get(key).and_then(|v| v.as_str()).map(str::to_string);

    match (field("version"), field("url"), field("digest")) {
        (Some(version), Some(url), Some(digest)) => Ok(Entry { version: Some(version), url, digest: format!("sha256-{digest}") }),
        _ => bail!("the registry sent no release for '{name}'"),
    }
}

fn add(kit: &Kit, path: &Path, pkgs: &[String]) -> Result<()> {
    if pkgs.is_empty() {
        bail!("nothing to add: pass one or more package names");
    }
    // every spec is checked first so one unknown name aborts before any write or print
    let mut releases = lock::Lock::default();
    let resolved: Vec<(&str, String)> = pkgs
        .iter()
        .map(|spec| {
            let (name, version, url) = parse_spec(spec);
            if url.as_deref().is_some_and(javascript) {
                bail!("module '{name}' is JavaScript, ship a .{} or a .wasm", kit.descriptor().extension);
            }
            let target = match url {
                Some(url) => url,
                // the manifest keeps only a version and `edge lock` turns it into an address
                None => {
                    let entry = release(name, version, false)?;
                    let version = entry.version.clone().unwrap_or_default();
                    releases.insert(name, entry);
                    version
                }
            };
            Ok::<_, anyhow::Error>((name, target))
        })
        .collect::<Result<_>>()?;

    let added: Map<String, Value> = resolved.iter().map(|(name, target)| (name.to_string(), Value::String(target.clone()))).collect();
    let asks = kit.grants().asks(kit, path, &added, &releases)?;
    let mut m = kit.manifest(path)?;
    let versions = resolved.iter().any(|(_, target)| shaped_like_version(target));
    for (name, target) in resolved {
        ui::added(name, &target);
        m.imports.insert(name.to_string(), target);
    }
    for (who, asked) in &asks {
        ui::asks(who, asked);
    }
    m.save(path)?;
    let field = kit.grants().field().unwrap_or_default();
    ui::note(&match (!asks.is_empty(), versions) {
        (true, _) => format!("updated edge.json, grant what they ask for under {field}, then run {} lock", kit.name()),
        (false, true) => format!("updated edge.json, run {} lock to resolve it", kit.name()),
        (false, false) => "updated edge.json".to_string(),
    });
    Ok(())
}

fn remove(kit: &Kit, path: &Path, pkgs: &[String]) -> Result<()> {
    if pkgs.is_empty() {
        bail!("nothing to remove: pass one or more package names");
    }
    let mut m = kit.manifest(path)?;
    let names: Vec<&str> = pkgs.iter().map(|s| parse_spec(s).0).collect();
    // every name is checked first so one bad name aborts before any write or print
    for name in &names {
        if !m.imports.contains_key(*name) {
            bail!("'{name}' is not in {}", path.display());
        }
    }
    for name in names {
        m.imports.remove(name);
        ui::removed(name);
    }
    m.save(path)?;
    ui::note("updated edge.json");
    Ok(())
}

/* resolves the manifest into the lock so runs never ask the registry where names point */
fn lock(kit: &Kit, path: &Path) -> Result<()> {
    let manifest = kit.manifest(path)?;
    let mut lock = lock::Lock::default();

    for (name, target) in &manifest.imports {
        if javascript(target) {
            bail!("module '{name}' is JavaScript, ship a .{} or a .wasm", kit.descriptor().extension);
        }
        let entry = match shaped_like_version(target) {
            true => release(name, Some(target), true)?,
            // a path carries its own bytes and a pinned url its own digest
            false if !target.contains("://") || target.contains("#sha256-") => continue,
            false => Entry { version: None, url: target.clone(), digest: lock::digest_of(&download(target)?) },
        };
        ui::added(name, &entry.url);
        lock.insert(name, entry);
    }

    // nothing is written until the root grants what every package in the tree asks for
    kit.grants().project(kit, path, Some(&lock))?;
    let written = lock::save(&lock, path)?;
    ui::note(&format!("wrote {}", written.display()));
    Ok(())
}

/* the bytes at `url` hashed into the lock so a later run sees any change */
fn download(url: &str) -> Result<Vec<u8>> {
    let source = cdn(url);
    let mut response = get(&source).map_err(|e| anyhow!("fetching {source}: {e}"))?;
    crate::host::body(&mut response).map_err(|e| anyhow!("reading {source}: {e}"))
}

// parses `name` or `name@version` or `name=url`
fn parse_spec(spec: &str) -> (&str, Option<&str>, Option<String>) {
    if let Some((name, url)) = spec.split_once('=') {
        return (name, None, Some(url.to_string()));
    }
    match spec.split_once('@') {
        Some((name, version)) => (name, Some(version), None),
        None => (spec, None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_names_a_package_a_version_or_a_url() {
        assert_eq!(parse_spec("json"), ("json", None, None));
        assert_eq!(parse_spec("json@0.1.0"), ("json", Some("0.1.0"), None));
        assert_eq!(parse_spec("foo=https://x/foo.wasm"), ("foo", None, Some("https://x/foo.wasm".to_string())));
    }

    #[test]
    fn a_javascript_target_is_told_apart_from_the_modules_a_host_loads() {
        for target in ["https://x/charts.js", "https://x/charts.mjs", "https://x/time/index.js?v=2"] {
            assert!(javascript(target), "{target}");
        }
        for target in ["https://x/json.wasm", "https://x/pkg/json/0.1.0/app.edge", "https://x/helper.mini"] {
            assert!(!javascript(target), "{target}");
        }
    }
}
