/* The `op_` handlers driven over the corpus as a glue drives them, the stack the parser's and its values held outside
   the VM, against what the VM itself gives each case. Checks `code::effect` too, a wrong count misplaces the stack.
   Built with `gc-torture` it collects and moves the heap at every slow step. */

// Every run meters what it allocates, as the native build does.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);

use compiler::code::{OpCode, effect};
use compiler::native::{self, Rt, JUMP, KEEP_OUTPUT, RETURN};
use compiler::types::BUILTIN_TYPES;

const UNDEF: i64 = 0x7FFC_0000_0000_0000;

fn truth(v: i64) -> bool { v == 0x7FFC_0000_0000_0002 }

/* Runs body `k`, the module's alone here, every instruction through its handler but the stack's own moves and jumps. */
fn body(rt: *mut Rt, chunk: &compiler::code::SSAChunk, k: i64) -> native::Ret {
    let stop = BUILTIN_TYPES.iter().position(|&t| t == "StopIteration").unwrap() as i32;
    let (mut stack, mut ip, mut depth_at) = (Vec::<i64>::new(), 0usize, std::collections::HashMap::new());
    while let Some(&ins) = chunk.instructions.get(ip) {
        let site = k << 32 | (ip as i64) << 16 | ins.operand as i64;
        let call = |stack: &mut Vec<i64>, a: i64, rest: &mut Vec<i64>| {
            let h = native::handler(ins.opcode).expect("every parser opcode has a handler");
            let (b, c) = (rest.first().copied().unwrap_or(UNDEF), rest.get(1).copied().unwrap_or(UNDEF));
            let tail = if rest.len() > 2 { rest.split_off(2) } else { Vec::new() };
            let mut io: Vec<i64> = tail.into_iter().chain(std::iter::repeat_n(UNDEF, 8)).collect();
            let r = unsafe { h(rt, site, a, b, c, io.as_mut_ptr(), stack.as_mut_ptr(), stack.len() as u32) };
            (r, io)
        };
        let mut next = ip + 1;
        let r = match ins.opcode {
            OpCode::Jump => { ip = ins.operand as usize; continue; }
            OpCode::PopTop => { stack.pop(); continue_at(&mut ip) }
            OpCode::Dup => { let v = *stack.last().unwrap(); stack.push(v); continue_at(&mut ip) }
            OpCode::Dup2 => { let n = stack.len(); stack.extend_from_slice(&stack[n - 2..].to_vec()); continue_at(&mut ip) }
            OpCode::Swap => { let n = stack.len(); stack.swap(n - 1, n - 2); continue_at(&mut ip) }
            OpCode::Rot3 => { let n = stack.len(); let v = stack.remove(n - 3); stack.push(v); continue_at(&mut ip) }
            OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop => {
                let top = *stack.last().unwrap();
                let (r, _) = call(&mut stack, top, &mut Vec::new());
                if r.err == 0 {
                    let jumps = truth(r.val) == (ins.opcode == OpCode::JumpIfTrueOrPop);
                    if jumps { next = ins.operand as usize; }
                    if ins.opcode == OpCode::JumpIfFalse || !jumps { stack.pop(); }
                }
                r
            }
            op => {
                let (pops, pushes) = effect(&ins, chunk).unwrap_or_else(|| panic!("{op:?} has no effect"));
                let pops = if op == OpCode::ReturnValue { pops.min(stack.len() as u32) } else { pops };
                if matches!(op, OpCode::SetupExcept | OpCode::SetupFinally) { depth_at.insert(ins.operand as usize, stack.len()); }
                let Some(under) = stack.len().checked_sub(pops as usize) else { panic!("{op:?} at {ip} pops {pops} of {}: {:?}", stack.len(), chunk.instructions) };
                let mut args = stack.split_off(under);
                let a = if args.is_empty() { UNDEF } else { args.remove(0) };
                let (r, io) = call(&mut stack, a, &mut args);
                match r.err {
                    0 if op == OpCode::ForIter || pushes == 1 => stack.push(r.val),
                    0 => { if pushes > 1 { stack.push(r.val); stack.extend_from_slice(&io[..pushes as usize - 1]); } }
                    JUMP => next = r.val as usize,
                    RETURN => return r,
                    e if e == stop && op == OpCode::ForIter => { next = ins.operand as usize; }
                    _ => {}
                }
                if op == OpCode::ForIter && r.err == stop { ip = next; continue; }
                r
            }
        };
        if std::env::var("EDGE_TRACE").is_ok() { eprintln!("{ip} {:?} -> {:x} {}", ins.opcode, r.val, r.err); }
        if r.err > 0 {
            let mut at = 0u32;
            let caught = unsafe { native::edge_raise(rt, site, &mut at) };
            if caught.err != 0 { return caught; }
            stack.truncate(depth_at.get(&(at as usize)).copied().unwrap_or(0));
            if caught.val != UNDEF { stack.push(caught.val); }
            next = at as usize;
        }
        ip = next;
    }
    native::Ret { val: 0x7FFC_0000_0000_0001, err: 0 }
}

fn continue_at(_ip: &mut usize) -> native::Ret { native::Ret { val: 0, err: 0 } }

#[derive(serde::Deserialize)]
struct Case { src: String, output: Vec<String>, #[serde(default)] error: Option<String>, #[serde(default)] input: Vec<String>, #[serde(default)] events: Vec<String>, #[serde(default)] interactive_events: Vec<String>, #[serde(default)] sends: Option<Vec<(String, String)>> }

#[test]
fn corpus_through_handlers() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases/vm.json");
    let cases: Vec<Case> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let (mut failed, mut ran) = (Vec::new(), 0);
    for case in cases.iter().filter(|c| c.input.is_empty() && c.events.is_empty() && c.interactive_events.is_empty() && c.sends.is_none() && std::env::var("EDGE_SRC").map_or(true, |f| c.src.contains(&f))).take(std::env::var("EDGE_CASES").ok().and_then(|n| n.parse().ok()).unwrap_or(usize::MAX)) {
        let limits = compiler::embed::Limits::sandbox();
        let rt = unsafe { native::edge_boot(case.src.as_ptr(), case.src.len(), limits.ops, limits.memory as u64, KEEP_OUTPUT) };
        if rt.is_null() { continue; }
        ran += 1;
        if std::env::var("EDGE_DUMP").is_ok() { eprintln!("{:?}", native::chunk(unsafe { &*rt }).instructions); }
        let r = body(rt, native::chunk(unsafe { &*rt }), 0);
        let got = native::output(unsafe { &*rt });
        let err = if r.err > 0 { native::error(unsafe { &*rt }) } else { None };
        let want: String = case.output.iter().map(|l| format!("{l}\n")).collect();
        let same_out = got.trim_end_matches('\n') == want.trim_end_matches('\n');
        let same_err = match (&err, &case.error) { (None, None) => true, (Some(g), Some(w)) => g.contains(w.as_str()), _ => false };
        if !same_out || !same_err { failed.push(format!("{:?}\n   got {got:?} {err:?}\n   want {want:?} {:?}", case.src, case.error)); }
    }
    let shown = failed.len().min(60);
    assert!(failed.is_empty(), "{} of {ran} case(s) differ from the VM:\n{}", failed.len(), failed[..shown].join("\n"));
}
