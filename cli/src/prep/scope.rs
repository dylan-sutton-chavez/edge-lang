use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

// links followed in one lookup before it counts as a loop as linux does
const HOPS: usize = 40;

/* reads writes and walks that never leave one real directory */
#[derive(Clone, Debug)]
pub struct Scope {
    root: PathBuf,
}

fn outside(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is outside the scope", path.display()))
}

// the parts of a path last first so the next one to walk pops off the end
fn pieces(path: &Path) -> Vec<PathBuf> {
    path.components().rev().map(|c| PathBuf::from(c.as_os_str())).collect()
}

impl Scope {
    pub fn new(root: &Path) -> io::Result<Scope> {
        Ok(Scope { root: root.canonicalize()? })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /* the real path `path` names inside the root where its last part may not exist yet */
    pub fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        // an absolute path is taken only when it already starts at the root
        let relative = if path.is_absolute() { path.strip_prefix(&self.root).map_err(|_| outside(path))? } else { path };
        let mut parts = pieces(relative);
        let mut at = self.root.clone();
        let mut hops = 0;
        while let Some(part) = parts.pop() {
            match part.components().next() {
                None | Some(Component::CurDir) => continue,
                Some(Component::ParentDir) if at == self.root => return Err(outside(path)),
                Some(Component::ParentDir) => {
                    at.pop();
                }
                Some(Component::Normal(name)) => {
                    let next = at.join(name);
                    match next.symlink_metadata() {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            hops += 1;
                            if hops > HOPS {
                                return Err(io::Error::other(format!("{} loops through its links", path.display())));
                            }
                            // a link is walked part by part so each hop is held to the root again
                            let target = std::fs::read_link(&next)?;
                            if target.is_absolute() {
                                parts.extend(pieces(target.strip_prefix(&self.root).map_err(|_| outside(path))?));
                                at = self.root.clone();
                            } else {
                                parts.extend(pieces(&target));
                            }
                        }
                        Ok(_) => at = next,
                        // only the last part may be missing so a write can create it
                        Err(e) if e.kind() == io::ErrorKind::NotFound && parts.is_empty() => at = next,
                        Err(e) => return Err(e),
                    }
                }
                Some(Component::RootDir | Component::Prefix(_)) => return Err(outside(path)),
            }
        }
        if at.starts_with(&self.root) { Ok(at) } else { Err(outside(path)) }
    }

    fn open(&self, path: &Path, options: &mut OpenOptions) -> io::Result<File> {
        // the resolved path holds no link so one appearing after the lookup is refused
        options.custom_flags(libc::O_NOFOLLOW).open(self.resolve(path)?)
    }

    /* a file's bytes refused past `cap` so a read never holds more than asked */
    pub fn read(&self, path: &Path, cap: u64) -> io::Result<Vec<u8>> {
        let file = self.open(path, OpenOptions::new().read(true))?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other(format!("{} is not a plain file", path.display())));
        }
        let mut bytes = Vec::new();
        file.take(cap + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > cap {
            return Err(io::Error::other(format!("{} is over {cap} bytes", path.display())));
        }
        Ok(bytes)
    }

    pub fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.open(path, OpenOptions::new().write(true).create(true).truncate(true).mode(0o644))?.write_all(bytes)
    }

    /* every plain file below the root relative to it where no link is followed */
    pub fn walk(&self) -> io::Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let mut dirs = vec![self.root.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    dirs.push(entry.path());
                } else if kind.is_file() {
                    found.push(entry.path().strip_prefix(&self.root).map_err(|_| outside(&entry.path()))?.to_path_buf());
                }
            }
        }
        found.sort();
        Ok(found)
    }
}
