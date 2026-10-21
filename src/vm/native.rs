/* The VM as a library of opcode handlers for `edge build`: each parser opcode an `op_<Name>` symbol taking and returning
   `Val`s, the frames, raises and safepoints its glue calls. A handler's fast path is the
   register loop's own, its slow path the VM's handler run as the host runs one, frameless, a call nesting to its end. */

use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};

use crate::parser::{Instruction, OpCode, SSAChunk};
use super::lower::{self, Code, Ins};
use super::registers::{int_add, int_bits, int_div, int_floordiv, int_mod, int_mul, int_sub, float_div};
use super::types::*;
use super::VM;

/* A handler gives one i64: the value, or under `TAG_ERR` an error with its id in bits 4 to 35. The id is Edge's own
   exception id, the class's index in the builtin type tree, or a move of the frame: `JUMP` to the instruction in its
   low bits, `RETURN` with the value `edge_returned` gives. No Val has the tag, a heap index takes the nibble 4. */
pub use crate::parser::{JUMP, RETURN, TAG_ERR};

#[unsafe(no_mangle)] pub extern "C" fn is_err(v: i64) -> bool { (v as u64 & 0xFFFF_0000_0000_000F) == TAG_ERR }
#[unsafe(no_mangle)] pub extern "C" fn err_id(v: i64) -> u32 { (v as u64 >> 4) as u32 }

#[inline(always)]
fn err(id: u32) -> i64 { (TAG_ERR | (id as u64) << 4) as i64 }

/* The chunk of body `body`, the module's at 0 and each function's at its index plus one, as the glue numbers them. */
pub fn chunk(rt: &Rt, body: usize) -> &'static SSAChunk { rt.bodies[body].chunk }

