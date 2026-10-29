use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub use edge::deps::bundle::{Bundle, Entry, MAGIC};

// marks an eval body carrying a base64 project bundle rather than a raw snippet
pub const BUNDLE_TAG: &str = "EDGEPKG:";

// marks a standalone binary whose trailer holds the payload length before it
pub const STANDALONE_MAGIC: &[u8] = b"EDGESFX\x01";

/* only the trailer of this exe is read so plain runs stay cheap */
pub fn embedded_payload() -> Option<Vec<u8>> {
    trailer_payload(&std::env::current_exe().ok()?)
}

/* the bundle a raw .edge or a standalone binary with a trailer carries */
pub fn file_payload(path: &Path) -> Option<Vec<u8>> {
    let mut head = [0u8; 8];
    if let Ok(mut f) = fs::File::open(path) {
        use std::io::Read;
        if f.read_exact(&mut head).is_ok() && head.starts_with(MAGIC) {
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

/* the files keyed by path as the tree an in-memory resolver serves */
pub fn into_files(bundle: Bundle) -> HashMap<String, Vec<u8>> {
    bundle.files.into_iter().map(|f| (f.path, f.bytes)).collect()
}

/* standard padded base64 as the wire form of a bundle sent to an eval group */
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
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
            std::fs::write(&path, [&b"print(1)\n"[..], &len.to_le_bytes(), STANDALONE_MAGIC].concat()).unwrap();
            assert!(file_payload(&path).is_none(), "{len}");
        }
    }

    #[test]
    fn decodes_base64_with_and_without_padding() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64_decode("RURHRVBLRwE=").unwrap(), MAGIC);
        assert!(base64_decode("not base64!").is_none());
    }
}
