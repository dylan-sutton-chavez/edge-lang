use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::s;
use super::json::{self, Value};

/* a parsed `edge.json` whose `extends` lends another manifest's imports for names it lacks */
#[derive(Clone, Default, Debug)]
pub struct Manifest {
    // bare names to specs scanned in order since a Vec spares a hashbrown monomorphization
    pub imports: Vec<(String, String)>,
    pub extends: Option<String>,
    // what a package carries into a registry which the compiler never reads
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub repository: Option<String>,
    // the lowest engine the manifest runs on
    pub edge: Option<String>,
}

/* an edge.json read with unknown keys skipped and numbers and booleans refused */
pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    parse_manifest_with(bytes, "").map(|(m, _)| m)
}

/* the manifest and the value under `carried` a host names for its own component */
pub fn parse_manifest_with(bytes: &[u8], carried: &str) -> Result<(Manifest, Option<Value>), String> {
    let Value::Obj(fields) = json::parse(bytes, "edge.json")? else {
        return Err(s!("edge.json must be a JSON object"));
    };
    let mut m = Manifest::default();
    let mut held = None;
    for (key, value) in fields {
        match key.as_str() {
            "imports" => m.imports = imports_of(value)?,
            "extends" => m.extends = Some(string_of(&key, value)?),
            "name" => m.name = Some(string_of(&key, value)?),
            "version" => m.version = Some(string_of(&key, value)?),
            "description" => m.description = Some(string_of(&key, value)?),
            "repository" => m.repository = Some(string_of(&key, value)?),
            "edge" => m.edge = Some(string_of(&key, value)?),
            _ if !carried.is_empty() && key == carried => held = Some(value),
            _ => {}
        }
    }
    Ok((m, held))
}

fn string_of(key: &str, value: Value) -> Result<String, String> {
    match value {
        Value::Str(s) => Ok(s),
        _ => Err(s!("'", str key, "' must be a string")),
    }
}

fn imports_of(value: Value) -> Result<Vec<(String, String)>, String> {
    let Value::Obj(fields) = value else { return Err(s!("'imports' must be an object")) };
    fields.into_iter().map(|(name, target)| match target {
        Value::Str(target) => Ok((name, target)),
        _ => Err(s!("'imports' maps '", str &name, "' to something that is not a string")),
    }).collect()
}

/* the directory of `start` then each parent ending in a slash or empty atop */
pub fn walk_up_dirs(start: &str) -> impl Iterator<Item = String> + '_ {
    let mut current = Some(start.to_string());
    core::iter::from_fn(move || {
        let dir = current.take()?;
        current = parent_dir(&dir);
        Some(dir)
    })
}

/* the directory of `spec` through its last slash where a packed package is one itself */
pub fn dir_of(spec: &str) -> String {
    let path = spec.split(['#', '?']).next().unwrap_or(spec);
    if path.ends_with(".edge") {
        return s!(str path, "/");
    }
    match spec.rfind('/') {
        Some(i) => spec[..=i].to_string(),
        None => String::new(),
    }
}

/* where a run resolves from given a script path or a directory or nothing */
pub fn entry_dir(entry: &str) -> String {
    // a leading `./` would fork the specs with phantom directories
    dir_of(entry.trim_start_matches("./"))
}

/* the spec a system module is served under to the package whose manifest `dir` holds */
pub fn system_spec(name: &str, dir: &str) -> String {
    s!("system:", str name, "@", str dir)
}

/* `target` against `dir` where absolute forms pass and `..` stops at the root or host */
pub fn join_relative(dir: &str, target: &str) -> String {
    join(dir, target).0
}

/* `target` joined to `dir` or the origin serving `dir` when the join would leave it */
pub fn joined(dir: &str, target: &str) -> Result<String, String> {
    let (spec, stopped) = join(dir, target);
    match origin(dir) {
        // a module served from a URL reaches URLs alone and never climbs past its host
        Some(home) if stopped || origin(&spec).is_none() => Err(home.to_string()),
        _ => Ok(spec),
    }
}

/* why a module served from `home` cannot reach `target` */
pub fn outside(target: &str, home: &str) -> String {
    s!("'", str target, "' is outside ", str home, " which serves its importer")
}

/* the scheme and host serving an http or https URL and none for anything else */
pub fn origin(spec: &str) -> Option<&str> {
    let rest = spec.strip_prefix("https://").or_else(|| spec.strip_prefix("http://"))?;
    let host = rest.find('/').map_or(rest.len(), |i| i + 1);
    Some(&spec[..spec.len() - rest.len() + host])
}

// the join and whether a `..` met the top of `dir` and stayed there
fn join(dir: &str, target: &str) -> (String, bool) {
    if target.contains("://") || target.starts_with('/') {
        return (target.to_string(), false);
    }
    let mut base = dir.to_string();
    let mut t = target;
    let mut stopped = false;
    let mut up = |base: &mut String| match parent_dir(base) {
        Some(parent) => *base = parent,
        None => stopped = true,
    };
    while let Some(rest) = t.strip_prefix("../") {
        up(&mut base);
        t = rest;
    }
    if t == ".." { up(&mut base); return (base, stopped); }
    if t == "." || t.is_empty() { return (base, stopped); }
    if !base.is_empty() {
        while let Some(rest) = t.strip_prefix("./") { t = rest; }
        if !base.ends_with('/') { base.push('/'); }
    }
    base.push_str(t);
    (base, stopped)
}

