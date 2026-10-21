/* Names in the VM's bitcode the handlers that may collect, `!edge.safepoints`, so the glue stores its live values
   before calling one and reads them back after. Read off the module's own call graph: a function may collect when it
   calls `edge_collect`, calls through a pointer, or calls one that may. Run on the `.ll` text before `llvm-as`. */

use std::collections::{HashMap, HashSet};

fn main() {
    let path = std::env::args().nth(1).expect("the .ll file of the VM's bitcode");
    let text = std::fs::read_to_string(&path).expect("a readable .ll file");
    // Each defined function and what it calls, None standing for a call through a pointer.
    let mut calls: HashMap<&str, Vec<Option<&str>>> = HashMap::new();
    let mut current: Option<&str> = None;
    let mut top = 0u64;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("define ") {
            current = rest.find('@').map(|at| name(&rest[at + 1..]));
            calls.entry(current.unwrap_or_default()).or_default();
        } else if line == "}" {
            current = None;
        } else if let Some(f) = current && let Some(at) = line.find("call ").or_else(|| line.find("invoke ")) {
            let callee = line[at..].split_whitespace().find(|w| (w.starts_with('@') || w.starts_with('%')) && w.contains('('));
            match callee {
                Some(c) if c.starts_with('@') => { if !c.starts_with("@llvm.") { calls.entry(f).or_default().push(Some(name(&c[1..]))); } }
                Some(_) => calls.entry(f).or_default().push(None),
                None => {}
            }
        }
        // Metadata the module numbers already, which the new nodes come after.
        if let Some(rest) = line.strip_prefix('!') && let Some(n) = rest.split(|c: char| !c.is_ascii_digit()).next().and_then(|n| n.parse::<u64>().ok()) { top = top.max(n + 1); }
    }
    // A collection inlined into its callers would leave no call to find, so none at all is an error, never an empty set.
    assert!(calls.values().flatten().any(|c| *c == Some("edge_collect")), "no function of {path} calls edge_collect, keep VM::collect out of line");
    let mut may: HashSet<&str> = HashSet::from(["edge_collect"]);
    loop {
        let more: Vec<&str> = calls.iter().filter(|(f, cs)| !may.contains(*f) && cs.iter().any(|c| c.is_none_or(|c| may.contains(c)))).map(|(f, _)| *f).collect();
        if more.is_empty() { break; }
        may.extend(more);
    }
    let mut handlers: Vec<&str> = may.into_iter().filter(|f| f.starts_with("op_")).collect();
    handlers.sort_unstable();
    let mut out = format!("{text}\n!edge.safepoints = !{{{}}}\n", (0..handlers.len()).map(|k| format!("!{}", top + k as u64)).collect::<Vec<_>>().join(", "));
    for (k, h) in handlers.iter().enumerate() { out += &format!("!{} = !{{!\"{h}\"}}\n", top + k as u64); }
    std::fs::write(&path, out).expect("the .ll file written back");
    eprintln!("{} of the handlers may collect", handlers.len());
}

/* A symbol from the text after its `@`, quoted or plain, up to its argument list. */
fn name(s: &str) -> &str {
    match s.strip_prefix('"') { Some(q) => &q[..q.find('"').unwrap_or(q.len())], None => &s[..s.find(['(', ' ', ',']).unwrap_or(s.len())] }
}