/* One body the glue runs, the module's first then each function's at its index plus one. */
struct Body { chunk: &'static SSAChunk, code: Rc<Code>, pool: usize }

/* A native frame, its body, window, the exception and spread frames below it and how its call entered. */
struct Frame { body: usize, depth: usize, exc: usize, deltas: usize, outer: usize, entry: Option<super::opcodes::function::Entry> }

pub struct Rt {
    vm: Box<VM<'static>>,
    bodies: Vec<Body>,
    frames: Vec<Frame>,
    // Each instruction a slow step ran, by its body and index, as the code that runs it alone.
    steps: crate::util::hash::FxHashMap<(usize, usize), Rc<Code>>,
    // Each spread the glue holds as one tuple, with what it adds to its call's positional and keyword counts.
    packs: Vec<(Val, i32, i32, usize)>,
    // What a frame a `finally` ended returns.
    returned: Val,
    src: &'static str,
    // The file the tracebacks name, as `edge run` names the script.
    path: Option<&'static str>,
    _paying: crate::value::meter::Paying,
}

// What the program prints goes to stdout, an error to stderr as `edge run` prints one, a blank line after its block.
#[cfg(feature = "std")]
fn out(text: &str) { use std::io::Write; let _ = std::io::stdout().write_all(text.as_bytes()); }
#[cfg(feature = "std")]
fn fail(text: &str) { std::eprintln!("{text}"); }
#[cfg(not(feature = "std"))]
fn out(text: &str) { wasi::write(1, text.as_bytes()) }
#[cfg(not(feature = "std"))]
fn fail(text: &str) { wasi::write(2, text.as_bytes()); wasi::write(2, b"\n") }

/* The two WASI calls a `.wasm` makes, its only imports. */
#[cfg(not(feature = "std"))]
mod wasi {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    unsafe extern "C" {
        fn fd_write(fd: u32, iovs: *const [usize; 2], n: usize, written: *mut usize) -> u16;
        pub fn proc_exit(code: u32) -> !;
    }
    pub fn write(fd: u32, mut b: &[u8]) {
        while !b.is_empty() {
            let mut done = 0;
            if unsafe { fd_write(fd, &[b.as_ptr() as usize, b.len()], 1, &mut done) } != 0 || done == 0 { return; }
            b = &b[done..];
        }
    }
}

/* Ends the program with `code`, as a panic of a `.wasm` does. */
#[cfg(not(feature = "std"))]
pub fn exit(code: u32) -> ! { unsafe { wasi::proc_exit(code) } }

#[inline(always)]
fn ok(v: Val) -> i64 { v.0 as i64 }

/* Edge's id of the error kept in the VM, the class's index in the builtin type tree, a user class its nearest builtin one. */
#[cold]
fn raised(vm: &mut VM<'static>, e: VmErr) -> i64 {
    let id = exc_id(vm, &e);
    vm.reg_error = Some(e);
    err(id)
}

fn exc_id(vm: &VM<'static>, e: &VmErr) -> u32 {
    let name = e.class_name();
    let t = type_id(&name);
    if t != u32::MAX { return t; }
    // A user class takes the first builtin of its order of resolution.
    let class = vm.pending.exc_val.and_then(|v| match vm.heap.try_get(v) { Some(HeapObj::Instance(c, _)) => Some(*c), _ => None });
    let mro = class.and_then(|c| vm.mro_cache.get(&c.0).cloned());
    mro.iter().flat_map(|m| m.iter().copied()).map(|c| match vm.heap.try_get(c) { Some(HeapObj::Class(n, ..)) => type_id(n), _ => u32::MAX })
        .find(|&t| t != u32::MAX).unwrap_or(const { type_id("Exception") })
}

#[inline(always)]
fn parts(site: i64) -> (usize, usize) { ((site >> 32) as usize, (site >> 16 & 0xFFFF) as usize) }

/* The handler of every parser opcode, `op` constant in each symbol so its match folds once inlined. */
#[inline(always)]
unsafe fn run(rt: *mut Rt, site: i64, op: OpCode, a: i64, b: i64, c: i64, rest: *mut i64, live: *mut i64, nlive: u32) -> i64 {
    let (x, y) = (Val(a as u64), Val(b as u64));
    macro_rules! num {
        ($int:expr, $float:expr) => {{
            let v = if x.is_int() && y.is_int() { $int(x.as_int(), y.as_int()) } else if x.is_float() && y.is_float() { $float(x.as_float(), y.as_float()) } else { None };
            if let Some(v) = v { return ok(v); }
        }};
    }
    macro_rules! cmp {
        ($cmp:tt) => {{
            if x.is_int() && y.is_int() { return ok(Val::bool(x.as_int() $cmp y.as_int())); }
            if x.is_float() && y.is_float() { return ok(Val::bool(x.as_float() $cmp y.as_float())); }
        }};
    }
    match op {
        OpCode::Add | OpCode::InPlaceAdd => num!(int_add, |p: f64, q: f64| Some(Val::float(p + q))),
        OpCode::Sub | OpCode::InPlaceSub => num!(int_sub, |p: f64, q: f64| Some(Val::float(p - q))),
        OpCode::Mul => num!(int_mul, |p: f64, q: f64| Some(Val::float(p * q))),
        OpCode::Div => num!(int_div, float_div),
        OpCode::Mod => num!(int_mod, |_, _| None),
        OpCode::FloorDiv => num!(int_floordiv, |_, _| None),
        OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor | OpCode::Shl | OpCode::Shr if x.is_int() && y.is_int()
            => if let Some(v) = int_bits(match op { OpCode::BitAnd => OpCode::BitAndR, OpCode::BitOr => OpCode::BitOrR, OpCode::BitXor => OpCode::BitXorR, OpCode::Shl => OpCode::ShlR, _ => OpCode::ShrR }, x.as_int(), y.as_int()) { return ok(v); },
        OpCode::Lt => cmp!(<),
        OpCode::LtEq => cmp!(<=),
        OpCode::Gt => cmp!(>),
        OpCode::GtEq => cmp!(>=),
        OpCode::Eq => cmp!(==),
        OpCode::NotEq => cmp!(!=),
        OpCode::Is | OpCode::IsNot if !x.is_undef() && !y.is_undef() => return ok(Val::bool((x.0 == y.0) == (op == OpCode::Is))),
        OpCode::Minus if x.is_int() => if let Some(v) = Val::int_checked(-x.as_int()) { return ok(v); },
        OpCode::Minus if x.is_float() => return ok(Val::float(-x.as_float())),
        OpCode::Not if x.is_bool() || x.is_int() || x.is_none() => return ok(Val::bool(!truth(x))),
        OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop if x.is_bool() || x.is_int() || x.is_none() => return ok(Val::bool(truth(x))),
        // A name read or bound where its body keeps it, a module binding or a slot of the frame, a builtin under the bindings.
        OpCode::LoadName | OpCode::StoreName => {
            let rt = unsafe { &mut *rt };
            // Read with `get` throughout, so a fast path holds no panic and inlines alone.
            let (Some(b), slot) = (rt.bodies.get((site >> 32) as usize), (site & 0xFFFF) as usize) else { return unsafe { slow(&mut *rt, site, op, [a, b, c], rest, live, nlive) } };
            match b.code.kinds.get(slot) {
                Some(&super::scope::Kind::Global(g)) if let Some(scope) = rt.vm.scopes.get_mut(b.code.module) => {
                    // One that rebinds a builtin's name tells the fused calls, in the slow path.
                    if op == OpCode::StoreName { if !x.is_undef() && scope.names_builtin(g).is_none() { scope.set_at(g, x); return ok(Val::none()); } }
                    else { let v = match scope.at(g) { v if v.is_undef() => scope.fallback(g), v => v }; if !v.is_undef() { return ok(v); } }
                }
                Some(super::scope::Kind::Local) if op == OpCode::LoadName && let Some(&v) = rt.vm.regs.get(rt.vm.base + slot) && !v.is_undef() => return ok(v),
                _ => {}
            }
        }
        OpCode::LoadTrue => return ok(Val::bool(true)),
        OpCode::LoadFalse => return ok(Val::bool(false)),
        OpCode::LoadNone => return ok(Val::none()),
        OpCode::GetItem if x.is_heap() && y.is_int() => {
            let vm = unsafe { &*(*rt).vm };
            let hit = match vm.heap.plain(x) {
                // SAFETY nothing writes the list while one item is copied out.
                Some(HeapObj::List(l)) => match unsafe { l.try_borrow_unguarded() } { Ok(l) => index(y.as_int(), l.len()).and_then(|j| l.get(j).copied()), Err(_) => None },
                Some(HeapObj::Tuple(t)) => index(y.as_int(), t.len()).and_then(|j| t.get(j).copied()),
                _ => None,
            };
            if let Some(v) = hit { return ok(v); }
        }
        // A list item stored, the list in `a`, the index in `b`, the value in `c`.
        OpCode::StoreItem if x.is_heap() && y.is_int() => {
            let vm = unsafe { &*(*rt).vm };
            if let Some(HeapObj::List(l)) = vm.heap.plain(x) && let Ok(mut l) = l.try_borrow_mut() && let Some(j) = index(y.as_int(), l.len()) && let Some(at) = l.get_mut(j) {
                *at = Val(c as u64);
                return ok(Val::none());
            }
        }
        _ => {}
    }
    unsafe { slow(&mut *rt, site, op, [a, b, c], rest, live, nlive) }
}

#[inline(always)]
fn truth(v: Val) -> bool { if v.is_bool() { v.as_bool() } else if v.is_int() { v.as_int() != 0 } else { false } }

#[inline(always)]
fn index(i: i64, len: usize) -> Option<usize> {
    let j = if i < 0 { i + len as i64 } else { i };
    (0..len as i64).contains(&j).then_some(j as usize)
}

/* The VM's own handler for `op`, its inputs pushed in stack order and its results taken back, the `live` values the
   glue holds kept as roots meanwhile and handed back as the VM leaves them. */
#[cold]
#[inline(never)]
#[unsafe(export_name = "edge_slow")]
unsafe fn slow(rt: &mut Rt, site: i64, op: OpCode, abc: [i64; 3], rest: *mut i64, live: *mut i64, nlive: u32) -> i64 {
    unsafe { rooted(rt, live, nlive, |rt| slow_step(rt, site, op, abc, rest)) }
}

/* Runs `f` with the glue's `n` values at `live` kept as roots, each read back as the VM leaves it. */
unsafe fn rooted<R>(rt: &mut Rt, live: *mut i64, n: u32, f: impl FnOnce(&mut Rt) -> R) -> R {
    let mark = unsafe { edge_root(rt, live, n) };
    let r = f(rt);
    unsafe { edge_unroot(rt, live, n, mark) };
    r
}

/* The two halves of `rooted` for the glue's own calls that may collect, a direct call among them: the `n` values at
   `live` kept as roots, then read back as the VM leaves them, `mark` where they start. */
/// # Safety
/// `rt` comes from `edge_boot`, `live` holds `n` values.
#[unsafe(no_mangle)]
#[cold]
pub unsafe extern "C" fn edge_root(rt: *mut Rt, live: *mut i64, n: u32) -> u32 {
    let vm = unsafe { &mut *(*rt).vm };
    let mark = vm.temp_roots.len() as u32;
    for k in 0..n as usize { vm.temp_roots.push(Val(unsafe { *live.add(k) } as u64)); }
    mark
}

/// # Safety
/// As `edge_root`, `mark` what it gave.
#[unsafe(no_mangle)]
#[cold]
pub unsafe extern "C" fn edge_unroot(rt: *mut Rt, live: *mut i64, n: u32, mark: u32) {
    let vm = unsafe { &mut *(*rt).vm };
    for k in 0..n as usize { unsafe { *live.add(k) = vm.temp_roots[mark as usize + k].0 as i64; } }
    vm.temp_roots.truncate(mark as usize);
}

unsafe fn slow_step(rt: &mut Rt, site: i64, op: OpCode, abc: [i64; 3], rest: *mut i64) -> i64 {
    let (body, ip) = parts(site);
    framed(rt, body);
    let ins: Instruction = rt.bodies[body].chunk.instructions[ip];
    let (pops, pushes) = crate::parser::effect(&ins, rt.bodies[body].chunk).unwrap_or((0, 0));
    let at = rt.vm.stack.len();
    let input = |k: usize| Val(if k < 3 { abc[k] } else { unsafe { *rest.add(k - 3) } } as u64);
    // A branch takes the value it tests alone, which the glue keeps or pops.
    let branch = matches!(op, OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop);
    if branch { rt.vm.stack.push(input(0)); } else {
        for k in 0..pops as usize {
            let v = input(k);
            // A spread's items go back where the spread stood, its counts with them.
            match rt.packs.iter().position(|p| p.0.0 == v.0) {
                Some(j) => {
                    let (t, dpos, dkw, below) = rt.packs.swap_remove(j);
                    let items = match rt.vm.heap.try_get(t) { Some(HeapObj::Tuple(items)) => items.clone(), _ => Vec::new() };
                    // Positionals spread after keywords go under them, as the VM's own spread puts them.
                    let under = rt.vm.stack.len().saturating_sub(below);
                    rt.vm.stack.splice(under..under, items);
                    (rt.vm.pending.pos_delta, rt.vm.pending.kw_delta) = (rt.vm.pending.pos_delta + dpos, rt.vm.pending.kw_delta + dkw);
                }
                None => rt.vm.stack.push(v),
            }
        }
    }
    let deltas = (rt.vm.pending.pos_delta, rt.vm.pending.kw_delta);
    // The safepoint itself, where the glue's values and the inputs are rooted and nothing the step makes exists yet.
    if torture() { tortured(&mut rt.vm); }
    let step = step(rt, body, ip, ins, if branch { 1 } else { pushes });
    // A spread's items, as many as its value holds, stand on the glue's stack as one tuple until the call takes them.
    let step = match step {
        Ok(Flow::Gives(top)) if op == OpCode::UnpackArgs => {
            let vm = &mut *rt.vm;
            let (dpos, dkw) = (vm.pending.pos_delta - deltas.0, vm.pending.kw_delta - deltas.1);
            // The return that ended the step took the last item, of the one more than its added positionals it pushed, twice per keyword.
            if dpos + 1 + 2 * dkw > 0 { vm.stack.push(top); }
            (vm.pending.pos_delta, vm.pending.kw_delta) = deltas;
            let items = vm.stack.split_off(at.min(vm.stack.len()));
            let below = if ins.operand & 0x3 == 1 { 2 * (ins.operand >> 2) as usize } else { 0 };
            vm.heap.alloc(HeapObj::Tuple(items)).map(|t| { rt.packs.push((t, dpos, dkw, below)); Flow::Gives(t) })
        }
        other => other,
    };
    let vm = &mut *rt.vm;
    let step = step.and_then(|r| if vm.heap.has_work() { vm.pay_work().map(|()| r) } else { Ok(r) });
    let ret = match step {
        Ok(Flow::Gives(v)) => ok(v),
        Ok(Flow::Return(v)) => { rt.returned = v; err(RETURN) }
        Ok(Flow::Jump(k)) => err(JUMP | k as u32),
        // A loop's end is no error the VM keeps, only its id.
        Ok(Flow::Ended) => err(const { type_id("StopIteration") }),
        Ok(Flow::Next) => {
            let got = &vm.stack[at.min(vm.stack.len())..];
            for (k, &v) in got.iter().enumerate().skip(1) { unsafe { *rest.add(k - 1) = v.0 as i64; } }
            ok(got.first().copied().filter(|_| pushes != 0).unwrap_or(Val::none()))
        }
        Err(e) => raised(vm, e),
    };
    vm.stack.truncate(at);
    ret
}

/* Where an opcode the VM ran leaves the frame: on past it with what it pushed, with a value it gives, at another
   instruction, or returning. */
enum Flow { Next, Gives(Val), Jump(usize), Return(Val), Ended }

/* Runs instruction `ip` of `body`. One that only moves the frame's `try`, `finally` and return runs alone. Any other
   runs as a frame of the VM's loop, in a copy of its body's code returning right after it, so the frames, jobs and
   generator steps it opens run there to their end as the interpreter runs them. */
fn step(rt: &mut Rt, body: usize, ip: usize, ins: Instruction, pushes: u32) -> Result<Flow, VmErr> {
    let (chunk, pool) = (rt.bodies[body].chunk, rt.bodies[body].pool);
    let exc = rt.frames.last().map_or(0, |f| f.exc);
    if matches!(ins.opcode, OpCode::SetupExcept | OpCode::SetupFinally | OpCode::PopExcept | OpCode::BeginFinally | OpCode::EndFinally | OpCode::UnwindFinally | OpCode::ReturnValue) {
        let (code, vm, mut next) = (rt.bodies[body].code.clone(), &mut *rt.vm, ip + 1);
        return Ok(match vm.dispatch(Ins { op: ins.opcode, x: 0, a: ins.operand, b: 0, c: 0 }, ip, chunk, &code.cache, &code, pool, &mut next, exc)? {
            Some(v) => Flow::Return(v),
            None if next != ip + 1 => Flow::Jump(next),
            None => Flow::Next,
        });
    }
    let code = rt.steps.entry((body, ip)).or_insert_with(|| {
        let mut src = chunk.instructions.clone();
        let stop = Instruction { opcode: OpCode::ReturnValue, operand: 0 };
        // A `for` whose iterator ends goes to its exit, a return too.
        let mut ends = alloc::vec![ip + 1];
        if ins.opcode == OpCode::ForIter { ends.push(ins.operand as usize); }
        // A spread runs alone, the keywords before it, which it would go under, put back by the call that takes it.
        if ins.opcode == OpCode::UnpackArgs { src[ip].operand &= 0x3; }
        for e in ends { if e < src.len() { src[e] = stop } else { src.resize(e + 1, stop) } }
        // A branch ends at either side with its truth, which a `__bool__` or `__len__` frame decided.
        if matches!(ins.opcode, OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop) {
            let (end, on) = (src.len() as u16, ins.opcode == OpCode::JumpIfTrueOrPop);
            let truth = |t: bool| Instruction { opcode: if t { OpCode::LoadTrue } else { OpCode::LoadFalse }, operand: 0 };
            src[ip + 1] = Instruction { opcode: OpCode::Jump, operand: end };
            src[ins.operand as usize] = Instruction { opcode: OpCode::Jump, operand: end + 2 };
            src.extend([truth(!on), stop, truth(on), stop]);
        }
        Rc::new(lower::identity(&src, chunk.names.len(), rt.bodies[body].code.kinds.clone(), rt.bodies[body].code.module))
    }).clone();
    let vm = &mut *rt.vm;
    let (base, iters) = (vm.base, vm.iter_stack.len());
    // Its loop takes the frames its calls open, as the module's own loop does.
    (vm.native_code, vm.resume_ip, vm.pending_exec_safe) = (Some(code), ip, true);
    let v = vm.exec_in(chunk, base, pool)?;
    if ins.opcode == OpCode::ForIter && vm.iter_stack.len() < iters { return Ok(Flow::Ended); }
    Ok(match pushes { 0 => Flow::Next, 1 => Flow::Gives(v), _ => { vm.stack.push(v); Flow::Next } })
}

macro_rules! handler {
    ($op:ident) => {
        #[allow(non_snake_case)]
        mod $op {
            /// # Safety
            /// `rt` comes from `edge_boot`, `rest` and `live` hold what `code::effect` and `nlive` count.
            #[unsafe(export_name = concat!("op_", stringify!($op)))]
            pub unsafe extern "C" fn handler(rt: *mut super::Rt, site: i64, a: i64, b: i64, c: i64, rest: *mut i64, live: *mut i64, nlive: u32) -> i64 {
                unsafe { super::run(rt, site, crate::parser::OpCode::$op, a, b, c, rest, live, nlive) }
            }
        }
    };
}
crate::parser::for_each_op!(handler);

pub type Handler = unsafe extern "C" fn(*mut Rt, i64, i64, i64, i64, *mut i64, *mut i64, u32) -> i64;

/* The handler of parser opcode `op`, the same function its `op_` symbol names, for a caller in Rust. */
pub fn handler(op: OpCode) -> Option<Handler> {
    macro_rules! of { ($o:ident) => { if op == OpCode::$o { return Some($o::handler); } }; }
    crate::parser::for_each_op!(of);
    None
}

/* The tag tests and the heap kind, kept as functions of the bitcode for the proofs and the IR checks. */
#[unsafe(no_mangle)] pub extern "C" fn is_int(v: i64) -> bool { Val(v as u64).is_int() }
#[unsafe(no_mangle)] pub extern "C" fn is_float(v: i64) -> bool { Val(v as u64).is_float() }
#[unsafe(no_mangle)] pub extern "C" fn is_bool(v: i64) -> bool { Val(v as u64).is_bool() }
#[unsafe(no_mangle)] pub extern "C" fn is_none(v: i64) -> bool { Val(v as u64).is_none() }
#[unsafe(no_mangle)] pub extern "C" fn is_heap(v: i64) -> bool { Val(v as u64).is_heap() }
#[unsafe(no_mangle)] pub extern "C" fn is_undef(v: i64) -> bool { Val(v as u64).is_undef() }

#[unsafe(no_mangle)] pub extern "C" fn fits_int(i: i64) -> bool { Val::int_checked(i).is_some() }
#[unsafe(no_mangle)] pub extern "C" fn as_int(v: i64) -> i64 { Val(v as u64).as_int() }

/* Edge's type id of any `v`, the tag `edge.ept` gives it: its exact type's place in the builtin type tree, past its end
   an instance, a function, an iterator, a generator and a coroutine, u32::MAX for an unbound slot. */
/// # Safety
/// `rt` comes from `edge_boot`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn heap_type(rt: *mut Rt, v: i64) -> u32 {
    // Each kind of value by the name `type_name_by` gives it, a constant once compiled, as tests/native.rs checks.
    macro_rules! id { ($n:literal) => {{ const ID: u32 = type_id($n); ID }}; }
    let v = Val(v as u64);
    if v.is_int() { return id!("int"); }
    if v.is_float() { return id!("float"); }
    if v.is_bool() { return id!("bool"); }
    if v.is_none() { return id!("NoneType"); }
    if !v.is_heap() && !v.is_foreign() { return u32::MAX; }
    let vm = unsafe { &*(*rt).vm };
    match vm.heap.try_get(v) {
        None if v.is_heap() => u32::MAX,
        Some(HeapObj::Str(_)) => id!("str"),
        Some(HeapObj::Bytes(_)) => id!("bytes"),
        Some(HeapObj::ByteArray(_)) => id!("bytearray"),
        Some(HeapObj::MemoryView(_)) => id!("memoryview"),
        Some(HeapObj::List(_)) => id!("list"),
        Some(HeapObj::Dict(_)) => id!("dict"),
        Some(HeapObj::FrozenDict(_)) => id!("frozendict"),
        Some(HeapObj::Set(_)) => id!("set"),
        Some(HeapObj::FrozenSet(_)) => id!("frozenset"),
        Some(HeapObj::Tuple(_)) => id!("tuple"),
        Some(HeapObj::Range(..) | HeapObj::BigRange(..)) => id!("range"),
        Some(HeapObj::Slice(..)) => id!("slice"),
        Some(HeapObj::Type(_) | HeapObj::Class(..)) => id!("type"),
        Some(HeapObj::Instance(c, _)) if matches!(vm.heap.try_get(*c), Some(HeapObj::Class(..))) => id!("instance"),
        Some(HeapObj::Func(..) | HeapObj::BoundMethod(..) | HeapObj::NativeFn(_) | HeapObj::Extern(_) | HeapObj::BoundUserMethod(..)) => id!("function"),
        Some(HeapObj::Iter(..)) => id!("iterator"),
        Some(HeapObj::Module(..)) => id!("module"),
        Some(HeapObj::Cell(_)) => id!("cell"),
        Some(HeapObj::Code(_) | HeapObj::FnCode(_)) => id!("code"),
        Some(HeapObj::ExcInstance(name, ..)) => by_name(name),
        // What the VM names by its state, a suspended body's kind or a type parameter's, and a plugin's object.
        _ => match by_name(vm.type_name(v)) { u32::MAX => id!("object"), t => t },
    }
}

