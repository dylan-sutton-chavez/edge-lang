use anyhow::{anyhow, bail, Context, Result};
use edge::deps::{parse_manifest, rules};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

use crate::kit::Kit;

/* the manifest as the core reads it with every other key kept as written */
#[derive(Default)]
pub struct Manifest {
    // the lowest engine the project runs on
    pub edge: Option<String>,
    pub imports: BTreeMap<String, String>,
    extends: Option<String>,
    // the fields components declared which a save writes first in that order
    leading: Vec<&'static str>,
    rest: Map<String, Value>,
}

impl Manifest {
    // an absent file loads as an empty manifest
    pub(crate) fn load(path: &Path, kit: &Kit) -> Result<Self> {
        let leading = kit.declared().iter().map(|field| field.name).collect();
        if !path.exists() {
            return Ok(Manifest { leading, ..Manifest::default() });
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let value: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        // the engine rules the registry also runs so a build never packs what it refuses
        let at = path.display();
        let parsed = parse_manifest(text.as_bytes()).map_err(|e| anyhow!("edge.json at '{at}': {e}"))?;
        rules::check(&parsed, &kit.reserved(), &kit.holders()).map_err(|e| anyhow!("edge.json at '{at}': {e}"))?;
        let Value::Object(mut rest) = value else { bail!("edge.json at '{at}': edge.json must be a JSON object") };
        for field in kit.declared() {
            if let Some(value) = rest.get(field.name) {
                (field.check)(field.name, value).map_err(|e| anyhow!("edge.json at '{at}': {e}"))?;
            }
        }
        // the engine already held these keys to their shapes
        let text_of = |value: Option<Value>| value.and_then(|v| v.as_str().map(str::to_string));
        let imports = rest.remove("imports").map(serde_json::from_value).transpose().with_context(|| format!("parsing {}", path.display()))?;
        Ok(Manifest { edge: text_of(rest.remove("edge")), imports: imports.unwrap_or_default(), extends: text_of(rest.remove("extends")), leading, rest })
    }

    /* an old binary names the version a newer project needs before tripping on its fields */
    pub fn check_engine(path: &Path, kit: &Kit) -> Result<()> {
        let Some(floor) = Self::load(path, kit)?.edge else { return Ok(()) };
        if rules::newer(&floor, rules::ENGINE) {
            let help = kit.upgrade_help().map(|help| format!("\nhelp: {help}")).unwrap_or_default();
            bail!("this project needs edge {floor}, this is {}{help}", rules::ENGINE);
        }
        Ok(())
    }

    /* a key the core does not read as written */
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.rest.get(key)
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, format!("{text}\n")).with_context(|| format!("writing {}", path.display()))
    }
}

