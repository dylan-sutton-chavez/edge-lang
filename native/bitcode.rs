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
    // What vm.o defines for other objects: a function or a global that is neither internal nor private.
    let open = |l: &str| !l.contains(" internal ") && !l.contains(" private ");
    let mut exported: std::collections::HashSet<&str> = vm.lines().filter(|l| l.starts_with("define ") && open(l)).filter_map(|l| ir::reach_name(&l[l.find('@')? + 1..])).collect();
    exported.extend(vm.lines().filter(|l| l.starts_with('@') && open(l)).filter_map(|l| ir::reach_name(&l[1..])));
    let mut missing = Vec::new();
    let out: String = text.lines().map(|l| {
        // A function declared, or a global left without its value, is one vm.o must give.
        let wanted = if let Some(rest) = l.strip_prefix("declare ") { rest.find('@').and_then(|at| ir::reach_name(&rest[at + 1..])) }
            else if l.starts_with('@') && l.contains(" external ") { ir::reach_name(&l[1..]) } else { None };
        if let Some(n) = wanted && !n.starts_with("llvm.") && !exported.contains(n) { missing.push(n.to_string()); }
        match l.strip_prefix("define ") { Some(rest) => format!("define available_externally {}\n", rest.trim_start_matches("dso_local ")), None => format!("{l}\n") }
    }).collect();
    assert!(missing.is_empty(), "the inline bodies use what vm.o does not export, keep the fast paths to exported calls:\n{}", missing.join("\n"));
    std::fs::write(path, out).expect("inline.ll written back");
}