/* A type's id by its name: its index in `BUILTIN_TYPES`, then past its end in `OTHER_KINDS`, u32::MAX for none. A
   constant wherever the name is, so the code a type claim stands on is one compare. */
pub const fn type_id(name: &str) -> u32 {
    let (types, other) = (crate::parser::BUILTIN_TYPES, crate::types::OTHER_KINDS);
    let mut k = 0;
    while k < types.len() + other.len() {
        let t = if k < types.len() { types[k] } else { other[k - types.len()] };
        let (a, b) = (t.as_bytes(), name.as_bytes());
        let mut j = 0;
        while j < a.len() && j < b.len() && a[j] == b[j] { j += 1; }
        if j == a.len() && j == b.len() { return k as u32; }
        k += 1;
    }
    u32::MAX
}

#[cold]
fn by_name(name: &str) -> u32 { type_id(name) }

/* The value a frame returns once a handler gave `RETURN`. */
/// # Safety
/// `rt` comes from `edge_boot`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_returned(rt: *mut Rt) -> i64 { ok(unsafe { &*rt }.returned) }

/* Starts the VM on program `src` as the binary's `main` was called, null after printing its syntax errors, a bad flag
   or a failed import. Its flags are `edge run`'s: `--memory MB`, and `--events FILE` whose lines are the messages
   `receive()` takes, read at the start. Native code has no step budget, so `--ops` is refused. */
