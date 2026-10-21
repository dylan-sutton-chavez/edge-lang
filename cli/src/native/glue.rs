use compiler::code::SSAChunk;
use std::fmt::Write;

/* The program as LLVM text: one function per block calling the VM's handler of each opcode in order with its operand constant, and the entry that runs the module's block. */
pub fn module(chunk: &SSAChunk, target: &str, _torture: bool) -> String {
    let mut ir = String::from("declare void @edge.op(i32, i32)\n");
    let mut meta = Vec::new();
    let mut blocks = vec![chunk];
    let mut at = 0;
    while let Some(block) = blocks.get(at).copied() {
        blocks.extend(block.functions.iter().map(|f| &f.1).chain(&block.classes));
        // Each block names its (opcode, operand) list, so a reader of the unoptimized glue checks it against the bytecode.
        let pairs: Vec<String> = block.instructions.iter().map(|i| format!("!{{!\"{:?}\", i32 {}}}", i.opcode, i.operand)).collect();
        let _ = writeln!(ir, "define internal void @edge.block.{at}() !edge.block !{} {{", meta.len());
        for i in &block.instructions {
            let _ = writeln!(ir, "  call void @edge.op(i32 {}, i32 {})", i.opcode as u8, i.operand);
        }
        ir.push_str("  ret void\n}\n");
        meta.push(format!("!{} = !{{{}}}", meta.len(), pairs.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")));
        at += 1;
    }
    let _ = writeln!(ir, "define i32 @{}() {{\n  call void @edge.block.0()\n  ret i32 0\n}}", super::entry(target));
    ir + &meta.join("\n") + "\n"
}
