use std::path::Path;

use edge::lexer::Spelling;
use spell::check::{compiled, same_program};
use spell::{translate, Braces, Indent, Mini};

enum Outcome {
    Clean { exact: bool },
    Fails(String),
}

// a program of spelling `A` written in `B` and back compiles alike each way
fn round_trip<A: Spelling, B: Spelling>(src: &str) -> Outcome {
    let line = |at: usize, s: &str| spell::line_of(s, at);
    let other = match translate::<A, B>(src) {
        Ok(m) => m,
        Err(e) => return Outcome::Fails(format!("to {}, line {}, {}", B::SYNTAX.name, line(e.at, src), e.why)),
    };
    let first = compiled::<A>(src);
    if let Err(why) = same_program(&first, &compiled::<B>(&other)) { return Outcome::Fails(format!("{} compiles differently, {why}\n{other}", B::SYNTAX.name)); }
    let back = match translate::<B, A>(&other) {
        Ok(b) => b,
        Err(e) => return Outcome::Fails(format!("back from {}, line {}, {}", B::SYNTAX.name, line(e.at, &other), e.why)),
    };
    if let Err(why) = same_program(&first, &compiled::<A>(&back)) { return Outcome::Fails(format!("the way back compiles differently, {why}")); }
    Outcome::Clean { exact: back == src }
}

// why a valid program has no spelling in the other table any layout fixes
const ACCEPTED: &[&str] = &["is a reserved word of", "the text of an `=` field spells differently"];

// vm cases come back through braces and syntax cases through mini or indent
#[test]
fn corpus_round_trips_through_the_other_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let vm: Vec<String> = spell::cases::vm_cases(&root).iter().map(|c| c["src"].as_str().unwrap_or("").to_string()).collect();
    let mut syntax: Vec<(bool, String)> = Vec::new();
    for file in ["bytecode.yml", "diagnostics.yml"] {
        let text = std::fs::read_to_string(root.join("tests/cases/syntax").join(file)).expect("a syntax case file");
        let cases: Vec<serde_json::Value> = serde_yaml_ng::from_str(&text).expect(file);
        // a case in mini leaves indent to spell so the mini text goes across
        syntax.extend(cases.iter().filter_map(|c| c["mini"].as_str().map(|m| (true, m.to_string())).or_else(|| c["indent"].as_str().map(|i| (false, i.to_string())))));
    }
    let (mut clean, mut exact, mut accepted, mut broken) = (0, 0, Vec::new(), Vec::new());
    let mut tally = |from: &str, src: &str, invalid: bool, outcome: Outcome| match outcome {
        Outcome::Clean { exact: e } => { clean += 1; exact += e as usize; }
        // a program its table rejects keeps its error or needs no spelling there
        Outcome::Fails(why) if invalid || ACCEPTED.iter().any(|a| why.contains(a)) => accepted.push(format!("{from} {src:?}\n   {why}")),
        Outcome::Fails(why) => broken.push(format!("{from} {src:?}\n   {why}")),
    };
    for src in &vm { tally("vm", src, !compiled::<Mini>(src).1.is_empty(), round_trip::<Mini, Braces>(src)); }
    for (mini, src) in &syntax {
        match mini {
            true => tally("syntax", src, !compiled::<Mini>(src).1.is_empty(), round_trip::<Mini, Indent>(src)),
            false => tally("syntax", src, !compiled::<Indent>(src).1.is_empty(), round_trip::<Indent, Mini>(src)),
        }
    }
    println!("{clean} cases come back through the other table, {exact} of them to the same text, {} have no spelling there", accepted.len());
    for a in &accepted { println!("no spelling in the other table: {a}"); }
    assert!(broken.is_empty(), "{} cases break in the other table:\n{}", broken.len(), broken.join("\n"));
}
