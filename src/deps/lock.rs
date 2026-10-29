use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::s;
use crate::util::sha256::{hex_encode, sha256};
use super::json::{self, Value};
use super::{parse_integrity, rules};

/* the generated file beside an edge.json holding what each name it declares resolves to */
pub const FILE: &str = "edge.lock";

/* one resolved name with its address and digest and no version for a url */
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub version: Option<String>,
    pub url: String,
    pub digest: String,
}

/* what a manifest's versions and urls resolved to once each in name order */
#[derive(Clone, Debug, Default)]
pub struct Lock {
    entries: Vec<(String, Entry)>,
}

impl Lock {
    pub fn parse(bytes: &[u8]) -> Result<Lock, String> {
        let Value::Obj(fields) = json::parse(bytes, FILE)? else {
            return Err(s!(str FILE, " must map each name to its entry"));
        };
        let mut lock = Lock::default();
        for (name, value) in fields {
            let text = |key: &str| value.get(key).map(|v| v.as_str().map(String::from));
            let shape = || s!("'", str &name, "' in ", str FILE, " must hold a url and a digest as strings");
            let (Some(Some(url)), Some(Some(digest))) = (text("url"), text("digest")) else { return Err(shape()) };
            let version = match text("version") {
                None => None,
                Some(Some(v)) => Some(v),
                Some(None) => return Err(shape()),
            };
            lock.insert(&name, Entry { version, url, digest });
        }
        Ok(lock)
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|(n, _)| n == name).map(|(_, e)| e)
    }

    pub fn insert(&mut self, name: &str, entry: Entry) {
        match self.entries.binary_search_by(|(n, _)| n.as_str().cmp(name)) {
            Ok(at) => self.entries[at].1 = entry,
            Err(at) => self.entries.insert(at, (name.to_string(), entry)),
        }
    }

    pub fn entries(&self) -> &[(String, Entry)] {
        &self.entries
    }

    /* every entry as a registry holds it so a bad published lock fails when read */
    pub fn check(&self) -> Result<(), String> {
        self.entries.iter().try_for_each(|(name, entry)| entry_error(name, entry).map_or(Ok(()), Err))
    }

    /* the lock as its writer writes it in two space JSON and name order */
    pub fn to_json(&self) -> String {
        if self.entries.is_empty() {
            return s!("{}");
        }
        let mut out = s!("{\n");
        for (i, (name, entry)) in self.entries.iter().enumerate() {
            out.push_str("  ");
            json::quote(&mut out, name);
            out.push_str(": {\n");
            if let Some(version) = &entry.version {
                out.push_str("    \"version\": ");
                json::quote(&mut out, version);
                out.push_str(",\n");
            }
            out.push_str("    \"url\": ");
            json::quote(&mut out, &entry.url);
            out.push_str(",\n    \"digest\": ");
            json::quote(&mut out, &entry.digest);
            out.push_str(if i + 1 == self.entries.len() { "\n  }\n" } else { "\n  },\n" });
        }
        out.push('}');
        out
    }
}

/* why an entry cannot be published for its address or its digest or its release */
fn entry_error(name: &str, entry: &Entry) -> Option<String> {
    if !entry.url.starts_with("https://") {
        return Some(s!("'", str name, "' holds '", str &entry.url, "' in ", str FILE, ", which is not an https url"));
    }
    digest_error(name, entry).or_else(|| {
        entry.version.as_deref().filter(|v| !rules::is_version(v)).map(|v| s!("'", str name, "' is locked at '", str v, "', which is not a version"))
    })
}

// any host fetches the schemes it reaches but a malformed digest pins nothing anywhere
fn digest_error(name: &str, entry: &Entry) -> Option<String> {
    let hex = entry.digest.strip_prefix("sha256-").filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    hex.is_none().then(|| s!("'", str name, "' holds '", str &entry.digest, "' in ", str FILE, ", which is not sha256- and 64 hex characters"))
}

/* whether a target needs a lock to say where its bytes are or their digest */
pub fn needs_lock(target: &str) -> bool {
    rules::shaped_like_version(target) || (target.contains("://") && !target.contains("#sha256-"))
}

/* a declared target as the pinned spec its lock spells where paths and pins pass */
pub fn locked_spec(name: &str, target: &str, lock: Option<&Lock>) -> Result<String, String> {
    let entry = lock.and_then(|l| l.get(name));
    if rules::shaped_like_version(target) {
        let Some(entry) = entry else {
            return Err(s!("'", str name, "' is not locked in ", str FILE));
        };
        if entry.version.as_deref() != Some(target) {
            let held = entry.version.as_deref().unwrap_or("a url");
            return Err(s!("'", str name, "' is declared ", str target, " and locked ", str held, " in ", str FILE));
        }
        return pinned(name, entry);
    }
    match entry {
        Some(entry) if target.contains("://") && !target.contains("#sha256-") => pinned(name, entry),
        _ => Ok(target.to_string()),
    }
}

