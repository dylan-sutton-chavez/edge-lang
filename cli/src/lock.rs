use anyhow::{anyhow, Context, Result};
use edge::deps::lock::locked_spec;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use edge::deps::lock::{digest_of, Entry, Lock, FILE};

// an absent lock file reads as an empty lock
pub fn beside(manifest: &Path) -> Result<Lock> {
    let path = path_beside(manifest);
    if !path.exists() {
        return Ok(Lock::default());
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Lock::parse(&bytes).map_err(|e| anyhow!("parsing {}: {e}", path.display()))
}

// writes `lock` beside `manifest` as `edge lock` always has
pub fn save(lock: &Lock, manifest: &Path) -> Result<PathBuf> {
    let path = path_beside(manifest);
    std::fs::write(&path, format!("{}\n", lock.to_json())).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/* each declared target as the lock resolved it so callers see addresses and never versions */
pub fn resolved(manifest: &Path, imports: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let lock = beside(manifest)?;
    imports.iter().map(|(name, target)| Ok((name.clone(), locked_spec(name, target, Some(&lock)).map_err(|e| anyhow!(crate::host::lock_help(&e)))?))).collect()
}

fn path_beside(manifest: &Path) -> PathBuf {
    manifest.with_file_name(FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256-a4bf0317b809477e6e475cd791573953d3ae05c4e2b03bd5f35199609557dd90";

    #[test]
    fn a_lock_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("edge.json");
        let mut lock = Lock::default();
        lock.insert("json", Entry { version: Some("0.1.0".to_string()), url: "https://cdn.test/pkg/json/0.1.0/app.edge".to_string(), digest: DIGEST.to_string() });
        save(&lock, &manifest).unwrap();
        let back = beside(&manifest).unwrap();
        assert_eq!(back.get("json").unwrap().version.as_deref(), Some("0.1.0"));
        let text = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.ends_with("}\n"), "{text}");
        assert!(!text.contains("entries"), "the map is the whole file: {text}");
    }

    #[test]
    fn a_missing_lock_reads_as_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        assert!(beside(&dir.path().join("edge.json")).unwrap().get("json").is_none());
    }
}
