use anyhow::{anyhow, bail, Context, Result};
use edge::deps::json;
use edge::deps::perms as grants;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;

use crate::host::{Access, Held, HostModule, Package, Packages, Scopes, Untrusted};
use crate::kit::{Field, Kit};
use crate::lock::Lock;

// the asks of every package a project pulls in held against its root grants
mod tree;

/* the manifest field a package writes its grants under with the holders it reserves */
pub const FIELD: Field = Field { name: grants::FIELD, check: field, reserves: &grants::HOLDERS };

/* each package holds what its manifest chain grants it from the root down */
pub struct Perms;

impl Access for Perms {
    fn field(&self) -> Option<&'static str> {
        Some(grants::FIELD)
    }

    fn check(&self, packages: &Packages, modules: &[Arc<dyn HostModule>]) -> Option<String> {
        root(packages, modules)
    }

    fn held(&self, _packages: &Packages, package: &Package, module: &str) -> Result<Held, String> {
        held(package, module)
    }

    fn untrusted(&self, kit: &Kit, pool: &str) -> Result<Untrusted> {
        Ok(Untrusted { bundle: Arc::new(Bundled { ceiling: ceiling(kit, pool)? }), snippet: Arc::new(Snippet) })
    }

    fn project(&self, kit: &Kit, manifest: &Path, lock: Option<&Lock>) -> Result<()> {
        match lock {
            Some(lock) => tree::check(kit, manifest, lock),
            None => tree::check(kit, manifest, &crate::lock::beside(manifest)?),
        }
    }

    fn asks(&self, kit: &Kit, manifest: &Path, added: &Map<String, Value>, lock: &Lock) -> Result<Vec<(String, Vec<String>)>> {
        let packages = tree::packages(kit, manifest, added, lock)?;
        // an empty grant leaves every ask unmet so all of them get listed
        let asked = packages.iter().filter(|p| !p.section.is_null()).map(|p| (p.who(), unmet(&[], &p.section)));
        Ok(asked.filter(|(_, asks)| !asks.is_empty()).collect())
    }
}

/* an untrusted bundle holding what its manifest grants within its pool's eval grant */
struct Bundled {
    ceiling: Vec<String>,
}

impl Access for Bundled {
    fn field(&self) -> Option<&'static str> {
        Some(grants::FIELD)
    }

    fn check(&self, packages: &Packages, modules: &[Arc<dyn HostModule>]) -> Option<String> {
        if let Some(problem) = root(packages, modules) {
            return Some(problem);
        }
        let section = tree_of(&packages.section).ok().and_then(|tree| grants::section(&tree, grants::FIELD).ok()).unwrap_or_default();
        let held = grants::held(&[(vec![(EVAL.to_string(), self.ceiling.clone())], EVAL.to_string())]);
        // the refusal names what the bundle grants and never the eval grant of the pool
        let over: Vec<(&String, String)> = section
            .iter()
            .filter_map(|(holder, listed)| {
                let missing = grants::unmet(&held, &vec![(MAIN.to_string(), listed.clone())]);
                (!missing.is_empty()).then(|| (holder, missing.join(", ")))
            })
            .collect();
        if over.is_empty() {
            return None;
        }
        // one holder per line as `edge lock` lays out what a root misses
        let width = over.iter().map(|(holder, _)| holder.len()).max().unwrap_or(0);
        let lines: Vec<String> = over.iter().map(|(holder, asks)| format!("  {holder:<width$}   {asks}")).collect();
        Some(format!("the pool does not grant eval what this bundle grants\n{}", lines.join("\n")))
    }

    fn held(&self, _packages: &Packages, package: &Package, module: &str) -> Result<Held, String> {
        held(package, module)
    }
}

/* an untrusted snippet that brought no manifest and so holds nothing */
struct Snippet;

impl Access for Snippet {
    fn field(&self) -> Option<&'static str> {
        Some(grants::FIELD)
    }

    fn held(&self, _packages: &Packages, package: &Package, module: &str) -> Result<Held, String> {
        Err(grants::unheld(&package.name, module))
    }
}

// the holder of the root package and the one an eval pool caps bundles with
const MAIN: &str = "main";
const EVAL: &str = "eval";

/* a json value as the engine reads it since the grant logic lives there */
fn tree_of(value: &Value) -> Result<json::Value, String> {
    json::parse(value.to_string().as_bytes(), grants::FIELD)
}

/* the shape a permissions value must hold in the words the walk uses for it */
fn field(name: &str, value: &Value) -> Result<(), String> {
    grants::section(&tree_of(value)?, name).map(|_| ())
}

/* why `section` names a module the composition lacks or a scope that module refuses */
fn check(section: &Value, modules: &[Arc<dyn HostModule>]) -> Option<String> {
    let tree = match tree_of(section) {
        Ok(tree) => tree,
        Err(e) => return Some(e),
    };
    let names: Vec<&str> = modules.iter().map(|module| module.name()).collect();
    grants::check(&tree, &names, &|name, scope| modules.iter().any(|module| module.name() == name && module.scope(scope)))
}

/* the root grants checked once per walk where empty grants need no check */
fn root(packages: &Packages, modules: &[Arc<dyn HostModule>]) -> Option<String> {
    if packages.section.as_object().is_none_or(|holders| holders.is_empty()) {
        return None;
    }
    check(&packages.section, modules).map(|problem| format!("edge.json at '{}edge.json': {problem}", packages.root))
}