/* the declared fields lead then the core keys and then the rest in key order */
impl Serialize for Manifest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for key in &self.leading {
            if let Some(value) = self.rest.get(*key) {
                map.serialize_entry(key, value)?;
            }
        }
        if let Some(edge) = &self.edge {
            map.serialize_entry("edge", edge)?;
        }
        if !self.imports.is_empty() {
            map.serialize_entry("imports", &self.imports)?;
        }
        if let Some(extends) = &self.extends {
            map.serialize_entry("extends", extends)?;
        }
        for (key, value) in self.rest.iter().filter(|(key, _)| !self.leading.contains(&key.as_str())) {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/* the check of a field that holds text */
pub fn string(name: &str, value: &Value) -> Result<(), String> {
    match value.is_string() {
        true => Ok(()),
        false => Err(format!("'{name}' must be a string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(body: &str) -> Result<Manifest> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge.json");
        std::fs::write(&path, body).unwrap();
        Manifest::load(&path, &crate::kit::tests::kit())
    }

    #[test]
    fn the_registry_fields_round_trip() {
        let m = load(r#"{ "name": "slugify", "version": "0.1.0", "description": "Turn text into a slug.", "repository": "https://github.com/x/slugify", "docs": "./docs" }"#).unwrap();
        assert_eq!(m.text("name"), Some("slugify"));
        assert_eq!(m.text("version"), Some("0.1.0"));
        assert_eq!(m.text("repository"), Some("https://github.com/x/slugify"));
        assert_eq!(m.text("docs"), Some("./docs"));
    }

    #[test]
    fn a_manifest_without_the_registry_fields_still_loads() {
        let m = load(r#"{ "imports": { "json": "https://x/json.wasm" } }"#).unwrap();
        assert!(m.text("name").is_none() && m.text("version").is_none() && m.text("docs").is_none());
        assert_eq!(m.imports.get("json").map(String::as_str), Some("https://x/json.wasm"));
    }

    /* the engine holds the rules and the CLI applies them naming the file it read */
    #[test]
    fn the_fields_a_registry_would_turn_away_are_refused() {
        for (body, want) in [
            (r#"{ "name": "Slugify" }"#, "must be lowercase"),
            (r#"{ "name": "quiet" }"#, "is reserved for the system module"),
            (r#"{ "version": "01.0.0" }"#, "must be major.minor.patch"),
            (r#"{ "description": "Turn absolutely any text that you have into a tidy url slug fast." }"#, "the cap is 60"),
            (r#"{ "imports": { "quiet": "./quiet.mini" } }"#, "takes the name of a system module"),
            (r#"{ "docs": ["./docs"] }"#, "'docs' must be a string"),
        ] {
            let Err(e) = load(body) else { panic!("{body} should be refused") };
            let err = format!("{e:#}");
            assert!(err.contains("edge.json at '") && err.contains(want), "{body} wanted '{want}', got '{err}'");
        }
    }

    /* the grant component reserves its holders and checks its field when wired */
    #[cfg(feature = "perms")]
    #[test]
    fn the_permissions_field_is_held_to_its_shape_and_reserves_its_holders() {
        for (body, want) in [
            (r#"{ "name": "main" }"#, "name 'main' is reserved for permissions"),
            (r#"{ "imports": { "eval": "./e.mini" } }"#, "import 'eval' is reserved for permissions"),
            (r#"{ "permissions": ["net"] }"#, "permissions must map each package to a list of entries"),
            (r#"{ "permissions": { "main": "net:a.test" } }"#, "permissions for 'main' must be a list of entries"),
        ] {
            let Err(e) = load(body) else { panic!("{body} should be refused") };
            let err = format!("{e:#}");
            assert!(err.contains(want), "{body} wanted '{want}', got '{err}'");
        }
        assert!(load(r#"{ "permissions": { "main": ["net:a.test"], "all": [] } }"#).is_ok());
    }

    /* only a floor above the running engine is refused and no floor runs anywhere */
    #[test]
    fn a_project_runs_on_its_engine_or_a_later_one() {
        let running = rules::parts(rules::ENGINE);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge.json");
        let at = |major, minor, patch| format!("{{ \"edge\": \"{major}.{minor}.{patch}\" }}");
        let kit = crate::kit::tests::kit();

        for body in ["{}", &at(0, 0, 1), &at(running.0, running.1, running.2)] {
            std::fs::write(&path, body).unwrap();
            assert!(Manifest::check_engine(&path, &kit).is_ok(), "{body} should run");
        }

        std::fs::write(&path, at(running.0, running.1, running.2 + 1)).unwrap();
        let err = format!("{:#}", Manifest::check_engine(&path, &kit).unwrap_err());
        assert!(err.contains("this project needs edge") && err.contains(rules::ENGINE), "{err}");
    }

    #[test]
    fn unknown_keys_survive_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge.json");
        std::fs::write(&path, r#"{ "extends": "..", "imports": {}, "future": "kept" }"#).unwrap();
        Manifest::load(&path, &crate::kit::tests::kit()).unwrap().save(&path).unwrap();
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.contains("\"extends\": \"..\""), "{back}");
        assert!(back.contains("\"future\": \"kept\""), "{back}");
    }

    /* a rewrite lays out declared fields first then the core and the rest */
    #[cfg(feature = "publish")]
    #[test]
    fn a_rewrite_leads_with_the_declared_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge.json");
        std::fs::write(&path, r#"{ "zeta": "z", "imports": { "a": "./a.mini" }, "docs": "./docs", "edge": "0.1.0", "name": "app", "alpha": ["a"] }"#).unwrap();
        Manifest::load(&path, &crate::kit::tests::kit()).unwrap().save(&path).unwrap();
        let back: Vec<String> = std::fs::read_to_string(&path).unwrap().lines().filter_map(|line| line.strip_prefix("  \"")?.split('"').next().map(str::to_string)).collect();
        assert_eq!(back, ["name", "docs", "edge", "imports", "alpha", "zeta"]);
    }
}