/// # Safety
/// `src` points at `len` bytes of UTF-8, `argv` at `argc` C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_boot(src: *const u8, len: u64, path: *const u8, plen: u64, argc: i32, argv: *const *const core::ffi::c_char) -> *mut Rt {
    let path = (plen != 0).then(|| crate::util::leak(String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(path, plen as usize) }).into_owned().into_boxed_str()) as &'static str);
    // A `.wasm` reads no arguments, environment or file, the sandbox's own rule: it imports only fd_write and proc_exit.
    #[cfg(not(feature = "std"))]
    { let _ = (argc, argv); unsafe { boot(src, len as usize, path, Limits::sandbox().memory, false) } }
    #[cfg(feature = "std")]
    {
        let args: Vec<String> = (1..argc.max(0) as usize).map(|k| unsafe { core::ffi::CStr::from_ptr(*argv.add(k)) }.to_string_lossy().into_owned()).collect();
        let (mut memory, mut events) = (Limits::sandbox().memory, Vec::new());
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let value = it.next();
            match (flag.as_str(), value.map(String::as_str)) {
                ("--memory", Some(mb)) if let Ok(mb) = mb.parse::<usize>() => memory = mb.saturating_mul(1 << 20),
                ("--events", Some(path)) => match std::fs::read_to_string(path) {
                    Ok(text) => events.extend(text.lines().map(String::from)),
                    Err(e) => { fail(&crate::s!("error: cannot open events '", str path, "': ", str &alloc::string::ToString::to_string(&e))); return core::ptr::null_mut(); }
                },
                _ => { fail(&crate::s!("error: unknown flag '", str flag, "', a native program takes --memory MB and --events FILE")); return core::ptr::null_mut(); }
            }
        }
        let rt = unsafe { boot(src, len as usize, path, memory, false) };
        if !rt.is_null() { for e in &events { let _ = unsafe { &mut *rt }.vm.push_event(e); } }
        rt
    }
}

