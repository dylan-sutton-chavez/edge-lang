use std::path::Path;

use spell::split::{classify_tree, moved, Fate, Kind};
use spell::{yml, Spell};

// the corpus holds no case left to move and its files still read back
#[test]
fn the_corpus_splits_into_files_that_read_back() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let rows = classify_tree(&root, Spell::Mini).expect("every vm case classifies");
    // a native or syntax case added to the vm files fails here with its place
    let strays: Vec<String> = rows.iter().filter(|r| r.fate.kind() != Kind::Engine).map(|r| format!("{}:{}", r.file, r.line)).collect();
    assert!(strays.is_empty(), "these cases belong in lib/tests/cases or tests/cases/syntax, {strays:?}");
    assert!(rows.iter().any(|r| matches!(r.fate, Fate::Kept(..))), "the cases the split keeps still classify");
    let moved = moved(&root, &rows).expect("the split writes its files");
    for (rel, text) in &moved.files {
        let read: Result<serde_yaml_ng::Value, _> = serde_yaml_ng::from_str(text);
        assert!(rel.ends_with(".tsv") || rel.ends_with(".snapshot") || read.is_ok(), "{rel} does not parse");
    }
}

// a text comes back from its quoted and its block form byte for byte
#[test]
fn texts_read_back_as_they_were_written() {
    let back = |yml: &str| serde_yaml_ng::from_str::<std::collections::BTreeMap<String, String>>(&format!("k: {yml}\n")).expect(yml).remove("k");
    let chars = (0..0x3000).chain([0xfeff, 0xfffd, 0xfffe, 0xffff, 0x10000, 0x1f600, 0x10ffff]).filter_map(char::from_u32);
    for c in chars {
        let s = format!("a{c}b");
        assert_eq!(back(&yml::quoted(&s)).as_deref(), Some(s.as_str()), "{:x}", c as u32);
    }
    for s in ["x = 1", "x = 1\n", "if x:\n    y\n\n\tz = 2", "a\n\n\nb\n", "  lead", "\nfirst", "end\n\n", "tab\there\n", "", "\u{85}x", "a\r\nb"] {
        assert_eq!(back(&yml::text(s, 4)).as_deref(), Some(s), "{s:?}");
    }
}
