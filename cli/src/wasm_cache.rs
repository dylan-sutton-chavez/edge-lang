use edge::util::sha256::{hex_encode, sha256};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use wasmtime::{Engine, Module};

// an entry no run maps for this long leaves at the next compile
const KEEP: Duration = Duration::from_secs(30 * 24 * 60 * 60);

// a mapped entry whose stamp is older than this gets a fresh one
const STAMP: Duration = Duration::from_secs(24 * 60 * 60);

/* a third party plugin as native code compiled once and mapped from the cache after */
pub fn load(engine: &Engine, bytes: &[u8]) -> Result<Module, String> {
    match cache_dir() {
        Some(dir) => load_in(engine, bytes, &dir),
        None => compile(engine, bytes),
    }
}

fn load_in(engine: &Engine, bytes: &[u8], dir: &Path) -> Result<Module, String> {
    let path = dir.join(format!("{}.cwasm", key(engine, bytes)));
    if let Some(module) = mapped(engine, &path) {
        stamp(&path, SystemTime::now());
        return Ok(module);
    }
    // a missing or torn or foreign entry is a miss the fresh compile replaces
    let module = compile(engine, bytes)?;
    if let Ok(native) = module.serialize() {
        let _ = store(dir, &path, &native);
        sweep(dir, SystemTime::now());
    }
    Ok(module)
}

/* refreshes the stamp of a mapped entry at most once a day */
fn stamp(path: &Path, now: SystemTime) {
    if std::fs::metadata(path).and_then(|m| m.modified()).is_ok_and(|at| older(at, now, STAMP)) {
        let _ = std::fs::File::options().write(true).open(path).and_then(|file| file.set_modified(now));
    }
}

/* drops every entry and torn write no run has mapped within `KEEP` */
fn sweep(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let ours = path.extension().is_some_and(|ext| ext == "cwasm" || ext == "tmp");
        if ours && entry.metadata().and_then(|m| m.modified()).is_ok_and(|at| older(at, now, KEEP)) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// a stamp from the future reads as fresh so a skewed clock drops nothing
fn older(at: SystemTime, now: SystemTime, span: Duration) -> bool {
    now.duration_since(at).is_ok_and(|age| age > span)
}

/* the module an entry holds once its native code matches the digest it claims */
fn mapped(engine: &Engine, path: &Path) -> Option<Module> {
    let entry = std::fs::read(path).ok()?;
    let (claimed, native) = entry.split_at_checked(DIGEST)?;
    if sha256(native)[..] != *claimed {
        return None;
    }
    // SAFETY the digest shows these are the bytes this engine's serialization wrote
    unsafe { Module::deserialize(engine, native) }.ok()
}

fn compile(engine: &Engine, bytes: &[u8]) -> Result<Module, String> {
    Module::new(engine, bytes).map_err(|e| format!("{e:#}"))
}

/* the bytes hashed with the engine compatibility hash so a new wasmtime or config misses */
fn key(engine: &Engine, bytes: &[u8]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut hasher);
    let mut input = hasher.finish().to_le_bytes().to_vec();
    input.extend_from_slice(bytes);
    hex_encode(&sha256(&input))
}

// the bytes of the sha256 an entry writes before its native code
const DIGEST: usize = 32;

// a temp file then a rename so a reader never maps a half written artifact
fn store(dir: &Path, path: &Path, native: &[u8]) -> std::io::Result<()> {
    private(dir)?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, [&sha256(native)[..], native].concat())?;
    std::fs::rename(&tmp, path)
}