/* `edge_boot` from Rust, what the program prints kept in the VM when `keep`. */
/// # Safety
/// `src` points at `len` bytes of UTF-8.
pub unsafe fn boot(src: *const u8, len: usize, path: Option<&'static str>, memory: usize, keep: bool) -> *mut Rt {
    let src: &'static str = crate::util::leak(String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(src, len) }).into_owned().into_boxed_str());
    let chunk = match crate::code::compile_with(src, || path.map(String::from), || Box::new(crate::modules::NoopResolver)) { Ok(c) => c, Err(report) => { fail(&report); return core::ptr::null_mut() } };
    // SAFETY the program lives as long as the process, its one reference never given back.
    let chunk: &'static SSAChunk = unsafe { &*Rc::into_raw(chunk) };
    let mut vm = VM::boxed(chunk, Limits { ops: u64::MAX, memory });
    if !keep { vm.print_hook = Some(out); }
    let Ok(paying) = vm.paying() else { return core::ptr::null_mut() };
    let mut rt = Rt { vm, path, bodies: Vec::new(), frames: Vec::new(), steps: Default::default(), packs: Vec::new(), returned: Val::none(), src, _paying: paying };
    let vm = &mut *rt.vm;
    let started = vm.init_modules(chunk, &mut Default::default());
    let chunks: Vec<&'static SSAChunk> = core::iter::once(chunk).chain(vm.functions.iter().map(|f| &f.1)).collect();
    for (k, &c) in chunks.iter().enumerate() {
        let pool = if k == 0 { vm.pool_of(c) } else { vm.fn_pool[k - 1] };
        if vm.pools[pool].consts.is_none() && let Ok(consts) = super::cache::const_vals(c, &mut vm.heap) { vm.pools[pool].consts = Some(consts); }
        let (kinds, module) = vm.chunk_kinds(c);
        rt.bodies.push(Body { chunk: c, code: Rc::new(lower::identity(&c.instructions, c.names.len(), kinds, module)), pool });
    }
    let pool = rt.bodies[0].pool;
    let base = vm.open_window(pool, chunk.names.len() + chunk.constants.len() + 3, chunk.names.len(), false);
    let slots = vm.fill_builtins(&chunk.names);
    vm.regs[base..base + slots.len()].copy_from_slice(&slots);
    rt.frames.push(Frame { body: 0, depth: vm.depth, exc: 0, deltas: 0, outer: vm.pool_now, entry: None });
    (vm.pool_now, vm.base) = (pool, base);
    if let Err(e) = started { report(&mut rt, &e); return core::ptr::null_mut(); }
    Box::into_raw(Box::new(rt))
}

