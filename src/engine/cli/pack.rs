use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub(crate) use crate::engine::deps::bundle::{Bundle, PREFIX};

// marks an eval body carrying a base64 project bundle rather than a raw snippet
pub(crate) const BUNDLE_TAG: &str = "INTRPKG:";

// marks a standalone binary whose trailer holds the payload length before it
pub(crate) const STANDALONE_MAGIC: &[u8] = b"INTRSFX\x01";

/* only the trailer of this exe is read so plain runs stay cheap */
pub(crate) fn embedded_payload() -> Option<Vec<u8>> {
    trailer_payload(&std::env::current_exe().ok()?)
}

/* the bundle a raw .interpreter or a standalone binary with a trailer carries */
pub(crate) fn file_payload(path: &Path) -> Option<Vec<u8>> {
    let mut head = [0u8; 8];
    if let Ok(mut f) = fs::File::open(path) {
        use std::io::Read;
        if f.read_exact(&mut head).is_ok() && head.starts_with(PREFIX) {
            return fs::read(path).ok();
        }
    }
    trailer_payload(path)
}

/* the appended payload of a standalone binary and None without a trailer */
fn trailer_payload(path: &Path) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path).ok()?;
    let trailer = (8 + STANDALONE_MAGIC.len()) as u64;
    let total = file.seek(SeekFrom::End(0)).ok()?;
    if total < trailer {
        return None;
    }
    file.seek(SeekFrom::End(-(trailer as i64))).ok()?;
    let mut buf = [0u8; 8 + 8];
    file.read_exact(&mut buf[..trailer as usize]).ok()?;
    if &buf[8..trailer as usize] != STANDALONE_MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(buf[..8].try_into().ok()?);
    // a length past the file is no payload whatever the trailer claims
    let start = total.checked_sub(trailer.checked_add(len)?)?;
    let mut payload = vec![0u8; usize::try_from(len).ok()?];
    file.seek(SeekFrom::Start(start)).ok()?;
    file.read_exact(&mut payload).ok()?;
    Some(payload)
}

// the path a bundle carries its runtime under so it runs on the runtime it was built on
pub(crate) const RUNTIME_FILE: &str = "interpreter.runtime.wasm";

// the path a bundle carries a native runtime under for the platform it was built on
pub(crate) const RUNTIME_NATIVE_FILE: &str = "interpreter.runtime.native";

/* the file a bundle carries at `path` as its runtime module or native library */
pub(crate) fn carried(payload: &[u8], path: &str) -> Option<Vec<u8>> {
    Bundle::decode(payload).ok()?.files.into_iter().find(|f| f.path == path).map(|f| f.bytes)
}

/* a native library a bundle carries written once under the cache by its digest so a child can load it */
pub(crate) fn unpacked_native(bytes: &[u8]) -> anyhow::Result<std::path::PathBuf> {
    let digest = crate::engine::util::sha256::hex_encode(&crate::engine::util::sha256::sha256(bytes));
    let dir = crate::engine::cli::host::cache_root().map_err(|e| anyhow::anyhow!(e))?.join("native");
    fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
    let path = dir.join(format!("{digest}.{}", std::env::consts::DLL_EXTENSION));
    if !path.exists() {
        let partial = path.with_extension("partial");
        fs::write(&partial, bytes).map_err(|e| anyhow::anyhow!("unpacking the native runtime: {e}"))?;
        fs::rename(&partial, &path).map_err(|e| anyhow::anyhow!("unpacking the native runtime: {e}"))?;
    }
    Ok(path)
}

/* the files keyed by path as the tree an in-memory resolver serves */
pub(crate) fn into_files(bundle: Bundle) -> HashMap<String, Vec<u8>> {
    bundle.files.into_iter().map(|f| (f.path, f.bytes)).collect()
}

/* standard padded base64 as the wire form of a bundle sent to an eval group */
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trailer_claiming_more_than_the_file_holds_is_no_payload() {
        let dir = tempfile::tempdir().unwrap();
        for len in [u64::MAX - 5, u64::MAX, 1 << 40, 64] {
            let path = dir.path().join("crafted");
            std::fs::write(&path, [&b"print 1;\n"[..], &len.to_le_bytes(), STANDALONE_MAGIC].concat()).unwrap();
            assert!(file_payload(&path).is_none(), "{len}");
        }
    }

    #[test]
    fn decodes_base64_with_and_without_padding() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64_decode("SU5UUlBLRwE=").unwrap(), crate::engine::deps::bundle::MAGIC);
        assert!(base64_decode("not base64!").is_none());
    }
}