pub fn parent_dir(dir: &str) -> Option<String> {
    if dir.is_empty() { return None; }
    let trimmed = dir.trim_end_matches('/');
    // a URL keeps its host so a parent needs a slash past its host
    if let Some(scheme_end) = trimmed.find("://") {
        let after = &trimmed[scheme_end + 3..];
        if !after.contains('/') { return None; }
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => Some(String::new()),
        Some((head, _)) => Some(s!(str head, "/")),
        None => Some(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_relative_routes() {
        assert_eq!(join_relative("lib/test/", "./helper.mini"), "lib/test/helper.mini");
        assert_eq!(join_relative("lib/test/", "../parse.mini"), "lib/parse.mini");
        assert_eq!(join_relative("lib/test/", "../../main.mini"), "main.mini");
        assert_eq!(join_relative("lib/", "../../escape.mini"), "escape.mini"); // clamped at root
        assert_eq!(join_relative("lib/test/", "/std/x.mini"), "/std/x.mini");
        assert_eq!(join_relative("", "https://x/y.mini"), "https://x/y.mini");
    }

    // code served from a URL reaches URLs alone and never climbs past its host
    #[test]
    fn a_url_never_joins_into_a_local_path() {
        let pkg = "https://cdn.test/pkg/pay/0.1.0/app.edge/";
        assert_eq!(join_relative(pkg, "../../../../../../config.mini"), "https://cdn.test/config.mini");
        assert_eq!(join_relative("https://cdn.test/", ".."), "https://cdn.test/");
        assert_eq!(joined(pkg, "./util.mini"), Ok(s!("https://cdn.test/pkg/pay/0.1.0/app.edge/util.mini")));
        assert_eq!(joined(pkg, "../../../../x.mini"), Ok(s!("https://cdn.test/x.mini")));
        assert_eq!(joined(pkg, "https://other.test/y.mini"), Ok(s!("https://other.test/y.mini")));
        for escape in ["../../../../../config.mini", "/abs/project/config.mini", "file:///home/me/.env", "../../../../.."] {
            assert_eq!(joined(pkg, escape), Err(s!("https://cdn.test/")), "{escape}");
        }
        // local code joins as it always did with `..` stopping at the root
        assert_eq!(joined("lib/", "../../x.mini"), Ok(s!("x.mini")));
        assert_eq!(joined("lib/", "/abs/x.mini"), Ok(s!("/abs/x.mini")));
        assert_eq!(origin("http://host:8080/a/b"), Some("http://host:8080/"));
        assert_eq!(origin("https://host"), Some("https://host"));
        assert_eq!((origin("lib/a.mini"), origin("file:///a")), (None, None));
    }

    // the field a component reads stays as written and only its host names it
    #[test]
    fn a_carried_field_is_read_as_written_and_unnamed_it_is_kept_out() {
        let text = br#"{ "imports": { "a": "./a.mini" }, "permissions": ["net"] }"#;
        let (m, held) = parse_manifest_with(text, "permissions").unwrap();
        assert_eq!(m.imports, alloc::vec![(String::from("a"), String::from("./a.mini"))]);
        assert!(matches!(held, Some(Value::List(items)) if items.len() == 1 && items[0].as_str() == Some("net")));
        assert!(parse_manifest(text).is_ok());
        assert!(parse_manifest_with(text, "").unwrap().1.is_none());
        assert!(parse_manifest(br#"{ "permissions": { "main": [1] } }"#).is_err());
        assert!(parse_manifest(br#"{ "permissions": { "main": ["net" "time"] } }"#).is_err());
    }

    #[test]
    fn the_registry_fields_are_read_and_must_be_strings() {
        let m = parse_manifest(r#"{ "name": "café", "version": "0.1.0", "edge": "0.7.5", "future": { "kept": ["x"] } }"#.as_bytes()).unwrap();
        assert_eq!((m.name.as_deref(), m.version.as_deref(), m.edge.as_deref()), (Some("café"), Some("0.1.0"), Some("0.7.5")));
        assert!(parse_manifest(br#"{ "name": ["x"] }"#).unwrap_err().contains("'name' must be a string"));
    }

    #[test]
    fn a_system_module_is_spelled_per_package() {
        assert_eq!(system_spec("net", ""), "system:net@");
        assert_eq!(system_spec("time", "https://cdn/pkg/clock/0.1.0/app.edge/"), "system:time@https://cdn/pkg/clock/0.1.0/app.edge/");
    }

    #[test]
    fn dir_walk_routes() {
        assert_eq!(dir_of("lib/test/a.mini"), "lib/test/");
        assert_eq!(dir_of("a.mini"), "");
        assert_eq!(dir_of("https://cdn/pkg/test/0.1.0/app.edge#sha256-00"), "https://cdn/pkg/test/0.1.0/app.edge/");
        assert_eq!(parent_dir("lib/test/"), Some("lib/".into()));
        assert_eq!(parent_dir("lib/"), Some("".into()));
        assert_eq!(parent_dir(""), None);
    }

    #[test]
    fn a_run_resolves_from_its_entry_directory() {
        assert_eq!(entry_dir("sub/main.mini"), "sub/");
        assert_eq!(entry_dir("././sub/a_test.mini"), "sub/");
        assert_eq!(entry_dir("actors/"), "actors/");
        assert_eq!(entry_dir("main.mini"), "");
        assert_eq!(entry_dir(""), "");
    }
}
