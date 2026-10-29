use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::Digest;

// the hour of public GitHub events both jobs read and its file's hash
pub const HOUR: &str = "https://data.gharchive.org/2026-10-01-15.json.gz";
pub const HOUR_SHA256: &str = "2a9282966415bf7208e2e9bea5dc59239e60dd2d2b572523033d590d989207ed";

// every step-th event of each type in the hour's order as tiny's cases cut it
const STEPS: [(&str, usize); 7] = [("WatchEvent", 3), ("IssuesEvent", 12), ("PushEvent", 500), ("CreateEvent", 250), ("PullRequestEvent", 160), ("IssueCommentEvent", 70), ("DeleteEvent", 300)];
pub const SLICE_SHA256: &str = "e3c1dfebae5772e1523b88b639b882446b20f694f58dc17de7e39b3438e85366";
pub const SLICE_RECORDS: usize = 431;

pub fn sha(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/* the directory tiny's cases keep the hour in under a target directory */
pub fn cache(target_tmp: &Path) -> PathBuf {
    let dir = target_tmp.join("tiny");
    std::fs::create_dir_all(&dir).expect("the target directory takes the hour");
    dir
}

/* the hour's gzip file checked against its pinned hash and fetched once into `dir` */
pub fn hour(dir: &Path) -> Vec<u8> {
    let file = dir.join("2026-10-01-15.json.gz");
    if let Ok(b) = std::fs::read(&file)
        && sha(&b) == HOUR_SHA256
    {
        return b;
    }
    let status = Command::new("curl").args(["-fsSL", "--retry", "3", "-o"]).arg(&file).arg(HOUR).status();
    assert!(status.is_ok_and(|s| s.success()), "the job fetches the hour {HOUR} over the network and the fetch failed, so fetch it into {} by hand when offline", file.display());
    let b = std::fs::read(&file).expect("reading the hour");
    assert_eq!(sha(&b), HOUR_SHA256, "the hour fetched over the network from {HOUR} is not the file the job pins, and a cut fetch fails here until a rerun fetches it again");
    b
}

/* the lines a gzip stream holds as gzip writes them out */
pub fn inflate(gz: &[u8]) -> Vec<u8> {
    let mut child = Command::new("gzip").arg("-dc").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("gzip inflates the hour");
    let mut stdin = child.stdin.take().expect("gzip reads its input");
    let gz = gz.to_vec();
    let feeder = std::thread::spawn(move || stdin.write_all(&gz));
    let out = child.wait_with_output().expect("gzip inflates the hour");
    feeder.join().expect("feeding gzip").expect("feeding gzip");
    assert!(out.status.success(), "gzip could not inflate the hour");
    out.stdout
}

/* the slice tiny's cases read cut from the hour once and kept beside it */
pub fn slice(dir: &Path) -> Vec<u8> {
    let file = dir.join("slice.jsonl");
    if let Ok(b) = std::fs::read(&file)
        && sha(&b) == SLICE_SHA256
    {
        return b;
    }
    let body = cut(&inflate(&hour(dir)));
    assert_eq!(sha(&body), SLICE_SHA256, "the slice cut from {HOUR} differs from the pinned one");
    std::fs::write(&file, &body).expect("writing the slice");
    body
}

/* every step-th event of each type of the hour as its lines are */
fn cut(events: &[u8]) -> Vec<u8> {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    let mut body = Vec::new();
    for line in events.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        let text = std::str::from_utf8(line).expect("the hour is utf-8");
        // the type is the second field of every event GH Archive writes
        let kind = text.split("\"type\":\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("");
        let Some(&(name, step)) = STEPS.iter().find(|(t, _)| *t == kind) else { continue };
        let n = seen.entry(name).or_insert(0);
        if n.is_multiple_of(step) {
            body.extend_from_slice(line);
            body.push(b'\n');
        }
        *n += 1;
    }
    body
}
