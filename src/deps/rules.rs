use alloc::string::String;

use crate::s;
use super::manifest::Manifest;

// the engine this build is and the version every floor is held to
pub const ENGINE: &str = env!("CARGO_PKG_VERSION");

// a description reads as one line in a listing and past this a readme
pub const MAX_DESCRIPTION: usize = 60;
// a name reads the same in a url and an import and a listing
pub const MAX_NAME: usize = 40;
pub const MAX_REPOSITORY: usize = 256;

const VERSION_RULE: &str = "major.minor.patch, each part 0 to 99 without leading zeros";

/* whether `target` names a JavaScript module which no host loads but its own */
pub fn javascript(target: &str) -> bool {
    let path = target.split(['?', '#']).next().unwrap_or(target);
    matches!(path.rsplit('.').next(), Some("js" | "mjs"))
}

/* whether `path` is names joined by slashes that stay below the directory it joins */
pub fn plain(path: &str) -> bool {
    let name = |segment: &str| !segment.is_empty() && segment != "." && segment != ".." && !segment.contains(['\\', ':', '\0']);
    !path.is_empty() && path.split('/').all(name)
}

/* each part 0 to 99 written plainly so a release has one spelling and order */
pub fn is_version(v: &str) -> bool {
    let mut parts = v.split('.');
    let plain = |p: &str| p == "0" || (matches!(p.len(), 1 | 2) && !p.starts_with('0') && p.bytes().all(|b| b.is_ascii_digit()));
    let three = (parts.next(), parts.next(), parts.next(), parts.next());
    matches!(three, (Some(a), Some(b), Some(c), None) if plain(a) && plain(b) && plain(c))
}

/* three runs of digits between dots as a target reads before the rule applies */
pub fn shaped_like_version(v: &str) -> bool {
    let parts: alloc::vec::Vec<&str> = v.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/* the three numbers of a version each its own integer so 0.7.0 outranks 0.6.45 */
pub fn parts(v: &str) -> (u32, u32, u32) {
    let mut read = v.split('.').map(|part| part.parse().unwrap_or(0));
    (read.next().unwrap_or(0), read.next().unwrap_or(0), read.next().unwrap_or(0))
}

/* whether `floor` asks for an engine later than `engine` */
pub fn newer(floor: &str, engine: &str) -> bool {
    parts(floor) > parts(engine)
}

/* why this engine cannot run a manifest for a later engine or none */
pub fn floor_error(m: &Manifest) -> Option<String> {
    let floor = m.edge.as_deref()?;
    newer(floor, ENGINE).then(|| s!("needs edge ", str floor, ", this is ", str ENGINE))
}

/* a name that reads the same in a url and an import and a listing */
pub fn named(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && !name.contains("--")
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/* an address a listing can open so ssh remotes and bare hosts are refused */
pub fn linked(url: &str) -> bool {
    let Some(host) = url.strip_prefix("https://") else { return false };
    !host.is_empty() && url.len() <= MAX_REPOSITORY && !url.contains(char::is_whitespace)
}

/* the field reserving `name` among the holder names a host's components reserve */
fn reserving<'a>(name: &str, reserved: &[(&str, &'a str)]) -> Option<&'a str> {
    reserved.iter().find(|(held, _)| *held == name).map(|(_, by)| *by)
}

/* why no package may take `name` or none when one may */
pub fn name_error(name: &str, system: &[&str], reserved: &[(&str, &str)]) -> Option<String> {
    if !named(name) {
        return Some(s!("name '", str name, "' must be lowercase letters, digits and single hyphens, starting with a letter, ", int MAX_NAME, " at most"));
    }
    if let Some(by) = reserving(name, reserved) {
        return Some(s!("name '", str name, "' is reserved for ", str by));
    }
    system.contains(&name).then(|| s!("name '", str name, "' is reserved for the system module of that name"))
}

