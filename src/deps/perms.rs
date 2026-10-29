use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::s;
use super::json::{self, list, quote, Value};

// the manifest field the grants of each package sit under
pub const FIELD: &str = "permissions";

// the holder whose entries every other holder of a section also gets
const ALL: &str = "all";

// the holders a section names beside import keys so no package or import takes one
pub const HOLDERS: [&str; 3] = [ALL, "main", "eval"];

/* each holder of a section with its entries and the holders in name order */
pub type Section = Vec<(String, Vec<String>)>;

/* each name the holders reserve beside the field that reserves it */
pub fn reserved() -> [(&'static str, &'static str); 3] {
    HOLDERS.map(|holder| (holder, FIELD))
}

/* an entry as its module and the scope after the first colon */
fn split(entry: &str) -> (&str, Option<&str>) {
    match entry.split_once(':') {
        Some((module, scope)) => (module, Some(scope)),
        None => (entry, None),
    }
}

/* joins `items` after `prefix` apart from the slice join the VM keeps inlined */
fn joined<'a>(items: impl IntoIterator<Item = &'a str>, prefix: &str) -> String {
    let mut out = String::new();
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(prefix);
        out.push_str(item);
    }
    out
}

/* the entries in their first order with every later copy gone */
fn first(entries: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for entry in entries {
        if !kept.contains(&entry) {
            kept.push(entry);
        }
    }
    kept
}

// name order makes every host read a section alike whatever order its json keeps
fn holders(value: &Value) -> Option<Vec<(&str, &Value)>> {
    let Value::Obj(fields) = value else { return None };
    let mut named: Vec<(&str, &Value)> = fields.iter().map(|(holder, entries)| (holder.as_str(), entries)).collect();
    named.sort_by(|a, b| a.0.cmp(b.0));
    Some(named)
}

/* the texts a holder lists or none when it lists anything else */
fn entries(value: &Value) -> Option<Vec<&str>> {
    match value {
        Value::List(items) => items.iter().map(Value::as_str).collect(),
        _ => None,
    }
}

fn unmapped(field: &str) -> String {
    s!(str field, " must map each package to a list of entries")
}

fn unlisted(field: &str, holder: &str) -> String {
    s!(str field, " for '", str holder, "' must be a list of entries such as \"net:api.example.com\"")
}

/* the section a `field` value holds or why it is malformed */
pub fn section(value: &Value, field: &str) -> Result<Section, String> {
    let holders = holders(value).ok_or_else(|| unmapped(field))?;
    holders
        .into_iter()
        .map(|(holder, listed)| {
            let listed = entries(listed).ok_or_else(|| unlisted(field, holder))?;
            Ok((holder.to_string(), listed.into_iter().map(str::to_string).collect()))
        })
        .collect()
}

/* why a section is malformed or names a module or scope the host lacks */
pub fn check(value: &Value, modules: &[&str], takes: &dyn Fn(&str, &str) -> bool) -> Option<String> {
    let Some(holders) = holders(value) else { return Some(unmapped(FIELD)) };
    for (holder, listed) in holders {
        let Some(listed) = entries(listed) else { return Some(unlisted(FIELD, holder)) };
        for (name, scope) in listed.into_iter().map(split) {
            if !modules.contains(&name) {
                return Some(s!(str FIELD, " for '", str holder, "' name '", str name, "', which is not a host module (", str &joined(modules.iter().copied(), ""), ")"));
            }
            if let Some(scope) = scope
                && !takes(name, scope)
            {
                return Some(s!(str FIELD, " for '", str holder, "' give ", str name, " the scope '", str scope, "', which it does not have"));
            }
        }
    }
    None
}

// a bare entry asks only to import its module which any entry for it covers
fn covers(held: &[String], entry: &str) -> bool {
    match split(entry) {
        (module, None) => held.iter().any(|e| split(e).0 == module),
        _ => held.iter().any(|e| e == entry),
    }
}

fn listed<'a>(section: &'a Section, holder: &str) -> &'a [String] {
    section.iter().find(|(name, _)| name == holder).map_or(&[], |(_, entries)| entries.as_slice())
}

