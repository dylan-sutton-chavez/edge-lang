use anyhow::{anyhow, bail, Result};
use clap::{ArgMatches, Args, FromArgMatches};
use edge::deps::rules::{javascript, shaped_like_version};
use serde_json::{Map, Value};
use std::path::Path;

use crate::host::get;
use crate::kit::{self, Command, Kit};
use crate::lock::{self, Entry};
use crate::ui;

#[derive(Args)]
struct Names {
    /// Packages to add, each as name=url or name=path.
    pkgs: Vec<String>,
}

/* `add` declaring packages by url or path */
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

fn add(kit: &Kit, path: &Path, pkgs: &[String]) -> Result<()> {
    if pkgs.is_empty() {
        bail!("nothing to add: pass one or more package names");
    }
    // every spec is checked first so one bad spec aborts before any write or print
    let resolved: Vec<(&str, &str)> = pkgs
        .iter()
        .map(|spec| match spec.split_once('=') {
            Some((name, target)) if javascript(target) => bail!("module '{name}' is JavaScript, ship a .{} or a .wasm", kit.descriptor().extension),
            Some((name, target)) if !shaped_like_version(target) => Ok((name, target)),
            _ => bail!("'{spec}' names no source, give it as name=<url> or name=<path>"),
        })
        .collect::<Result<_>>()?;

    let added: Map<String, Value> = resolved.iter().map(|(name, target)| (name.to_string(), Value::String(target.to_string()))).collect();
    let asks = kit.grants().asks(kit, path, &added, &lock::Lock::default())?;
    let mut m = kit.manifest(path)?;
    let urls = resolved.iter().any(|(_, target)| target.contains("://"));
    for (name, target) in resolved {
        ui::added(name, target);
        m.imports.insert(name.to_string(), target.to_string());
    }
    for (who, asked) in &asks {
        ui::asks(who, asked);
    }
    m.save(path)?;
    let field = kit.grants().field().unwrap_or_default();
    ui::note(&match (!asks.is_empty(), urls) {
        (true, _) => format!("updated edge.json, grant what they ask for under {field}, then run {} lock", kit.name()),
        (false, true) => format!("updated edge.json, run {} lock to pin it", kit.name()),
        (false, false) => "updated edge.json".to_string(),
    });
    Ok(())
}

fn remove(kit: &Kit, path: &Path, pkgs: &[String]) -> Result<()> {
    if pkgs.is_empty() {
        bail!("nothing to remove: pass one or more package names");
    }
    let mut m = kit.manifest(path)?;
    let names: Vec<&str> = pkgs.iter().map(|s| s.split_once('=').map_or(s.as_str(), |(name, _)| name)).collect();
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

/* pins every url the manifest declares into the lock so a run sees any change */
fn lock(kit: &Kit, path: &Path) -> Result<()> {
    let manifest = kit.manifest(path)?;
    let mut lock = lock::Lock::default();

    for (name, target) in &manifest.imports {
        if javascript(target) {
            bail!("module '{name}' is JavaScript, ship a .{} or a .wasm", kit.descriptor().extension);
        }
        if shaped_like_version(target) {
            bail!("'{name}' is declared '{target}' but no version resolves, give it a url or a path");
        }
        // a path carries its own bytes and a pinned url its own digest
        if !target.contains("://") || target.contains("#sha256-") {
            continue;
        }
        let entry = Entry { version: None, url: target.clone(), digest: lock::digest_of(&download(target)?) };
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
    let mut response = get(url).map_err(|e| anyhow!("fetching {url}: {e}"))?;
    crate::host::body(&mut response).map_err(|e| anyhow!("reading {url}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_javascript_target_is_told_apart_from_the_modules_a_host_loads() {
        for target in ["https://x/charts.js", "https://x/charts.mjs", "https://x/time/index.js?v=2"] {
            assert!(javascript(target), "{target}");
        }
        for target in ["https://x/json.wasm", "https://x/pkg/json/0.1.0/app.edge", "https://x/helper.lox"] {
            assert!(!javascript(target), "{target}");
        }
    }
}