/* the cache directory made where only this user may write */
fn private(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

fn cache_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    Some(base.join(&crate::host::names().name).join("plugins"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Instance, Store};

    // a module exporting `f` that returns 42 assembled by hand
    const ANSWER: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x05,
        0x01, 0x01, 0x66, 0x00, 0x00, 0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2a, 0x0b,
    ];

    fn call_f(engine: &Engine, module: &Module) -> i32 {
        let mut store = Store::new(engine, ());
        let instance = Instance::new(&mut store, module, &[]).unwrap();
        instance.get_typed_func::<(), i32>(&mut store, "f").unwrap().call(&mut store, ()).unwrap()
    }

    #[test]
    fn the_first_load_compiles_and_the_second_maps_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::default();
        assert_eq!(call_f(&engine, &load_in(&engine, ANSWER, dir.path()).unwrap()), 42);
        let cached = dir.path().join(format!("{}.cwasm", key(&engine, ANSWER)));
        assert!(cached.exists());
        let stamp = std::fs::metadata(&cached).unwrap().modified().unwrap();
        assert_eq!(call_f(&engine, &load_in(&engine, ANSWER, dir.path()).unwrap()), 42);
        assert_eq!(std::fs::metadata(&cached).unwrap().modified().unwrap(), stamp, "a hit must not rewrite the entry");
    }

    #[test]
    fn a_corrupt_entry_is_a_miss_and_gets_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::default();
        let cached = dir.path().join(format!("{}.cwasm", key(&engine, ANSWER)));
        std::fs::write(&cached, b"not a wasmtime artifact").unwrap();
        assert_eq!(call_f(&engine, &load_in(&engine, ANSWER, dir.path()).unwrap()), 42);
        assert!(mapped(&engine, &cached).is_some());
    }

    #[test]
    fn native_code_that_differs_from_its_digest_is_never_mapped() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::default();
        load_in(&engine, ANSWER, dir.path()).unwrap();
        let cached = dir.path().join(format!("{}.cwasm", key(&engine, ANSWER)));
        let mut entry = std::fs::read(&cached).unwrap();
        let last = entry.len() - 1;
        entry[last] ^= 0x5a;
        std::fs::write(&cached, &entry).unwrap();
        assert!(mapped(&engine, &cached).is_none(), "a changed byte was mapped");
        assert_eq!(call_f(&engine, &load_in(&engine, ANSWER, dir.path()).unwrap()), 42);
        assert_ne!(std::fs::read(&cached).unwrap(), entry, "the changed entry was not replaced");
        assert!(mapped(&engine, &cached).is_some());
    }

    #[test]
    fn the_key_follows_the_bytes() {
        let engine = Engine::default();
        let mut other = ANSWER.to_vec();
        other[32] = 0x07;
        assert_ne!(key(&engine, ANSWER), key(&engine, &other));
        assert_eq!(key(&engine, ANSWER), key(&engine, ANSWER));
    }

    /* sets the stamp of `path` to `age` before now */
    fn aged(path: &Path, age: Duration) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn an_entry_no_run_mapped_for_thirty_days_leaves_at_the_next_compile() {
        let dir = tempfile::tempdir().unwrap();
        for (name, days) in [("old.cwasm", 31), ("kept.cwasm", 29), ("torn.4242.tmp", 31), ("notes.txt", 31)] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"entry").unwrap();
            aged(&path, DAY * days);
        }
        let engine = Engine::default();
        assert_eq!(call_f(&engine, &load_in(&engine, ANSWER, dir.path()).unwrap()), 42);
        let left = |name: &str| dir.path().join(name).exists();
        assert!(!left("old.cwasm") && !left("torn.4242.tmp"), "an entry past thirty days stayed");
        assert!(left("kept.cwasm") && left("notes.txt"), "the sweep dropped what it must keep");
        assert!(left(&format!("{}.cwasm", key(&engine, ANSWER))), "the fresh entry went with the old ones");
    }

    #[test]
    fn a_run_that_maps_an_entry_keeps_it_from_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::default();
        load_in(&engine, ANSWER, dir.path()).unwrap();
        let cached = dir.path().join(format!("{}.cwasm", key(&engine, ANSWER)));
        aged(&cached, DAY * 2);
        assert_eq!(call_f(&engine, &load_in(&engine, ANSWER, dir.path()).unwrap()), 42);
        let mapped_at = std::fs::metadata(&cached).unwrap().modified().unwrap();
        assert!(!older(mapped_at, SystemTime::now(), DAY), "a hit left the stamp two days old");
        sweep(dir.path(), mapped_at + DAY * 29);
        assert!(cached.exists(), "an entry mapped 29 days before the sweep left");
        sweep(dir.path(), mapped_at + DAY * 31);
        assert!(!cached.exists(), "an entry mapped 31 days before the sweep stayed");
    }

    #[test]
    fn invalid_bytes_fail_to_compile() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_in(&Engine::default(), b"\0asm\x01\0\0\0\x7f", dir.path()).unwrap_err();
        assert!(err.contains("WebAssembly"), "error was {err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