fn pinned(name: &str, entry: &Entry) -> Result<String, String> {
    if let Some(e) = digest_error(name, entry) {
        return Err(e);
    }
    Ok(s!(str &entry.url, "#", str &entry.digest))
}

/* the digest of `bytes` as a lock records it */
pub fn digest_of(bytes: &[u8]) -> String {
    s!("sha256-", str &hex_encode(&sha256(bytes)))
}

/* the bytes behind a pinned spec hash to its digest while an unpinned spec passes */
pub fn verify_pin(spec: &str, bytes: &[u8]) -> Result<(), String> {
    let (target, pin) = parse_integrity(spec)?;
    let Some(want) = pin else { return Ok(()) };
    let got = sha256(bytes);
    if got == want {
        return Ok(());
    }
    Err(s!("integrity check failed for '", str target, "'\n expected sha256-", str &hex_encode(&want), "\n got sha256-", str &hex_encode(&got)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // a digest the pin parser accepts so no fixture holds a shape a fetch refuses
    const DIGEST: &str = "sha256-a4bf0317b809477e6e475cd791573953d3ae05c4e2b03bd5f35199609557dd90";

    fn locked(name: &str, version: Option<&str>) -> Lock {
        let mut lock = Lock::default();
        lock.insert(name, Entry { version: version.map(String::from), url: s!("https://cdn.test/pkg/", str name, "/0.1.0/app.edge"), digest: DIGEST.to_string() });
        lock
    }

    #[test]
    fn a_locked_version_becomes_a_pinned_url() {
        let spec = locked_spec("json", "0.1.0", Some(&locked("json", Some("0.1.0")))).unwrap();
        assert_eq!(spec, s!("https://cdn.test/pkg/json/0.1.0/app.edge#", str DIGEST));
    }

    #[test]
    fn a_version_the_lock_does_not_hold_names_the_command_that_writes_it() {
        assert_eq!(locked_spec("json", "0.1.0", None).unwrap_err(), "'json' is not locked in edge.lock");
        let err = locked_spec("json", "0.2.0", Some(&locked("json", Some("0.1.0")))).unwrap_err();
        assert!(err.contains("declared 0.2.0 and locked 0.1.0"), "{err}");
    }

    // a path holds its bytes and a pinned url its digest so neither needs locking
    #[test]
    fn a_path_and_a_pinned_url_pass_through_and_a_bare_url_gains_its_digest() {
        for target in ["./lib/util.mini", "https://x/y.wasm#sha256-ab"] {
            assert_eq!(locked_spec("util", target, None).unwrap(), target);
        }
        assert_eq!(locked_spec("foo", "https://x/y.wasm", None).unwrap(), "https://x/y.wasm");
        let mut lock = Lock::default();
        lock.insert("foo", Entry { version: None, url: s!("https://x/y.wasm"), digest: DIGEST.to_string() });
        assert_eq!(locked_spec("foo", "https://x/y.wasm", Some(&lock)).unwrap(), s!("https://x/y.wasm#", str DIGEST));
    }

    #[test]
    fn an_entry_that_could_never_pin_names_the_lock() {
        let mut lock = Lock::default();
        lock.insert("json", Entry { version: Some(s!("0.1.0")), url: s!("https://x/y.edge"), digest: s!("sha256-00") });
        assert!(locked_spec("json", "0.1.0", Some(&lock)).unwrap_err().contains("holds 'sha256-00' in edge.lock"));
        lock.insert("json", Entry { version: Some(s!("0.1.0")), url: s!("http://x/y.edge"), digest: DIGEST.to_string() });
        assert!(lock.check().unwrap_err().contains("which is not an https url"));
    }

    #[test]
    fn a_lock_round_trips_through_its_text_in_name_order() {
        let mut lock = locked("re", Some("0.1.0"));
        lock.insert("json", Entry { version: None, url: s!("https://x/json.wasm"), digest: DIGEST.to_string() });
        let text = lock.to_json();
        assert!(text.find("\"json\"").unwrap() < text.find("\"re\"").unwrap(), "{text}");
        let back = Lock::parse(text.as_bytes()).unwrap();
        assert_eq!(back.get("re"), lock.get("re"));
        assert_eq!(back.get("json").unwrap().version, None);
        assert_eq!(Lock::default().to_json(), "{}");
        assert!(Lock::parse(br#"{ "json": { "url": "https://x" } }"#).unwrap_err().contains("must hold a url and a digest"));
    }

    #[test]
    fn a_pin_holds_the_bytes_to_their_digest() {
        let digest = digest_of(b"print(1)\n");
        assert!(verify_pin(&s!("./a.mini#", str &digest), b"print(1)\n").is_ok());
        assert!(verify_pin("./a.mini", b"anything").is_ok());
        let err = verify_pin(&s!("./a.mini#", str &digest), b"print(2)\n").unwrap_err();
        assert!(err.starts_with("integrity check failed for './a.mini'"), "{err}");
    }
}