/* why a declared import cannot stand for its name or the version it spells */
pub fn import_error(name: &str, target: &str, system: &[&str], reserved: &[(&str, &str)]) -> Option<String> {
    if system.contains(&name) {
        return Some(s!("import '", str name, "' takes the name of a system module, grant it under permissions instead"));
    }
    // a holder as an import key would hand the package the grants of that holder
    if let Some(by) = reserving(name, reserved) {
        return Some(s!("import '", str name, "' is reserved for ", str by));
    }
    (shaped_like_version(target) && !is_version(target)).then(|| s!("'", str name, "' is declared '", str target, "', and a version is ", str VERSION_RULE))
}

/* every field a registry reads and every import held to a packed package's rules */
pub fn check(m: &Manifest, system: &[&str], reserved: &[(&str, &str)]) -> Result<(), String> {
    if let Some(name) = &m.name
        && let Some(e) = name_error(name, system, reserved)
    {
        return Err(e);
    }
    if let Some(version) = &m.version
        && !is_version(version)
    {
        return Err(s!("version '", str version, "' must be ", str VERSION_RULE));
    }
    if let Some(description) = &m.description {
        let len = description.chars().count();
        if description.trim().is_empty() {
            return Err(s!("description is empty"));
        }
        if description.contains('\n') {
            return Err(s!("description must be one line"));
        }
        if len > MAX_DESCRIPTION {
            return Err(s!("description is ", int len, " characters, the cap is ", int MAX_DESCRIPTION));
        }
    }
    if let Some(repository) = &m.repository
        && !linked(repository)
    {
        return Err(s!("repository '", str repository, "' must be an https url a listing can link"));
    }
    if let Some(edge) = &m.edge
        && !is_version(edge)
    {
        return Err(s!("edge '", str edge, "' must be ", str VERSION_RULE));
    }
    m.imports.iter().find_map(|(name, target)| import_error(name, target, system, reserved)).map_or(Ok(()), Err)
}

