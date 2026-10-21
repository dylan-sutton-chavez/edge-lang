/* The `op_` handlers driven over the corpus as a glue drives them, the stack the parser's and its values held outside
   the VM, against what the VM itself gives each case. Checks `code::effect` too, a wrong count misplaces the stack.
   Native code has no step budget, so a case the budget stops is not run. Built with `gc-torture` it collects and moves the heap at every slow step. */

// Every run meters what it allocates, as the native build does.
#[global_allocator]
static METERED: compiler::value::meter::Metered<std::alloc::System> = compiler::value::meter::Metered(std::alloc::System);

use compiler::code::{Kind, OpCode, ParamKind, Program, effect};
use compiler::native::{self, Rt, JUMP, RETURN, err_id, is_err};
use compiler::types::BUILTIN_TYPES;

const UNDEF: i64 = 0x7FFC_0000_0000_0000;
const NONE: i64 = 0x7FFC_0000_0000_0001;

fn truth(v: i64) -> bool { v == 0x7FFC_0000_0000_0002 }

std::thread_local! {
    // Each opcode whose stack effect a run of the corpus used, the ones `effect` is checked on.
    static SEEN: std::cell::RefCell<std::collections::BTreeSet<u8>> = const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/* Runs body `k`, the module's here, every instruction through its handler but the stack's own moves and jumps, the
   `try`s open kept as the glue knows them, each handler with the stack depth it starts from. */
fn body(rt: *mut Rt, prog: &mut Program, chunk: &compiler::code::SSAChunk, k: i64, args: &[i64]) -> i64 {
    let stop = BUILTIN_TYPES.iter().position(|&t| t == "StopIteration").unwrap() as u32;
    let (mut stack, mut ip, mut tries) = (Vec::<i64>::new(), 0usize, Vec::<(usize, usize)>::new());
    // A function's plain locals live here, as the glue keeps them in registers, its parameters bound first.
    let local: Vec<bool> = match prog.scope_of(chunk) { Some((_, s)) if k != 0 => s.kinds.iter().map(|k| *k == Kind::Local).collect(), _ => Vec::new() };
    let mut slots = vec![UNDEF; chunk.names.len()];
    if k != 0 { for (j, &(_, slot)) in prog.params_of(k as usize - 1).iter().enumerate() { if let (Some(&a), Some(s)) = (args.get(j), slots.get_mut(slot)) { *s = a; } } }
    while let Some(&ins) = chunk.instructions.get(ip) {
        let site = k << 32 | (ip as i64) << 16 | ins.operand as i64;
        let h = native::handler(ins.opcode).expect("every parser opcode has a handler");
        let mut next = ip + 1;
        let slot = ins.operand as usize;
        // What the glue holds live at a call that may collect: its stack and its locals, the args a call takes too.
        let mut live: Vec<i64> = stack.iter().chain(&slots).chain(args).copied().collect();
        let r = match ins.opcode {
            OpCode::LoadName if local.get(slot) == Some(&true) && slots[slot] != UNDEF => { stack.push(slots[slot]); NONE }
            OpCode::StoreName if local.get(slot) == Some(&true) => { slots[slot] = stack.pop().unwrap(); NONE }
            // A call of a plain function by position runs its body here, lazily framed where it may be.
            OpCode::Call if ins.operand >> 8 == 0 && let Some(&f) = stack.len().checked_sub(slot + 1).and_then(|at| stack.get(at)) && let Some(fi) = native::function_of(unsafe { &*rt }, f)
                // One the program defines itself, not one an import or an `exec` made as it ran.
                && prog.scope_of(native::chunk(unsafe { &*rt }, fi + 1)).is_some_and(|(f, _)| f == fi)
                && prog.params_of(fi).len() == slot && prog.params_of(fi).iter().all(|p| p.0 == ParamKind::Normal) => {
                let args = stack.split_off(stack.len() - slot);
                stack.pop();
                // The caller's values stay rooted while the callee runs, as edge_root keeps them.
                let mark = unsafe { native::edge_root(rt, live.as_mut_ptr(), live.len() as u32) };
                let opened = if prog.lazy_frame(fi) { unsafe { native::edge_go(rt) } } else { unsafe { native::edge_enter(rt, fi as u32, f, args.as_ptr(), slot as u32) } };
                let r = if is_err(opened) { opened } else { body(rt, prog, native::chunk(unsafe { &*rt }, fi + 1), fi as i64 + 1, &args) };
                if !is_err(opened) { unsafe { native::edge_back(rt, fi as u32, site, r) }; }
                unsafe { native::edge_unroot(rt, live.as_mut_ptr(), live.len() as u32, mark) };
                if !is_err(r) { stack.push(r); }
                r
            }
            // A back edge is a safepoint, where the heap may collect and the memory limit raises.
            OpCode::Jump if (ins.operand as usize) <= ip => unsafe { native::edge_safepoint(rt, live.as_mut_ptr(), live.len() as u32) },
            OpCode::Jump => { ip = ins.operand as usize; continue; }
            OpCode::PopTop => { stack.pop(); NONE }
            OpCode::Dup => { let v = *stack.last().unwrap(); stack.push(v); NONE }
            OpCode::Dup2 => { let n = stack.len(); stack.extend_from_slice(&stack[n - 2..].to_vec()); NONE }
            OpCode::Swap => { let n = stack.len(); stack.swap(n - 1, n - 2); NONE }
            OpCode::Rot3 => { let n = stack.len(); let v = stack.remove(n - 3); stack.push(v); NONE }
            OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop => {
                let top = *stack.last().unwrap();
                let r = unsafe { h(rt, site, top, UNDEF, UNDEF, std::ptr::null_mut(), live.as_mut_ptr(), live.len() as u32) };
                if !is_err(r) {
                    let jumps = truth(r) == (ins.opcode == OpCode::JumpIfTrueOrPop);
                    if jumps { next = ins.operand as usize; }
                    if ins.opcode == OpCode::JumpIfFalse || !jumps { stack.pop(); }
                }
                r
            }
            op => {
                let (pops, pushes) = effect(&ins, chunk).unwrap_or_else(|| panic!("{op:?} has no effect"));
                SEEN.with(|s| s.borrow_mut().insert(op as u8));
                let pops = if op == OpCode::ReturnValue { pops.min(stack.len() as u32) } else { pops };
                let Some(under) = stack.len().checked_sub(pops as usize) else { panic!("{op:?} at {ip} pops {pops} of {}: {:?}", stack.len(), chunk.instructions) };
                let args = stack.split_off(under);
                let at = |j: usize| args.get(j).copied().unwrap_or(UNDEF);
                let mut io: Vec<i64> = args.iter().skip(3).copied().chain(std::iter::repeat_n(UNDEF, 8)).collect();
                let r = unsafe { h(rt, site, at(0), at(1), at(2), io.as_mut_ptr(), live.as_mut_ptr(), live.len() as u32) };
                // Every value a handler gives has its type in Edge's tree, heap_type is total.
                if !is_err(r) && r != UNDEF {
                    let (t, name) = (unsafe { native::heap_type(rt, r) }, native::type_name(unsafe { &*rt }, r));
                    assert_ne!(t, u32::MAX, "{op:?} gave {r:x}, which heap_type does not name");
                    // Its code is the one the VM's own name of the type has, a family's for an instance, a function or an iterator.
                    let family = (BUILTIN_TYPES.len()..BUILTIN_TYPES.len() + 3).contains(&(t as usize)) || name == "exception" || name.starts_with("method");
                    assert!(family || t == native::type_id(name) || native::type_id(name) == u32::MAX && t == native::type_id("object"), "{op:?} gave a {name} heap_type codes {t}");
                }
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
        let rt = unsafe { native::boot(case.src.as_ptr(), case.src.len(), None, limits.memory, true) };
        if rt.is_null() { continue; }
        ran += 1;
        let mut prog = Program::new(native::chunk(unsafe { &*rt }, 0));
        let r = body(rt, &mut prog, native::chunk(unsafe { &*rt }, 0), 0, &[]);
        let got = native::output(unsafe { &*rt });
        let err = if is_err(r) { native::error(unsafe { &*rt }) } else { None };
        let want: String = case.output.iter().map(|l| format!("{l}\n")).collect();
        // What a run that raised printed is not the case's, as the VM's own corpus test reads it.
        let same_out = err.is_some() || got.trim_end_matches('\n') == want.trim_end_matches('\n');
        let same_err = match (&err, &case.error) { (None, None) => true, (Some(g), Some(w)) => g.contains(w.as_str()), _ => false };
        if !same_out || !same_err { failed.push(format!("{:?}\n   got {got:?} {err:?}\n   want {want:?} {:?}", case.src, case.error)); }
    }
    let unseen: Vec<OpCode> = OpCode::ALL.iter().copied().filter(|o| !o.is_register() && !SEEN.with(|s| s.borrow().contains(&(*o as u8)))).collect();
    eprintln!("opcodes whose stack effect no case ran: {unseen:?}");
    let shown = failed.len().min(60);
    assert!(failed.is_empty(), "{} of {ran} case(s) differ from the VM:\n{}", failed.len(), failed[..shown].join("\n"));
}

/* A collection leaves every live value at its bits and its object, and a root naming a free or a reused slot harms nothing.
   Native code keeps values in registers between safepoints and stores them only there. */
#[test]
fn gc_keeps_values_in_place() {
    const WHY: &str = "edge build's native code stores live values at safepoints, update it if the GC moves objects or collects elsewhere";
    let src = "a = [1, 2]\nb = 'x' * 3\nc = {'k': a}\nd = [3]\n";
    let rt = unsafe { native::boot(src.as_ptr(), src.len(), None, 1 << 26, true) };
    let mut prog = Program::new(native::chunk(unsafe { &*rt }, 0));
    assert!(!is_err(body(rt, &mut prog, native::chunk(unsafe { &*rt }, 0), 0, &[])));
    let load = move |slot: i64| unsafe { native::handler(OpCode::LoadName).unwrap()(rt, slot, UNDEF, UNDEF, UNDEF, std::ptr::null_mut(), std::ptr::null_mut(), 0) };
    let (live, kinds): (Vec<i64>, Vec<u32>) = (0..3).map(|s| (load(s), unsafe { native::heap_type(rt, load(s)) })).unzip();
    // The list `d` held is garbage once it holds None, its slot free once collected.
    let freed = load(3);
    unsafe { native::handler(OpCode::StoreName).unwrap()(rt, 3, NONE, UNDEF, UNDEF, std::ptr::null_mut(), std::ptr::null_mut(), 0) };
    native::collect(unsafe { &mut *rt }, &mut live.clone());
    assert_eq!(unsafe { native::heap_type(rt, freed) }, u32::MAX, "{WHY}");
    native::collect(unsafe { &mut *rt }, &mut [live.clone(), vec![freed, freed + 0x7FF0]].concat());
    let again: Vec<i64> = (0..3).map(load).collect();
    assert_eq!(again, live, "{WHY}");
    assert_eq!(again.iter().map(|&v| unsafe { native::heap_type(rt, v) }).collect::<Vec<_>>(), kinds, "{WHY}");
    assert_eq!(unsafe { native::heap_type(rt, freed) }, u32::MAX, "{WHY}");
}
