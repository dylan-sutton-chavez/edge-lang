/* The two steps of `make bitcode` that read the VM's module as text, after rustc wrote it (`vm.ll`) and compiled it to
   `vm.o`, the same module:
   - `bitcode safepoints vm.ll` names in it the handlers that may collect, `!edge.safepoints`, so the glue stores its
     live values before calling one and reads them back after. Read off the module's own call graph, as bench/llvm reads
     the IR: a function may collect when it calls `edge_collect`, calls through a pointer, or calls one that may.
   - `bitcode inline vm.ll inline.ll` makes the handlers and tag tests llvm-extract took out of it `available_externally`,
     bodies the glue inlines whose symbols vm.o defines, and fails on any function they call that vm.o does not export. */

#[path = "../bench/src/ir.rs"]
#[allow(dead_code)]
mod ir;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{p}: {e}"));
    match args.get(1).map(String::as_str) {
        Some("safepoints") => safepoints(&args[2], read(&args[2])),
        Some("inline") => inline(&read(&args[2]), &args[3], read(&args[3])),
        _ => panic!("bitcode safepoints <vm.ll> | bitcode inline <vm.ll> <inline.ll>"),
    }
}

fn safepoints(path: &str, text: String) {
    let may = ir::reach(&text, "edge_collect");
    // A collection inlined into its callers would leave no call to find, so none at all is an error, never an empty set.
    assert!(ir::bodies(&text, "").iter().any(|f| ir::calls(f, "edge_collect") > 0), "no function of {path} calls edge_collect, keep VM::collect out of line");
    let mut handlers: Vec<&str> = may.into_iter().filter(|f| f.starts_with("op_")).collect();
    handlers.sort_unstable();
    // Metadata the module numbers already, which the new nodes come after.
    let top = text.lines().filter_map(|l| l.strip_prefix('!')?.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok()).max().map_or(0, |n| n + 1);
    let mut out = format!("{text}\n!edge.safepoints = !{{{}}}\n", (0..handlers.len()).map(|k| format!("!{}", top + k as u64)).collect::<Vec<_>>().join(", "));
    for (k, h) in handlers.iter().enumerate() { out += &format!("!{} = !{{!\"{h}\"}}\n", top + k as u64); }
    std::fs::write(path, out).expect("the .ll file written back");
    eprintln!("{} of the handlers may collect", handlers.len());
}

fn inline(vm: &str, path: &str, text: String) {
    // What vm.o defines for other objects: a define that is neither internal nor private.
    let exported: std::collections::HashSet<&str> = vm.lines().filter(|l| l.starts_with("define ") && !l.contains(" internal ") && !l.contains(" private "))
        .filter_map(|l| Some(ir::symbol(&l[l.find('@')? + 1..]))).collect();
    let mut missing = Vec::new();
    let out: String = text.lines().map(|l| {
        if let Some(rest) = l.strip_prefix("declare ") && let Some(n) = rest.find('@').map(|at| ir::symbol(&rest[at + 1..])) && !n.starts_with("llvm.") && !exported.contains(n) { missing.push(n.to_string()); }
        match l.strip_prefix("define ") { Some(rest) => format!("define available_externally {}\n", rest.trim_start_matches("dso_local ")), None => format!("{l}\n") }
    }).collect();
    // Each one named by the function or constant whose text uses it, so the fast path to cut is plain.
    let users: Vec<String> = missing.iter().map(|m| {
        let at = format!("@{m}");
        let user = text.lines().find(|l| (l.starts_with("define ") || l.starts_with('@')) && l.contains(&at) && !l.starts_with("declare"))
            .or_else(|| text.split("\ndefine ").find(|f| f.contains(&at)).and_then(|f| f.lines().next())).unwrap_or("?");
        // A constant is named by the function that reads it.
        let by = user.strip_prefix('@').and_then(|u| u.split(' ').next()).and_then(|c| text.split("\ndefine ").skip(1).find(|f| f.contains(&format!("{c},")) || f.contains(&format!("{c})")) || f.contains(&format!("{c} "))).and_then(|f| f.lines().next()));
        format!("{m} <- {} <- {}", &user[..user.len().min(80)], by.map_or("", |b| &b[..b.len().min(120)]))
    }).collect();
    assert!(missing.is_empty(), "the inline handlers call what vm.o does not export, keep their fast paths to exported calls:\n{}", users.join("\n"));
    std::fs::write(path, out).expect("inline.ll written back");
}
