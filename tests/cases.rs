use std::path::Path;

// the vm cases of tests/cases/vm read as the json objects the suites run and the bench keys
pub fn vm_cases(root: &Path) -> Vec<serde_json::Value> {
    #[derive(serde::Deserialize)]
    struct Case { case: String, expected: Option<String>, #[serde(flatten)] rest: serde_json::Map<String, serde_json::Value> }
    let mut files: Vec<_> = std::fs::read_dir(root.join("tests/cases/vm")).expect("tests/cases/vm").map(|e| e.expect("a case file").path()).collect();
    files.sort();
    files.iter().flat_map(|f| {
        let text = std::fs::read_to_string(f).expect("a readable case file");
        serde_yaml_ng::from_str::<Vec<Case>>(&text).unwrap_or_else(|e| panic!("{}: {e}", f.display()))
    }).map(|c| {
        let mut v = c.rest;
        v.insert("src".into(), c.case.into());
        v.insert("output".into(), c.expected.map_or(Vec::new(), |e| e.split('\n').map(String::from).collect()).into());
        serde_json::Value::Object(v)
    }).collect()
}
