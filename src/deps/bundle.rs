use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::s;

// the leading bytes of an edge package checked before anything is trusted
pub const MAGIC: &[u8] = b"EDGEPKG\x01";

// caps on a hostile bundle with room for any real project tree
const MAX_FILES: usize = 4096;
const MAX_TOTAL: u64 = 64 << 20;

// one file inside the archive at its path below the project root
pub struct Entry {
    pub path: String,
    pub bytes: Vec<u8>,
}

/* a project as a flat length prefixed archive whose paths are checked relative on read */
pub struct Bundle {
    pub entry: String,
    pub files: Vec<Entry>,
}

impl Bundle {
    /* the magic then the entry then each file as `<len>\n<path>\n<bytes>` with ascii lengths */
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(MAGIC);
        put_bytes(&mut b, self.entry.as_bytes());
        put_usz(&mut b, self.files.len());
        for f in &self.files {
            put_bytes(&mut b, f.path.as_bytes());
            put_bytes(&mut b, &f.bytes);
        }
        b
    }

    /* a bundle decoded and checked so every path stays inside the tree */
    pub fn decode(buf: &[u8]) -> Result<Bundle, String> {
        let index = Bundle::index(buf)?;
        let files = index.files.into_iter().map(|(path, at, len)| Entry { path, bytes: buf[at..at + len].to_vec() }).collect();
        Ok(Bundle { entry: index.entry, files })
    }

    /* checks a bundle as `decode` does and says where each file sits without copying */
    pub fn index(buf: &[u8]) -> Result<Index, String> {
        let mut r = Reader { buf, p: 0 };
        if !r.take(MAGIC.len())?.starts_with(MAGIC) {
            return Err(s!("not an edge package"));
        }
        let entry = r.str()?;
        plain(&entry)?;
        let n = r.usz()?;
        if n > MAX_FILES {
            return Err(s!("bundle has ", int n, " files, over the ", int MAX_FILES, " cap"));
        }
        let mut files = Vec::with_capacity(n);
        let mut total = 0u64;
        for _ in 0..n {
            let path = r.str()?;
            plain(&path)?;
            let len = r.usz()?;
            let at = r.p;
            r.take(len)?;
            total = total.saturating_add(len as u64);
            if total > MAX_TOTAL {
                return Err(s!("bundle exceeds the ", int MAX_TOTAL, " byte cap"));
            }
            files.push((path, at, len));
        }
        Ok(Index { entry, files })
    }
}

/* where each file of a bundle sits inside its bytes */
pub struct Index {
    pub entry: String,
    // each file's path then the offset and length of its bytes
    pub files: Vec<(String, usize, usize)>,
}

/* a plain relative path or a vendored https url so no file lands outside */
fn plain(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err(s!("bundle has an empty path"));
    }
    // a vendored url keeps its host and port before the path the one rule holds
    let safe = match path.strip_prefix("https://") {
        Some(rest) => rest.split_once('/').is_some_and(|(host, below)| !host.is_empty() && !host.contains(['\\', '\0']) && super::rules::plain(below)),
        None => super::rules::plain(path),
    };
    if safe { Ok(()) } else { Err(s!("bundle path '", str path, "' is not a plain relative path")) }
}

fn put_usz(b: &mut Vec<u8>, n: usize) {
    b.extend_from_slice(n.to_string().as_bytes());
    b.push(b'\n');
}

fn put_bytes(b: &mut Vec<u8>, bytes: &[u8]) {
    put_usz(b, bytes.len());
    b.extend_from_slice(bytes);
}

struct Reader<'a> {
    buf: &'a [u8],
    p: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.p.checked_add(n).ok_or_else(|| s!("bundle length overflow"))?;
        if end > self.buf.len() {
            return Err(s!("bundle truncated"));
        }
        let slice = &self.buf[self.p..end];
        self.p = end;
        Ok(slice)
    }

    // reads an ascii length a newline ends
    fn usz(&mut self) -> Result<usize, String> {
        let start = self.p;
        while self.p < self.buf.len() && self.buf[self.p] != b'\n' {
            self.p += 1;
        }
        if self.p >= self.buf.len() {
            return Err(s!("bundle truncated reading a length"));
        }
        let text = core::str::from_utf8(&self.buf[start..self.p]).map_err(|_| s!("bundle length not ascii"))?;
        self.p += 1;
        text.parse().map_err(|_| s!("bundle length '", str text, "' is not a number"))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let n = self.usz()?;
        Ok(self.take(n)?.to_vec())
    }

    fn str(&mut self) -> Result<String, String> {
        String::from_utf8(self.bytes()?).map_err(|_| s!("bundle string not utf-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bundle {
        Bundle {
            entry: s!("main.mini"),
            files: alloc::vec![
                Entry { path: s!("main.mini"), bytes: b"import lib\n".to_vec() },
                Entry { path: s!("lib/util.mini"), bytes: b"fn f() { pass }\n".to_vec() },
                Entry { path: s!("https://example.com/dep.edge"), bytes: b"EDGEPKG\x01".to_vec() },
            ],
        }
    }

    #[test]
    fn roundtrips_a_tree() {
        let b = Bundle::decode(&sample().encode()).unwrap();
        assert_eq!(b.entry, "main.mini");
        assert_eq!(b.files.len(), 3);
        assert_eq!(b.files[1].path, "lib/util.mini");
        assert_eq!(b.files[1].bytes, b"fn f() { pass }\n");
    }

    #[test]
    fn an_index_points_at_each_file_inside_the_bytes() {
        let bytes = sample().encode();
        let index = Bundle::index(&bytes).unwrap();
        let (path, at, len) = &index.files[1];
        assert_eq!((path.as_str(), &bytes[*at..at + len]), ("lib/util.mini", &b"fn f() { pass }\n"[..]));
    }

    #[test]
    fn rejects_bad_magic_and_traversal_paths() {
        assert!(Bundle::decode(b"NOTPKG\x00\x00junk").is_err());
        for bad in ["../evil.mini", "/etc/passwd", "a/../../b", "a//b", "https://example.com/../x", "a\\b"] {
            let mut b = sample();
            b.files[0].path = bad.to_string();
            assert!(Bundle::decode(&b.encode()).is_err(), "path '{bad}' should be rejected");
        }
    }
}
