use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use super::Message;

// bytes a log may reach before done records get rewritten away
const COMPACT_AT: u64 = 64 << 20;

/* sent messages each marked done once finished so a restart replays only the rest */
pub struct Wal {
    path: PathBuf,
    file: File,
    next: u64,
    // the record size of each message not yet done which a compaction keeps
    open: HashMap<u64, u64>,
    live: u64,
    size: u64,
    // a message logged since the last fsync
    dirty: bool,
}

impl Wal {
    /* opens the log rewritten with only the unfinished messages of the previous run */
    pub fn open(path: &Path) -> std::io::Result<(Self, Vec<Message>)> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let (mut left, mut next) = (BTreeMap::new(), 0);
        if let Ok(file) = File::open(path) {
            for line in BufReader::new(file).lines().map_while(Result::ok) {
                match line.split('\t').collect::<Vec<_>>()[..] {
                    [id] if let Ok(id) = id.parse::<u64>() => {
                        left.remove(&id);
                    }
                    [id, group, body] if let Ok(id) = id.parse::<u64>() => {
                        next = next.max(id + 1);
                        left.insert(id, Message { id: Some(id), ..Message::new(unesc(group), unesc(body)) });
                    }
                    _ => {}
                }
            }
        }
        let recovered: Vec<Message> = left.into_values().collect();
        let tmp = path.with_extension("wal.tmp");
        let mut out = BufWriter::new(private(&tmp)?);
        let mut open = HashMap::new();
        for m in &recovered {
            let id = m.id.unwrap_or_default();
            let record = record(id, m);
            out.write_all(record.as_bytes())?;
            open.insert(id, record.len() as u64);
        }
        let file = install(out, &tmp, path)?;
        let size = open.values().sum();
        Ok((Wal { path: path.to_path_buf(), file, next, open, live: size, size, dirty: false }, recovered))
    }

    /* logs a message under the next id at once so a process crash keeps it */
    pub fn append(&mut self, msg: &mut Message) -> std::io::Result<()> {
        let id = self.next;
        let record = record(id, msg);
        self.write(record.as_bytes())?;
        self.next += 1;
        msg.id = Some(id);
        self.open.insert(id, record.len() as u64);
        self.live += record.len() as u64;
        self.dirty = true;
        Ok(())
    }

    /* marks a message done and compacts once done records make up most of the log */
    pub fn ack(&mut self, id: u64) {
        let Some(len) = self.open.remove(&id) else { return };
        self.live -= len;
        // a done record needs no fsync since losing one only replays a message twice
        let _ = self.write(format!("{id}\n").as_bytes());
        if self.size >= COMPACT_AT.max(2 * self.live) {
            self.compact();
        }
    }

    /* an fsync so a power cut keeps every message a client heard was taken */
    pub fn sync(&mut self) -> std::io::Result<()> {
        if self.dirty {
            self.file.sync_data()?;
            self.dirty = false;
        }
        Ok(())
    }

    // a record that failed halfway is cut so the next starts its own line
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if let Err(e) = self.file.write_all(bytes) {
            let _ = self.file.set_len(self.size);
            return Err(e);
        }
        self.size += bytes.len() as u64;
        Ok(())
    }

    /* rewrites the log with only the records of messages not yet done */
    fn compact(&mut self) {
        let tmp = self.path.with_extension("wal.tmp");
        let kept = (|| {
            let mut out = BufWriter::new(private(&tmp)?);
            for line in BufReader::new(File::open(&self.path)?).lines() {
                let line = line?;
                if let Some((id, _)) = line.split_once('\t')
                    && id.parse().is_ok_and(|id| self.open.contains_key(&id))
                {
                    out.write_all(line.as_bytes())?;
                    out.write_all(b"\n")?;
                }
            }
            install(out, &tmp, &self.path)
        })();
        if let Ok(file) = kept {
            self.file = file;
            self.size = self.live;
        }
    }
}

// one message record of id group and body tab-separated with control chars escaped
fn record(id: u64, m: &Message) -> String {
    format!("{id}\t{}\t{}\n", esc(&m.group), esc(&m.body))
}

/* renames a synced rewrite into place then syncs the directory so the swap survives */
fn install(out: BufWriter<File>, tmp: &Path, path: &Path) -> std::io::Result<File> {
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(tmp, path)?;
    #[cfg(unix)]
    {
        let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let _ = File::open(dir).and_then(|d| d.sync_all());
    }
    OpenOptions::new().append(true).open(path)
}

/* a log file only its owner reads since it holds every message body */
fn private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/* the lock a live server holds on its log for life or why it cannot */
pub fn hold(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let lock = OpenOptions::new().write(true).create(true).truncate(false).open(path.with_extension("wal.lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(std::io::Error::other("another server holds it")),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

// a carriage return is escaped too since reading a line drops one before its newline
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(group: &str, body: &str) -> Message {
        Message::new(group.to_string(), body.to_string())
    }

    // a body ending in a carriage return replays with it after a restart
    #[test]
    fn a_body_replays_exactly_as_it_was_logged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actor.wal");
        let (mut wal, _) = Wal::open(&path).unwrap();
        for body in ["line\r", "tab\there\nand\\more", "\r\n"] {
            wal.append(&mut message("g", body)).unwrap();
        }
        wal.sync().unwrap();
        drop(wal);
        let (_, recovered) = Wal::open(&path).unwrap();
        let bodies: Vec<&str> = recovered.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["line\r", "tab\there\nand\\more", "\r\n"]);
    }

    // message bodies sit in a log only its owner may read
    #[cfg(unix)]
    #[test]
    fn the_log_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actor.wal");
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&mut message("g", "secret")).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // a failed write is reported and leaves no half record the next one continues
    #[test]
    fn a_failed_write_is_reported_and_logs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actor.wal");
        let (mut wal, _) = Wal::open(&path).unwrap();
        wal.append(&mut message("g", "kept")).unwrap();
        let writable = std::mem::replace(&mut wal.file, File::open(&path).unwrap());
        let mut lost = message("g", "lost");
        assert!(wal.append(&mut lost).is_err() && lost.id.is_none());
        wal.file = writable;
        wal.append(&mut message("g", "after")).unwrap();
        drop(wal);
        let (_, recovered) = Wal::open(&path).unwrap();
        let bodies: Vec<&str> = recovered.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["kept", "after"]);
    }

    // a second server on the same log is turned away while the first holds it
    #[test]
    fn a_held_log_refuses_a_second_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actor.wal");
        let held = hold(&path).unwrap();
        assert!(hold(&path).is_err());
        drop(held);
        assert!(hold(&path).is_ok());
    }
}