/* what the end of `chain` holds with a section and key per link */
pub fn held(chain: &[(Section, String)]) -> Vec<String> {
    let mut held: Option<Vec<String>> = None;
    for (section, key) in chain {
        let given = listed(section, ALL).iter().chain(listed(section, key)).cloned();
        // no importer passes on more than it holds itself
        held = Some(match held {
            None => given.collect(),
            Some(prior) => given.filter(|entry| covers(&prior, entry)).collect(),
        });
    }
    first(held.unwrap_or_default())
}

/* the scopes `held` gives `module` or none when no entry names it */
pub fn scopes(held: &[String], module: &str) -> Option<Vec<String>> {
    let mut found: Option<Vec<String>> = None;
    for (name, scope) in held.iter().map(|entry| split(entry)) {
        if name != module {
            continue;
        }
        let scopes = found.get_or_insert_with(Vec::new);
        if let Some(scope) = scope {
            scopes.push(scope.to_string());
        }
    }
    found
}

/* every entry of `section` that `held` misses since a package needs what it passes on */
pub fn unmet(held: &[String], section: &Section) -> Vec<String> {
    let asked = first(section.iter().flat_map(|(_, entries)| entries.iter().cloned()));
    asked.into_iter().filter(|entry| !covers(held, entry)).collect()
}

/* why `pkg` may not import `module` when no entry names it */
pub fn unheld(pkg: &str, module: &str) -> String {
    s!("'", str pkg, "' imports ", str module, ", which edge.json does not grant it")
}

/* the words a refusal of `pkg` puts before and after the scope it asked */
pub fn refusal(pkg: &str, module: &str, scopes: &[String]) -> (String, String) {
    let granted = match scopes.is_empty() {
        true => s!("nothing"),
        false => joined(scopes.iter().map(String::as_str), &s!(str module, ":")),
    };
    (s!("'", str pkg, "' has no ", str module, ":"), s!(", edge.json grants it ", str &granted))
}

/* refuses `pkg` unless `scopes` holds `scope` naming what it holds instead */
pub fn need(pkg: &str, module: &str, scopes: &[String], scope: &str) -> Result<(), String> {
    if scopes.iter().any(|held| held == scope) {
        return Ok(());
    }
    let (before, after) = refusal(pkg, module, scopes);
    Err(s!(str &before, str scope, str &after))
}

/* each chain link as its section and key with a malformed one giving nothing */
pub fn links(chain: &Value) -> Vec<(Section, String)> {
    let Value::List(links) = chain else { return Vec::new() };
    links
        .iter()
        .map(|link| match link {
            Value::List(pair) if pair.len() == 2 => (section(&pair[0], FIELD).unwrap_or_default(), pair[1].as_str().unwrap_or_default().to_string()),
            _ => (Vec::new(), String::new()),
        })
        .collect()
}

/* the texts a json list holds or why the bytes are none */
fn texts(bytes: &[u8], what: &str) -> Result<Vec<String>, String> {
    match json::parse(bytes, what)? {
        Value::List(items) => items.into_iter().map(|item| item.as_str().map(str::to_string)).collect::<Option<_>>().ok_or_else(|| s!(str what, " must be a list of texts")),
        _ => Err(s!(str what, " must be a list of texts")),
    }
}

/* `check` over json where `taken` lists each scoped entry the host modules take */
pub fn check_json(section: &[u8], modules: &[u8], taken: &[u8]) -> String {
    let read = || Ok::<_, String>((json::parse(section, FIELD)?, texts(modules, "modules")?, texts(taken, "taken")?));
    let (section, modules, taken) = match read() {
        Ok(read) => read,
        Err(e) => return e,
    };
    let names: Vec<&str> = modules.iter().map(String::as_str).collect();
    check(&section, &names, &|name, scope| taken.iter().any(|entry| split(entry) == (name, Some(scope)))).unwrap_or_default()
}

