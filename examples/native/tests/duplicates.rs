use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

// the lockfiles of this repository the job reads as its real data
const LOCKS: [&str; 2] = ["Cargo.lock", "cli/Cargo.lock"];

fn here(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

/* what the binary gave running `program` with `input` on its stdin */
fn native(program: &Path, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_native"))
        .arg(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    let mut stdin = child.stdin.take().expect("the binary reads stdin");
    let input = input.to_vec();
    // a program that stops reading closes the pipe so a short write is no failure
    let feeder = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().expect("the binary ends");
    let _ = feeder.join();
    out
}

/* `src` written as a mini file of its own under the target directory */
fn script(name: &str, src: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, src).expect("the target directory takes the program");
    path
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/* each crate held at more than one version with the packages asking for each */
fn duplicates(lock: &str) -> String {
    let mut versions: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut askers: BTreeMap<(&str, &str), Vec<String>> = BTreeMap::new();
    for block in lock.split("[[package]]\n").skip(1) {
        let field = |key: &str| block.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(" = \"")?.strip_suffix('"')).expect("each package names itself");
        let (name, version) = (field("name"), field("version"));
        versions.entry(name).or_default().push(version);
        let listed = block.split_once("dependencies = [\n").map_or("", |(_, rest)| rest.split_once("\n]").map_or(rest, |(list, _)| list));
        for entry in listed.lines() {
            let mut parts = entry.trim().trim_matches(['"', ',']).split(' ');
            if let (Some(asked), Some(at)) = (parts.next(), parts.next()) {
                askers.entry((asked, at)).or_default().push(format!("{name} {version}"));
            }
        }
    }
    let mut out = String::new();
    let twice: Vec<(&str, &Vec<&str>)> = versions.iter().filter(|(_, held)| held.len() > 1).map(|(name, held)| (*name, held)).collect();
    for &(name, held) in &twice {
        for &version in held {
            let mut who = askers.get(&(name, version)).cloned().unwrap_or_default();
            who.sort();
            match who.is_empty() {
                true => out.push_str(&format!("{name} {version}\n")),
                false => out.push_str(&format!("{name} {version} for {}\n", who.join(", "))),
            }
        }
    }
    out.push_str(&format!("{} of {} crates are held at more than one version\n", twice.len(), versions.len()));
    out
}

// the job read off stdin reports each lockfile as the rule does
#[test]
fn the_job_reports_every_crate_a_lockfile_holds_twice() {
    let mut reported = 0;
    for lock in LOCKS {
        let input = std::fs::read_to_string(here("../..").join(lock)).expect("the repository holds its lockfiles");
        let out = native(&here("duplicates.mini"), input.as_bytes());
        assert_eq!(text(&out.stderr), "", "{lock}");
        assert!(out.status.success(), "{lock} ended with {}", out.status);
        let expected = duplicates(&input);
        assert_eq!(text(&out.stdout), expected, "{lock}");
        reported += expected.lines().count() - 1;
    }
    assert!(reported > 0, "no lockfile holds a crate twice so the job checked only its count");
}

// an uncaught error reports on stderr in mini's words and fails the process
#[test]
fn an_uncaught_error_reports_in_the_runtime_words() {
    let program = script("missing.mini", "versions = {'syn': ['1.0.109']}\nprint(versions['hashbrown'])\n");
    let out = native(&program, b"");
    assert_eq!(text(&out.stdout), "");
    assert_eq!(text(&out.stderr).lines().next(), Some("error: KeyError: 'hashbrown'"));
    assert_eq!(out.status.code(), Some(1));
}

// an exit ends the process with its status after what the program printed
#[test]
fn an_exit_ends_with_its_status() {
    let program = script("exit.mini", "print('2 crates held twice')\nexit(3)\n");
    let out = native(&program, b"");
    assert_eq!(text(&out.stdout), "2 crates held twice\n");
    assert_eq!(text(&out.stderr), "");
    assert_eq!(out.status.code(), Some(3));
}

// a program that does not compile reports where and never runs
#[test]
fn a_program_that_does_not_compile_never_runs() {
    let program = script("broken.mini", "print('read')\nheld = (1,\n");
    let out = native(&program, b"");
    assert_eq!(text(&out.stdout), "");
    assert!(text(&out.stderr).starts_with("error: "), "{}", text(&out.stderr));
    assert_eq!(out.status.code(), Some(1));
}

// a path that is no UTF-8 reads lossily in the report and never panics
#[cfg(unix)]
#[test]
fn a_path_that_is_no_utf8_fails_cleanly() {
    use std::os::unix::ffi::OsStrExt;
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(std::ffi::OsStr::from_bytes(b"\xff\xfe.mini"));
    let _ = std::fs::remove_file(&path);
    let missing = native(&path, b"");
    assert_eq!(missing.status.code(), Some(1), "{}", text(&missing.stderr));
    assert!(text(&missing.stderr).starts_with("error: cannot read '"), "{}", text(&missing.stderr));
    // a file system that names files in bytes holds the file so it runs
    if std::fs::write(&path, "print('read')\n").is_ok() {
        let read = native(&path, b"");
        assert_eq!((read.status.code(), text(&read.stdout)), (Some(0), "read\n".to_string()), "{}", text(&read.stderr));
    }
}

// a script imports a module from its own directory as the walk resolves it
#[test]
fn a_script_imports_a_module_beside_it() {
    script("lockfile.mini", "// the value of key a lockfile line holds or nil for another line\nfn value(line, key):\n    head = key + ' = \"'\n    if line.startswith(head) && line.endswith('\"'):\n        return line[len(head):-1]\n    return nil\n");
    let program = script("packages.mini", "from .lockfile import value\nheld = 0\nwhile true:\n    try:\n        line = input()\n    except EOFError:\n        break\n    if value(line, 'name') != nil:\n        held += 1\nprint(f'{held} packages')\n");
    let input = std::fs::read_to_string(here("../..").join("Cargo.lock")).expect("the repository holds its lockfile");
    let out = native(&program, input.as_bytes());
    assert_eq!(text(&out.stderr), "");
    let expected = input.split("[[package]]\n").count() - 1;
    assert_eq!(text(&out.stdout), format!("{expected} packages\n"));
}

// a module its directory lacks stops the script before any line runs
#[test]
fn a_missing_module_stops_the_script_before_it_runs() {
    let program = script("absent.mini", "print('ran')\nfrom .nowhere import value\n");
    let out = native(&program, b"");
    assert_eq!(text(&out.stdout), "");
    assert!(text(&out.stderr).starts_with("error: ") && text(&out.stderr).contains("nowhere.mini"), "{}", text(&out.stderr));
    assert_eq!(out.status.code(), Some(1));
}
