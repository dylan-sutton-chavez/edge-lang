/* The `op_` handlers driven over the corpus as a glue drives them, the stack the parser's and its values held outside
   the VM, against what the VM itself gives each case. Checks `code::effect` too, a wrong count misplaces the stack.
   Native code has no step budget, so a case the budget stops is not run. Built with `gc-torture` it collects and moves the heap at every slow step. */

// Every run meters what it allocates, as the native build does.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);

use compiler::code::{OpCode, effect};
use compiler::native::{self, Rt, JUMP, KEEP_OUTPUT, RETURN, err_id, is_err};
use compiler::types::BUILTIN_TYPES;

const UNDEF: i64 = 0x7FFC_0000_0000_0000;
const NONE: i64 = 0x7FFC_0000_0000_0001;

fn truth(v: i64) -> bool { v == 0x7FFC_0000_0000_0002 }

/* Runs body `k`, the module's here, every instruction through its handler but the stack's own moves and jumps, the
   `try`s open kept as the glue knows them, each handler with the stack depth it starts from. */
fn body(rt: *mut Rt, chunk: &compiler::code::SSAChunk, k: i64) -> i64 {
    let stop = BUILTIN_TYPES.iter().position(|&t| t == "StopIteration").unwrap() as u32;
    let (mut stack, mut ip, mut tries) = (Vec::<i64>::new(), 0usize, Vec::<(usize, usize)>::new());
    while let Some(&ins) = chunk.instructions.get(ip) {
        let site = k << 32 | (ip as i64) << 16 | ins.operand as i64;
        let h = native::handler(ins.opcode).expect("every parser opcode has a handler");
        let mut next = ip + 1;
        let r = match ins.opcode {
            // A back edge is a safepoint, where the heap may collect and the memory limit raises.
            OpCode::Jump if (ins.operand as usize) <= ip => unsafe { native::edge_safepoint(rt, stack.as_mut_ptr(), stack.len() as u32) },
            OpCode::Jump => { ip = ins.operand as usize; continue; }
            OpCode::PopTop => { stack.pop(); NONE }
            OpCode::Dup => { let v = *stack.last().unwrap(); stack.push(v); NONE }
            OpCode::Dup2 => { let n = stack.len(); stack.extend_from_slice(&stack[n - 2..].to_vec()); NONE }
            OpCode::Swap => { let n = stack.len(); stack.swap(n - 1, n - 2); NONE }
            OpCode::Rot3 => { let n = stack.len(); let v = stack.remove(n - 3); stack.push(v); NONE }
            OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop => {
                let top = *stack.last().unwrap();
                let r = unsafe { h(rt, site, top, UNDEF, UNDEF, std::ptr::null_mut(), stack.as_mut_ptr(), stack.len() as u32) };
                if !is_err(r) {
                    let jumps = truth(r) == (ins.opcode == OpCode::JumpIfTrueOrPop);
                    if jumps { next = ins.operand as usize; }
                    if ins.opcode == OpCode::JumpIfFalse || !jumps { stack.pop(); }
                }
                r
            }
            op => {
                let (pops, pushes) = effect(&ins, chunk).unwrap_or_else(|| panic!("{op:?} has no effect"));
                let pops = if op == OpCode::ReturnValue { pops.min(stack.len() as u32) } else { pops };
                let Some(under) = stack.len().checked_sub(pops as usize) else { panic!("{op:?} at {ip} pops {pops} of {}: {:?}", stack.len(), chunk.instructions) };
                let args = stack.split_off(under);
                let at = |j: usize| args.get(j).copied().unwrap_or(UNDEF);
                let mut io: Vec<i64> = args.iter().skip(3).copied().chain(std::iter::repeat_n(UNDEF, 8)).collect();
                let r = unsafe { h(rt, site, at(0), at(1), at(2), io.as_mut_ptr(), stack.as_mut_ptr(), stack.len() as u32) };
                if !is_err(r) {
                    if matches!(op, OpCode::SetupExcept | OpCode::SetupFinally) { tries.push((ins.operand as usize, stack.len())); }
                    if op == OpCode::PopExcept { tries.pop(); }
                    if op == OpCode::ReturnValue { return r; }
                    if pushes >= 1 { stack.push(r); stack.extend_from_slice(&io[..pushes as usize - 1]); }
                } else if err_id(r) & JUMP != 0 && err_id(r) & RETURN == 0 {
                    next = (err_id(r) & !JUMP) as usize;
                } else if err_id(r) == RETURN {
                    return unsafe { native::edge_returned(rt) };
                } else if op == OpCode::ForIter && err_id(r) == stop {
                    ip = ins.operand as usize;
                    continue;
                }
                r
            }
        };
        if std::env::var("EDGE_TRACE").is_ok() { eprintln!("{ip} {:?} -> {:x}", ins.opcode, r); }
        if ins.opcode == OpCode::Jump && !is_err(r) { ip = ins.operand as usize; continue; }
        if is_err(r) && err_id(r) & (JUMP | RETURN) == 0 {
            let caught = unsafe { native::edge_raise(rt, site) };
            if is_err(caught) { return caught; }
            let (at, depth) = tries.pop().expect("a caught raise has its try");
            stack.truncate(depth);
            if caught != UNDEF { stack.push(caught); }
            next = at;
        }
        ip = next;
    }
    NONE
}

#[derive(serde::Deserialize)]
struct Case { src: String, output: Vec<String>, #[serde(default)] error: Option<String>, #[serde(default)] input: Vec<String>, #[serde(default)] events: Vec<String>, #[serde(default)] interactive_events: Vec<String>, #[serde(default)] sends: Option<Vec<(String, String)>> }

#[test]
fn corpus_through_handlers() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases/vm.json");
    let cases: Vec<Case> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let (mut failed, mut ran) = (Vec::new(), 0);
    for case in cases.iter().filter(|c| c.error.as_deref() != Some("budget exceeded") && c.input.is_empty() && c.events.is_empty() && c.interactive_events.is_empty() && c.sends.is_none() && std::env::var("EDGE_SRC").map_or(true, |f| c.src.contains(&f))).take(std::env::var("EDGE_CASES").ok().and_then(|n| n.parse().ok()).unwrap_or(usize::MAX)) {
        // Native code runs with no step budget, the memory limit as the VM's.
        let limits = compiler::embed::Limits::sandbox();
        let rt = unsafe { native::edge_boot(case.src.as_ptr(), case.src.len(), u64::MAX, limits.memory as u64, KEEP_OUTPUT) };
        if rt.is_null() { continue; }
        ran += 1;
        if std::env::var("EDGE_DUMP").is_ok() { eprintln!("{:?}", native::chunk(unsafe { &*rt }).instructions); }
        let r = body(rt, native::chunk(unsafe { &*rt }), 0);
        let got = native::output(unsafe { &*rt });
        let err = if is_err(r) { native::error(unsafe { &*rt }) } else { None };
        let want: String = case.output.iter().map(|l| format!("{l}\n")).collect();
        // What a run that raised printed is not the case's, as the VM's own corpus test reads it.
        let same_out = err.is_some() || got.trim_end_matches('\n') == want.trim_end_matches('\n');
        let same_err = match (&err, &case.error) { (None, None) => true, (Some(g), Some(w)) => g.contains(w.as_str()), _ => false };
        if !same_out || !same_err { failed.push(format!("{:?}\n   got {got:?} {err:?}\n   want {want:?} {:?}", case.src, case.error)); }
    }
    let shown = failed.len().min(60);
    assert!(failed.is_empty(), "{} of {ran} case(s) differ from the VM:\n{}", failed.len(), failed[..shown].join("\n"));
}