#[cold]
fn report(rt: &mut Rt, e: &VmErr) { let s = rt.vm.render_error(e, rt.src, rt.path); fail(&s); }

/* Ends the program after `edge_main` gave `err`, its traceback printed for an exception, and gives the exit code. */
/// # Safety
/// `rt` comes from `edge_boot`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_finish(rt: *mut Rt, last: i64) -> i32 {
    let rt = unsafe { &mut *rt };
    if !is_err(last) { return 0; }
    let Some(e) = rt.vm.reg_error.take() else { return 1 };
    if let Some(code) = rt.vm.system_exit_code() { return code as i32; }
    report(rt, &e);
    1
}

/* A direct call of function `fi` whose frame its body needs at once, its cells or what it captures: the frame opened as
   the VM's call opens it, `callee` with its `n` positionals bound, giving its window. `edge_back` ends it. */
/// # Safety
/// `rt` comes from `edge_boot`, `args` holds `n` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_enter(rt: *mut Rt, fi: u32, callee: i64, args: *const i64, n: u32) -> i64 {
    let rt = unsafe { &mut *rt };
    let (vm, fi) = (&mut *rt.vm, fi as usize);
    if vm.depth >= vm.max_calls { return raised(vm, cold_depth()); }
    let callee = Val(callee as u64);
    let at = vm.stack.len();
    for k in 0..n as usize { vm.stack.push(Val(unsafe { *args.add(k) } as u64)); }
    let base = vm.bind_plain(fi, callee, &super::opcodes::function::Passed::Stack { first: at, to: at }, n as usize);
    let captures = match vm.heap.try_get(callee) { Some(HeapObj::Func(_, _, c, _)) => c.clone(), _ => Default::default() };
    if let Err(e) = vm.enter_scope(fi, &captures, base) { vm.close_window(vm.fn_pool[fi]); return raised(vm, e); }
    let entry = vm.enter_body(fi);
    vm.depth += 1;
    rt.frames.push(Frame { body: fi + 1, depth: vm.depth, exc: vm.exception_stack.len(), deltas: vm.pending.delta_save.len(), outer: vm.pool_now, entry: Some(entry) });
    (vm.pool_now, vm.base) = (vm.fn_pool[fi], base);
    ok(Val::int(base as i64))
}

