use super::caps::Caps;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/* one output file by its path below the sealed directory and its sha256 */
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sealed {
    pub path: PathBuf,
    pub sha256: [u8; 32],
    pub bytes: u64,
}

/* copies what a swept step left in `from` into a new `to` its user cannot write and hashes it */
pub fn seal(from: &Path, to: &Path, caps: &Caps) -> io::Result<Vec<Sealed>> {
    // the caller owns `to` so only a directory made here is trusted
    DirBuilder::new().mode(0o755).create(to)?;
    let mut sealed = Vec::new();
    let mut total = 0u64;
    let mut dirs = vec![PathBuf::new()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(from.join(&dir))? {
            let entry = entry?;
            let (kind, path) = (entry.file_type()?, dir.join(entry.file_name()));
            if kind.is_dir() {
                DirBuilder::new().mode(0o755).create(to.join(&path))?;
                dirs.push(path);
                continue;
            }
            // a link or device or pipe among the outputs is a step reaching past them
            if !kind.is_file() {
                return Err(io::Error::other(format!("the output {} is not a plain file", path.display())));
            }
            let source = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(from.join(&path))?;
            let mut bytes = Vec::new();
            source.take(caps.file + 1).read_to_end(&mut bytes)?;
            total += bytes.len() as u64;
            if bytes.len() as u64 > caps.file || total > caps.repo {
                return Err(io::Error::other(format!("the output {} goes over its cap", path.display())));
            }
            OpenOptions::new().write(true).create_new(true).mode(0o644).open(to.join(&path))?.write_all(&bytes)?;
            sealed.push(Sealed { sha256: edge::util::sha256::sha256(&bytes), bytes: bytes.len() as u64, path });
        }
    }
    sealed.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(sealed)
}
