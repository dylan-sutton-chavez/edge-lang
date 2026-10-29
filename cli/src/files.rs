use crate::host::{plain_path as plain, Calls, Fault, Held, HostModule, Instance, Reply, WireValue};
use flate2::bufread::GzDecoder;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

/* reads of the files a grant names below the directory of the package's manifest */
pub struct Files;

const CALLS: [&str; 3] = ["open", "read", "close"];

// how much of a file one read hands over as net hands over a body
const CHUNK: usize = 64 << 10;

// one counter for every package so an id never names two files
static NEXT: AtomicI64 = AtomicI64::new(1);

impl HostModule for Files {
    fn name(&self) -> &str {
        "files"
    }

    // a scope is a plain path and one trailing slash makes it a whole directory
    fn scope(&self, scope: &str) -> bool {
        plain(scope.strip_suffix('/').unwrap_or(scope))
    }

    // an open reaches the path it names and reads of an open file reach nothing
    fn reaches(&self, call: &str, args: &[WireValue]) -> Result<Option<String>, Fault> {
        match call {
            "open" => path_of(args.first().unwrap_or(&WireValue::None)).map(|path| Some(path.to_string())),
            _ => Ok(None),
        }
    }

    // a scope names the file or with a trailing slash everything below the directory
    fn covers(&self, scope: &str, reached: &str) -> bool {
        scope == reached || (scope.ends_with('/') && reached.starts_with(scope))
    }

    // a grant reaching a file lets the package write it through a file channel too
    fn writes(&self, held: &Held, path: &str) -> bool {
        held.reach(self, path).is_ok()
    }

    fn open(&self, inst: &mut Instance, spec: &str, _pkg: &str, held: Held) -> Result<(), String> {
        // a package reads below the manifest its spec names and untrusted runs below the pool's
        let dir = match (inst.project.untrusted, &inst.project.pool) {
            (false, _) => spec.strip_prefix("system:files@").unwrap_or_default().to_string(),
            (true, Some(pool)) => pool.clone(),
            (true, None) => return Err(format!("module '{}' is not available to untrusted eval runs", self.name())),
        };
        inst.serve(spec, self.name(), &CALLS, Box::new(Opened { held, dir, open: HashMap::new() }))
    }
}

/* the calls of one package and the files it opened */
struct Opened {
    held: Held,
    dir: String,
    open: HashMap<i64, Reader>,
}

enum Reader {
    Plain(File),
    Gzip(Box<GzDecoder<BufReader<File>>>),
}

impl Calls for Opened {
    fn call(&mut self, index: usize, args: Vec<WireValue>, reply: Reply) {
        let arg = |at: usize| args.get(at).unwrap_or(&WireValue::None);
        reply.answer(match CALLS[index] {
            "open" => self.open(arg(0), arg(1)),
            "read" => self.read(arg(0)),
            _ => self.close_one(arg(0)),
        });
    }

    fn close(&mut self) {
        self.open.clear();
    }
}

impl Opened {
    /* the id of the file at `path` under the package once its grant reaches it */
    fn open(&mut self, path: &WireValue, inflate: &WireValue) -> Result<WireValue, Fault> {
        let path = path_of(path)?;
        let inflate = match inflate {
            WireValue::None => false,
            WireValue::Bytes(b) if b == b"gzip" => true,
            _ => return Err(Fault::invalid("inflate must be None or 'gzip'")),
        };
        let (real, resolved) = self.resolve(path)?;
        // a link the name runs through must land where the grant reaches too
        if resolved != path {
            self.held.reach(&Files, &resolved)?;
        }
        let file = File::open(&real).map_err(|e| Fault::failed(format!("cannot open {path}, {}", why(&e))))?;
        let reader = match inflate {
            true => Reader::Gzip(Box::new(GzDecoder::new(BufReader::new(file)))),
            false => Reader::Plain(file),
        };
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        self.open.insert(id, reader);
        Ok(WireValue::Int(id.into()))
    }

    /* the real file `path` names with every link resolved and its path below the package */
    fn resolve(&self, path: &str) -> Result<(PathBuf, String), Fault> {
        let base = Path::new(if self.dir.is_empty() { "." } else { &self.dir });
        let opened = |e: std::io::Error| Fault::failed(format!("cannot open {path}, {}", why(&e)));
        let (root, real) = (base.canonicalize().map_err(opened)?, base.join(path).canonicalize().map_err(opened)?);
        let parts = real.strip_prefix(&root).ok().and_then(|rest| rest.iter().map(|part| part.to_str()).collect::<Option<Vec<_>>>());
        match parts.filter(|parts| !parts.is_empty()).map(|parts| parts.join("/")) {
            Some(below) => Ok((real, below)),
            None => Err(Fault::denied(format!("'{path}' is a link out of the package, files reads only below it"))),
        }
    }

