/* Names in the VM's bitcode the handlers that may collect, `!edge.safepoints`, so the glue stores its live values
   before calling one and reads them back after. Read off the module's own call graph, as bench/llvm reads the IR: a
   function may collect when it calls `edge_collect`, calls through a pointer, or calls one that may. Run on the `.ll`
   text before `llvm-as`. */

#[path = "../bench/src/ir.rs"]
#[allow(dead_code)]
mod ir;

fn main() {
    let path = std::env::args().nth(1).expect("the .ll file of the VM's bitcode");
    let text = std::fs::read_to_string(&path).expect("a readable .ll file");
    let may = ir::reach(&text, "edge_collect");
    // A collection inlined into its callers would leave no call to find, so none at all is an error, never an empty set.
    assert!(ir::bodies(&text, "").iter().any(|f| ir::calls(f, "edge_collect") > 0), "no function of {path} calls edge_collect, keep VM::collect out of line");
    let mut handlers: Vec<&str> = may.into_iter().filter(|f| f.starts_with("op_")).collect();
    handlers.sort_unstable();
    // Metadata the module numbers already, which the new nodes come after.
    let top = text.lines().filter_map(|l| l.strip_prefix('!')?.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok()).max().map_or(0, |n| n + 1);
    let mut out = format!("{text}\n!edge.safepoints = !{{{}}}\n", (0..handlers.len()).map(|k| format!("!{}", top + k as u64)).collect::<Vec<_>>().join(", "));
    for (k, h) in handlers.iter().enumerate() { out += &format!("!{} = !{{!\"{h}\"}}\n", top + k as u64); }
    std::fs::write(&path, out).expect("the .ll file written back");
    eprintln!("{} of the handlers may collect", handlers.len());
}
