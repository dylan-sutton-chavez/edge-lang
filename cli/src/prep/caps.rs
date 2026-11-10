use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/* the most a clone and a log may hold where a plan names none */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    // a whole repository on disk as github strongly recommends
    pub repo: u64,
    // one file as github refuses past
    pub file: u64,
    // a step's output kept for its log
    pub log: u64,
}

impl Default for Caps {
    fn default() -> Caps {
        Caps { repo: 5 << 30, file: 100 << 20, log: 100 << 20 }
    }
}

/* what a tree holds or the first cap it goes over */
#[derive(Debug, PartialEq, Eq)]
pub enum Tree {
    Held { bytes: u64, files: u64 },
    Over { path: PathBuf, bytes: u64, cap: u64 },
}

/* the disk a tree takes counted without following a link and stopped at the first cap */
pub fn tree(root: &Path, caps: &Caps, skip: &[&str]) -> io::Result<Tree> {
    let (mut bytes, mut files) = (0u64, 0u64);
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.path().symlink_metadata()?;
            if meta.is_dir() {
                if !skip.iter().any(|name| entry.file_name() == *name) {
                    dirs.push(entry.path());
                }
                continue;
            }
            // blocks rather than length so a sparse file counts what it really takes
            let used = meta.blocks() * 512;
            if meta.is_file() && meta.len() > caps.file {
                return Ok(Tree::Over { path: entry.path(), bytes: meta.len(), cap: caps.file });
            }
            bytes += used;
            files += 1;
            if bytes > caps.repo {
                return Ok(Tree::Over { path: root.to_path_buf(), bytes, cap: caps.repo });
            }
        }
    }
    Ok(Tree::Held { bytes, files })
}

/* a step's output kept up to its cap with what went past only counted */
#[derive(Debug, Default)]
pub struct Log {
    pub kept: Vec<u8>,
    pub dropped: u64,
    cap: u64,
}

impl Log {
    pub fn new(cap: u64) -> Log {
        Log { kept: Vec::new(), dropped: 0, cap }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let room = self.cap.saturating_sub(self.kept.len() as u64).min(bytes.len() as u64) as usize;
        self.kept.extend_from_slice(&bytes[..room]);
        self.dropped += (bytes.len() - room) as u64;
    }
}
