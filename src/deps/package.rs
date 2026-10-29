use alloc::string::String;
use alloc::vec::Vec;

use super::bundle::Bundle;
use super::json::quote;
use super::{parse_manifest, perms, rules, split};

/* why a registry refuses a package manifest and its lock or empty when both hold */
pub fn check(manifest: &[u8], lock: &[u8], system: &[u8]) -> String {
    let system = split(system, '\n');
    let system: Vec<&str> = system.iter().map(String::as_str).collect();
    // the registry lists packages for hosts wiring the grant logic so its holders stay taken
    rules::check_package(manifest, (!lock.is_empty()).then_some(lock), &system, &perms::reserved()).err().unwrap_or_default()
}

/* the registry fields of a manifest as JSON or why the bytes are not one */
pub fn fields(bytes: &[u8]) -> String {
    let mut out = String::from("{");
    match parse_manifest(bytes) {
        Err(e) => {
            out.push_str("\"error\":");
            quote(&mut out, &e);
        }
        Ok(m) => {
            for (i, (key, value)) in [("name", m.name), ("version", m.version), ("description", m.description), ("repository", m.repository), ("edge", m.edge)].into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                quote(&mut out, key);
                out.push(':');
                match value {
                    Some(text) => quote(&mut out, &text),
                    None => out.push_str("null"),
                }
            }
        }
    }
    out.push('}');
    out
}

/* where each file of a bundle sits as JSON or why the bytes are none */
pub fn index(bytes: &[u8]) -> String {
    let mut out = String::new();
    match Bundle::index(bytes) {
        Err(e) => {
            out.push_str("{\"error\":");
            quote(&mut out, &e);
        }
        Ok(index) => {
            out.push_str("{\"entry\":");
            quote(&mut out, &index.entry);
            out.push_str(",\"files\":[");
            for (i, (path, at, size)) in index.files.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                quote(&mut out, path);
                out.push(',');
                out.push_str(itoa::Buffer::new().format(*at));
                out.push(',');
                out.push_str(itoa::Buffer::new().format(*size));
                out.push(']');
            }
            out.push(']');
        }
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::bundle::Entry;
    use crate::s;

    #[test]
    fn a_manifest_is_checked_with_its_lock_and_the_system_modules() {
        assert_eq!(check(br#"{ "name": "slugify" }"#, b"", b"net\ntime\n"), "");
        assert_eq!(check(br#"{ "name": "time" }"#, b"", b"net\ntime"), "name 'time' is reserved for the system module of that name");
        assert_eq!(check(br#"{ "imports": { "dep": "0.1.0" } }"#, b"", b""), "'dep' is not locked in edge.lock");
        assert!(check(b"[]", b"", b"").contains("must be a JSON object"));
    }

    #[test]
    fn the_registry_fields_read_as_json_or_say_why_not() {
        assert_eq!(fields(br#"{ "name": "slugify", "edge": "0.7.5" }"#), r#"{"name":"slugify","version":null,"description":null,"repository":null,"edge":"0.7.5"}"#);
        assert_eq!(fields(br#"{ "name": ["x"] }"#), r#"{"error":"'name' must be a string"}"#);
    }

    #[test]
    fn a_bundle_index_points_at_each_file_or_says_why_not() {
        let bytes = Bundle { entry: s!("main.mini"), files: alloc::vec![Entry { path: s!("main.mini"), bytes: b"x = 1\n".to_vec() }] }.encode();
        let at = bytes.len() - 6;
        assert_eq!(index(&bytes), s!("{\"entry\":\"main.mini\",\"files\":[[\"main.mini\",", int at, ",6]]}"));
        assert_eq!(index(b"junk"), r#"{"error":"bundle truncated"}"#);
    }
}