/* A direct call of a function `code::Program::lazy_frame` allows: only the depth the recursion limit counts. Its VM
   frame is made the first time its body needs one, a slow step or a raise, so the hot path keeps nothing. */
/// # Safety
/// `rt` comes from `edge_boot`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_go(rt: *mut Rt) -> i64 {
    let vm = unsafe { &mut *(*rt).vm };
    if vm.depth >= vm.max_calls { return unsafe { edge_deep(rt) }; }
    vm.depth += 1;
    ok(Val::none())
}

/* The end of a direct call of `fi` made at `site` in its caller, `last` what it gave: the frame closed if one was made,
   and on a raise the traceback line the VM's call leaves. */
/// # Safety
/// `rt` comes from `edge_boot`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_back(rt: *mut Rt, fi: u32, site: i64, last: i64) {
    let r = unsafe { &mut *rt };
    if r.frames.last().is_some_and(|f| f.depth == r.vm.depth && f.body == fi as usize + 1) { unsafe { edge_close(rt) } }
    r.vm.depth -= 1;
    if is_err(last) { unsafe { edge_note(rt, fi, site) } }
}

/* A raise leaving the body of generator `fi`, which the glue laid in the loop at `site` consuming it, `last`: a
   StopIteration there leaves it as RuntimeError, as it leaves a generator the VM runs (PEP 479), and the traceback
   keeps the generator's line as the VM's call of it does. */