/* a package's manifest held to the rules with its versions resolved by its lock */
pub fn check_package(manifest: &[u8], lock: Option<&[u8]>, system: &[&str], reserved: &[(&str, &str)]) -> Result<(), String> {
    let m = super::parse_manifest(manifest)?;
    check(&m, system, reserved)?;
    let held = lock.map(super::lock::Lock::parse).transpose()?;
    if let Some(held) = &held {
        held.check()?;
    }
    m.imports.iter().try_for_each(|(name, target)| super::lock::locked_spec(name, target, held.as_ref()).map(|_| ()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::{parse_manifest, perms};

    const SYSTEM: [&str; 2] = ["net", "time"];

    #[test]
    fn a_version_has_one_spelling_with_parts_up_to_99() {
        for v in ["0.1.0", "10.99.0", "0.0.0", "99.99.99"] {
            assert!(is_version(v), "{v}");
        }
        for v in ["01.2.3", "1.2.100", "1.2.3-beta", "v1.2.3", "1.2", "1.2.3.4", "", "1..3", "1.2.-3"] {
            assert!(!is_version(v), "{v}");
        }
    }

    // each field is its own integer so patch 45 sits below minor 7
    #[test]
    fn a_version_orders_by_its_numbers() {
        assert!(newer("0.7.0", "0.6.45") && !newer("0.6.5", "0.6.41") && newer("1.0.0", "0.99.99"));
        assert!(!newer(ENGINE, ENGINE));
    }

    #[test]
    fn a_floor_above_this_engine_says_both_versions() {
        let later = parse_manifest(br#"{ "edge": "99.0.0" }"#).unwrap();
        assert_eq!(floor_error(&later), Some(s!("needs edge 99.0.0, this is ", str ENGINE)));
        assert_eq!(floor_error(&parse_manifest(br#"{ "edge": "0.0.1" }"#).unwrap()), None);
        assert_eq!(floor_error(&parse_manifest(b"{}").unwrap()), None);
    }

    #[test]
    fn the_fields_a_registry_would_turn_away_are_refused() {
        for (body, want) in [
            (r#"{ "name": "Slugify" }"#, "must be lowercase"),
            (r#"{ "name": "slug--ify" }"#, "must be lowercase"),
            (r#"{ "name": "slugify-" }"#, "must be lowercase"),
            (r#"{ "name": "a-name-far-too-long-for-any-listing-to-show" }"#, "must be lowercase"),
            (r#"{ "name": "main" }"#, "is reserved for permissions"),
            (r#"{ "name": "eval" }"#, "is reserved for permissions"),
            (r#"{ "name": "time" }"#, "is reserved for the system module"),
            (r#"{ "version": "1.0" }"#, "must be major.minor.patch"),
            (r#"{ "version": "01.2.3" }"#, "without leading zeros"),
            (r#"{ "edge": "0.7.100" }"#, "edge '0.7.100' must be"),
            (r#"{ "description": "  " }"#, "description is empty"),
            (r#"{ "description": "two\nlines" }"#, "must be one line"),
            (r#"{ "description": "Turn absolutely any text that you have into a tidy url slug fast." }"#, "the cap is 60"),
            (r#"{ "repository": "git@github.com:x/slugify.git" }"#, "must be an https url"),
            (r#"{ "imports": { "time": "./fake.mini" } }"#, "takes the name of a system module"),
            (r#"{ "imports": { "main": "./lib/main.mini" } }"#, "import 'main' is reserved for permissions"),
            (r#"{ "imports": { "json": "0.01.0" } }"#, "is declared '0.01.0'"),
        ] {
            let err = check(&parse_manifest(body.as_bytes()).unwrap(), &SYSTEM, &perms::reserved()).unwrap_err();
            assert!(err.contains(want), "{body} wanted '{want}', got '{err}'");
        }
        let fine = r#"{ "name": "slugify", "version": "0.1.0", "description": "Turn text into a slug.", "repository": "https://github.com/x/slugify", "imports": { "json": "0.1.0", "lib": "./lib.mini" } }"#;
        assert!(check(&parse_manifest(fine.as_bytes()).unwrap(), &SYSTEM, &perms::reserved()).is_ok());
    }

    // with no component reserving holders a package may take their names
    #[test]
    fn only_the_holders_a_host_reserves_are_refused() {
        let main = parse_manifest(br#"{ "name": "main", "imports": { "eval": "./eval.mini" } }"#).unwrap();
        assert!(check(&main, &SYSTEM, &[]).is_ok());
        assert_eq!(check(&main, &SYSTEM, &[("main", "grants")]).unwrap_err(), "name 'main' is reserved for grants");
        assert_eq!(import_error("main", "./main.mini", &SYSTEM, &perms::reserved()).as_deref(), Some("import 'main' is reserved for permissions"));
    }

    // a version says nothing about where its bytes are so its package carries the lock
    #[test]
    fn a_package_declaring_a_version_carries_its_lock() {
        let manifest = br#"{ "name": "app", "imports": { "dep": "0.1.0" } }"#;
        let held = br#"{ "dep": { "version": "0.1.0", "url": "https://example.com/dep.edge", "digest": "sha256-a4bf0317b809477e6e475cd791573953d3ae05c4e2b03bd5f35199609557dd90" } }"#;
        let reserved = perms::reserved();
        assert!(check_package(manifest, Some(held), &SYSTEM, &reserved).is_ok());
        assert_eq!(check_package(manifest, None, &SYSTEM, &reserved).unwrap_err(), "'dep' is not locked in edge.lock");
        assert!(check_package(br#"{ "imports": { "dep": "0.2.0" } }"#, Some(held), &SYSTEM, &reserved).unwrap_err().contains("locked 0.1.0"));
        assert!(check_package(manifest, Some(b"{ nope"), &SYSTEM, &reserved).is_err());
        let short = br#"{ "dep": { "version": "0.1.0", "url": "https://example.com/dep.edge", "digest": "sha256-ab" } }"#;
        assert!(check_package(manifest, Some(short), &SYSTEM, &reserved).unwrap_err().contains("64 hex characters"));
    }
}