/* each scoped entry of a json section as a module and scope its host judges */
pub fn scopes_json(section: &[u8]) -> String {
    let value = json::parse(section, FIELD).ok();
    let mut out = s!("[");
    let asked = value.as_ref().and_then(holders).unwrap_or_default().into_iter().filter_map(|(_, listed)| entries(listed)).flatten();
    let scoped = asked.filter_map(|entry| match split(entry) {
        (module, Some(scope)) => Some((module, scope)),
        _ => None,
    });
    for (i, (module, scope)) in scoped.enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        quote(&mut out, module);
        out.push(',');
        quote(&mut out, scope);
        out.push(']');
    }
    out.push(']');
    out
}

/* what a json chain gives `pkg` of `module` and how a refusal of it reads */
pub fn held_json(chain: &[u8], pkg: &str, module: &str) -> String {
    let links = json::parse(chain, "chain").map(|chain| links(&chain)).unwrap_or_default();
    let mut out = String::from("{");
    match scopes(&held(&links), module) {
        None => {
            out.push_str("\"refused\":");
            quote(&mut out, &unheld(pkg, module));
        }
        Some(scopes) => {
            let (before, after) = refusal(pkg, module, &scopes);
            out.push_str("\"scopes\":");
            list(&mut out, &scopes);
            out.push_str(",\"refusal\":");
            list(&mut out, &[before, after]);
        }
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn value(text: &str) -> Value {
        json::parse(text.as_bytes(), FIELD).unwrap()
    }

    fn sec(text: &str) -> Section {
        section(&value(text), FIELD).unwrap()
    }

    fn chain(links: &[(&str, &str)]) -> Vec<(Section, String)> {
        links.iter().map(|(text, key)| (sec(text), key.to_string())).collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    /* scopes that are lowercase words */
    fn words(module: &str, scope: &str) -> bool {
        module == "words" && !scope.is_empty() && scope.bytes().all(|b| b.is_ascii_lowercase())
    }

    #[test]
    fn a_package_holds_what_its_importer_grants_its_key_and_passes_on_only_what_it_holds() {
        let root = r#"{ "all": ["words:wall"], "main": ["words:api", "words:zone"], "http": ["words"] }"#;
        assert_eq!(scopes(&held(&chain(&[(root, "main")])), "words"), Some(strings(&["wall", "api", "zone"])));
        assert_eq!(scopes(&held(&chain(&[(root, "http")])), "words"), Some(strings(&["wall"])));
        assert_eq!(scopes(&held(&chain(&[(r#"{ "http": ["words"] }"#, "http")])), "words"), Some(vec![]));
        assert_eq!(scopes(&held(&chain(&[(root, "http")])), "db"), None);
        assert_eq!(scopes(&held(&[]), "words"), None);
        let granted = (r#"{ "pay": ["words:stripe", "words:wall"] }"#, "pay");
        assert_eq!(held(&chain(&[granted, (r#"{ "all": ["words:wall"], "http": ["words:stripe", "words:other"] }"#, "http")])), strings(&["words:wall", "words:stripe"]));
        assert_eq!(held(&chain(&[granted, (r#"{ "dns": ["words"] }"#, "dns")])), strings(&["words"]));
    }

    #[test]
    fn a_package_needs_what_it_lists_for_itself_and_for_what_it_imports() {
        let has = strings(&["words:wall", "words:telemetry"]);
        assert!(unmet(&has, &sec(r#"{ "main": ["words:telemetry", "words:wall"] }"#)).is_empty());
        assert_eq!(unmet(&has, &sec(r#"{ "main": ["words:clock"], "all": ["words:evil"] }"#)), strings(&["words:evil", "words:clock"]));
        assert!(unmet(&has, &sec(r#"{ "main": ["words"] }"#)).is_empty());
        assert_eq!(unmet(&[], &sec(r#"{ "main": ["words"] }"#)), strings(&["words"]));
        assert_eq!(unmet(&has, &sec(r#"{ "http": ["words:evil", "words:evil"] }"#)), strings(&["words:evil"]));
    }

    #[test]
    fn a_malformed_section_says_what_it_needs() {
        let modules = ["words"];
        assert_eq!(check(&value(r#"{ "all": ["words:wall"], "main": ["words", "words:api"] }"#), &modules, &words), None);
        let cases = [
            (r#"["words"]"#, "permissions must map each package to a list of entries"),
            (r#"{ "main": "words:api" }"#, "permissions for 'main' must be a list of entries such as \"net:api.example.com\""),
            (r#"{ "main": ["db:/tmp"] }"#, "permissions for 'main' name 'db', which is not a host module (words)"),
            (r#"{ "main": ["words:API"] }"#, "permissions for 'main' give words the scope 'API', which it does not have"),
            (r#"{ "main": ["words:"] }"#, "permissions for 'main' give words the scope '', which it does not have"),
        ];
        for (text, want) in cases {
            assert_eq!(check(&value(text), &modules, &words).as_deref(), Some(want), "{text}");
        }
        assert_eq!(check(&value(r#"{ "main": ["words"] }"#), &[], &words).as_deref(), Some("permissions for 'main' name 'words', which is not a host module ()"));
    }

    // a section reads in holder name order whatever order the host kept its keys in
    #[test]
    fn holders_read_in_name_order() {
        let both = r#"{ "zeta": ["db:/"], "alpha": ["words:API"] }"#;
        assert_eq!(check(&value(both), &["words"], &words).as_deref(), Some("permissions for 'alpha' give words the scope 'API', which it does not have"));
        assert_eq!(section(&value(r#"{ "zeta": "x", "alpha": "y" }"#), FIELD).unwrap_err(), "permissions for 'alpha' must be a list of entries such as \"net:api.example.com\"");
        assert_eq!(sec(r#"{ "main": ["b"], "all": ["a"] }"#), vec![(s!("all"), strings(&["a"])), (s!("main"), strings(&["b"]))]);
    }

    #[test]
    fn a_refusal_names_what_the_package_holds_instead() {
        let held = strings(&["api", "wall"]);
        assert!(need("main", "words", &held, "api").is_ok());
        assert_eq!(need("main", "words", &held, "zone").unwrap_err(), "'main' has no words:zone, edge.json grants it words:api, words:wall");
        assert_eq!(need("analytics", "words", &[], "zone").unwrap_err(), "'analytics' has no words:zone, edge.json grants it nothing");
        assert_eq!(unheld("clock", "words"), "'clock' imports words, which edge.json does not grant it");
    }

    #[test]
    fn a_host_reads_each_rule_through_json() {
        let section = br#"{ "main": ["words:api", "words", "db:/x"] }"#;
        assert_eq!(scopes_json(section), r#"[["words","api"],["db","/x"]]"#);
        assert_eq!(scopes_json(b"junk"), "[]");
        assert_eq!(check_json(section, br#"["words", "db"]"#, br#"["words:api", "db:/x"]"#), "");
        assert_eq!(check_json(section, br#"["words", "db"]"#, br#"["words:api"]"#), "permissions for 'main' give db the scope '/x', which it does not have");
        assert_eq!(check_json(section, br#"["words"]"#, br#"["words:api"]"#), "permissions for 'main' name 'db', which is not a host module (words)");
        assert!(check_json(b"{", b"[]", b"[]").contains("permissions"));
        let chain = br#"[[{ "all": ["words:wall"], "pay": ["words:api"] }, "pay"]]"#;
        assert_eq!(held_json(chain, "pay", "words"), r#"{"scopes":["wall","api"],"refusal":["'pay' has no words:",", edge.json grants it words:wall, words:api"]}"#);
        assert_eq!(held_json(chain, "pay", "db"), r#"{"refused":"'pay' imports db, which edge.json does not grant it"}"#);
        // a malformed link grants nothing rather than failing open
        assert_eq!(held_json(br#"[[["words"], "pay"]]"#, "pay", "words"), r#"{"refused":"'pay' imports words, which edge.json does not grant it"}"#);
        assert_eq!(held_json(b"junk", "pay", "words"), r#"{"refused":"'pay' imports words, which edge.json does not grant it"}"#);
    }
}
