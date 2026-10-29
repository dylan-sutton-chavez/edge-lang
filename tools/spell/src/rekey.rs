use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde_json::Value;

use crate::cases::{key, vm_cases};

// one case of the vm corpus with its bench key before and after the rewrite
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    pub old: String,
    pub new: String,
    pub file: String,
    pub index: usize,
}

// the case files of a tree in the loader's order with the cases each holds
pub fn files(root: &Path) -> Result<Vec<(String, usize)>, String> {
    let dir = root.join("tests/cases/vm");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).map_err(|e| format!("{} {e}", dir.display()))?.flatten().map(|e| e.path()).collect();
    paths.sort();
    paths.iter().map(|p| {
        let text = std::fs::read_to_string(p).map_err(|e| format!("{} {e}", p.display()))?;
        let cases: Vec<serde_yaml_ng::Value> = serde_yaml_ng::from_str(&text).map_err(|e| format!("{} {e}", p.display()))?;
        Ok((p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), cases.len()))
    }).collect()
}

// the file and index of each case a tsv of `file` and `index` names
pub fn moved(tsv: &str) -> Result<Vec<(String, usize)>, String> {
    tsv.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("file\t")).map(|l| {
        let (file, index) = l.split_once('\t').ok_or_else(|| format!("a moved line without a tab {l:?}"))?;
        Ok((file.to_string(), index.trim().parse().map_err(|_| format!("a moved line without an index {l:?}"))?))
    }).collect()
}

// pairs kept `old` cases with `new` ones by file and index skipping those `back` names
pub fn pairs(old: &Path, new: &Path, moved: &[(String, usize)], back: &[(String, usize)]) -> Result<(Vec<Pair>, Vec<String>), String> {
    let (layout, other) = (files(old)?, files(new)?);
    let kept: Vec<(String, usize)> = layout.iter().map(|(f, n)| (f.clone(), n - moved.iter().filter(|m| &m.0 == f).count())).collect();
    let came: Vec<(String, usize)> = other.iter().map(|(f, n)| (f.clone(), n - back.iter().filter(|m| &m.0 == f).count())).collect();
    if kept != came { return Err("the two trees hold other case files or other counts once the moved cases leave and the returned ones stand apart".into()); }
    let (a, b) = (vm_cases(old), vm_cases(new));
    let total: usize = layout.iter().map(|f| f.1).sum();
    if a.len() != total || b.len() != other.iter().map(|f| f.1).sum::<usize>() { return Err(format!("the loader reads {} and {} cases where the files hold {total}", a.len(), b.len())); }
    let mut out = Vec::with_capacity(total);
    let mut gone = Vec::new();
    let (mut at, mut to) = (0, 0);
    for ((file, n), (_, m)) in layout.iter().zip(&other) {
        let mut j = 0;
        for i in 0..*n {
            if moved.iter().any(|x| &x.0 == file && x.1 == i) {
                gone.push(key(&a[at]));
                at += 1;
                continue;
            }
            // a case that came back has no partner in the old tree
            while back.iter().any(|x| &x.0 == file && x.1 == j) { (to, j) = (to + 1, j + 1); }
            out.push(Pair { old: key(&a[at]), new: key(&b[to]), file: file.clone(), index: j });
            (to, j, at) = (to + 1, j + 1, at + 1);
        }
        while j < *m { (to, j) = (to + 1, j + 1); }
    }
    // two cases under one key would share a single bench entry
    let keys: [fn(&Pair) -> String; 2] = [|p| p.old.clone(), |p| p.new.clone()];
    for keyed in keys {
        let mut seen: HashMap<String, &Pair> = HashMap::new();
        for p in &out {
            if let Some(first) = seen.insert(keyed(p), p) {
                return Err(format!("{} case {} and {} case {} share the key {}", first.file, first.index, p.file, p.index, keyed(p)));
            }
        }
    }
    Ok((out, gone))
}

// a snapshot with every entry under its new key and what found no partner
pub struct Applied {
    pub text: String,
    pub mapped: usize,
    // entries of cases that left the bench
    pub dropped: usize,
    // entries whose key no case of the old tree has
    pub stale: Vec<String>,
    // cases of the old tree the snapshot has no entry for
    pub missing: Vec<Pair>,
}

// rewrites the keys of a snapshot the bench wrote one entry per line
pub fn apply(text: &str, pairs: &[Pair], gone: &[String]) -> Result<Applied, String> {
    let map: HashMap<&str, &str> = pairs.iter().map(|p| (p.old.as_str(), p.new.as_str())).collect();
    let rows: Vec<&str> = text.split('\n').collect();
    let open = rows.iter().position(|l| l.trim() == "\"cases\": {").ok_or("no cases object")?;
    let close = (open + 1..rows.len()).find(|&i| rows[i].trim() == "}").ok_or("the cases object never closes")?;
    let mut entries = BTreeMap::new();
    let (mut mapped, mut stale, mut dropped) = (0, Vec::new(), 0);
    for row in &rows[open + 1..close] {
        let entry = row.trim().trim_end_matches(',');
        let (k, value) = entry.strip_prefix('"').and_then(|e| e.split_once("\": ")).ok_or_else(|| format!("an entry the bench did not write {row:?}"))?;
        if gone.iter().any(|g| g == k) { dropped += 1; continue; }
        let new = match map.get(k) {
            Some(n) => { mapped += 1; *n }
            None => { stale.push(k.to_string()); k }
        };
        if entries.insert(new.to_string(), value.to_string()).is_some() { return Err(format!("two entries land on the key {new}")); }
    }
    let body: Vec<String> = entries.iter().map(|(k, v)| format!("    \"{k}\": {v}")).collect();
    let mut out: Vec<String> = rows[..=open].iter().map(|r| r.to_string()).collect();
    out.push(body.join(",\n"));
    out.extend(rows[close..].iter().map(|r| r.to_string()));
    let out = out.join("\n");
    let present: std::collections::HashSet<&str> = rows[open + 1..close].iter().filter_map(|r| r.trim().strip_prefix('"').and_then(|e| e.split_once('"')).map(|e| e.0)).collect();
    let missing = pairs.iter().filter(|p| !present.contains(p.old.as_str())).cloned().collect();
    // the rewritten text parses to the same numbers under the new keys
    let (before, after): (Value, Value) = (serde_json::from_str(text).map_err(|e| e.to_string())?, serde_json::from_str(&out).map_err(|e| e.to_string())?);
    let renamed: serde_json::Map<String, Value> = before["cases"].as_object().ok_or("cases is no object")?.iter()
        .filter(|(k, _)| !gone.contains(k))
        .map(|(k, v)| (map.get(k.as_str()).map_or(k.clone(), |n| n.to_string()), v.clone())).collect();
    let fields = |v: &Value| v.as_object().map(|m| m.iter().filter(|(k, _)| *k != "cases").map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>());
    if after["cases"].as_object() != Some(&renamed) || fields(&before) != fields(&after) { return Err("the rewritten snapshot reads other numbers".into()); }
    Ok(Applied { text: out, mapped, dropped, stale, missing })
}