    /* the next chunk of a file inflated when asked or None at its end */
    fn read(&mut self, id: &WireValue) -> Result<WireValue, Fault> {
        let reader = self.open.get_mut(&id_of(id)?).ok_or_else(|| Fault::invalid(format!("no open file {}", shown(id))))?;
        let mut buf = vec![0u8; CHUNK];
        let read = match reader {
            Reader::Plain(file) => file.read(&mut buf),
            Reader::Gzip(file) => file.read(&mut buf),
        };
        let n = read.map_err(|e| Fault::failed(format!("reading the file failed, {}", why(&e))))?;
        if n > 0 {
            buf.truncate(n);
            return Ok(WireValue::Raw(buf));
        }
        // bytes after the gzip stream fail the file as net fails a body
        if let Reader::Gzip(file) = reader
            && !file.get_mut().fill_buf().map_err(|e| Fault::failed(format!("reading the file failed, {}", why(&e))))?.is_empty()
        {
            return Err(Fault::failed("reading the file failed, junk found after the end of the gzip stream"));
        }
        Ok(WireValue::None)
    }

    fn close_one(&mut self, id: &WireValue) -> Result<WireValue, Fault> {
        self.open.remove(&id_of(id)?).ok_or_else(|| Fault::invalid(format!("no open file {}", shown(id))))?;
        Ok(WireValue::None)
    }
}

/* a path a call names checked to stay below the package */
fn path_of(path: &WireValue) -> Result<&str, Fault> {
    let WireValue::Bytes(raw) = path else { return Err(Fault::invalid("a path must be a str")) };
    let path = std::str::from_utf8(raw).map_err(|_| Fault::invalid("a path must be a str"))?;
    if !plain(path) {
        return Err(Fault::invalid(format!("'{path}' is not a plain path, files takes names joined by slashes below the package")));
    }
    Ok(path)
}

/* an id as the int `open` gave */
fn id_of(id: &WireValue) -> Result<i64, Fault> {
    match id {
        WireValue::Int(n) => i64::try_from(*n).map_err(|_| Fault::invalid("an id is an int from files.open")),
        _ => Err(Fault::invalid("an id is an int from files.open")),
    }
}

fn shown(id: &WireValue) -> String {
    match id {
        WireValue::Int(n) => n.to_string(),
        _ => String::from("?"),
    }
}

