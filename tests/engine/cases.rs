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

/* the vm cases a corpus test walks */
pub fn corpus() -> Vec<serde_json::Value> { vm_cases(Path::new(env!("CARGO_MANIFEST_DIR"))) }

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
