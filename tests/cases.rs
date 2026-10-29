#![allow(dead_code)]

use std::path::Path;

#[derive(serde::Deserialize)]
struct Case { case: String, expected: Option<String>, #[serde(flatten)] rest: serde_json::Map<String, serde_json::Value> }

/* a topic file case as the json object the suites run and the bench keys */
fn object(c: Case) -> serde_json::Value {
    let mut v = c.rest;
    v.insert("src".into(), c.case.into());
    v.insert("output".into(), c.expected.map_or(Vec::new(), |e| e.split('\n').map(String::from).collect()).into());
    serde_json::Value::Object(v)
}

// the cases of tests/cases/vm as the json objects the suites run and the bench keys
pub fn vm_cases(root: &Path) -> Vec<serde_json::Value> {
    vm_files(root).into_iter().flat_map(|(_, cases)| cases).collect()
}

/* each topic file of tests/cases/vm by name with its cases in the loader's order */
pub fn vm_files(root: &Path) -> Vec<(String, Vec<serde_json::Value>)> {
    let mut files: Vec<_> = std::fs::read_dir(root.join("tests/cases/vm")).expect("tests/cases/vm").map(|e| e.expect("a case file").path()).collect();
    files.sort();
    files.iter().map(|f| {
        let text = std::fs::read_to_string(f).expect("a readable case file");
        let cases = serde_yaml_ng::from_str::<Vec<Case>>(&text).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        (f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), cases.into_iter().map(object).collect())
    }).collect()
}

/* hashes every field with FNV-1a so two cases sharing a source stay apart */
pub fn key(case: &serde_json::Value) -> String {
    let hash = case.to_string().bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{hash:016x}")
}

/* the cases at `indexes` of a topic file read without parsing the rest */
pub fn vm_cases_at(root: &Path, file: &str, indexes: &[usize]) -> Vec<serde_json::Value> {
    let path = root.join("tests/cases/vm").join(file);
    let text = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let (s, mut left): (&[u8], usize) = (&text, indexes.iter().max().map_or(0, |&i| i + 1));
    let (n, mut i, mut starts) = (s.len(), 1, vec![0]);
    // a case starts at a line opening with a dash as its fields sit indented
    while left > 0 && i < n {
        if s[i] == b'-' && s[i - 1] == b'\n' { starts.push(i); left -= 1; }
        i += 1;
    }
    starts.push(n);
    indexes.iter().map(|&i| {
        let (a, b) = (starts.get(i).copied(), starts.get(i + 1).copied());
        let (Some(a), Some(b)) = (a, b) else { panic!("{file} holds no case {i}") };
        let entry = std::str::from_utf8(&s[a..b]).unwrap_or_else(|e| panic!("{file} case {i} {e}"));
        // a plain map skips the buffering a flattened struct costs under Miri
        let mut one = serde_yaml_ng::from_str::<Vec<serde_json::Map<String, serde_json::Value>>>(entry).unwrap_or_else(|e| panic!("{file} case {i} {e}"));
        assert_eq!(one.len(), 1, "{file} case {i} parsed as {} cases", one.len());
        let mut rest = one.remove(0);
        let mut text = |f: &str| rest.remove(f).and_then(|v| v.as_str().map(String::from));
        let (case, expected) = (text("case").unwrap_or_else(|| panic!("{file} case {i} has no source")), text("expected"));
        object(Case { case, expected, rest })
    }).collect()
}

/* the lines of tests/cases/miri.tsv as each case's key and its topic file and index there */
pub fn miri_lines(root: &Path) -> Vec<(String, String, usize)> {
    let path = root.join("tests/cases/miri.tsv");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines().filter(|l| !l.is_empty() && !l.starts_with("key\t")).map(|l| {
        let mut f = l.split('\t');
        let (Some(k), Some(file), Some(i), None) = (f.next(), f.next(), f.next(), f.next()) else { panic!("miri.tsv line {l:?} is not a key a file and an index") };
        (k.to_string(), file.to_string(), i.parse().unwrap_or_else(|_| panic!("miri.tsv line {l:?} has no index")))
    }).collect()
}

/* the sample Miri runs since a whole topic file takes it hours to parse */
pub fn miri_sample(root: &Path) -> Vec<serde_json::Value> { miri_cases(root, &miri_lines(root)) }

/* the cases some lines of tests/cases/miri.tsv name each held to its key */
pub fn miri_cases(root: &Path, lines: &[(String, String, usize)]) -> Vec<serde_json::Value> {
    let mut files: Vec<&str> = Vec::new();
    for (_, file, _) in lines { if !files.contains(&file.as_str()) { files.push(file); } }
    let mut found = std::collections::HashMap::new();
    for file in files {
        let indexes: Vec<usize> = lines.iter().filter(|l| l.1 == file).map(|l| l.2).collect();
        found.extend(indexes.iter().map(|&i| (file, i)).zip(vm_cases_at(root, file, &indexes)));
    }
    lines.iter().map(|(k, file, i)| {
        let case = found.remove(&(file.as_str(), *i)).unwrap_or_else(|| panic!("miri.tsv names {file} case {i} twice"));
        assert_eq!(&key(&case), k, "miri.tsv names {k} as {file} case {i} which holds another case");
        case
    }).collect()
}