/// # Safety
/// `rt` comes from `edge_boot`.
#[unsafe(no_mangle)]
#[cold]
pub unsafe extern "C" fn edge_stop_out(rt: *mut Rt, fi: u32, site: i64, last: i64) -> i64 {
    if !is_err(last) || err_id(last) & (JUMP | RETURN) != 0 { return last; }
    let vm = unsafe { &mut *(*rt).vm };
    let r = if err_id(last) == const { type_id("StopIteration") } { let e = vm.reg_error.take().unwrap_or_else(|| VmErr::Raised("StopIteration".into())); let e = vm.stop_out_of(None, e); raised(vm, e) } else { last };
    unsafe { edge_note(rt, fi, site) };
    r
}

/* The cold halves of `edge_go` and `edge_back`, out of line so the glue inlines only the depth. */
/// # Safety
/// `rt` comes from `edge_boot`.
#[unsafe(no_mangle)]
#[cold]
pub unsafe extern "C" fn edge_deep(rt: *mut Rt) -> i64 { raised(unsafe { &mut *(*rt).vm }, cold_depth()) }

/// # Safety
/// As `edge_deep`.
#[unsafe(no_mangle)]
#[cold]
pub unsafe extern "C" fn edge_close(rt: *mut Rt) {
    let rt = unsafe { &mut *rt };
    let Some(f) = rt.frames.pop() else { return };
    let (vm, chunk) = (&mut *rt.vm, rt.bodies[f.body].chunk);
    vm.exception_stack.truncate(f.exc);
    vm.close_window(rt.bodies[f.body].pool);
    if let Some(entry) = f.entry { vm.leave_body(f.body - 1, entry, true, chunk); }
    vm.run_in(f.outer);
}

/// # Safety
/// As `edge_deep`.
#[unsafe(no_mangle)]
#[cold]
pub unsafe extern "C" fn edge_note(rt: *mut Rt, fi: u32, site: i64) {
    let rt = unsafe { &mut *rt };
    if let Some(b) = rt.bodies.get(parts(site).0) { rt.vm.note_frame(fi as usize, Some(parts(site).1 as u32), b.chunk); }
}

/* The frame of `body` the running call needs and its lazy call did not make, opened as a call with nothing bound opens
   it, as a slow step or a raise first asks. */
#[cold]
fn framed(rt: &mut Rt, body: usize) {
    if body == 0 || rt.frames.last().is_some_and(|f| f.depth == rt.vm.depth && f.body == body) { return; }
    let (vm, chunk, pool) = (&mut *rt.vm, rt.bodies[body].chunk, rt.bodies[body].pool);
    let base = vm.open_window(pool, chunk.names.len() + chunk.constants.len() + 3, chunk.names.len(), true);
    rt.frames.push(Frame { body, depth: vm.depth, exc: vm.exception_stack.len(), deltas: vm.pending.delta_save.len(), outer: vm.pool_now, entry: None });
    (vm.pool_now, vm.base) = (pool, base);
}

/* A raise at `site`, caught by the innermost `try` of the running frame, giving the exception its handler binds, or the
   error again to leave the function with. */
/// # Safety
/// `rt` comes from `edge_boot`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn edge_raise(rt: *mut Rt, site: i64) -> i64 {
    let rt = unsafe { &mut *rt };
    let (body, ip) = parts(site);
    framed(rt, body);
    let Some(&Frame { exc, deltas, .. }) = rt.frames.last() else { return err(0) };
    let (chunk, code) = (rt.bodies[body].chunk, rt.bodies[body].code.clone());
    let vm = &mut *rt.vm;
    let Some(e) = vm.reg_error.take() else { return ok(Val::none()) };
    let depth = vm.stack.len();
    let mut next = ip;
    match vm.catch(&code, chunk, &mut next, exc, deltas, e, ip) {
        Ok(()) => {
            // A `finally` or a `with` takes no value, its exception held for the end of its cleanup.
            ok(if vm.stack.len() > depth { vm.stack.pop().unwrap_or(Val::undef()) } else { Val::undef() })
        }
        Err(e) => raised(vm, e),
    }
}

/* Whether this build collects and moves every object at each safepoint, `edge build --gc-torture`, for the tests. */
const fn torture() -> bool { cfg!(feature = "gc-torture") }

/* Collects, then moves every live object's storage, the old storage poisoned, so a value the glue did not store is
   swept and a pointer it kept reads poison. */
#[cold]
fn tortured(vm: &mut VM<'static>) {
    vm.collect();
    vm.heap.relocate();
}

/* A collection now with `live` as roots too, what `edge_safepoint` runs once the heap asks, for the tests. */
pub fn collect(rt: &mut Rt, live: &mut [i64]) { unsafe { rooted(rt, live.as_mut_ptr(), live.len() as u32, |rt| rt.vm.collect()) } }

/* `collect_point` for the glue's own loops, and under torture the move. */
/// # Safety
/// `rt` comes from `edge_boot`, `live` holds `nlive` values.
#[unsafe(no_mangle)]
#[cold]
pub unsafe extern "C" fn edge_safepoint(rt: *mut Rt, live: *mut i64, nlive: u32) -> i64 {
    let rt = unsafe { &mut *rt };
    if !rt.vm.heap.needs_gc() && !torture() { return ok(Val::none()); }
    let r = unsafe { rooted(rt, live, nlive, |rt| { let r = rt.vm.collect_point(); if torture() { rt.vm.heap.relocate(); } r }) };
    match r { Ok(()) => ok(Val::none()), Err(e) => raised(&mut rt.vm, e) }
}