/* what the end of a json chain holds with a section and key per link */
fn chained(chain: &Value) -> Vec<String> {
    grants::held(&tree_of(chain).map(|chain| grants::links(&chain)).unwrap_or_default())
}

/* what `package` holds of `module` or the text its import fails with */
fn held(package: &Package, module: &str) -> Result<Held, String> {
    match grants::scopes(&chained(&package.chain), module) {
        Some(scopes) => {
            let refusal = grants::refusal(&package.name, module, &scopes);
            Ok(Held::Scopes(Scopes::new(scopes, refusal)))
        }
        None => Err(grants::unheld(&package.name, module)),
    }
}

/* every entry of a json section that `held` misses */
fn unmet(held: &[String], section: &Value) -> Vec<String> {
    let section = tree_of(section).ok().and_then(|tree| grants::section(&tree, grants::FIELD).ok()).unwrap_or_default();
    grants::unmet(held, &section)
}

/* what the pool manifest grants eval groups or nothing without an eval holder */
fn ceiling(kit: &Kit, path: &str) -> Result<Vec<String>> {
    let Ok(bytes) = std::fs::read(path) else { return Ok(Vec::new()) };
    let manifest: Value = serde_json::from_slice(&bytes).with_context(|| format!("parsing {path}"))?;
    let Some(entries) = manifest.get(grants::FIELD).and_then(|section| section.get(EVAL)) else { return Ok(Vec::new()) };
    let eval = Value::Object(Map::from_iter([(EVAL.to_string(), entries.clone())]));
    if let Some(problem) = check(&eval, kit.wired()) {
        bail!("edge.json at '{path}': {problem}");
    }
    serde_json::from_value(entries.clone()).map_err(|e| anyhow!("edge.json at '{path}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Serve;
    use serde_json::json;

    /* a module whose scopes are lowercase words */
    struct Words;

    impl HostModule for Words {
        fn name(&self) -> &str {
            "words"
        }

        fn scope(&self, scope: &str) -> bool {
            !scope.is_empty() && scope.bytes().all(|b| b.is_ascii_lowercase())
        }

        fn reaches(&self, _call: &str, _args: &[crate::host::WireValue]) -> Result<Option<String>, crate::host::Fault> {
            Ok(None)
        }

        fn open(&self, _inst: &mut dyn Serve, _spec: &str, _pkg: &str, _held: Held) -> Result<(), String> {
            Ok(())
        }
    }

    fn modules() -> Vec<Arc<dyn HostModule>> {
        vec![Arc::new(Words)]
    }

    fn packages(section: Value, chain: Value) -> Packages {
        Packages::read(&json!({ "root": "app/", "needed": true, "dirs": [["app/", "main", chain]], "permissions": section }), Some(grants::FIELD))
    }

    #[test]
    fn a_package_holds_its_chain_and_a_refusal_names_what_it_holds() {
        let p = packages(json!({ "main": ["words:api"] }), json!([[{ "main": ["words:api"] }, "main"]]));
        assert!(Perms.check(&p, &modules()).is_none());
        let held = Perms.held(&p, &p.dirs[0], "words").unwrap();
        assert_eq!(held.scopes(), Some(&["api".to_string()][..]));
        let refused = held.need("zone").unwrap_err();
        assert_eq!((refused.class, refused.text.as_str()), (Some(crate::host::Class::Denied), "'main' has no words:zone, edge.json grants it words:api"));
        let Err(refused) = Perms.held(&p, &p.dirs[0], "db") else { panic!("db was never granted") };
        assert_eq!(refused, "'main' imports db, which edge.json does not grant it");
    }

    #[test]
    fn the_root_grants_are_checked_against_the_modules_wired() {
        let p = packages(json!({ "main": ["db:/"] }), json!([]));
        assert_eq!(Perms.check(&p, &modules()).as_deref(), Some("edge.json at 'app/edge.json': permissions for 'main' name 'db', which is not a host module (words)"));
        let p = packages(json!({ "main": ["words:API"] }), json!([]));
        assert_eq!(Perms.check(&p, &modules()).as_deref(), Some("edge.json at 'app/edge.json': permissions for 'main' give words the scope 'API', which it does not have"));
        assert!(Perms.check(&packages(json!({}), json!([])), &modules()).is_none());
    }

    #[test]
    fn an_untrusted_bundle_grants_within_the_eval_grant_and_a_snippet_holds_nothing() {
        let bundled = Bundled { ceiling: vec!["words:eval".to_string()] };
        let within = packages(json!({ "main": ["words:eval"] }), json!([[{ "main": ["words:eval"] }, "main"]]));
        assert!(bundled.check(&within, &modules()).is_none());
        let beyond = packages(json!({ "main": ["words:other"], "all": ["words"] }), json!([]));
        assert_eq!(bundled.check(&beyond, &modules()).as_deref(), Some("the pool does not grant eval what this bundle grants\n  main   words:other"));
        let Err(refused) = Snippet.held(&within, &within.dirs[0], "words") else { panic!("a snippet held words") };
        assert_eq!(refused, "'main' imports words, which edge.json does not grant it");
    }

    #[test]
    fn the_field_check_reads_the_shape_in_the_walk_words() {
        assert!(field("permissions", &json!({ "main": ["words:api"], "all": [] })).is_ok());
        assert_eq!(field("permissions", &json!(["words"])).unwrap_err(), "permissions must map each package to a list of entries");
        assert_eq!(field("permissions", &json!({ "main": "words" })).unwrap_err(), "permissions for 'main' must be a list of entries such as \"net:api.example.com\"");
    }
}