/* the words of an io failure without its os code */
fn why(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => String::from("no such file"),
        std::io::ErrorKind::PermissionDenied => String::from("the system refused it"),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Host, Project, Scopes, Status};
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_scope_is_a_plain_path_below_the_package() {
        for scope in ["events.jsonl", "data/", "data/2026/hour.json.gz", "a b.csv"] {
            assert!(Files.scope(scope), "{scope}");
        }
        for scope in ["", "/etc/passwd", "../up.csv", "data/../x", "./x", "data//x", "c:\\x", "data\\x", "//"] {
            assert!(!Files.scope(scope), "{scope}");
        }
    }

    #[test]
    fn a_grant_reaches_its_file_and_what_its_directory_holds() {
        let refusal = ("'main' has no files:".to_string(), ", edge.json grants it files:events.jsonl, files:logs/".to_string());
        let held = Held::Scopes(Scopes::new(vec!["events.jsonl".to_string(), "logs/".to_string()], refusal));
        assert!(held.reach(&Files, "events.jsonl").is_ok() && held.reach(&Files, "logs/2026/a.txt").is_ok());
        for path in ["other.jsonl", "events.jsonl.gz", "logsx/a.txt"] {
            let refused = held.reach(&Files, path).unwrap_err();
            assert_eq!(refused.text, format!("'main' has no files:{path}, edge.json grants it files:events.jsonl, files:logs/"));
        }
        assert!(Held::All.reach(&Files, "anything/at/all").is_ok());
    }

    #[test]
    fn a_gzip_file_reads_inflated_and_refuses_bytes_after_its_stream() {
        let dir = tempfile::tempdir().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, b"one\ntwo\n").unwrap();
        let bytes = gz.finish().unwrap();
        std::fs::write(dir.path().join("hour.txt.gz"), &bytes).unwrap();
        std::fs::write(dir.path().join("twice.txt.gz"), [bytes.as_slice(), bytes.as_slice()].concat()).unwrap();
        let mut opened = Opened { held: Held::All, dir: format!("{}/", dir.path().display()), open: HashMap::new() };
        let gzip = WireValue::Bytes(b"gzip".to_vec());
        let id = opened.open(&WireValue::Bytes(b"hour.txt.gz".to_vec()), &gzip).unwrap();
        assert_eq!(opened.read(&id).unwrap(), WireValue::Raw(b"one\ntwo\n".to_vec()));
        assert_eq!(opened.read(&id).unwrap(), WireValue::None);
        let id = opened.open(&WireValue::Bytes(b"twice.txt.gz".to_vec()), &gzip).unwrap();
        assert_eq!(opened.read(&id).unwrap(), WireValue::Raw(b"one\ntwo\n".to_vec()));
        assert_eq!(opened.read(&id).unwrap_err().text, "reading the file failed, junk found after the end of the gzip stream");
        opened.close_one(&id).unwrap();
        assert_eq!(opened.read(&id).unwrap_err().text, format!("no open file {}", shown(&id)));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_held_to_the_grant_where_it_lands() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("data")).unwrap();
        std::fs::write(dir.path().join("data/a.txt"), "granted\n").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "outside the grant\n").unwrap();
        for (link, to) in [("data/inner.txt", "a.txt"), ("data/leak.txt", "../secret.txt"), ("data/hosts", "/etc/hosts")] {
            std::os::unix::fs::symlink(to, dir.path().join(link)).unwrap();
        }
        let refusal = ("'main' has no files:".to_string(), ", edge.json grants it files:data/".to_string());
        let held = Held::Scopes(Scopes::new(vec!["data/".to_string()], refusal));
        let mut opened = Opened { held, dir: format!("{}/", dir.path().display()), open: HashMap::new() };
        let mut read = |path: &str| opened.open(&WireValue::Bytes(path.as_bytes().to_vec()), &WireValue::None).map(|_| ()).map_err(|fault| fault.text);
        assert_eq!(read("data/a.txt"), Ok(()));
        assert_eq!(read("data/inner.txt"), Ok(()));
        assert_eq!(read("data/leak.txt"), Err("'main' has no files:secret.txt, edge.json grants it files:data/".to_string()));
        assert_eq!(read("data/hosts"), Err("'data/hosts' is a link out of the package, files reads only below it".to_string()));
    }

    #[test]
    fn a_trusted_run_reads_below_its_entry_and_an_untrusted_one_reaches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/a.txt"), "inside\n").unwrap();
        // paths join the directory of the package's manifest
        std::fs::write(dir.path().join("edge.json"), "{}\n").unwrap();
        let entry = dir.path().join("main.mini").display().to_string();
        let kit = crate::kit::tests::bare().module(Arc::new(Files));
        let host = Host::new(kit.load().unwrap()).unwrap();
        let printed = Arc::new(Mutex::new(String::new()));
        let into = printed.clone();
        let mut vm = host.vm(Box::new(move |text: &str| into.lock().unwrap().push_str(text)), Project::disk(&entry, None), None, None).unwrap();
        let status = vm.start("import files\nprint(files.read(files.open('logs/a.txt')))\n", None).unwrap();
        assert!(matches!(status, Status::Done), "the trusted run did not finish");
        assert_eq!(printed.lock().unwrap().as_str(), "b'inside\\n'\n");
        let mut vm = host.vm(Box::new(|_: &str| {}), Project::bundle(std::collections::HashMap::new(), "", true), None, None).unwrap();
        let Status::Error(refused) = vm.start("import files\n", None).unwrap() else { panic!("an untrusted run imported files") };
        assert!(refused.contains("module 'files' is not available to untrusted eval runs"), "{refused}");
    }

    #[test]
    fn an_untrusted_run_reads_below_its_pool_and_never_where_the_process_started() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("logs")).unwrap();
        std::fs::write(dir.path().join("logs/a.txt"), "pooled\n").unwrap();
        let kit = crate::kit::tests::bare().module(Arc::new(Files));
        let host = Host::new(kit.load().unwrap()).unwrap();
        // an untrusted run that may open any file below `pool`
        let untrusted = |pool: Option<String>| {
            let mut project = Project::bundle(std::collections::HashMap::new(), "", true);
            project.access = Some(Arc::new(crate::host::Open));
            project.pool = pool;
            project
        };
        let printed = Arc::new(Mutex::new(String::new()));
        let into = printed.clone();
        let pool = format!("{}/", dir.path().display());
        let mut vm = host.vm(Box::new(move |text: &str| into.lock().unwrap().push_str(text)), untrusted(Some(pool)), None, None).unwrap();
        let status = vm.start("import files\nprint(files.read(files.open('logs/a.txt')))\n", None).unwrap();
        assert!(matches!(status, Status::Done), "the pooled run did not finish");
        assert_eq!(printed.lock().unwrap().as_str(), "b'pooled\\n'\n");
        let mut vm = host.vm(Box::new(|_: &str| {}), untrusted(None), None, None).unwrap();
        let Status::Error(refused) = vm.start("import files\n", None).unwrap() else { panic!("a run with no pool imported files") };
        assert!(refused.contains("module 'files' is not available to untrusted eval runs"), "{refused}");
    }
}
