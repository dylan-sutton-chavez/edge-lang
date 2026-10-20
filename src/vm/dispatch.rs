use crate::s;
use alloc::{rc::Rc, string::{String, ToString}, vec::Vec};
use core::cell::RefCell;

use crate::parser::{OpCode, SSAChunk, ssa_strip};

use super::{ExceptionFrame, VM};
use super::types::*;
use super::cache::OpcodeCache;
use super::sites::Site;
use super::lower::{self, Code, Ins, STORE_NOTES_BUILTIN};
use super::scope::Kind;
use super::opcodes::{attr_lookup::AttrLookup, function::{Args, Passed}};
use super::registers::{int_add, int_sub, int_mul, int_mod, int_div, int_floordiv, float_div, int_bits, numeric_binop};
use crate::vm::natives::Builtins;

/* The frame an instruction loop began with, which the frames it opens return to last. */
pub(crate) struct Floor {
    pub(crate) depth: usize,
    pub(crate) code: *const Code,
    pub(crate) chunk: *const SSAChunk,
    // The pool whose window the floor runs in.
    pub(crate) pool: usize,
    // The lengths of the exception, unwind and spread-delta stacks it began at.
    pub(crate) bases: [usize; 3],
}

/* What a step of the loop leaves it to do with its frames. */
enum Turn {
    Enter,
    // A run of text exec or eval at the index given handed the loop, which takes its chunk as its floor.
    Floor(usize),
    Lower,
    Grow,
    Yield(Val),
    Return(Val),
    Raise(VmErr),
}

/* Index `i` into `len` items, negatives from the end, None out of range. */
#[inline(always)]
fn index_of(i: i64, len: usize) -> Option<usize> {
    let j = if i < 0 { i + len as i64 } else { i };
    (0..len as i64).contains(&j).then_some(j as usize)
}

/* Whether `k` hashes without user code, an inline value or a str. */
#[inline(always)]
fn plain_key_in(heap: &HeapPool, k: Val) -> bool { !k.is_heap() && !k.is_undef() || matches!(heap.try_get(k), Some(HeapObj::Str(_))) }

/* The value a plain dict holds under a plain key, None on a miss so the slow path raises. */
#[inline(never)]
fn dict_get(heap: &HeapPool, d: &RefCell<DictMap>, k: Val) -> Option<Val> {
    if !plain_key_in(heap, k) { return None; }
    d.borrow().get(&k, heap).copied()
}

/* Stores under a plain key of a plain dict, false when the key needs the slow path. */
#[inline(never)]
fn dict_set(heap: &HeapPool, d: &RefCell<DictMap>, k: Val, v: Val) -> bool {
    if !plain_key_in(heap, k) { return false; }
    d.borrow_mut().insert(k, v, heap);
    true
}

/* Opcodes the register loop hands to `regs_aside`, done there when common. */
#[inline(always)]
fn aside_op(op: OpCode) -> bool {
    matches!(op, OpCode::PowR
        | OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR | OpCode::UnpackR
        | OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop
        | OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildString | OpCode::UnpackSequence | OpCode::FormatValue
        | OpCode::ListAppend | OpCode::SetAdd | OpCode::MapAdd
        | OpCode::CallStr | OpCode::CallInt | OpCode::CallFloat | OpCode::CallBool | OpCode::CallAbs | OpCode::CallChr | OpCode::CallOrd | OpCode::CallLen
        | OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Mod | OpCode::FloorDiv | OpCode::InPlaceAdd | OpCode::InPlaceSub
        | OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::LtEq | OpCode::Gt | OpCode::GtEq
        | OpCode::LoadGlobalAtR | OpCode::StoreGlobalAtR | OpCode::UnbindR | OpCode::SuperR | OpCode::WrapCellR
        | OpCode::SetupExcept | OpCode::SetupFinally | OpCode::PopExcept | OpCode::BeginFinally | OpCode::EndFinally
        | OpCode::WithEnter | OpCode::WithExit | OpCode::WithJudge | OpCode::DunderSectionR | OpCode::ReturnDunderR | OpCode::DunderCheckR)
}

/* Whether builtin method `id` runs again as it began once a call it made comes back, as `redoable` holds for a builtin. */
fn method_redoable(id: super::methods::BuiltinMethodId) -> bool {
    matches!(id.kind(), super::methods::MethodKind::Iterates | super::methods::MethodKind::Search)
}

impl VM<'_> {
    /* Whether builtin method `id` given `args` may call Python, walking, searching or hashing a class's key, so its op hands those calls to the loop. */
    #[inline(always)]
    fn method_calls(&self, id: super::methods::BuiltinMethodId, args: &[Val]) -> bool {
        self.heap.users && (method_redoable(id) || self.user_hash && self.user_keys(args))
    }

    /* Whether one of `args` may be a key whose `__hash__` or `__eq__` a class defines, a user value or a tuple that may hold one. */
    #[inline(never)]
    pub(crate) fn user_keys(&self, args: &[Val]) -> bool {
        self.user_hash && args.iter().any(|&a| self.heap.user(a) || matches!(self.heap.try_get(a), Some(HeapObj::Tuple(_))))
    }
}


/* Whether a class member named `n` is no dunder an op or a builtin runs, so not a dunder or one each class body has. */
#[inline]
fn plain_name(n: &[u8]) -> bool {
    let k = n.len();
    if k < 5 || n[0] != b'_' || n[1] != b'_' || n[k - 1] != b'_' || n[k - 2] != b'_' { return true; }
    match k {
        7 => n == b"__doc__",
        8 => n == b"__init__",
        10 => n == b"__module__",
        12 => n == b"__qualname__",
        15 => n == b"__firstlineno__",
        21 => n == b"__static_attributes__",
        _ => false,
    }
}

/* The operands generic op `op` takes off the stack when its handler may call a user dunder first, so it runs again after that call, None when it runs as is. */
pub(crate) fn redo_arity(op: OpCode, operand: u16) -> Option<usize> {
    Some(match op {
        OpCode::BitNot | OpCode::UnpackSequence | OpCode::UnpackEx | OpCode::Assert | OpCode::UnpackArgs => 1,
        OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor | OpCode::Shl | OpCode::Shr | OpCode::InPlaceBitOr | OpCode::InPlaceBitAnd
        | OpCode::InPlaceBitXor | OpCode::MatMul | OpCode::In | OpCode::NotIn | OpCode::DelItem => 2,
        // A spread merges what it iterates once it has all of it.
        OpCode::DictUpdate | OpCode::SetUpdate | OpCode::ListExtend => 2,
        OpCode::FormatValue => 1 + (operand & 1) as usize,
        // A raise takes what it raises, and its cause, and displays them through their `__str__`.
        OpCode::Raise if operand != 1 => 1,
        OpCode::RaiseFrom => 2,
        OpCode::StoreItem | OpCode::MatchClass => 3,
        // A key a class hashes is hashed and compared before the container takes it.
        OpCode::SetAdd => 1,
        OpCode::MapAdd => 2,
        OpCode::BuildSet => operand as usize,
        OpCode::BuildDict => 2 * operand as usize,
        _ => return None,
    })
}

/* Whether `e` is a StopIteration, which ends the loop stepping what raised it. */
fn is_stop(e: &VmErr) -> bool { matches!(e, VmErr::Raised(m) if m == "StopIteration" || m.starts_with("StopIteration:")) }

/* Whether `e` is a GeneratorExit, which a `close` throws in. */
fn is_exit(e: &VmErr) -> bool { matches!(e, VmErr::Raised(m) if m == "GeneratorExit" || m.starts_with("GeneratorExit:")) }

/* The module binding a CallGlobalR calls, read out of the register loop. */
#[inline(never)]
fn global_callee(scopes: &[super::scope::Globals], code: &Code, ins: Ins) -> Val { scopes[code.module].read(ins.a as u32) }

/* Marks a resume point as an index of the lowered code, for a run suspended inside a section, which code laid with sections keeps for good. */
pub(crate) const RESUME_RAW: usize = 1 << 30;

/* `VM::chunk_at` over the functions alone. */
#[inline]
fn chunk_in<'c, 'a: 'c>(functions: &[&'a (Vec<alloc::string::String>, SSAChunk, u16, u16)], code: &Code, chunk: &'c SSAChunk, rip: usize) -> &'c SSAChunk {
    let Some(block) = code.block.as_deref() else { return chunk };
    // The code's own instructions lie before every section, which lie in order.
    if block.sections.first().is_none_or(|s| rip < s.start as usize) { return chunk; }
    let k = block.sections.partition_point(|s| s.start as usize <= rip);
    match k.checked_sub(1).map(|k| &block.sections[k]).filter(|s| rip < s.end as usize) {
        Some(s) => &functions[s.fi].1,
        // A head or a stub laid past the end for an op inside a section names what the section's function does.
        None if block.moved => match block.holding(rip) { Some((s, _)) => &functions[s.fi].1, None => chunk },
        None => chunk,
    }
}

/* Whether every section of `block` is still what its binding holds, noted so the next call checks none. */
#[cold]
#[inline(never)]
fn sections_hold(scopes: &[super::scope::Globals], block: &lower::Block) -> bool {
    let now = super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed);
    // A method's section is checked at each call on its receiver as no binding holds it, a builtin's is the one its name finds.
    let hold = block.sections.iter().all(|s| s.global == u16::MAX || scopes.get(s.bound).is_some_and(|g| g.read(s.global as u32).0 == s.value.get().0));
    if hold { block.binds.set(now); }
    hold
}

/* Whether the class section `s` was laid for still finds the `__init__` it was laid from, noted per class version so the next call checks nothing. */
#[cold]
#[inline(never)]
fn ctor_holds(heap: &crate::value::HeapPool, mro: &crate::util::hash::FxHashMap<u64, Rc<alloc::vec::Vec<Val>>>, s: &lower::Section, epoch: u32) -> bool {
    let Some((at, _, init)) = &s.ctor else { return false };
    let hold = super::opcodes::attr_lookup::class_member(heap, mro, s.value.get(), "__init__").is_some_and(|(f, _)| f.0 == init.0);
    if hold { at.set(epoch); }
    hold
}

/* Whether `f` is a function of the body section `s` was laid from, then writing its captured cells and the defaults past the `n` passed arguments into it. */
#[inline(never)]
fn body_enter(heap: &crate::value::HeapPool, r: &mut [Val], f: Val, s: &lower::Section, n: usize) -> bool {
    let (Some((arity, cells)), Some(HeapObj::Func(fi, defaults, captures, _))) = (&s.by_body, heap.try_get(f)) else { return false };
    if *fi != s.fi || captures.len() != cells.len() || n + defaults.len() < *arity as usize { return false; }
    for (&at, &(_, cell)) in cells.iter().zip(captures.iter()) { r[at as usize] = cell; }
    let (arity, first) = (*arity as usize, s.link as usize + 1);
    for p in n..arity { r[first + p] = defaults[p + defaults.len() - arity]; }
    true
}

/* Whether `f` is a function of the body section `s` was laid from that a call of `n` arguments may enter, as `body_enter` takes. */
fn body_fits(heap: &crate::value::HeapPool, f: Val, s: &lower::Section, n: usize) -> bool {
    let (Some((arity, cells)), Some(HeapObj::Func(fi, defaults, captures, _))) = (&s.by_body, heap.try_get(f)) else { return false };
    *fi == s.fi && captures.len() == cells.len() && n + defaults.len() >= *arity as usize
}

/* A fresh instance of `class` with room for `attrs` attributes, None past the memory limit. */
#[cold]
#[inline(never)]
fn new_object(heap: &mut crate::value::HeapPool, class: Val, attrs: usize) -> Option<Val> {
    let mut dict = crate::value::DictMap::with_capacity(attrs);
    dict.set_shape(super::sites::ROOT_SHAPE);
    heap.alloc(HeapObj::Instance(class, Rc::new(core::cell::RefCell::new(dict)))).ok()
}

/* A handler laid inside a section names its own index in the code, flagged so, any other the compiler's instruction. */
pub(crate) const RAW_HANDLER: usize = 1 << 30;

/* Where the section `s` of `code` lays the handler at compiler instruction `h` of its body, as a raw handler. */
fn handler_in(s: &lower::Section, code: &Code, h: usize) -> Option<usize> {
    if h & RAW_HANDLER != 0 { return None; }
    // The handler one of the section's own `try`s names, laid from that compiler instruction.
    let (start, end) = (s.start as usize, (s.end as usize).min(code.ins.len()));
    code.ins[start..end].iter().filter(|i| matches!(i.op, OpCode::SetupExcept | OpCode::SetupFinally) && i.x == 1).map(|i| i.a as usize)
        .find(|&e| (start..end).contains(&e) && s.orig.get(e - start).is_some_and(|&o| o as usize == h)).map(|e| e | RAW_HANDLER)
}

/* Whether a generator holding the handlers `excs` may be taken into section `s` of `code`, one with none or one whose section notes its stacks and lays each handler. */
fn adoptable(s: &lower::Section, code: &Code, excs: &[ExceptionFrame]) -> bool {
    excs.is_empty() || s.held != u16::MAX && excs.iter().all(|f| handler_in(s, code, f.handler_ip).is_some())
}

#[inline]
fn handler_at(code: &Code, h: usize) -> usize { if h & RAW_HANDLER != 0 { h & !RAW_HANDLER } else { code.lowered(h) } }

/* The section of the generator loop headed at `site`, when not `s` and laid to return past a yielded call's result, the call `s` returned past. */
fn tail_of<'c>(code: &'c Code, s: &lower::Section, site: usize) -> Option<&'c lower::Section> {
    if !lower::section_call(code.ins.get(site)?.op) { return None; }
    let g = code.block.as_deref()?.sections.get(code.ins.get(site + 2)?.c as usize)?;
    (g.tail.is_some() && !core::ptr::eq(g, s)).then_some(g)
}

/* The section of `code` that holds index `at`. */
fn section_at(code: &Code, at: usize) -> Option<&lower::Section> { code.block.as_deref()?.holding(at).map(|h| h.0) }

/* How far the second depth a section's register of noted depths packs lies up. */
const HELD_SHIFT: usize = 23;

/* The state of the builtin iterator `it`, None for any other value. */
#[inline(always)]
fn iter_cell(heap: &HeapPool, it: Val) -> Option<*const RefCell<IterFrame>> {
    match heap.try_get(it) { Some(HeapObj::Iter(g, _)) => Some(Rc::as_ptr(g)), _ => None }
}

/* Whether the register loop runs `op`, never under coverage so every ip is marked. */
#[inline(always)]
fn in_regs(op: OpCode) -> bool {
    !cfg!(feature = "coverage") && (op.is_register() || aside_op(op) || matches!(op, OpCode::Jump | OpCode::PopTop | OpCode::Call | OpCode::CallMethod | OpCode::LoadName | OpCode::StoreName | OpCode::LoadConst | OpCode::ForIter))
}

/* What a call site holds, as far as the register loop's call needs to know. */
#[derive(Clone, Copy)]
enum Known { Call(Val, u32), Ctor(Val), Method, Other }

// The most times a code is laid again for the functions its calls met, past the two any code takes for class changes.

// What a head's turning down gives for a growth asked, the loop leaving at the head for its handler.
const GROW: usize = usize::MAX;

// The pairs of a head and a function one code asks a growth for at most.
const TRIED: usize = 256;

// The preempt count `note_alt` leaves when preemption is off, so the next back-edge leaves for its handler and no preemption follows.
const RELAY: usize = usize::MAX;

/* Notes a function `func` the stub call at `i` met beyond its head's, sixteen at most, so code laid again lays it, enough sending the next back-edge to the handler. */
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn note_alt(heap: &HeapPool, learned: &mut alloc::vec::Vec<Val>, body_to_fi: &crate::util::hash::FxHashMap<*const SSAChunk, usize>, functions: &[&(Vec<alloc::string::String>, SSAChunk, u16, u16)], preempt_left: &mut usize, code: &Code, chunk: &SSAChunk, i: usize, func: Val, known: bool) {
    // Only a function is noted, a class's call from a stub (its constructor laid for the class as it was) only counting toward laying the code again.
    if matches!(heap.try_get(func), Some(HeapObj::Class(..))) && is_stub(code, i) && code.misses.get() < lower::MISSES { miss(preempt_left, code); }
    if !matches!(heap.try_get(func), Some(HeapObj::Func(..))) { return; }
    note_met(heap, learned, body_to_fi, functions, preempt_left, code, chunk, i, func, known);
}

/* Notes the dunder `func` the op at `i` ran, inside a section under the function it was laid from, as `note_alt` notes a call's, so code laid again calls its section. */
#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn note_dunder(heap: &HeapPool, learned: &mut alloc::vec::Vec<Val>, body_to_fi: &crate::util::hash::FxHashMap<*const SSAChunk, usize>, functions: &[&(Vec<alloc::string::String>, SSAChunk, u16, u16)], preempt_left: &mut usize, code: &Code, chunk: &SSAChunk, i: usize, func: Val) {
    // One outside the sections asks for a head of its own as it runs again.
    if let Some(b) = code.block.as_deref() && b.plain_at(i) && !code.unchanged() && !b.grown().refuses(i as u32, func.0) { b.grown().met(i as u32); }
    note_met(heap, learned, body_to_fi, functions, preempt_left, code, chunk, i, func, false);
}

/* Counts a call the code's lowering did not lay toward laying it again, as `note_met` does. */
#[cold]
#[inline(never)]
fn miss(preempt_left: &mut usize, code: &Code) {
    code.misses.set(code.misses.get() + 1);
    if code.misses.get() == lower::MISSES && *preempt_left == 0 { *preempt_left = RELAY; }
}

/* `note_alt` past where the call lies. */
#[allow(clippy::too_many_arguments)]
fn note_met(heap: &HeapPool, learned: &mut alloc::vec::Vec<Val>, body_to_fi: &crate::util::hash::FxHashMap<*const SSAChunk, usize>, functions: &[&(Vec<alloc::string::String>, SSAChunk, u16, u16)], preempt_left: &mut usize, code: &Code, chunk: &SSAChunk, i: usize, func: Val, known: bool) {
    // A comprehension is made again at each pass and a call of the very body the code runs is a recursion the tier lays as it reaches it, so neither is noted.
    match heap.try_get(func) { Some(&HeapObj::Func(fi, ..)) if !functions.get(fi).is_some_and(|f| super::scope::is_comp(&f.0) || core::ptr::eq(&f.1, chunk)) => {}, _ => return }
    if code.misses.get() >= lower::MISSES { return; }
    // A stub's call is its head's, which notes what it met as it asks the code to grow, and so is an op's in its stub, whose last head asks for its dunder.
    if code.block.is_some() && code.ins.get(i + 1).is_some_and(|s| s.b == lower::STUB_STORE || s.op == OpCode::ArgsR || s.op == OpCode::Jump) && asked_by_head(code, i) { return; }
    // So is one that asked for a head of its own for the function, which the growth laid or found it could not.
    if let Some(g) = code.block.as_deref().and_then(|b| b.grown.get()) && let Some(&HeapObj::Func(fi, ..)) = heap.try_get(func) && g.tried.borrow().binary_search(&(i as u32, fi as u64 | 1 << 63)).is_ok() { return; }
    // A call inside a section lies in the body of the function it was laid from, one outside them where the code laid again finds it.
    let (key, at) = match code.block.as_deref().and_then(|b| b.holding(i)) {
        Some((s, i)) => (s.fi as i64, s.orig[i - s.start as usize]),
        None if known => return miss(preempt_left, code),
        None => (VM::code_key_of(chunk, body_to_fi.get(&(chunk as *const SSAChunk)).copied()), code.orig(i)),
    };
    // One noted before the lowering was laid as far as it could be, and one past the most a call notes adds nothing, so code laid again lays nothing new for either.
    let seen = learned.chunks(3).enumerate().filter(|(_, t)| t[0].as_int() == key && t[1].as_int() == at as i64);
    if let Some((k, _)) = seen.clone().find(|(_, t)| t[2].0 == func.0) {
        if k >= code.read.get() as usize { miss(preempt_left, code); }
        return;
    }
    if seen.count() >= 16 { return; }
    miss(preempt_left, code);
    learned.extend_from_slice(&[Val::int(key), Val::int(at as i64), func]);
}

/* The section a head's table of `mask + 1` slots from `start` lays for function `f`, the one laid for it or for its body, which a table keys by. */
#[inline(always)]
fn table_find(heap: &HeapPool, block: &lower::Block, start: usize, mask: usize, f: Val) -> Option<usize> {
    let find = |key: Val| {
        let mut slot = lower::alt_slot(key, mask);
        loop {
            // SAFETY a table holds `mask + 1` slots from `start`, at least one of them free.
            let (v, k) = unsafe { *block.alt_tab.get_unchecked(start + slot) };
            if v.0 == key.0 { return Some((k & !lower::DEFAULTS) as usize); }
            if k == u32::MAX { return None; }
            slot = (slot + 1) & mask;
        }
    };
    find(f).or_else(|| find(lower::with_defaults(f))).or_else(|| match heap.try_get(f) { Some(&HeapObj::Func(fi, ..)) => find(lower::body_key(fi)), _ => None })
}

/* The section the table of a method head starting at `start`, of size `mask` + 1, lays for `func`, `u32::MAX` for none. */
fn alt_of(block: &lower::Block, start: u32, mask: u32, func: Val) -> usize {
    if mask == u32::MAX { return u32::MAX as usize; }
    match alt_key(block, start, mask, func) { u32::MAX => alt_key(block, start, mask, lower::with_defaults(func)) as usize, k => k as usize }
}

/* The section the table finds for key `func`, `u32::MAX` for none. */
fn alt_key(block: &lower::Block, start: u32, mask: u32, func: Val) -> u32 {
    let mut slot = lower::alt_slot(func, mask as usize);
    loop {
        let Some(&(v, k)) = block.alt_tab.get(start as usize + slot) else { return u32::MAX };
        if k == u32::MAX { return k; }
        if v.0 == func.0 { return k & !lower::DEFAULTS; }
        slot = (slot + 1) & mask as usize;
    }
}

/* Copies the parameters past the second that a head's moves wrote for section `from` into section `to`, entered instead, and fills defaults past the `n` passed. */
#[inline(never)]
fn moved(r: &mut [Val], base: usize, from: &lower::Section, to: &lower::Section, n: usize) {
    let (first, at) = (base + to.link as usize + 1, base + from.link as usize + 1);
    for p in 2..n.min(to.arity as usize) { r[first + p] = r[at + p]; }
    // The parameters past those passed take the defaults of the function the section was laid from.
    let (arity, nd) = (to.arity as usize, to.defaults.len());
    for p in n.max(arity - nd)..arity { r[first + p] = to.defaults[p + nd - arity]; }
}


/* Whether the call or op at `i` of `code` lies in a head's stub, whose head asks for the growth what it meets wants. */
#[inline(never)]
fn asked_by_head(code: &Code, i: usize) -> bool {
    matches!(code.ins[i].op, OpCode::CallR | OpCode::CallGlobalR | OpCode::CallMethodR) && is_stub(code, i) || lower::dunder_op(code.ins[i].op) && dunder_run(code, i)
}

/* Whether the op at `i` of `code` is the one the stub of an op's dunder heads runs. */
fn dunder_run(code: &Code, i: usize) -> bool {
    // It goes back past its head, or past the inversion after it for a `!=` its head inverts.
    let Some(to) = code.ins.get(i + 1).filter(|j| j.op == OpCode::Jump && j.x == 1).map(|j| j.a as usize) else { return false };
    [lower::HEADER as usize, lower::HEADER as usize + 1].iter().filter_map(|&d| to.checked_sub(d))
        .any(|h| code.ins.get(h).is_some_and(|x| x.op == OpCode::DunderSectionR) && code.ins.get(h + 3).is_some_and(|s| s.op == OpCode::SectionR && s.x == lower::STUB && s.b as usize == i))
}

/* The head a section's call returns past to `ret`, the one right before it, or the one its block notes for a call laid past the end of code that grew. */
#[inline(always)]
pub(crate) fn head_of(code: &Code, ret: usize) -> Option<usize> {
    if let Some(b) = code.block.as_deref() && !b.displaced().is_empty() { return displaced_head(b, ret); }
    ret.checked_sub(lower::HEADER as usize)
}

/* `head_of` for a block that laid heads past the end of code that grew. */
#[inline(never)]
fn displaced_head(b: &lower::Block, ret: usize) -> Option<usize> {
    match b.displaced().binary_search_by_key(&(ret as u32), |d| d.0) { Ok(k) => Some(b.displaced()[k].1 as usize), Err(_) => ret.checked_sub(lower::HEADER as usize) }
}

/* The head whose stub makes the call at `i` of `code`. */
fn stub_head(code: &Code, i: usize) -> Option<usize> {
    if !is_stub(code, i) { return None; }
    let j = code.ins[i + 1..].iter().take(10).find(|j| j.op == OpCode::Jump && j.x == 1)?;
    let h = (j.a as usize).checked_sub(lower::HEADER as usize)?;
    code.ins.get(h + 3).is_some_and(|s| s.op == OpCode::SectionR && s.x == lower::STUB && s.b as usize == i).then_some(h)
}

fn is_stub(code: &Code, i: usize) -> bool {
    // A stub's call stores its result through a StoreTopR its `b` marks.
    let w = code.ins.get(i).is_some_and(|c| lower::wide(*c)) as usize;
    if !code.ins.get(i + 1 + w).is_some_and(|s| s.op == OpCode::StoreTopR && s.b == lower::STUB_STORE) { return false; }
    // Every stub lies past the code's own instructions, among or after its sections.
    if !code.block.as_deref().is_some_and(|b| b.sections.first().is_some_and(|s| i >= s.start as usize)) { return false; }
    let Some(j) = code.ins[i + 1..].iter().take(10).find(|j| j.op == OpCode::Jump && j.x == 1) else { return false };
    (j.a as usize).checked_sub(lower::HEADER as usize).and_then(|h| code.ins.get(h + 3)).is_some_and(|s| s.op == OpCode::SectionR && s.x == lower::STUB && s.b as usize == i)
}

/* Whether a call of `fi` with `n` positionals opens as a frame of the loop, as `VM::opens_in_loop` says, read off fields alone so the loop keeps its frame borrowed. */
#[inline(always)]
fn opens_in_loop(facts: &[super::act::CallFacts], is_async: &[bool], pools: &[super::cache::CachePool], fi: usize, n: usize) -> bool {
    let f = &facts[fi];
    f.in_loop && f.arity.is_some_and(|k| n <= k && n >= f.required as usize) && !is_async.get(fi).copied().unwrap_or(false)
        && pools[f.pool].code.is_some() && !pools[f.pool].cold && pools[f.pool].consts.is_some()
}

/* The `__enter__` and `__exit__` a `with` site learned for `cm`, while its class is the learned one, unchanged, and neither names `super` (which the handler calls bound). */
#[inline(always)]
fn with_func(heap: &HeapPool, class_epoch: u32, site: &Site, cm: Val) -> Option<(Val, Val)> {
    match *site {
        Site::Dunder { class, func, owner, epoch, arity: 0 } if epoch == class_epoch && matches!(heap.try_get(cm), Some(&HeapObj::Instance(c, _)) if c.0 == class.0) => Some((func, owner)),
        _ => None,
    }
}

/* What `v`'s items are when it is a tuple or list, `f` seeing them. */
#[inline(always)]
fn seq_items<R>(heap: &HeapPool, v: Val, f: impl FnOnce(&[Val]) -> R) -> Option<R> {
    match heap.try_get(v) {
        Some(HeapObj::Tuple(t)) => Some(f(t)),
        Some(HeapObj::List(l)) => Some(f(&l.borrow())),
        _ => None,
    }
}

impl<'a> VM<'a> {

    /* Runs a learned operator dunder, false with operands restored on a miss or `NotImplemented`, as a frame of the loop when the op at `rip` runs again per `rerun`. */
    #[inline]
    fn exec_dunder(&mut self, site: Site, rip: usize, rerun: super::redo::Rerun, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let Site::Dunder { class, func, owner, arity, epoch } = site else { return Ok(false) };
        let arity = arity as usize;
        let len = self.stack.len();
        // A dunder the register loop ran that declined leaves the op to its handler.
        if epoch != self.class_epoch || len < arity || !self.declined.is_undef() { return Ok(false); }
        let recv = self.stack[len - arity];
        if !matches!(self.heap.try_get(recv), Some(&HeapObj::Instance(c, _)) if c.0 == class.0) || arity == 2 && !self.dunder_holds(class, recv, self.stack[len - 1]) { return Ok(false); }
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        let operands = Args::of(&self.stack[len - arity..]);
        let armed = self.redo_late(rip, rerun, len, &operands);
        self.stack.truncate(len - arity);
        self.pending.method_binding = Some((owner, recv));
        self.push(func);
        self.stack.extend_from_slice(&operands);
        let called = self.exec_call(arity as u16, chunk);
        self.redo_done(armed);
        called?;
        let result = self.pop()?;
        if self.heap.is_not_implemented(result) {
            // The slow handler sees its operands unchanged and tries the reflected dunder, passing over this one, which declined.
            self.stack.extend_from_slice(&operands);
            self.declined = func;
            return Ok(false);
        }
        self.push(result);
        Ok(true)
    }

    /* Opens the dunder learned for the instance under the stack's top as the next frame, operands left for the op to rerun on a decline, false when the handler takes it. */
    #[inline(never)]
    fn open_dunder(&mut self, rip: usize, site: Site) -> Result<bool, VmErr> {
        if self.redo_replays() { return Ok(false); }
        let Site::Dunder { class, func, owner, arity: 2, epoch } = site else { return Ok(false) };
        let len = self.stack.len();
        if epoch != self.class_epoch || len < 2 || !self.declined.is_undef() || !matches!(self.heap.try_get(func), Some(HeapObj::Func(..))) { return Ok(false); }
        let (a, b) = (self.stack[len - 2], self.stack[len - 1]);
        if !self.dunder_holds(class, a, b) { return Ok(false); }
        self.pending.method_binding = Some((owner, a));
        if !self.open_in_loop(func, Passed::Stack { first: len - 2, to: len }, 2)? { self.pending.method_binding = None; return Ok(false); }
        self.mark_dunder(rip, super::act::NO_DEST);
        Ok(true)
    }

    /* Opens the dunder register op `ins` at `rip` learned for its left instance as the next frame, its result going to the op's register, false when the handler takes it. */
    #[inline(never)]
    pub(crate) fn open_reg_dunder(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache) -> Result<bool, VmErr> {
        if self.redo_replays() { return Ok(false); }
        let site = cache.site(rip);
        // An op meeting an instance for the first time learns its dunder first, as a stack op does.
        if matches!(site, Site::Empty) { return self.first_reg_dunder(ins, rip, cache); }
        let Site::Dunder { class, func, owner, arity: 2, epoch } = site else { return Ok(false) };
        let (a, b) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
        if epoch != self.class_epoch || b.is_undef() || !self.declined.is_undef() || !matches!(self.heap.try_get(func), Some(HeapObj::Func(..))) { return Ok(false); }
        if !self.dunder_holds(class, a, b) { return Ok(false); }
        self.pending.method_binding = Some((owner, a));
        if !self.open_in_loop(func, Passed::Regs([a, b, Val::undef()]), 2)? { self.pending.method_binding = None; return Ok(false); }
        self.mark_dunder(rip, ins.a);
        Ok(true)
    }

    /* Opens property getter or setter `func` with its `n` operands as the next frame, past the load or store at `rip` into `dest`, false when that op calls it itself. */
    #[cold]
    #[inline(never)]
    pub(crate) fn open_prop(&mut self, func: Val, passed: Passed, n: usize, rip: usize, dest: u16) -> Result<bool, VmErr> {
        if self.redo_replays() { return Ok(false); }
        if !self.open_in_loop(func, passed, n)? { return Ok(false); }
        self.mark_dunder(rip, dest);
        Ok(true)
    }

    /* `open_dunder` for a stack op inside a call whose site learned nothing, learning the left instance's dunder first (outside calls the handler is cheaper). */
    #[cold]
    #[inline(never)]
    fn open_first(&mut self, opcode: OpCode, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<bool, VmErr> {
        let n = self.stack.len();
        if n < 2 { return Ok(false); }
        let Some(site) = self.first_dunder(opcode, self.stack[n - 2], matches!(opcode, OpCode::InPlaceAdd | OpCode::InPlaceSub), rip, cache) else { return Ok(false) };
        if self.open_dunder(rip, site)? { return Ok(true); }
        // One whose body never ran is called with the dunder found and looked up no more, one that declined is passed over by the handler for the reflected one.
        self.exec_dunder(site, rip, super::redo::Rerun::Stack, chunk)
    }

    /* `open_reg_dunder` for an op inside a call whose site learned nothing yet, learning the left instance's dunder first. */
    #[cold]
    #[inline(never)]
    fn first_reg_dunder(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache) -> Result<bool, VmErr> {
        let a = self.regs[self.base + ins.b as usize];
        if self.pools[self.pool_now].func == super::cache::NO_FN { return Ok(false); }
        if self.first_dunder(lower::stack_form(ins.op), a, ins.x & lower::AUGMENTED != 0, rip, cache).is_none() { return Ok(false); }
        self.open_reg_dunder(ins, rip, cache)
    }

    /* The site a load, or a store for `set`, of attribute `name` keeps when instance `o`'s class makes it a property of a plain getter or setter, called as a dunder is. */
    fn learn_property(&self, o: Val, name: &str, set: bool) -> Option<Site> {
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(o) else { return None };
        let (func, owner) = self.property_of(class, name, set)?;
        Some(Site::Dunder { class, func, owner, arity: 1 + set as u8, epoch: self.class_epoch })
    }

    /* The site a load of attribute `name` of instance `o` keeps when neither it nor its class holds the name and the class's `__getattr__` answers it. */
    fn learn_getattr(&self, o: Val, name: &str) -> Option<Site> {
        let Some(&HeapObj::Instance(class, ref attrs)) = self.heap.try_get(o) else { return None };
        let name = crate::parser::ssa_strip(name);
        if self.lookup_class_member(class, name).is_some() || attrs.borrow().position_str(name, crate::value::hash_str(name), &self.heap).is_some() { return None; }
        let (func, owner) = self.getattr_of(class, name)?;
        Some(Site::Dunder { class, func, owner, arity: 2, epoch: self.class_epoch })
    }

    /* The `__getattr__` instances of `class` fall back to for `name`, a plain function, and the class it lies in. */
    pub(crate) fn getattr_of(&self, class: Val, name: &str) -> Option<(Val, Val)> {
        if self.lookup_class_member(class, name).is_some() { return None; }
        let (f, owner) = self.lookup_class_member(class, "__getattr__")?;
        matches!(self.heap.try_get(f), Some(HeapObj::Func(..))).then_some((f, owner))
    }

    /* The getter, or setter for `set`, of the property `name` instances of `class` find, and the class it lies in, when a plain function. */
    pub(crate) fn property_of(&self, class: Val, name: &str, set: bool) -> Option<(Val, Val)> {
        let (m, owner) = self.lookup_class_member(class, crate::parser::ssa_strip(name))?;
        let Some(&HeapObj::Property(get, put, _)) = self.heap.try_get(m) else { return None };
        let f = if set { put } else { get };
        matches!(self.heap.try_get(f), Some(HeapObj::Func(..))).then_some((f, owner))
    }

    /* Learns the dunder a truth test of instance `v` at `rip` runs, its `__bool__` or else its `__len__`. */
    fn learn_truth(&mut self, rip: usize, cache: &OpcodeCache, code: &Code, chunk: &SSAChunk, v: Val) {
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(v) else { return };
        let name = if self.lookup_class_member(class, "__bool__").is_some() { "__bool__" } else { "__len__" };
        self.learn_dunder(rip, cache, code, chunk, v, name, 1);
    }

    /* What a length, truth or string test takes from the value `v` its dunder on `o` gave, kind `x` as DunderCheckR says, or the error for a value of the wrong kind. */
    #[cold]
    #[inline(never)]
    fn dunder_checked(&mut self, x: u8, v: Val, o: Val) -> Result<Val, VmErr> {
        if x >= 4 { return self.require_str(v, if x == 4 { "__str__" } else { "__repr__" }).map(|_| v); }
        if x == 2 { return if v.is_bool() { Ok(v) } else { Err(VmErr::TypeMsg(crate::s!("__bool__ should return bool, returned ", str self.type_name(v)))) }; }
        // A NotImplemented is no dunder's answer, the builtin's then.
        if self.heap.is_not_implemented(v) { return if x == 1 { self.builtin_len(o).and_then(|n| self.int_to_val(Some(n))) } else { Ok(Val::bool(true)) }; }
        let n = if v.is_int() { v.as_int() as i128 } else if let Some(i) = super::types::as_i128(v, &self.heap) { i } else { return Err(cold_type("__len__ must return int")) };
        if n < 0 { return Err(cold_value("__len__() should return >= 0")); }
        self.int_to_val(Some(n))
    }

    /* Learns the dunder `name` of `arity` operands an instance `recv` answers the op at `rip` with, noted for the code laid again, any other receiver costing only a look. */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn learn_dunder(&mut self, rip: usize, cache: &OpcodeCache, code: &Code, chunk: &SSAChunk, recv: Val, name: &str, arity: u8) {
        if let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(recv) { self.learn_instance_dunder(rip, cache, code, chunk, class, name, arity); }
    }

    /* `learn_dunder` of an instance of `class`. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn learn_instance_dunder(&mut self, rip: usize, cache: &OpcodeCache, code: &Code, chunk: &SSAChunk, class: Val, name: &str, arity: u8) {
        self.record_class_dunder(rip, cache, class, name, arity);
        if !code.unchanged() && let Site::Dunder { func, .. } = cache.site(rip) { note_dunder(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, rip, func); }
    }

    /* Learns the operator dunder an instance resolved at `ip`, nothing for any other receiver. */
    pub(crate) fn record_dunder_hit(&self, ip: usize, cache: &OpcodeCache, recv: Val, name: &str, arity: u8) {
        if let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(recv) { self.record_class_dunder(ip, cache, class, name, arity); }
    }

    /* `record_dunder_hit` of an instance of `class`. */
    fn record_class_dunder(&self, ip: usize, cache: &OpcodeCache, class: Val, name: &str, arity: u8) {
        // A site that learned this class at this epoch already holds what the lookup finds.
        if cache.with_site(ip, |s| matches!(*s, Site::Dunder { class: c, arity: a, epoch, .. } if c.0 == class.0 && a == arity && epoch == self.class_epoch)) { return; }
        let Some((func, owner)) = self.lookup_class_member(class, name) else { return };
        cache.set_site(ip, Site::Dunder { class, func, owner, arity, epoch: self.class_epoch });
    }

    /* Runs `chunk` on `frame` copied into its window, and copies the window back after. */
    pub(crate) fn exec(&mut self, chunk: &SSAChunk, frame: &mut Vec<Val>) -> Result<Val, VmErr> { self.exec_kept(chunk, frame, true) }

    /* `exec`, the window copied back into `frame` only when `back` or the run suspended. */
    fn exec_kept(&mut self, chunk: &SSAChunk, frame: &mut Vec<Val>, back: bool) -> Result<Val, VmErr> {
        let pool = self.pool_of(chunk);
        self.exec_pooled(chunk, pool, frame, back)
    }

    /* `exec_kept` in `pool`, the pool of `chunk`. */
    pub(crate) fn exec_pooled(&mut self, chunk: &SSAChunk, pool: usize, frame: &mut Vec<Val>, back: bool) -> Result<Val, VmErr> {
        // A coroutine resumed in the window that still holds its state runs on in it as it lies.
        let co = core::mem::replace(&mut self.resuming, Val::undef());
        let co = if co.is_undef() { co } else { match self.heap.try_get(co) { Some(HeapObj::Coroutine(c)) if matches!(c.body, crate::value::BodyRef::Fn(fi) if self.fn_pool[fi] == pool) => co, _ => Val::undef() } };
        let kept = self.resumes_in(pool, self.pools[pool].active as usize, co);
        // One a frame of the loop left in a window of another level takes its state back from there.
        if !kept && !co.is_undef() { self.evict_into(co, frame); }
        let fresh = !kept && self.pools[pool].active == 0 && self.pools[pool].w0.base == super::window::NO_WINDOW;
        let base = if fresh { self.first_window(pool, frame, chunk.names.len()) } else { self.open_window(pool, frame.len().max(chunk.names.len()), chunk.names.len(), false) };
        if !kept && !fresh {
            self.regs[base..base + frame.len()].copy_from_slice(frame);
            // A frame copied out of this window as long as it holds its constants as they were laid, any other takes them again.
            if frame.len() != self.pools[pool].wlen {
                self.pools[pool].wcode = 0;
                self.note_window(pool);
            }
        }
        let result = self.exec_in(chunk, base, pool);
        frame.clear();
        // A coroutine that suspended at the first level leaves its state in the window, which it owns until another run takes it.
        if !co.is_undef() && self.yielded && result.is_ok() {
            let level = self.pools[pool].active as usize - 1;
            self.close_window(pool);
            self.own(pool, level, co);
            return result;
        }
        if back || self.yielded {
            let (at, len) = (self.pools[pool].window, self.pools[pool].wlen);
            frame.extend_from_slice(&self.regs[at..at + len]);
        }
        self.close_window(pool);
        result
    }

    /* The index of `chunk`'s pool, made empty on its first run. */
    pub(crate) fn pool_of(&mut self, chunk: &SSAChunk) -> usize {
        let key = chunk as *const SSAChunk;
        if let Some(&p) = self.pool_ids.get(&key) { return p; }
        self.pools.push(Default::default());
        self.pool_ids.insert(key, self.pools.len() - 1);
        self.pools.len() - 1
    }

    /* Runs the frame at `base` of the register stack, its registers the top of it. */
    pub(crate) fn exec_in(&mut self, chunk: &SSAChunk, base: usize, pool: usize) -> Result<Val, VmErr> {
        // Each loop nests in Rust, so the host stack bounds how many run at once, whatever started them.
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        let outer = core::mem::replace(&mut self.pool_now, pool);
        self.base = base;
        self.depth += 1;
        #[cfg(any(debug_assertions, feature = "coverage"))]
        self.loops.open(self.hosted != 0);
        let result = self.exec_frame(chunk, pool);
        #[cfg(any(debug_assertions, feature = "coverage"))]
        self.loops.close();
        self.depth -= 1;
        self.run_in(outer);
        result
    }


    /* Where a section call the loop gave way on carries on, in a body one of its functions enters, else at its stub. */
    #[inline(never)]
    fn section_turned(&mut self, code: &Code, chunk: &SSAChunk, rip: usize) -> usize {
        self.enter_section_of_body(code, rip).or_else(|| self.enter_super(code, chunk, rip)).or_else(|| self.enter_spread(code, rip)).unwrap_or(code.ins[rip + 3].a as usize)
    }

    /* Lays `code` in the window of `pool` unless it lies there, out of line for the loop, which meets it only as frames change. */
    #[inline(never)]
    pub(crate) fn lay(&mut self, chunk: &SSAChunk, code: &Code, pool: usize) {
        if !self.laid(code, pool) { self.grow_frame(chunk, code, pool); }
    }

    /* `set_ret`, `deliver`, `top_linked` and `top` out of line, for the loop paths that end jobs, folds, generators and parked or failed runs. */
    #[inline(never)]
    fn set_ret_apart(&mut self, ret: usize, dest: u16) { self.set_ret(ret, dest) }

    #[inline(never)]
    fn deliver_apart(&mut self, v: Val, ret: usize, dest: u16) -> usize { self.deliver(v, ret, dest) }

    #[inline(never)]
    fn top_linked_apart(&self) -> bool { self.top_linked() }

    #[inline(never)]
    fn top_apart(&self) -> Option<super::act::Act> { self.top() }

    /* `run_of` out of line, so the loop that takes it so often stays small. */
    #[inline(never)]
    fn load_run(&mut self, floor: &Floor) -> (*const Code, *const SSAChunk, usize, usize) { self.run_of(floor) }

    /* Makes the running frame the one running, its code, chunk, pool and exception base, or the loop's own `floor` once no frame runs above it. */
    #[inline(always)]
    fn run_of(&mut self, floor: &Floor) -> (*const Code, *const SSAChunk, usize, usize) {
        if self.acts > floor.depth {
            let (code, fi, top, b) = self.top_run();
            self.run_in(top);
            (code, &self.functions[fi].1 as *const SSAChunk, top, b.exc)
        } else {
            self.run_in(floor.pool);
            (floor.code, floor.chunk, floor.pool, floor.bases[0])
        }
    }

    /* The unwind and spread-delta stack lengths the running frame began at, or the loop's own `floor`'s. */
    #[inline(never)]
    fn run_bases(&self, floor: &Floor) -> (usize, usize) {
        if self.acts > floor.depth { let b = self.top_run().3; (b.unwind, b.delta) } else { (floor.bases[1], floor.bases[2]) }
    }
    /* Runs the chunk's lowered code, register forms and stack opcodes alike, and each plain call it makes as a frame of this same loop. */
    fn exec_frame(&mut self, chunk: &SSAChunk, pool: usize) -> Result<Val, VmErr> {
        // `resume_coroutine` pre-pushes restored exception frames before calling us. Honor its override so dispatch's handler search includes them.
        let mut exc_base = self.pending_exec_exc_base.take().unwrap_or(self.exception_stack.len());
        let outer_safe = core::mem::replace(&mut self.frame_safe, core::mem::take(&mut self.pending_exec_safe));
        // Cleanup reasons belong to this frame's finally bodies, drop leftovers when it returns.
        let mut unwind_base = self.unwind_stack.len();
        // Drop spread-delta frames leaked by an aborted arg list on unwind.
        let mut delta_base = self.pending.delta_save.len();
        // The run of text exec or eval hands this loop takes its chunk as the floor, the one under it set aside until it ends.
        let (mut pool, floors0) = (pool, self.floors.len());

        let (mut floor_code, mut once) = self.frame_code(chunk, pool);
        // Constants become values on the chunk's first run, strings in the live heap.
        if self.pools[pool].consts.is_none() {
            match super::cache::const_vals(chunk, &mut self.heap) {
                Ok(consts) => self.pools[pool].consts = Some(consts),
                Err(e) => { self.frame_safe = outer_safe; return Err(e); }
            }
        }
        self.lay(chunk, &floor_code, pool);
        // The frames this loop opens sit above `floor`, and the running one is the innermost of them or the frame it began with.
        let (mut floor, mut floor_chunk) = (self.acts, chunk as *const SSAChunk);
        let (mut cur, mut at, mut now, mut exc) = (Rc::as_ptr(&floor_code), floor_chunk, pool, exc_base);
        // Where the frames this loop opens return to last, made once since only a lowering changes it.
        let mut floor_at = Floor { depth: floor, code: Rc::as_ptr(&floor_code), chunk: floor_chunk, pool, bases: [exc_base, unwind_base, delta_base] };
        // Takes the running frame from the top of the frames, or this loop's own once none is left.
        macro_rules! load {
            () => {
                (cur, at, now, exc) = self.load_run(&floor_at);
            };
            // In place, where frames enter and return and the scheduler switches its coroutines, on every step.
            (hot) => {
                (cur, at, now, exc) = self.run_of(&floor_at);
            };
        }
        // SAFETY the running code is held by `floor_code` or by its frame, which outlive every use below.
        let mut ip = if self.resume_ip & RESUME_RAW != 0 { self.resume_ip & !RESUME_RAW } else { unsafe { &*cur }.lowered(self.resume_ip) };
        self.resume_ip = 0;
        // A coroutine waiting inside helpers carries on in the innermost, the others opening as each returns.
        if core::mem::take(&mut self.chain.next) { ip = self.chain_next(self.chain.coro, ip, None); load!(); }
        // One-shot raise at the coroutine's park point, a cancellation or an error delivered while parked.
        let mut raised = self.resume_raise.take();
        // The call a raise came back from, where it meets the frame that made it.
        let mut raised_at = None;
        // A callee that suspended suspends its caller too, at the instruction after the call.
        let mut parking = false;
        // Trades this loop's floor for the one `f` keeps.
        macro_rules! swap_floor {
            ($f:expr) => {
                let f = &mut $f;
                if let Some(code) = &mut f.code { core::mem::swap(&mut floor_code, code); }
                core::mem::swap(&mut once, &mut f.once);
                core::mem::swap(&mut floor_chunk, &mut f.chunk);
                core::mem::swap(&mut pool, &mut f.pool);
                core::mem::swap(&mut floor, &mut f.depth);
                core::mem::swap(&mut self.frame_safe, &mut f.safe);
                let b = [exc_base, unwind_base, delta_base];
                [exc_base, unwind_base, delta_base] = f.bases;
                f.bases = b;
            };
        }
        // Ends the run of text whose chunk is the floor with `r` (suspended when `y`) as the next turn starts, carrying on or raising in the call that ran it.
        let mut ending: Option<(Result<Val, VmErr>, bool)> = None;
        macro_rules! floor_end {
            ($r:expr, $y:expr) => {
                (ending, parking) = (Some(($r, $y)), true);
                continue;
            };
        }
        // Takes the floor on top of the floors set aside as this loop's own, `rip` carrying on at `ip` once it ends, or raising there when it cannot open.
        macro_rules! floor_begin {
            ($rip:expr) => {
                let rip = $rip;
                match self.floor_open() {
                    Ok(()) => {
                        if let Some(f) = self.floors.last_mut() {
                            (f.ip, f.rip) = (ip, rip);
                            swap_floor!(*f);
                            floor_at = Floor { depth: floor, code: Rc::as_ptr(&floor_code), chunk: floor_chunk, pool, bases: [exc_base, unwind_base, delta_base] };
                            load!();
                            // SAFETY as above.
                            ip = unsafe { &*cur }.lowered(0);
                        }
                    }
                    Err(e) => if let Err(e) = self.floor_end(Err(e), false, None) { (raised, raised_at) = (Some(e), Some(rip)); },
                }
            };
        }
        // Where the loop carries on once the job of a builtin whose calls run as frames went as `$went` with frame `f` ended, in its next call, past it, or raising.
        macro_rules! calls {
            ($went:expr, $f:expr) => {{
                let f = $f;
                match $went {
                    Ok(None) => {
                        (self.entered, self.yielded) = (false, false);
                        self.set_ret_apart(f.ret, f.dest);
                        load!();
                        // SAFETY as above.
                        let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                        self.lay(chunk, code_ref, now);
                        ip = code_ref.lowered(0);
                    }
                    // A delegation's step goes on through the waits it ended, as a `Then` job's generator does.
                    Ok(Some(_)) if let Some((base, step)) = self.then_out.take() => {
                        let went = self.then_end(base as usize, step, f.ret);
                        load!();
                        // SAFETY as above.
                        let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                        self.lay(chunk, code_ref, now);
                        match went { Ok(i) => ip = i, Err((at, e)) => { raised = Some(e); raised_at = Some(at); ip = at; } }
                    }
                    Ok(Some(v)) => { load!(); ip = self.deliver_apart(v, f.ret, f.dest); }
                    Err(e) => { load!(); raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                }
            }};
        }
        macro_rules! after {
            ($drive:expr, $ret:expr, $dest:expr) => {{
                let ret = $ret;
                let next = match $drive { Ok(d) => Ok(self.after_drive(d, ret, $dest)), Err(e) => Err(e) };
                load!();
                match next { Ok(i) => ip = i, Err(e) => { raised = Some(e); raised_at = Some(ret - 1); ip = ret - 1; } }
                parking |= self.floor_req;
            }};
        }
        macro_rules! prefetched {
            ($ret:expr, $drive:expr) => {{
                let ret = $ret;
                if !$drive { ip = ret - 1; continue; }
                after!(self.gather_drive(), ret, super::act::NO_DEST);
                continue;
            }};
        }
        let result = loop {
            let turn = {
                // What the turn before left to finish, a suspension to pass on or an error to raise here.
                let mut carried = if parking {
                    if let Some((r, y)) = ending.take() {
                        let Some(f) = self.floors.last_mut() else { break Err(cold_runtime("a floor ended with none set aside")) };
                        swap_floor!(*f);
                        let (back, rip) = (f.ip, f.rip);
                        floor_at = Floor { depth: floor, code: Rc::as_ptr(&floor_code), chunk: floor_chunk, pool, bases: [exc_base, unwind_base, delta_base] };
                        self.floor_close();
                        load!();
                        (ip, parking, raised_at) = (back, false, None);
                        if y { (self.yielded, self.resume_ip, self.resume_layout) = (false, 0, 0); }
                        self.entered = false;
                        match self.floor_end(r, y, Some((back, unsafe { &*cur }))) {
                            // A run a builtin's callback called hands it what it gave, the loop carrying on where it says, parked when that calls for the next run.
                            Ok(super::globals::compile::Ended::At(i)) => { load!(); (ip, parking) = (i, self.floor_req); }
                            // A class whose hooks run as frames of the loop takes its value from their job, the first frame running next.
                            Ok(super::globals::compile::Ended::Value(_)) if core::mem::take(&mut self.entered) => {
                                self.yielded = false;
                                self.set_ret_apart(ip, super::act::NO_DEST);
                                load!();
                                // SAFETY as above.
                                let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                                self.lay(chunk, code_ref, now);
                                ip = code_ref.lowered(0);
                            }
                            // A module a run of text imports hands the loop the next one, or the text, which carries on where this one did.
                            Ok(_) if self.floor_pending(floors0) => { floor_begin!(rip); }
                            Ok(super::globals::compile::Ended::Value(v)) => self.push(v),
                            Err(e) => (raised, raised_at) = (Some(e), Some(rip)),
                        }
                        continue;
                    }
                    Some(Ok(None))
                } else { raised.take().map(Err) };
                // SAFETY as above, and a frame's chunk outlives this loop, the first is its caller's and every callee lives as long as the VM.
                let (mut code, mut chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                let (mut exc_base, mut pool) = (exc, now);
                let mut cache = &code.cache;
                let mut n = code.ins.len();
                loop {
                    let mut rip = ip;
                    let step = if let Some(step) = carried.take() {
                        if step.is_err() && let Some(r) = raised_at.take() { rip = r; }
                        // A run of text a builtin's step called for as the last turn ended opens as this turn's floor, rising from the call, the loop parked meanwhile.
                        if self.floor_req { rip = ip.saturating_sub(1); }
                        step
                    } else {
                        // Only what the register loop runs enters it, and a call or return it ran in place can leave another frame running.
                        if code.ins.get(ip).is_some_and(|i| in_regs(i.op)) && self.run_frames(code, chunk, &mut ip, &floor_at) {
                            load!(hot);
                            // SAFETY as above.
                            (code, chunk) = unsafe { (&*cur, &*at) };
                            (cache, n, exc_base, pool) = (&code.cache, code.ins.len(), exc, now);
                        }
                        rip = ip;
                        // A call the loop made can fail or leave its callee parked.
                        if let Some(e) = self.reg_error.take() { Err(e) } else if self.yielded { Ok(None) } else if ip >= n { Ok(Some(Val::none())) } else {
                            let ins = code.ins[ip];
                            ip += 1;
                            #[cfg(feature = "coverage")]
                            { let at = code.orig(rip) as usize; self.executed.entry(chunk as *const _).or_insert_with(|| alloc::vec![0; chunk.instructions.len().div_ceil(64)])[at / 64] |= 1 << (at % 64); }
                            match ins.op {
                                // A return with no cleanup to run leaves at once.
                                OpCode::ReturnR if self.exception_stack.len() <= exc_base && !self.regs[self.base + ins.b as usize].is_undef() => Ok(Some(self.regs[self.base + ins.b as usize])),
                                // A plain call skips the fused-builtin checks, its operand is only counts.
                                OpCode::Call => self.call_stacked(ins.a, code.orig(rip), chunk, (rip, super::redo::Rerun::Stack)).map(|()| None),
                                // A section call the loop gave way on runs its stub as an ordinary call, unless a function of its body enters it, made here.
                                OpCode::CallSectionR | OpCode::CallGlobalSectionR | OpCode::CallMethodSectionR | OpCode::CallValueSectionR | OpCode::CallCtorSectionR | OpCode::CallMethodGlobalSectionR => { ip = self.section_turned(code, chunk, rip); Ok(None) }
                                // An op's section call the loop gave way on, its budget spent, runs its stub, the op itself.
                                OpCode::DunderSectionR => { ip = self.dunder_turned(code, rip); Ok(None) }
                                // An instruction of a section names what it names in the chunk of the function it was laid from.
                                op if op.is_register() => { let chunk = self.chunk_at(code, chunk, rip); self.exec_reg(ins, rip, chunk, cache, code, &mut ip, exc_base) }
                                // Stack work inside a section names what it names in the chunk of the function it was laid from.
                                _ => {
                                    let chunk = if code.block.is_some() { self.chunk_at(code, chunk, rip) } else { chunk };
                                    self.dispatch(ins, rip, chunk, cache, code, pool, &mut ip, exc_base)
                                }
                            }
                        }
                    };
                    // An instruction pays for the native work it did, loop or not.
                    let step = if self.heap.has_work() && let Err(e) = self.pay_work() { Err(e) } else { step };
                    match step {
                        Ok(None) if !self.yielded => continue,
                        // A floor a builtin's step called for opens no frame, whatever the step took it for.
                        Ok(None) if self.floor_req => { (self.floor_req, self.yielded, self.entered, parking) = (false, false, false, false); break Turn::Floor(rip) }
                        Ok(None) if core::mem::take(&mut self.entered) || core::mem::take(&mut self.thrown) => { self.yielded = false; break Turn::Enter }
                        // A head that asked its code to grow runs again in the grown code.
                        Ok(None) if self.growing.is_some() => { self.yielded = false; break Turn::Grow }
                        // A hot loop hands its frame over at the loop head.
                        Ok(None) if self.tier_up => { self.yielded = false; self.tier_up = false; break Turn::Lower }
                        Ok(None) => { parking = false; break Turn::Yield(self.yield_out(code, ip)) }
                        Ok(Some(v)) => {
                            self.exception_stack.truncate(exc_base);
                            if !self.unwind_stack.is_empty() { self.unwind_stack.truncate(self.run_bases(&floor_at).0); }
                            break Turn::Return(v)
                        }
                        // A StopIteration out of a user iterator's `__next__` a `for` laid as a section ends that loop.
                        Err(e) if code.block.is_some() && is_stop(&e) && let Some(exit) = self.next_stop(code, rip) => { ip = exit; continue }
                        // An op's handler handed its call to the loop, whose frame runs next.
                        Err(e) => {
                            if matches!(e, VmErr::Redo) { (self.yielded, self.entered) = (false, false); break Turn::Enter }
                            if code.block.is_some() { self.section_error(code, chunk, rip); }
                            match self.catch(code, chunk, &mut ip, exc_base, self.run_bases(&floor_at).1, e, rip) {
                                Ok(()) => continue,
                                Err(e) => break Turn::Raise(e),
                            }
                        }
                    }
                }
            };
            match turn {
                Turn::Floor(rip) => { floor_begin!(rip); }
                Turn::Enter => {
                    // A generator frame returns to the `for` that stepped it and resumes where it last yielded.
                    let generator = self.top().is_some_and(|f| !f.generator.is_undef());
                    // A dunder's frame carries on at its op, which its opening noted.
                    if let Some(f) = self.top() && !generator && !f.dunder { self.set_ret(ip, f.dest); }
                    // A generator a builtin runs returns past the builtin's call.
                    if generator && self.top_gather() { self.gather_ret(ip); }
                    load!(hot);
                    // SAFETY as above.
                    let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                    if !self.laid(code_ref, now) { self.grow_frame(chunk, code_ref, now); }
                    ip = if generator { self.gen_entry } else { code_ref.lowered(0) };
                    // A generator thrown into raises where it carries on, as a resume raises at its park point.
                    if generator && self.resume_raise.is_some() { (raised, raised_at) = (self.resume_raise.take(), None); }
                    // One waiting inside helpers opens them over its frame, the innermost carrying on.
                    if generator && self.chain.next { self.chain.next = false; ip = self.chain_next(self.chain.coro, ip, None); load!(); }
                }
                Turn::Grow => {
                    // SAFETY as above.
                    let (chunk, pool, old): (&SSAChunk, usize, &Code) = (unsafe { &*at }, now, unsafe { &*cur });
                    let Some(&(asked, head, kind, func)) = self.growing.take().as_deref() else { continue };
                    // The pool's newest code of the same lineage keeps every place of the frame's, which carries on in it, the frame's own code growing when it is the newest.
                    let lineage = |c: &Code| c.block.as_deref().map_or(c.layout, |b| b.lineage);
                    let newest = self.pools[pool].code.clone().filter(|c| lineage(c) == lineage(old) && c.ins.len() >= old.ins.len());
                    let next = match newest {
                        Some(c) if !core::ptr::eq(Rc::as_ptr(&c), cur) => Some(c),
                        Some(_) if core::ptr::eq(asked, cur) => self.grown_code(chunk, pool, head, kind, func),
                        _ => None,
                    };
                    if let Some(next) = next {
                        if self.acts > floor {
                            let h = self.head_at(self.top_pool) + 4;
                            self.regs[h] = Val::int(Rc::as_ptr(&next) as usize as i64);
                            if self.top_job() { self.refresh_again(Rc::as_ptr(&next)); }
                            self.unflat_top(&next);
                        } else {
                            (floor_code, once) = (next, false);
                            floor_at.code = Rc::as_ptr(&floor_code);
                        }
                        load!();
                        // SAFETY as above.
                        let code_ref: &Code = unsafe { &*cur };
                        self.lay(chunk, code_ref, now);
                    }
                }
                Turn::Lower => {
                    // A hot loop left unlowered carries on in the lowered code from its head, in the frame running now.
                    // SAFETY as above.
                    let (chunk, pool): (&SSAChunk, usize) = (unsafe { &*at }, now);
                    // The code that ran unlowered tells the lowering what its calls found.
                    self.hint = Some(floor_code.clone()).filter(|c| core::ptr::eq(Rc::as_ptr(c), cur));
                    // Laid code laid again resumes at the compiler instruction it stood at, its frame running the new code.
                    let relaying = core::mem::take(&mut self.relaying);
                    // With no frame but this one running the old code none needs its heads kept, so it is lowered whole.
                    if relaying { let old: &Code = unsafe { &*cur }; ip = old.orig(ip) as usize; }
                    self.pools[pool].whole = relaying && self.pools[pool].active <= 1;
                    let lowered = self.lowered_code(chunk, pool, true);
                    self.hint = None;
                    // A frame of the loop running its body's first code carries on in the code lowered for it, as one laid again does.
                    if self.acts > floor {
                        let at = self.head_at(self.top_pool) + 4;
                        self.regs[at] = Val::int(Rc::as_ptr(&lowered) as usize as i64);
                        // A job's callback runs the new code again too.
                        if self.top_job() { self.refresh_again(Rc::as_ptr(&lowered)); }
                        self.unflat_top(&lowered);
                    }
                    // A call's code is its pool's, which the lowering just made.
                    match self.acts > floor {
                        true => {}
                        false => {
                            if once { self.pools[pool].heat += floor_code.hot.get(); }
                            (floor_code, once) = (lowered, false);
                            floor_at.code = Rc::as_ptr(&floor_code);
                        }
                    }
                    load!();
                    // SAFETY as above.
                    let code_ref: &Code = unsafe { &*cur };
                    self.lay(chunk, code_ref, now);
                    ip = code_ref.lowered(ip);
                    // A loop over a generator laid as its section takes the generator the run made before, its state moved in.
                    if let Some(next) = self.adopt_gen(code_ref, ip) { ip = next; }
                }
                Turn::Return(v) => {
                    if self.acts == floor {
                        if self.floors.len() > floors0 { floor_end!(Ok(v), false); }
                        break Ok(v);
                    }
                    // A user iterable's `__iter__` hands its iterator to the builtin under it, which takes it on.
                    if self.top_job() && matches!(self.jobs.last(), Some(super::jobs::Job::IterOf)) && let Some(f) = self.top_apart() {
                        self.leave_frame();
                        load!();
                        // SAFETY as above.
                        match self.pull_from(v, unsafe { &*at }) {
                            Ok(super::jobs::Pulled::Pushed) => ip = f.ret,
                            // A run of text its step called for opens as a floor.
                            Ok(super::jobs::Pulled::Opened) if self.floor_req => (ip, parking) = (f.ret, true),
                            Ok(super::jobs::Pulled::Opened) => {
                                (self.entered, self.yielded) = (false, false);
                                self.set_ret_apart(f.ret, f.dest);
                                let generator = self.top_gather();
                                load!();
                                // SAFETY as above.
                                let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                                self.lay(chunk, code_ref, now);
                                ip = if generator { self.gen_entry } else { code_ref.lowered(0) };
                            }
                            Ok(super::jobs::Pulled::Fold(fi, func, fold, args, n)) => {
                                self.open_fold(fi, func, fold, args, n);
                                self.set_ret_apart(f.ret, f.dest);
                                load!();
                                // SAFETY as above.
                                let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                                self.lay(chunk, code_ref, now);
                                ip = code_ref.lowered(0);
                            }
                            Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                        }
                        continue;
                    }
                    // A dunder a builtin over a generator called hands its result to the builtin, the generator under it carrying on or suspended once the builtin is done.
                    if self.top_job() && let Some(&super::jobs::Job::Fold(d)) = self.jobs.last() && let Some(f) = self.top_apart() {
                        // A builtin stepping a container itself steps to its next fold or gives its result, the dunder's frame kept and rerun when the next fold calls it.
                        if d.resume() == super::jobs::NATIVE {
                            self.jobs.pop();
                            let out = match self.fold_take(d, v) {
                                Ok(super::jobs::FoldStep::Go) => self.fold_drive(),
                                Ok(super::jobs::FoldStep::Done(r)) => { self.jobs.pop(); Ok(super::jobs::Drive::Done(r)) }
                                Ok(super::jobs::FoldStep::Call(fi, func, d2, args, n)) => Ok(super::jobs::Drive::Fold(fi, func, d2, args, n)),
                                Err(e) => { self.jobs.pop(); Err(e) }
                            };
                            if let Ok(super::jobs::Drive::Fold(fi, func, d2, args, n)) = out && func.0 == f.callee.0 && !self.uses_super(fi) {
                                self.jobs.push(super::jobs::Job::Fold(d2));
                                let bases = f.cold != super::act::FLAT;
                                if n == 2 { self.rearm2(f.pool, f.fi, f.callee, f.stack_base, bases, (args[0], args[1])); } else { self.rearm(f.pool, f.fi, f.callee, f.stack_base, bases, args[0]); }
                                load!();
                                ip = unsafe { &*cur }.lowered(0);
                                continue;
                            }
                            // The frame's own job gone, a stand-in is what leaving it pops.
                            self.jobs.push(super::jobs::Job::IterOf);
                            self.leave_frame();
                            after!(self.drive_on(out), f.ret, f.dest);
                            continue;
                        }
                        self.leave_frame();
                        let step = self.fold_take(d, v);
                        // The frame under it, the generator's or a user iterator's `__next__` whose job it is.
                        let Some(under) = self.top_apart() else { break Err(cold_runtime("a fold over no frame")) };
                        match step {
                            Ok(super::jobs::FoldStep::Go) if d.resume() == super::jobs::PULL => {
                                let it = match self.jobs.last() { Some(super::jobs::Job::Gather(j)) => j.co, _ => Val::undef() };
                                self.rearm(under.pool, under.fi, under.callee, under.stack_base, under.cold != super::act::FLAT, it);
                                load!();
                                // SAFETY as above.
                                ip = unsafe { &*cur }.lowered(0);
                            }
                            Ok(super::jobs::FoldStep::Go) => { load!(); ip = f.ret; }
                            Ok(super::jobs::FoldStep::Call(fi, func, d2, args, n)) => {
                                self.open_fold(fi, func, d2, args, n);
                                self.set_ret_apart(f.ret, super::act::NO_DEST);
                                load!();
                                // SAFETY as above.
                                let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                                self.lay(chunk, code_ref, now);
                                ip = code_ref.lowered(0);
                            }
                            step => {
                                // The builtin is done, so the generator stays suspended where it yielded, or the `__next__` frame ends.
                                let (ret, dest) = if d.resume() == super::jobs::PULL { self.leave_frame(); (under.ret, under.dest) } else { (self.yield_gen(d.resume()), super::act::NO_DEST) };
                                load!();
                                if d.resume() != super::jobs::PULL { self.jobs.pop(); }
                                match step {
                                    Ok(super::jobs::FoldStep::Done(r)) => ip = self.deliver_apart(r, ret, dest),
                                    Err(e) => { raised = Some(e); raised_at = Some(ret - 1); ip = ret - 1; }
                                    Ok(_) => { raised = Some(cold_runtime("a fold went on past its end")); ip = ret; }
                                }
                            }
                        }
                        continue;
                    }
                    // A generator `next` runs that returned before it yielded gives the default, or raises StopIteration.
                    if self.top_gather() && let Some(&super::jobs::Job::NextGen(d)) = self.jobs.last() && let Some(f) = self.top_apart() {
                        self.finish_gen();
                        load!();
                        match self.next_end(d, v) {
                            Ok(r) => ip = self.deliver_apart(r, f.ret, super::act::NO_DEST),
                            Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                        }
                        continue;
                    }
                    // A generator a builtin iterator's step met that ended goes on as one that yielded, its end what the step meets.
                    if self.top_gather() && let Some(f) = self.top_apart() && let Some(super::jobs::Job::Gather(j)) = self.jobs.last() && let super::jobs::Takes::Prefetch(drive) = j.takes {
                        self.finish_gen();
                        load!();
                        prefetched!(f.ret, drive);
                    }
                    // A generator whose items a builtin takes as a list ends that list, which the builtin then runs on.
                    if self.top_gather() && let Some(f) = self.top_apart() && let Some(super::jobs::Job::Gather(j)) = self.jobs.last()
                        && matches!(j.takes, super::jobs::Takes::Collect(..) | super::jobs::Takes::Method(..) | super::jobs::Takes::Retry(..) | super::jobs::Takes::Hold) {
                        let j = self.take_gather();
                        self.finish_gen();
                        load!();
                        // SAFETY as above.
                        let chunk: &SSAChunk = unsafe { &*at };
                        // The instruction that met the generator runs again, the list in its place.
                        if let super::jobs::Takes::Retry(slot) = j.takes {
                            match self.alloc_list(j.items) {
                                Ok(l) if (slot as usize) < self.stack.len() => { self.stack[slot as usize] = l; ip = f.ret - 1; }
                                Ok(_) => { raised = Some(cold_runtime("a spread lost its value")); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                                Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                            }
                            continue;
                        }
                        match self.collected(j, chunk) {
                            Ok(Some(r)) => ip = self.deliver_apart(r, f.ret, super::act::NO_DEST),
                            // A builtin that runs Python of its own on the items opened its frame or floor, whose result the next turn takes, the loop parked meanwhile.
                            Ok(None) => (ip, parking) = (f.ret, true),
                            Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                        }
                        continue;
                    }
                    // A generator a builtin runs that returned ends the builtin, its job keeping a stand-in the frame's end drops while the gathered builtin moves out.
                    if self.top_gather() && let Some(f) = self.top_apart() && let Some(job @ super::jobs::Job::Gather(_)) = self.jobs.last_mut()
                        && let super::jobs::Job::Gather(mut j) = core::mem::replace(job, super::jobs::Job::Map { before: Val::undef(), ctor: false }) {
                        self.finish_gen();
                        let end = self.gather_end(&mut j, v, self.chunk);
                        match self.job_out(end, f.ret) {
                            Ok(i) => { load!(); ip = i; }
                            Err((at, e)) => { load!(); raised = Some(e); raised_at = Some(at); ip = at; }
                        }
                        continue;
                    }
                    // A generator that returned ends the `for` stepping it, its value what a `yield from` takes.
                    if self.top().is_some_and(|f| !f.generator.is_undef()) {
                        let at = self.finish_gen();
                        load!();
                        self.yield_from_value = v;
                        self.iter_stack.pop();
                        // SAFETY as above.
                        ip = unsafe { &*cur }.ins.get(at).map_or(at, |ins| ins.a as usize);
                        continue;
                    }
                    // A user iterator's `__next__` hands its item to the builtin, whose dunder on it runs as a frame over this one.
                    if self.top_job() && !self.top_gather() && matches!(self.jobs.last(), Some(super::jobs::Job::Gather(g)) if g.pull.src.is_undef()) && let Some((fi, func, fold, args, n)) = self.fold_of(v, super::jobs::PULL) {
                        if self.budget == 0 { raised = Some(VmErr::Budget); continue; }
                        self.open_fold(fi, func, fold, args, n);
                        load!();
                        // SAFETY as above.
                        let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                        self.lay(chunk, code_ref, now);
                        ip = code_ref.lowered(0);
                        continue;
                    }
                    // A builtin's callback hands what it returned to the builtin, which may call it again.
                    if self.top_job() && let Some(f) = self.top_apart() {
                        // A builtin whose calls run as frames takes what this one gave and makes its next call.
                        if matches!(self.jobs.last(), Some(super::jobs::Job::Calls(..))) {
                            let call = self.calls_step(Some(Ok(v)));
                            // The function this frame ran, lowered, called again runs again in it, as a callback a builtin calls again does.
                            if let Ok(super::jobs::Call::Frame(g, _, args)) = &call && g.0 == f.callee.0 && !self.pools[f.pool].cold && !self.uses_super(f.fi) {
                                let again = match *super::jobs::call_args(args) {
                                    [x] => { self.rearm(f.pool, f.fi, f.callee, f.stack_base, f.cold != super::act::FLAT, x); true }
                                    [x, y] => { self.rearm2(f.pool, f.fi, f.callee, f.stack_base, f.cold != super::act::FLAT, (x, y)); true }
                                    _ => false,
                                };
                                if again {
                                    load!();
                                    // SAFETY as above.
                                    ip = unsafe { &*cur }.lowered(0);
                                    continue;
                                }
                            }
                            self.calls_keep();
                            self.leave_frame();
                            calls!(self.calls_from(call, true), f);
                            continue;
                        }
                        match self.job_take(v) {
                            Ok(super::jobs::Step::Again(arg)) => { self.rearm(f.pool, f.fi, f.callee, f.stack_base, f.cold != super::act::FLAT, arg); load!(); ip = unsafe { &*cur }.lowered(0); }
                            Ok(super::jobs::Step::Again2(x, y)) => { self.rearm2(f.pool, f.fi, f.callee, f.stack_base, f.cold != super::act::FLAT, (x, y)); load!(); ip = unsafe { &*cur }.lowered(0); }
                            // Another function the job calls next opens in place of this frame, its result going where this one's went.
                            Ok(super::jobs::Step::Call1(g, x)) => {
                                let gi = self.next_fn(g, 1);
                                let job = self.jobs.last_mut().map(|j| core::mem::replace(j, super::jobs::Job::IterOf));
                                self.leave_frame();
                                match (job, gi) {
                                    (Some(job), Some(gi)) => {
                                        self.open_job(gi, g, x, f.ret, f.dest, job);
                                        load!();
                                        // SAFETY as above.
                                        let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                                        self.lay(chunk, code_ref, now);
                                        ip = code_ref.lowered(0);
                                    }
                                    _ => { load!(); raised = Some(cold_runtime("a job's next function opens no frame")); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                                }
                            }
                            Ok(super::jobs::Step::Call2(g, x, y)) => {
                                let gi = self.next_fn(g, 2);
                                let job = self.jobs.last_mut().map(|j| core::mem::replace(j, super::jobs::Job::IterOf));
                                self.leave_frame();
                                match (job, gi) {
                                    (Some(job), Some(gi)) => {
                                        self.open_job2(gi, g, x, y, 0, job);
                                        (self.entered, self.yielded) = (false, false);
                                        self.set_ret_apart(f.ret, f.dest);
                                        load!();
                                        // SAFETY as above.
                                        let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                                        self.lay(chunk, code_ref, now);
                                        ip = code_ref.lowered(0);
                                    }
                                    _ => { load!(); raised = Some(cold_runtime("a job's next function opens no frame")); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                                }
                            }
                            // A method bound to its value, or to its first value, opens in place of this frame, its result going where this one's went.
                            Ok(s @ (super::jobs::Step::Open(..) | super::jobs::Step::Bound(..))) => match self.job_bound(s, f.ret, f.dest) {
                                Ok(None) => {
                                    load!();
                                    // SAFETY as above.
                                    let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                                    self.lay(chunk, code_ref, now);
                                    ip = code_ref.lowered(0);
                                }
                                Ok(Some(v)) => { load!(); ip = self.deliver_apart(v, f.ret, f.dest); }
                                Err(e) => { load!(); raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                            },
                            // A step a builtin iterator's call made steps the builtin's job on.
                            Ok(super::jobs::Step::Drive) => { self.leave_frame(); after!(self.gather_drive(), f.ret, super::act::NO_DEST); }
                            step => {
                                // What the builtin raised rises from its call, or from the `for` a `map` or `filter` returns to.
                                let at = f.ret - !matches!(self.jobs.last(), Some(super::jobs::Job::Map { .. } | super::jobs::Job::Filter { .. })) as usize;
                                self.leave_frame();
                                load!();
                                match step {
                                    // SAFETY as above.
                                    Ok(step) => ip = self.job_done(step, unsafe { &*cur }, f.ret, f.dest),
                                    Err(e) => { raised = Some(e); raised_at = Some(at); ip = at; }
                                }
                            }
                        }
                        continue;
                    }
                    // A function exec defined in a dict hands the dict what it bound as its outermost call ends.
                    if self.top_linked() {
                        // A helper a coroutine waits inside hands what it returns to the helper kept under it, which opens.
                        if let Some((co, ret)) = self.chain_top() {
                            let (_, _, v, _) = self.return_frame(v);
                            let at = self.chain_next(co, ret, Some(v));
                            load!();
                            ip = at;
                            continue;
                        }
                        if let Err(e) = self.unlink_top() { raised = Some(e); continue; }
                    }
                    let (ret, dest, v, dunder) = self.return_frame(v);
                    load!(hot);
                    ip = match dunder { Some(func) => self.dunder_back(v, ret, dest, func), None => self.deliver(v, ret, dest) };
                }
                Turn::Yield(v) => {
                    if self.acts == floor {
                        if self.floors.len() > floors0 { floor_end!(Ok(Val::none()), true); }
                        break Ok(v);
                    }
                    if self.top().is_some_and(|f| !f.generator.is_undef()) {
                        self.yielded = false;
                        // A generator a `for` steps parks where it waits, down to the first frame that is no generator, one a builtin steps raising below as the builtin cannot wait.
                        if self.waits() && !self.top_gather() {
                            loop {
                                let step = self.park_gen();
                                let stepping = if self.acts > floor { self.top_run().0 } else { Rc::as_ptr(&floor_code) };
                                // SAFETY as above.
                                self.resume_ip = unsafe { &*stepping }.resume(step);
                                if !(self.acts > floor && self.top().is_some_and(|f| !f.generator.is_undef())) { break; }
                            }
                            self.yielded = true;
                            if self.acts == floor { self.run_in(pool); break Ok(Val::none()); }
                            ip = self.park_frame();
                            load!();
                            parking = true;
                            continue;
                        }
                        // A generator a builtin steps hands it values only, waiting inside it is an error there.
                        if self.waits() { raised = Some(self.suspended_in_generator(self.top_apart().map(|f| f.fi))); parking = false; continue; }
                        let resume = core::mem::take(&mut self.resume_ip);
                        // A generator a builtin runs hands it its value and carries on past the yield while the builtin takes more, or for `next` ends suspended where it yielded.
                        if self.top_gather() && let Some(super::jobs::Job::Gather(j)) = self.jobs.last() && let super::jobs::Takes::Prefetch(drive) = j.takes {
                            let g = self.head(self.head_at(self.top_pool) + 2);
                            let ret = self.yield_gen(resume);
                            self.jobs.pop();
                            if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(g) { c.pending = v; }
                            load!();
                            parking = false;
                            prefetched!(ret, drive);
                        }
                        if self.top_gather() && matches!(self.jobs.last(), Some(super::jobs::Job::NextGen(_))) {
                            let ret = self.yield_gen(resume);
                            load!();
                            self.jobs.pop();
                            ip = self.deliver_apart(v, ret, super::act::NO_DEST);
                            parking = false;
                            continue;
                        }
                        if self.top_gather() {
                            // A user dunder the builtin calls on the value runs as a frame over the generator's, live under it.
                            if let Some((fi, func, fold, args, n)) = self.fold_of(v, resume) {
                                if self.budget == 0 { raised = Some(VmErr::Budget); parking = false; continue; }
                                self.open_fold(fi, func, fold, args, n);
                                // SAFETY as above, it returns to where the generator carries on, taking nothing there.
                                let back = if resume & RESUME_RAW != 0 { resume & !RESUME_RAW } else { unsafe { &*cur }.lowered(resume) };
                                self.set_ret_apart(back, super::act::NO_DEST);
                                load!();
                                // SAFETY as above.
                                let (code_ref, chunk): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                                self.lay(chunk, code_ref, now);
                                ip = code_ref.lowered(0);
                                parking = false;
                                continue;
                            }
                            match self.gather_take(v) {
                                Ok(None) => {
                                    // SAFETY as above.
                                    ip = if resume & RESUME_RAW != 0 { resume & !RESUME_RAW } else { unsafe { &*cur }.lowered(resume) };
                                    parking = false;
                                    continue;
                                }
                                // A result, or an error, leaves the generator suspended where it yielded, as Python leaves it.
                                step => {
                                    let ret = self.yield_gen(resume);
                                    self.jobs.pop();
                                    match self.job_out(step.map(|r| r.unwrap_or(Val::none())), ret) {
                                        Ok(i) => { load!(); ip = i; }
                                        Err((at, e)) => { load!(); raised = Some(e); raised_at = Some(at); ip = at; }
                                    }
                                    parking = false;
                                    continue;
                                }
                            }
                        }
                        let at = self.yield_gen(resume);
                        load!();
                        // SAFETY as above.
                        self.hand_to_for(unsafe { &*cur }, at, v);
                        ip = at + 1;
                        parking = false;
                        continue;
                    }
                    // A builtin's callback hands its result to the builtin, which cannot wait with it.
                    if self.top().is_some_and(|f| f.job || self.acts - 1 > floor && self.mapped(&f) || self.acts - 1 == floor && f.dest != super::act::NO_DEST
                        && floor_code.ins.get(f.ret).is_some_and(|i| i.op == OpCode::ForIterR) && matches!(self.iter_stack.last(), Some(IterFrame::Map { .. }))) {
                        self.yielded = false;
                        raised = Some(self.suspended_in(" a callback a builtin runs"));
                        parking = false;
                        continue;
                    }
                    // An operator dunder's op has nothing to take a result on resume, so it never parks.
                    if self.top().is_some_and(|f| f.dunder) { self.yielded = false; raised = Some(self.suspended_in(" an operator method")); parking = false; continue; }
                    if self.top_linked_apart() && let Err(e) = self.unlink_top() { raised = Some(e); parking = false; continue; }
                    ip = self.park_frame();
                    load!();
                    parking = true;
                }
                Turn::Raise(e) => {
                    // An error out of flat plain frames, which keep no handler, leaves them all at once, the frame below taking it at its call.
                    if self.acts > floor && !matches!(e, VmErr::HostYield(_) | VmErr::Name(_) | VmErr::Redo) && !is_stop(&e) && let Some(at) = self.unwind_plain(&floor_at) {
                        load!();
                        (ip, raised, raised_at) = (at + 1, Some(e), Some(at));
                        continue;
                    }
                    let Some(f) = self.top().filter(|_| self.acts > floor) else {
                        if self.floors.len() > floors0 { floor_end!(Err(e), false); }
                        break Err(e)
                    };
                    let (fi, call_ip, generator) = (f.fi, f.call_ip, f.generator);
                    // What a job's generator raises goes as its job takes it, an `async for` ending or an async generator's turning RuntimeError.
                    let e = if f.job && !generator.is_undef() {
                        match self.job_raised(e) {
                            Ok(Ok(at)) => { load!(); ip = at; continue; }
                            Ok(Err((at, e))) => { load!(); (raised, raised_at, ip) = (Some(e), Some(at), at); continue; }
                            Err(e) => e,
                        }
                    } else { e };
                    // A builtin whose calls run as frames takes what its frame raised, its job kept past the frame's end.
                    if f.job && matches!(self.jobs.last(), Some(super::jobs::Job::Calls(..))) {
                        self.calls_keep();
                        self.leave_frame();
                        calls!(self.calls_go(Some(Err(e)), true), f);
                        continue;
                    }
                    // A StopIteration out of the function a `map` or `filter` calls ends the `for` stepping it, the one a job's or a plain `map` frame returns before.
                    let stop = is_stop(&e) && generator.is_undef();
                    let before = match self.jobs.last() {
                        Some(&super::jobs::Job::Map { before, .. } | &super::jobs::Job::Filter { before, .. }) if stop && f.job => Some((before, f.ret)),
                        _ => None,
                    };
                    // An error out of a dunder a builtin over a generator called rises from the builtin's call, the generator left suspended where it yielded, as in Python.
                    if f.job && let Some(&super::jobs::Job::Fold(d)) = self.jobs.last() {
                        self.leave_frame();
                        // A builtin stepping a container itself ends with the error, which rises from its call.
                        if d.resume() == super::jobs::NATIVE {
                            self.jobs.pop();
                            load!();
                            (raised, raised_at, ip) = (Some(e), Some(f.ret - 1), f.ret - 1);
                            continue;
                        }
                        let ret = if d.resume() == super::jobs::PULL { let r = self.top_apart().map_or(0, |u| u.ret); self.leave_frame(); r } else { self.yield_gen(d.resume()) };
                        load!();
                        if d.resume() != super::jobs::PULL { self.jobs.pop(); }
                        (raised, raised_at, ip) = (Some(e), Some(ret - 1), ret - 1);
                        continue;
                    }
                    // An error out of a user iterable's `__iter__` rises from the builtin's call, its job gone.
                    if f.job && matches!(self.jobs.last(), Some(super::jobs::Job::IterOf)) {
                        self.leave_frame();
                        self.jobs.pop();
                        load!();
                        (raised, raised_at, ip) = (Some(e), Some(f.ret - 1), f.ret - 1);
                        continue;
                    }
                    // A StopIteration out of a call a builtin iterator's step made ends that step, which then steps on.
                    if stop && f.job && let Some(drive) = self.feed_stopped() {
                        self.leave_frame();
                        if drive { after!(self.gather_drive(), f.ret, super::act::NO_DEST); } else { load!(); ip = f.ret; }
                        continue;
                    }
                    // A StopIteration out of the `__next__` a `next` with a default called gives the default.
                    if stop && f.job && let Some(&super::jobs::Job::Check(super::jobs::Check::Next(d, b))) = self.jobs.last() && !d.is_undef() {
                        self.leave_frame();
                        load!();
                        (self.error_byte_pos, _) = (None, self.swallow_stop(Some(b).filter(|b| !b.is_undef())));
                        self.call_stack.clear();
                        ip = self.deliver_apart(d, f.ret, f.dest);
                        continue;
                    }
                    // A StopIteration out of a user iterator's `__next__` a builtin's job calls ends the builtin, which gives its result.
                    if stop && f.job && let Some(super::jobs::Job::Gather(j)) = self.jobs.last() {
                        let before = Some(j.before).filter(|b| !b.is_undef());
                        let mut j = self.take_gather();
                        self.leave_frame();
                        load!();
                        (self.error_byte_pos, _) = (None, self.swallow_stop(before));
                        self.call_stack.clear();
                        // SAFETY as above.
                        match self.gather_end(&mut j, Val::none(), unsafe { &*at }) {
                            Ok(r) => ip = self.deliver_apart(r, f.ret, f.dest),
                            Err(e) => { raised = Some(e); raised_at = Some(f.ret - 1); ip = f.ret - 1; }
                        }
                        continue;
                    }
                    // The GeneratorExit a `close` threw, or a StopIteration, out of the generator it ran ends the close with None.
                    if f.job && !generator.is_undef() && (is_stop(&e) || is_exit(&e)) && let Some(super::jobs::Job::Gather(j)) = self.jobs.last() && j.takes == super::jobs::Takes::Close {
                        let before = Some(j.before).filter(|b| !b.is_undef());
                        self.finish_gen();
                        load!();
                        (self.error_byte_pos, _) = (None, self.swallow_stop(before));
                        self.call_stack.clear();
                        // A generator's frame keeps its loops' depth where a call keeps its register, so the result is pushed.
                        ip = self.deliver_apart(Val::none(), f.ret, super::act::NO_DEST);
                        continue;
                    }
                    // An error out of a helper a coroutine waits inside rises at the call of the helper kept under it.
                    if self.top_linked() && let Some((co, ret)) = self.chain_top() {
                        self.stack.truncate(f.stack_base);
                        self.leave_frame();
                        let at = self.chain_raise(co, ret, fi);
                        load!();
                        (ip, raised, raised_at) = (at, Some(e), None);
                        continue;
                    }
                    ip = f.ret;
                    let e = match self.top_linked().then(|| self.unlink_top()) { Some(Err(le)) => le, _ => e };
                    // A generator that raised stays finished, the error meeting its `for`.
                    if generator.is_undef() { self.leave_frame(); } else { self.finish_gen(); }
                    load!();
                    // SAFETY as above.
                    let before = before.or_else(|| (stop && !f.job && unsafe { &*cur }.ins.get(f.ret).is_some_and(|i| i.op == OpCode::ForIterR)
                        && matches!(self.iter_stack.last(), Some(IterFrame::Map { .. }))).then_some((Val::undef(), f.ret)));
                    if let Some((before, at)) = before {
                        (self.error_byte_pos, self.yield_from_value) = (None, self.swallow_stop(Some(before).filter(|v| !v.is_undef())));
                        self.call_stack.clear();
                        self.iter_stack.pop();
                        // SAFETY as above.
                        ip = unsafe { &*cur }.ins.get(at).map_or(at, |ins| ins.a as usize);
                        continue;
                    }
                    // SAFETY as above.
                    let (code_ref, caller): (&Code, &SSAChunk) = unsafe { (&*cur, &*at) };
                    // A site call kept the caller's own index, the compiler's is worked out only here.
                    let (call_ip, raw) = match call_ip {
                        Some(o) if o & super::act::RAW_IP != 0 => { let i = (o & !super::act::RAW_IP) as usize; (Some(code_ref.orig(i)), Some(i)) }
                        other => (other, None),
                    };
                    // A call made inside a section was made in the function the section was laid from.
                    match raw.and_then(|i| code_ref.block.as_deref().and_then(|b| b.holding(i)).map(|(s, i)| (i, s))) {
                        Some((i, s)) => { let o = s.orig.get(i - s.start as usize).copied(); self.note_frame(fi, o, &self.functions[s.fi].1) }
                        None => self.note_frame(fi, call_ip, caller),
                    }
                    // An error out of a call an op's handler made is what the call gave, the op running again to take it.
                    if f.dunder && f.dest == super::act::REDO { ip = self.redo_back(f.callee, f.ret, Err(e)); continue; }
                    raised = Some(e);
                    raised_at = Some(raw.unwrap_or_else(|| call_ip.map_or(ip, |o| code_ref.lowered(o as usize))));
                }
            }
        };
        // A run of text an error broke out of the loop with ends with it.
        while self.floors.len() > floors0 && let Some(f) = self.floors.last_mut() {
            // One the loop never took has no window to close.
            if f.code.is_some() {
                swap_floor!(*f);
                self.floor_close();
            }
            let _ = self.floor_end(Err(cold_runtime("a run of text ended by an error")), false, None);
        }
        if once { self.pools[pool].heat += floor_code.hot.get(); }
        self.frame_safe = outer_safe;
        result
    }

    /* Opens the run of text `req` asks for as the floor of the running loop, laid as a nested run would be, giving what the loop keeps or the run back with the error. */
    #[inline(never)]
    fn floor_open(&mut self) -> Result<(), VmErr> {
        let Some(f) = self.floors.last_mut() else { return Err(cold_runtime("a floor opened with none asked for")) };
        let (chunk, mut slots, cost) = (f.chunk, core::mem::take(&mut f.slots), f.cost);
        // SAFETY the VM keeps every compiled chunk until it drops.
        let chunk: &SSAChunk = unsafe { &*chunk };
        let (pool, names) = (self.pool_of(chunk), chunk.names.len());
        let fresh = self.pools[pool].active == 0 && self.pools[pool].w0.base == super::window::NO_WINDOW;
        let base = if fresh { self.first_window(pool, &slots, names) } else { self.open_window(pool, slots.len().max(names), names, false) };
        if !fresh {
            self.regs[base..base + slots.len()].copy_from_slice(&slots);
            // A frame copied out of this window as long as it holds its constants as they were laid, any other takes them again.
            if slots.len() != self.pools[pool].wlen {
                self.pools[pool].wcode = 0;
                self.note_window(pool);
            }
        }
        self.run_in(pool);
        // It nests as deep as its own run would.
        self.depth += cost;
        let (code, once) = self.frame_code(chunk, pool);
        if self.pools[pool].consts.is_none() {
            match super::cache::const_vals(chunk, &mut self.heap) {
                Ok(consts) => self.pools[pool].consts = Some(consts),
                Err(e) => {
                    self.depth -= cost;
                    self.close_window(pool);
                    return Err(e);
                }
            }
        }
        if !self.laid(&code, pool) { self.grow_frame(chunk, &code, pool); }
        let bases = [self.exception_stack.len(), self.unwind_stack.len(), self.pending.delta_save.len()];
        let depth = self.acts;
        // The slots' room is kept for a class body's members, which its end reads back into it.
        slots.clear();
        if let Some(f) = self.floors.last_mut() { (f.code, f.once, f.pool, f.depth, f.bases, f.slots) = (Some(code), once, pool, depth, bases, slots); }
        Ok(())
    }

    /* Ends the floor of the run of text on top of the floors set aside, its window closed and its depth given back. */
    fn floor_close(&mut self) {
        let Some(f) = self.floors.last() else { return };
        let (heat, pool, cost) = (if f.once { f.code.as_ref().map_or(0, |c| c.hot.get()) } else { 0 }, f.pool, f.cost);
        // A class body's slots are its members, which its end reads.
        if matches!(f.run, super::globals::compile::FloorRun::Class(_)) && let Some(f) = self.floors.last_mut() {
            // SAFETY the VM keeps every chunk it runs until it drops.
            let (at, names) = (self.pools[pool].window, unsafe { &*f.chunk }.names.len());
            f.slots.extend_from_slice(&self.regs[at..at + names]);
        }
        self.pools[pool].heat += heat;
        self.depth -= cost;
        self.close_window(pool);
    }

    /* What a suspending frame hands out, its resume point noted. */
    #[inline(never)]
    fn yield_out(&mut self, code: &Code, ip: usize) -> Val {
        // A wait keeps its None placeholder for what wakes it, a helper that parked pushed nothing.
        let event_yield = self.pending.event_wait_request;
        let sub_call_yield = !self.pending_sync_frames.is_empty();
        let host_yield = self.pending.host_call_request;
        let preempt_yield = self.pending.preempt_request;
        let regular_yield = !event_yield && !sub_call_yield && !host_yield && !preempt_yield;
        let val = if regular_yield { self.pop().unwrap_or(Val::none()) } else { Val::none() };
        let next_is_pop = ip < code.ins.len() && code.ins[ip].op == OpCode::PopTop;
        // A `yield` of a register drops its result itself.
        let dropped = ip > 0 && code.ins.get(ip - 1).is_some_and(|i| i.op == OpCode::Yield && i.x == 1);
        let at = if regular_yield && next_is_pop && !dropped { ip + 1 } else { ip };
        // Sections, their stubs and loops over a generator's section lie past the code's own instructions and have no compiler index.
        let raw = code.block.as_deref().is_some_and(|b| b.sections.first().is_some_and(|s| at >= s.start as usize) || b.loops.iter().any(|&(from, to, ..)| (from as usize..to as usize).contains(&at)));
        (self.resume_ip, self.resume_layout) = if raw { (at | RESUME_RAW, code.layout) } else { (code.resume(at), 0) };
        // Value-position yield, leave the sent value (None for next()) for the consumer on resume.
        if regular_yield && !next_is_pop && !dropped { self.push(Val::none()); }
        // DON'T truncate exception_stack here, frames pushed in this exec belong to active try/except blocks. The enclosing `resume_coroutine` drains them into the coroutine's saved state so `try` survives the yield.
        val
    }

    /* Jumps to this frame's handler for `e`, or hands `e` back when none. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn catch(&mut self, code: &Code, chunk: &SSAChunk, ip: &mut usize, exc_base: usize, delta_base: usize, e: VmErr, rip: usize) -> Result<(), VmErr> {
        // HostYield is a control-flow signal, not a Python exception, bypass try/except.
        if matches!(e, VmErr::HostYield(_)) { return Err(e); }
        // A missing name is measured against the frame it is missing from, which is still alive here.
        if let VmErr::Name(n) = &e { self.note_missing_name(n, chunk); }
        // Innermost frame wins, cleared below on swallow so later errors re-anchor.
        if self.error_byte_pos.is_none() {
            self.error_byte_pos = chunk.resolve(code.orig(rip));
            // Another text, run by `exec` or imported, keeps where in it the error rose, for the line its traceback starts at.
            if self.error_inner.is_none() && !alloc::sync::Arc::ptr_eq(&chunk.source, &self.chunk.source) && let Some(p) = self.error_byte_pos {
                self.error_inner = Some((chunk.source.clone(), chunk.path.clone(), p));
            }
        }
        if self.exception_stack.len() <= exc_base { return Err(e); }
        let frame = self.exception_stack.pop().unwrap();
        // A handler inside a section a recursion runs again takes back the registers its level kept for the call that raised.
        if frame.handler_ip & RAW_HANDLER != 0 && code.block.is_some() && self.turned > frame.turned { self.unwind_levels(code, rip, frame.stack_depth); }
        self.stack.truncate(frame.stack_depth);
        self.turned = frame.turned;
        self.iter_stack.truncate(frame.iter_depth);
        self.with_stack.truncate(frame.with_depth);
        self.pending.pos_delta = 0;
        self.pending.kw_delta = 0;
        self.pending.delta_save.truncate(delta_base);
        // Drop partial traceback so a later error doesn't inherit stale frames.
        self.call_stack.clear();
        let msg = e.class_name();
        // Prefer the user-raised instance, synthesize one for native errors.
        let exc = if let Some(v) = self.pending.exc_val.take() {
            v
        } else {
            // A MemoryError reports the limit itself, so its objects skip the soft limit.
            let heap_err = matches!(e, VmErr::Heap);
            let alloc = |heap: &mut HeapPool, obj| if heap_err { heap.alloc_emergency(obj) } else { heap.alloc(obj) };
            // An error with no message, a bare `StopIteration`, has no args.
            let text = e.message();
            let args = if text.is_empty() { alloc::vec![] } else { alloc::vec![alloc(&mut self.heap, HeapObj::Str(text))?] };
            alloc(&mut self.heap, HeapObj::ExcInstance(msg, args, Val::undef()))?
        };
        // Kept so raising the exception again reports where it was first raised.
        let at = self.error_byte_pos.take();
        (self.error_inner, self.missed) = (None, None);
        // An exception rising while another is handled takes it as its context.
        let no_context = core::mem::take(&mut self.pending.no_context);
        if let Some(&(handled, handled_at)) = self.handling.last() && !no_context {
            self.with_roots([exc], |vm| vm.link_context(exc, handled, handled_at))?;
        }
        // Drop reasons from finally bodies this exception unwinds past.
        self.unwind_stack.truncate(frame.unwind_depth);
        self.handling.truncate(frame.handling_depth);
        // A handler or a cleanup running for the exception handles it, so a bare `raise` re-raises it.
        self.handling.push((exc, at));
        match frame.kind {
            BlockKind::Except => self.push(exc),
            // finally/with run their cleanup, then re-raise via EndFinally.
            BlockKind::Finally => {
                self.pending.exc_val = Some(exc);
                self.unwind_stack.push(Unwind::Reraise(e, at));
            }
        }
        *ip = handler_at(code, frame.handler_ip);
        Ok(())
    }

    /* Resolve the receiver's method and call directly, args come from CallMethodArgs. */
    #[allow(clippy::too_many_arguments)]
    /* The items of a tuple or a list of at most three, as positionals a call passes. */
    fn few_items(&self, seq: Val) -> Option<([Val; 3], usize)> {
        let mut out = [Val::undef(); 3];
        let n = match self.heap.try_get(seq)? {
            HeapObj::Tuple(v) if v.len() <= 3 => { out[..v.len()].copy_from_slice(v); v.len() }
            HeapObj::List(rc) => { let v = rc.borrow(); if v.len() > 3 { return None; } out[..v.len()].copy_from_slice(&v); v.len() }
            _ => return None,
        };
        Some((out, n))
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_call_method(&mut self, attr_idx: u16, call_op: u16, rip: usize, code: &Code, chunk: &SSAChunk, rerun: super::redo::Rerun) -> Result<(), VmErr> {
        let cache = &code.cache;
        let raw = call_op as usize;
        let num_kw = (raw >> 8) & 0xFF;
        let num_pos = raw & 0xFF;
        let total = num_pos + 2 * num_kw;

        let at = self.stack.len().checked_sub(total).ok_or_else(|| cold_runtime("stack underflow"))?;
        let obj = *self.stack.get(at.wrapping_sub(1)).ok_or_else(|| cold_runtime("stack underflow"))?;
        // Borrow, don't clone, `chunk` outlives every `&mut self` call below.
        let Some(name) = chunk.names.get(attr_idx as usize) else { return Err(VmErr::Runtime("CallMethod: bad name index")) };
        let site = cache.site(rip);
        // A known builtin method with a few positionals copies them once and takes no keyword slot.
        if num_kw == 0 && num_pos <= 3 && let Some(id) = self.site_builtin(site, obj) {
            let mut args = [Val::undef(); 3];
            args[..num_pos].copy_from_slice(&self.stack[at..at + num_pos]);
            // A method given a user object hands the calls it makes to the loop, the call op run again after each.
            let armed = if self.method_calls(id, &args[..num_pos]) { self.redo_defer_held(rip, rerun, at + num_pos, obj, &args[..num_pos], &[]) } else { super::redo::Defer::No };
            self.stack.truncate(at - 1);
            // A sort here may run its `__lt__` as frames of this loop, and a generator's method its body.
            self.job_ok = true;
            let done = self.exec_bound_method(obj, id, &args[..num_pos], &[], chunk);
            self.job_ok = false;
            self.redo_undefer(armed);
            return done;
        }
        // Any call past a builtin one may run Python, whose frame points back here.
        self.pending.call_ip = Some(code.orig(rip));
        // A known method calls with the receiver slotted in as its first argument.
        if let Some((func, owner)) = self.site_method(site, obj, name) {
            self.pending.method_binding = Some((owner, obj));
            // The receiver and the arguments already lie in parameter order, so a plain method opens with no shift.
            if num_kw == 0 && self.open_in_loop(func, Passed::Stack { first: at - 1, to: at - 1 }, num_pos + 1)? { return Ok(()); }
            self.stack.insert(at - 1, func);
            self.pending_exec_safe = self.frame_safe;
            self.inline_call = true;
            let called = self.exec_call_n(num_pos + 1, num_kw, chunk);
            self.pending_exec_safe = false;
            return called;
        }
        if let Some(value) = self.site_get(site, obj) {
            self.stack[at - 1] = value;
            // A plain function the class holds, a static method most often, opens as a frame of this loop.
            if num_kw == 0 && self.open_in_loop(value, Passed::Stack { first: at, to: at - 1 }, num_pos)? { return Ok(()); }
            (self.inline_call, self.pending_exec_safe) = (true, self.frame_safe);
            let called = self.exec_call_n(num_pos, num_kw, chunk);
            self.pending_exec_safe = false;
            return called;
        }
        let (positional, kw_flat) = (Args::of(&self.stack[at..at + num_pos]), Args::of(&self.stack[at + num_pos..]));
        self.stack.truncate(at - 1);
        if let Some(id) = self.site_builtin(site, obj) {
            let armed = if self.method_calls(id, &positional) { self.redo_defer_held(rip, rerun, at + total, obj, &positional, &kw_flat) } else { super::redo::Defer::No };
            let done = self.exec_bound_method(obj, id, &positional, &kw_flat, chunk);
            self.redo_undefer(armed);
            return done;
        }

        let lookup = match self.resolve_attr(obj, name) {
            Ok(l) => l,
            Err(VmErr::Attribute(msg)) => {
                //  if `__getattr__` resolves the name to a callable, invoke it with the positional args.
                if let Some(v) = self.try_getattr_fallback(obj, name, chunk)? {
                    return self.call_in_loop_with(v, &positional, &kw_flat, chunk);
                }
                self.note_missing_attr(obj, name);
                return Err(VmErr::Attribute(msg));
            }
            Err(other) => return Err(other),
        };
        match lookup {
            AttrLookup::ModuleAttr(callee) => {
                cache.set_site(rip, self.learn_get(obj, name));
                // A function a module holds is noted as one an instance holds, so the code laid again calls its section.
                if matches!(self.heap.try_get(callee), Some(HeapObj::Func(..))) { self.note_called(code, chunk, rip, callee, lower::FIELD); }
                self.call_in_loop_with(callee, &positional, &kw_flat, chunk)
            }
            AttrLookup::Builtin(name) => {
                let callee = self.builtin_value(name)?;
                self.call_in_loop_with(callee, &positional, &kw_flat, chunk)
            }
            AttrLookup::ClassMember(callee)
            // An instance-attribute callable gets no `self`, only class-level functions bind.
            | AttrLookup::InstanceField(callee) => {
                let func = matches!(self.heap.try_get(callee), Some(HeapObj::Func(..)));
                if func {
                    self.note_class(code, chunk, rip, obj, 0);
                    // One the instance holds is noted so the code laid again calls its section, as is the static method its class holds.
                    match (self.heap.try_get(obj), &lookup) {
                        (Some(&HeapObj::Instance(class, _)), AttrLookup::ClassMember(_)) => self.note_class(code, chunk, rip, class, 0),
                        (Some(HeapObj::Instance(..)), _) => self.note_called(code, chunk, rip, callee, lower::FIELD),
                        _ => {}
                    }
                }
                // A function a class or an instance holds, a static method most often, runs as a frame of this loop as a method does.
                if !func { return self.call_in_loop_with(callee, &positional, &kw_flat, chunk); }
                if kw_flat.is_empty() && positional.len() <= 3 {
                    let mut args = [Val::undef(); 3];
                    args[..positional.len()].copy_from_slice(&positional);
                    if self.open_in_loop(callee, Passed::Regs(args), positional.len())? { return Ok(()); }
                }
                (self.pending_exec_safe, self.inline_call) = (self.frame_safe, true);
                let called = self.call_with(callee, None, &positional, &kw_flat, chunk);
                self.pending_exec_safe = false;
                called
            }
            AttrLookup::InstanceMethod { recv, func, class } => {
                if recv.0 == obj.0 && matches!(self.heap.try_get(obj), Some(HeapObj::Class(..))) { self.note_class(code, chunk, rip, obj, 0); }
                if recv.0 == obj.0 { cache.set_site(rip, self.learn_method(obj, name, func, class)); }
                // Prepend `self`, `super()` reads the binding off `pending`, and method bodies stage like plain calls.
                self.pending.method_binding = Some((class, recv));
                self.pending_exec_safe = self.frame_safe;
                self.inline_call = true;
                let called = self.call_with(func, Some(recv), &positional, &kw_flat, chunk);
                self.pending_exec_safe = false;
                called
            }
            AttrLookup::BuiltinMethod(id) => {
                cache.set_site(rip, self.learn_builtin(obj, id));
                let armed = if self.method_calls(id, &positional) { self.redo_defer_held(rip, rerun, at + total, obj, &positional, &kw_flat) } else { super::redo::Defer::No };
                self.job_ok = true;
                let done = self.exec_bound_method(obj, id, &positional, &kw_flat, chunk);
                self.redo_undefer(armed);
                self.job_ok = false;
                done
            }
            // So do one bound to another receiver, `super()`'s, and one called through its type.
            AttrLookup::BoundBuiltin(recv, id) => {
                self.job_ok = true;
                let done = self.exec_bound_method(recv, id, &positional, &kw_flat, chunk);
                self.job_ok = false;
                done
            }
            AttrLookup::UnboundMethod(id, _) => {
                self.job_ok = true;
                let done = self.exec_unbound_method(id, &positional, &kw_flat, chunk);
                self.job_ok = false;
                done
            }
            AttrLookup::DescGet { desc, recv, owner } => self.desc_call(desc, recv, owner, &positional, &kw_flat),
            AttrLookup::ExcArgs(_) | AttrLookup::Name(_) | AttrLookup::TypeOf(_) | AttrLookup::Mro(_) | AttrLookup::Bases(_) | AttrLookup::Members(_) | AttrLookup::Annotations(_) | AttrLookup::Code(_) | AttrLookup::Names(_) | AttrLookup::KwDefaults(_) | AttrLookup::Int(_) => {
                // `e.args()` / `f.__name__()`, the value isn't callable, reports as missing attribute.
                let ty = self.type_name(obj);
                Err(VmErr::Attribute(s!("'", str ty, "' object has no attribute '", str &name, "'")))
            }
            AttrLookup::PropertyGet { recv, getter } => {
                // Materialise the value first, then call it with the user's args, `foo.prop(arg)` where `prop` returns a callable.
                if self.depth >= self.max_calls { return Err(cold_depth()); }
                self.push(getter);
                self.push(recv);
                self.exec_call(1, chunk)?;
                let value = self.pop()?;
                self.call_with(value, None, &positional, &kw_flat, chunk)
            }
            AttrLookup::Dict(_) => Err(cold_type("'dict' object is not callable")),
            AttrLookup::Thunk(f) => {
                // `X.__value__(...)` evaluates the value, then calls it.
                self.push(f);
                self.exec_call(0, chunk)?;
                let value = self.pop()?;
                self.call_with(value, None, &positional, &kw_flat, chunk)
            }
            AttrLookup::PropertyPart(prop, slot) => {
                let v = self.heap.alloc(HeapObj::PropertyPart(prop, slot))?;
                self.call_with(v, None, &positional, &kw_flat, chunk)
            }
        }
    }

    /* Runs register forms from `ip` until one needs its handler, entering and leaving plain frames in place, true when it switched frames. */
    #[inline(always)]
    fn run_regs<'c>(&mut self, code_at: &mut &'c Code, chunk_at: &mut &'c SSAChunk, ip: &mut usize, floor: &Floor) -> bool {
        let (mut code, mut chunk, mut switched) = (*code_at, *chunk_at, false);
        let mut cache = &code.cache;
        // The running code as a local slice, taken again when a call or return switches frames.
        let mut prog: &[Ins] = &code.ins;
        let mut n = prog.len();
        // SAFETY every caller keeps `*ip` at most the code's length, so `p` stays within `prog` or on the sentinel past its end (`lower::sealed`).
        debug_assert!(*ip <= n);
        let mut p: *const Ins = unsafe { prog.as_ptr().add(*ip) };
        // The index of the running instruction.
        macro_rules! idx { () => { unsafe { p.offset_from(prog.as_ptr()) as usize } } }
        // Carries on at index `$k`, which every jump, call and return keeps at most the length of `prog`.
        macro_rules! goto { ($k:expr) => {{ let k = $k; debug_assert!(k <= n); p = unsafe { prog.as_ptr().add(k) }; }} }
        let mut base = self.base;
        // The frame as a local slice, taken again only after a call that may grow the stack.
        let mut r = &mut self.regs[base..];
        // Every register an instruction names lies below `code.frame` (`lower::reg_bound`), which `grow_frame` keeps the frame as long as, so registers read unchecked.
        debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
        macro_rules! reg {
            ($k:expr) => {{
                debug_assert!(($k as usize) < r.len());
                // SAFETY `$k` is below `code.frame`, which the frame slice covers.
                unsafe { *r.get_unchecked($k as usize) }
            }};
        }
        macro_rules! set {
            ($k:expr, $v:expr) => {{
                let v = $v;
                debug_assert!(($k as usize) < r.len());
                // SAFETY as for `reg!`.
                unsafe { *r.get_unchecked_mut($k as usize) = v; }
            }};
        }
        macro_rules! arith {
            ($ins:ident, $int:expr, $float:expr) => {{
                let (x, y) = (reg!($ins.b), reg!($ins.c));
                let v = if x.is_int() && y.is_int() { $int(x.as_int(), y.as_int()) }
                    else if x.is_float() && y.is_float() { $float(x.as_float(), y.as_float()) }
                    else { None };
                let Some(v) = v else { break };
                set!($ins.a, v);
                p = unsafe { p.add(1) };
            }};
        }
        macro_rules! compare {
            ($ins:ident, $cmp:tt) => {{
                let (x, y) = (reg!($ins.b), reg!($ins.c));
                let v = if x.is_int() && y.is_int() { x.as_int() $cmp y.as_int() }
                    else if x.is_float() && y.is_float() { x.as_float() $cmp y.as_float() }
                    else { break };
                set!($ins.a, Val::bool(v));
                p = unsafe { p.add(1) };
            }};
        }
        // A taken branch costs one step of the budget.
        macro_rules! branch {
            ($ins:ident, $holds:expr) => {{
                if $holds { p = unsafe { p.add(1) }; }
                else if self.budget > 0 && ($ins.a as usize) <= n { self.budget -= 1; goto!($ins.a as usize); }
                else { break; }
            }};
        }
        macro_rules! test {
            ($ins:ident, $cmp:tt) => {{
                let (x, y) = (reg!($ins.b), reg!($ins.c));
                let holds = if x.is_int() && y.is_int() { x.as_int() $cmp y.as_int() }
                    else if x.is_float() && y.is_float() { x.as_float() $cmp y.as_float() }
                    else { break };
                branch!($ins, holds)
            }};
        }
        // A call through the site, `$callee` the function or class it calls, which a method call never reads.
        macro_rules! call {
            ($ins:ident, $callee:expr) => {{
                // The site read in place, the few fields a call needs and not the whole site.
                let i = idx!();
                // A method call inside a section names its method in the chunk of the function the section was laid from.
                let named = if $ins.op == OpCode::CallMethodR && code.block.is_some() { chunk_in(&self.functions, code, chunk, i) } else { chunk };
                let known = cache.with_site(i, |site| match *site {
                    Site::Call { callee, fi } => Known::Call(callee, fi),
                    Site::Ctor { class, .. } => Known::Ctor(Val::heap(class)),
                    Site::Method { .. } => Known::Method,
                    _ => Known::Other,
                });
                // A call that meets a function no head lays asks for one before it runs, a stub's for its head.
                if code.block.is_some() && ($ins.op == OpCode::CallR || $ins.op == OpCode::CallGlobalR) && self.budget != 0 && let c = $callee && match self.heap.try_get(c) { Some(HeapObj::Func(..) | HeapObj::Class(..)) => true, Some(HeapObj::BoundUserMethod(..)) => $ins.op == OpCode::CallR, _ => false } {
                    let grows = self.site_grows(code, chunk, i, c);
                    r = &mut self.regs[base..];
                    if grows { break; }
                }
                // A head's stub outside every section counts the functions it meets toward laying them beside the first, a recursion none.
                if ($ins.op == OpCode::CallR || $ins.op == OpCode::CallGlobalR && code.block.is_some()) && code.misses.get() < lower::MISSES && !code.unchanged()
                    && !matches!(known, Known::Call(c, fi) if c.0 == $callee.0 && self.functions.get(fi as usize).is_some_and(|f| core::ptr::eq(&f.1, chunk))) { note_alt(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, i, $callee, matches!(known, Known::Call(c, _) if c.0 == $callee.0)); }
                let opened = match known {
                    Known::Call(callee, fi) if $ins.op != OpCode::CallMethodR && $callee.0 == callee.0 => {
                        if lower::wide($ins) { self.enter_wide(i, code, chunk, fi as usize, callee) }
                        else { let args = [reg!($ins.b), reg!($ins.c)]; self.enter_block(i, code, fi as usize, callee, &args[..$ins.x as usize], false) }
                    }
                    // Another function than the one the site knows opens the same way, its body found off the function itself.
                    Known::Call(..) if $ins.op != OpCode::CallMethodR && let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get($callee) && opens_in_loop(&self.call_facts, &self.is_async, &self.pools, fi, $ins.x as usize) => {
                        let (callee, args) = ($callee, [reg!($ins.b), reg!($ins.c)]);
                        if lower::wide($ins) { self.enter_wide(i, code, chunk, fi, callee) }
                        else { self.enter_block(i, code, fi, callee, &args[..$ins.x as usize], false) }
                    }
                    Known::Ctor(class) if $ins.op != OpCode::CallMethodR && $callee.0 == class.0 && !lower::wide($ins) => {
                        let passed = [reg!($ins.b), reg!($ins.c)];
                        self.enter_ctor(i, code, cache.site(i), passed, $ins.x as usize)
                    }
                    Known::Method if $ins.op == OpCode::CallMethodR => {
                        if lower::wide($ins) { self.enter_wide(i, code, named, 0, Val::undef()) }
                        else { let args = [reg!($ins.b), reg!($ins.c)]; self.enter_method(i, code, named, $ins.a, &args[..$ins.x as usize + 1], false) }
                    }
                    _ => None,
                };
                match opened {
                    Some((c, k)) => {
                        // SAFETY the callee code lives in its pool and its chunk as long as the VM.
                        (code, chunk) = unsafe { (&*c, &*k) };
                        (cache, prog, switched) = (&code.cache, &code.$ins, true);
                        n = prog.len();
                        goto!(code.lowered(0));
                    }
                    None => match self.call_aside($ins, i, code, named) {
                        Some(next) => { goto!(next); if self.yielded { break; } }
                        None => break,
                    },
                }
                base = self.base; r = &mut self.regs[base..];
                debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
            }};
        }
        loop {
            // SAFETY `p` lies inside `prog` or on its sentinel.
            let ins = unsafe { *p };
            // SAFETY as for `ins`, the opcode read as its own byte so the switch takes no range check before its table.
            match unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*p).op)) } {
                OpCode::AddR | OpCode::InPlaceAddR => arith!(ins, int_add, |a: f64, b: f64| Some(Val::float(a + b))),
                OpCode::SubR | OpCode::InPlaceSubR => arith!(ins, int_sub, |a: f64, b: f64| Some(Val::float(a - b))),
                OpCode::MulR => arith!(ins, int_mul, |a: f64, b: f64| Some(Val::float(a * b))),
                OpCode::DivR => arith!(ins, int_div, float_div),
                OpCode::ModR => arith!(ins, int_mod, |_, _| None),
                OpCode::FloorDivR => arith!(ins, int_floordiv, |_, _| None),
                // Integer bit operations stay in the loop, anything else leaves for the handler.
                OpCode::BitAndR | OpCode::BitOrR | OpCode::BitXorR | OpCode::ShlR | OpCode::ShrR => {
                    let (x, y) = (reg!(ins.b), reg!(ins.c));
                    let v = if x.is_int() && y.is_int() { int_bits(ins.op, x.as_int(), y.as_int()) } else { None };
                    let Some(v) = v else { break };
                    set!(ins.a, v);
                    p = unsafe { p.add(1) };
                }
                // Two strings compare their text here, any other pair past numbers leaves for the handler.
                OpCode::EqR | OpCode::NotEqR => {
                    let (x, y) = (reg!(ins.b), reg!(ins.c));
                    let eq = if x.is_int() && y.is_int() { x.as_int() == y.as_int() }
                        else if x.is_float() && y.is_float() { x.as_float() == y.as_float() }
                        else { match self.heap.str_eq(x, y) { Some(e) => e, None => break } };
                    set!(ins.a, Val::bool(eq == (ins.op == OpCode::EqR)));
                    p = unsafe { p.add(1) };
                }
                OpCode::LtR => compare!(ins, <),
                OpCode::LtEqR => compare!(ins, <=),
                OpCode::GtR => compare!(ins, >),
                OpCode::GtEqR => compare!(ins, >=),
                OpCode::JumpUnlessEq | OpCode::JumpUnlessNotEq => {
                    let (x, y) = (reg!(ins.b), reg!(ins.c));
                    let eq = if x.is_int() && y.is_int() { x.as_int() == y.as_int() }
                        else if x.is_float() && y.is_float() { x.as_float() == y.as_float() }
                        else { match self.heap.str_eq(x, y) { Some(e) => e, None => break } };
                    branch!(ins, eq == (ins.op == OpCode::JumpUnlessEq))
                }
                OpCode::JumpUnlessLt => test!(ins, <),
                OpCode::JumpUnlessLtEq => test!(ins, <=),
                OpCode::JumpUnlessGt => test!(ins, >),
                OpCode::JumpUnlessGtEq => test!(ins, >=),
                // `x` set jumps on a true value instead.
                OpCode::JumpIfFalseR => {
                    let v = reg!(ins.b);
                    let holds = if v.is_bool() { v.as_bool() } else if v.is_int() { v.as_int() != 0 } else { break };
                    branch!(ins, holds != (ins.x != 0))
                }
                OpCode::Move => {
                    let v = reg!(ins.b);
                    if v.is_undef() { break; }
                    set!(ins.a, v);
                    p = unsafe { p.add(1) };
                }
                OpCode::PushRegs => {
                    let (a, b, c) = (reg!(ins.a), reg!(ins.b), reg!(ins.c));
                    match ins.x {
                        1 if !a.is_undef() => self.stack.push(a),
                        2 if !a.is_undef() && !b.is_undef() => self.stack.extend_from_slice(&[a, b]),
                        3 if !a.is_undef() && !b.is_undef() && !c.is_undef() => self.stack.extend_from_slice(&[a, b, c]),
                        _ => break,
                    }
                    p = unsafe { p.add(1) };
                }
                OpCode::StoreTopR => {
                    let Some(v) = self.stack.pop() else { break };
                    if ins.x == 0 { set!(ins.a, v); } else { self.scopes[code.module].set_at(ins.a as u32, v); }
                    p = unsafe { p.add(1) };
                }
                OpCode::PopTop => {
                    if self.stack.pop().is_none() { break; }
                    p = unsafe { p.add(1) };
                }
                OpCode::IsR | OpCode::IsNotR => {
                    let (x, y) = (reg!(ins.b), reg!(ins.c));
                    if x.is_undef() || y.is_undef() { break; }
                    set!(ins.a, Val::bool((x.0 == y.0) == (ins.op == OpCode::IsR)));
                    p = unsafe { p.add(1) };
                }
                OpCode::LoadGlobalR => {
                    // SAFETY every module a code names stays in `scopes`.
                    let scope = unsafe { self.scopes.get_unchecked(code.module) };
                    let v = match scope.at(ins.b as u32) { v if v.is_undef() => scope.fallback(ins.b as u32), v => v };
                    if v.is_undef() { break; }
                    set!(ins.a, v);
                    p = unsafe { p.add(1) };
                }
                OpCode::StoreGlobalR if ins.x == 0 => {
                    let v = reg!(ins.b);
                    if v.is_undef() { break; }
                    self.scopes[code.module].set_at(ins.a as u32, v);
                    p = unsafe { p.add(1) };
                }
                OpCode::GetItemR => {
                    let (o, k) = (reg!(ins.b), reg!(ins.c));
                    if !o.is_heap() || !k.is_int() { break; }
                    let hit = match self.heap.plain(o) {
                        // SAFETY nothing writes the list while one item is copied out, so the read takes no borrow.
                        Some(HeapObj::List(v)) => match unsafe { v.try_borrow_unguarded() } { Ok(b) => index_of(k.as_int(), b.len()).map(|j| b[j]), Err(_) => None },
                        Some(HeapObj::Tuple(v)) => index_of(k.as_int(), v.len()).map(|j| v[j]),
                        Some(HeapObj::Str(_)) => self.heap.str_char(o, k.as_int()),
                        _ => None,
                    };
                    let Some(v) = hit else { break };
                    set!(ins.a, v);
                    p = unsafe { p.add(1) };
                }
                OpCode::StoreItemR => {
                    let (o, k) = (reg!(ins.a), reg!(ins.b));
                    if !o.is_heap() || !k.is_int() { break; }
                    let Some(HeapObj::List(v)) = self.heap.plain(o) else { break };
                    let mut b = v.borrow_mut();
                    let Some(j) = index_of(k.as_int(), b.len()) else { break };
                    b[j] = reg!(ins.c);
                    drop(b);
                    base = self.base; r = &mut self.regs[base..];
                    p = unsafe { p.add(1) };
                }
                // A generator a `for` steps runs as the next frame of this loop, its yields handing values straight back.
                OpCode::ForIterR if let Some(&IterFrame::Coroutine(co)) = self.iter_stack.last() => {
                    let Some((c, k, at)) = self.enter_gen(co, idx!()) else { break };
                    // SAFETY the generator code lives in its pool and its chunk as long as the VM.
                    (code, chunk) = unsafe { (&*c, &*k) };
                    (cache, prog, switched) = (&code.cache, &code.ins, true);
                    n = prog.len();
                    base = self.base; r = &mut self.regs[base..];
                    goto!(at);
                    debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
                }
                // Ranges and lists step here, the rest and an ended loop in the handler.
                OpCode::ForIterR if self.budget > 0 && !self.heap.needs_gc() => {
                    let Some(f) = self.iter_stack.last_mut() else { break };
                    let item = match f.next_ready(&self.heap) {
                        Some(v) => v,
                        None => match f {
                            // A failure sticks, so the handler meets it again and raises it.
                            IterFrame::DictWalk { .. } => match f.next_item(&mut self.heap) {
                                Ok(Some(v)) => v,
                                _ => break,
                            },
                            // A `map` or `filter` calls its function as the next frame, which the loop enters as it leaves.
                            IterFrame::Map { .. } | IterFrame::Filter { .. } => { self.step_called(idx!(), ins); break }
                            _ => break,
                        },
                    };
                    self.budget -= 1;
                    if ins.x == 0 { set!(ins.b, item); } else { self.scopes[code.module].set_at(ins.b as u32, item); }
                    p = unsafe { p.add(1) };
                }
                OpCode::GetAttrR => {
                    let o = reg!(ins.b);
                    let (heap, epoch) = (&self.heap, self.class_epoch);
                    let (found, method) = cache.with_site(idx!(), |site| match super::sites::site_value(heap, epoch, site, o) {
                        Some(v) => (Some(v), None),
                        None => (None, super::sites::site_method_id(heap, site, o)),
                    });
                    let v = match found {
                        Some(v) => v,
                        // A builtin type's method binds its receiver, the same bound object each time.
                        None => match method {
                            Some(id) => match self.heap.alloc(HeapObj::BoundMethod(o, id)) { Ok(v) => v, Err(_) => break },
                            None => break,
                        },
                    };
                    set!(ins.a, v);
                    p = unsafe { p.add(1) };
                }
                OpCode::SetAttrR => {
                    let (o, v) = (reg!(ins.a), reg!(ins.b));
                    let (heap, epoch) = (&self.heap, self.class_epoch);
                    if v.is_undef() || !cache.with_site(idx!(), |site| super::sites::site_put(heap, epoch, site, o, v)) { break; }
                    p = unsafe { p.add(1) };
                }
                // A jump to or from a head laid past the end of code that grew is no step of the program's.
                OpCode::JumpR => goto!(ins.a as usize),
                // A back-edge costs two steps, and only its handler collects or preempts.
                OpCode::Jump if (ins.a as usize) <= n
                    && self.budget >= 2
                    && ((ins.a as usize) > idx!() || ins.x != 0 || (self.preempt_left == 0 && !self.heap.needs_gc() && !code.unchanged())) => {
                    self.budget -= 1 + ((ins.a as usize) <= idx!() && ins.x == 0) as u64;
                    goto!(ins.a as usize);
                }
                OpCode::MinusR | OpCode::NotR => {
                    let x = reg!(ins.b);
                    let v = match ins.op {
                        OpCode::MinusR if x.is_int() => Val::int_checked(-x.as_int()),
                        OpCode::MinusR if x.is_float() => Some(Val::float(-x.as_float())),
                        OpCode::NotR if x.is_bool() => Some(Val::bool(!x.as_bool())),
                        OpCode::NotR if x.is_int() => Some(Val::bool(x.as_int() == 0)),
                        OpCode::NotR if x.is_none() => Some(Val::bool(true)),
                        _ => None,
                    };
                    let Some(v) = v else { break };
                    set!(ins.a, v);
                    p = unsafe { p.add(1) };
                }
                // A loop's iterator dropped, or a generator section's mark over its own, runs apart.
                OpCode::PopIter => {
                    self.pop_iter(ins);
                    base = self.base; r = &mut self.regs[base..];
                    p = unsafe { p.add(1) };
                }
                // Probing a key, a length or a cell runs apart, keeping this loop small.
                OpCode::InR | OpCode::NotInR | OpCode::LenR | OpCode::LoadCellR | OpCode::StoreCellR => {
                    if !self.heap_fast(ins) { break; }
                    base = self.base; r = &mut self.regs[base..];
                    p = unsafe { p.add(1) };
                }
                // A call its site knows opens its frame and runs on here, any other leaves aside.
                OpCode::CallR | OpCode::CallMethodR => call!(ins, reg!(ins.a)),
                OpCode::CallGlobalR => call!(ins, global_callee(&self.scopes, code, ins)),
                // A plain call of a binding's section, the most of them, checks only that no binding changed.
                OpCode::CallGlobalSectionR => {
                    // SAFETY as for CallSectionR.
                    let (s, x) = unsafe { (*p.add(1), *p.add(2)) };
                    let block = unsafe { code.block.as_deref().unwrap_unchecked() };
                    if super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed) != block.binds.get() && !sections_hold(&self.scopes, block) {
                        goto!(unsafe { (*p.add(3)).a } as usize);
                        continue;
                    }
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    let at = x.b as usize;
                    set!(at, reg!(ins.b));
                    set!(at + 1, reg!(ins.c));
                    self.budget = left;
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // A call through a register enters the section of the function it holds, any other its stub.
                OpCode::CallValueSectionR => {
                    // SAFETY as for CallSectionR.
                    let (s, x) = unsafe { (*p.add(1), *p.add(2)) };
                    let block = unsafe { code.block.as_deref().unwrap_unchecked() };
                    // Another function the call met enters the section its head's table finds for it, any other the stub.
                    if reg!(ins.a).0 != unsafe { block.sections.get_unchecked(x.c as usize) }.value.get().0 { let to = self.enter_value_alt(code, idx!()); r = &mut self.regs[base..]; goto!(to); continue; }
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    let at = x.b as usize;
                    set!(at, reg!(ins.b));
                    set!(at + 1, reg!(ins.c));
                    self.budget = left;
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // A method's section holds while the receiver is of the class it last held for and no class changed.
                OpCode::CallMethodSectionR => {
                    // SAFETY as for CallSectionR.
                    let (s, x) = unsafe { (*p.add(1), *p.add(2)) };
                    let block = unsafe { code.block.as_deref().unwrap_unchecked() };
                    let sec = unsafe { block.sections.get_unchecked(x.c as usize) };
                    let (class, epoch, last) = sec.guard.get();
                    let recv = reg!(ins.b);
                    let held = epoch == self.class_epoch && (recv.0 == last.0 || matches!(self.heap.try_get(recv), Some(HeapObj::Instance(c, _)) if c.0 == class.0));
                    // Any other receiver enters the section laid for the method it finds, or the stub.
                    if !held { let to = self.enter_method_alt(code, chunk, idx!()); r = &mut self.regs[base..]; if to == GROW { break; } goto!(to); continue; }
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    let at = x.b as usize;
                    set!(at, recv);
                    set!(at + 1, reg!(ins.c));
                    self.budget = left;
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // A class's call makes its instance, the first parameter, while its binding holds and the class is as it was laid for.
                OpCode::CallCtorSectionR => {
                    // SAFETY as for CallSectionR.
                    let (s, x) = unsafe { (*p.add(1), *p.add(2)) };
                    let block = unsafe { code.block.as_deref().unwrap_unchecked() };
                    if super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed) != block.binds.get() && !sections_hold(&self.scopes, block) {
                        goto!(unsafe { (*p.add(3)).a } as usize);
                        continue;
                    }
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    let sec = unsafe { block.sections.get_unchecked(x.c as usize) };
                    let holds = sec.ctor.as_ref().is_some_and(|c| c.0.get() == self.class_epoch || ctor_holds(&self.heap, &self.mro_cache, sec, self.class_epoch));
                    let Some(inst) = sec.ctor.as_ref().filter(|_| holds).and_then(|c| new_object(&mut self.heap, sec.value.get(), c.1)) else { goto!(unsafe { (*p.add(3)).a } as usize); continue; };
                    let at = x.b as usize;
                    set!(at, inst);
                    set!(at + 1, reg!(ins.b));
                    self.budget = left;
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // The same with the receiver read from a module binding, written to its register only for what checks it aside.
                OpCode::CallMethodGlobalSectionR => {
                    // SAFETY as for CallSectionR, and every module a code names stays in `scopes`.
                    let (s, x, t) = unsafe { (*p.add(1), *p.add(2), *p.add(3)) };
                    let block = unsafe { code.block.as_deref().unwrap_unchecked() };
                    let sec = unsafe { block.sections.get_unchecked(x.c as usize) };
                    let (class, epoch, last) = sec.guard.get();
                    let recv = unsafe { self.scopes.get_unchecked(code.module) }.at(t.c as u32 - 1);
                    let held = epoch == self.class_epoch && (recv.0 == last.0 || matches!(self.heap.try_get(recv), Some(HeapObj::Instance(c, _)) if c.0 == class.0));
                    // An unbound one goes to the stub, which reads it again.
                    if !held {
                        if recv.is_undef() { goto!(t.a as usize); continue; }
                        set!(ins.b, recv);
                        let to = self.enter_method_alt(code, chunk, idx!());
                        r = &mut self.regs[base..];
                        if to == GROW { break; }
                        goto!(to);
                        continue;
                    }
                    let Some(left) = self.budget.checked_sub(1) else { set!(ins.b, recv); break };
                    let at = x.b as usize;
                    set!(at, recv);
                    set!(at + 1, reg!(ins.c));
                    self.budget = left;
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // A call of a section jumps to it while its binding holds the function the section was laid from.
                OpCode::CallSectionR => {
                    // SAFETY a CallSectionR is followed by its StoreTopR (the function's heap index) and SectionR, and every module a code names stays in `scopes`.
                    let (s, x) = unsafe { (*p.add(1), *p.add(2)) };
                    // SAFETY code holding a CallSectionR laid its block, with section `x.c`.
                    let block = unsafe { code.block.as_deref().unwrap_unchecked() };
                    if x.x & 8 != 0 {
                        // A method a `super()` proxy finds is checked apart.
                        if x.x & 64 != 0 { break; }
                        // A method's section holds while the receiver's class finds the method laid and no class changed, the name read only once an instance may shadow it.
                        let sec = unsafe { block.sections.get_unchecked(x.c as usize) };
                        let (class, epoch, _) = sec.guard.get();
                        let class_of = |heap: &HeapPool, v: Val| match heap.try_get(v) { Some(HeapObj::Instance(c, _)) => *c, _ => Val::undef() };
                        // Any other receiver enters the section laid for the method it finds, or the stub.
                        if epoch != self.class_epoch || self.shadowed || class_of(&self.heap, reg!(ins.b)).0 != class.0 {
                            let to = self.enter_method_alt(code, chunk, idx!());
                            r = &mut self.regs[base..];
                            if to == GROW { break; }
                            goto!(to);
                            continue;
                        }
                    } else if x.x & 4 != 0 {
                        // Any function of the body the section was laid from enters it apart, its cells and defaults filled from it.
                        if reg!(ins.a).0 != unsafe { block.sections.get_unchecked(x.c as usize) }.value.get().0 {
                            let to = if x.x & 32 != 0 { self.enter_section_of_body(code, idx!()) } else { None };
                            r = &mut self.regs[base..];
                            goto!(to.unwrap_or(unsafe { (*p.add(3)).a } as usize));
                            continue;
                        }
                    } else if super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed) != block.binds.get() && !sections_hold(&self.scopes, block) {
                        goto!(unsafe { (*p.add(3)).a } as usize);
                        continue;
                    }
                    // A call pays a step as any call does, so a recursion through sections stays in the budget.
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    // Every section has two parameters at least, and an argument a call does not pass names a register of its own.
                    let at = x.b as usize;
                    if x.x & 16 == 0 {
                        set!(at, reg!(ins.b));
                        set!(at + 1, reg!(ins.c));
                    } else {
                        // A class's call makes its instance while the class is as it was laid for, the first parameter.
                        let s = unsafe { block.sections.get_unchecked(x.c as usize) };
                        let holds = s.ctor.as_ref().is_some_and(|c| c.0.get() == self.class_epoch || ctor_holds(&self.heap, &self.mro_cache, s, self.class_epoch));
                        let Some(inst) = s.ctor.as_ref().filter(|_| holds).and_then(|c| new_object(&mut self.heap, s.value.get(), c.1)) else { goto!(unsafe { (*p.add(3)).a } as usize); continue; };
                        set!(at, inst);
                        set!(at + 1, reg!(ins.b));
                    }
                    self.budget = left;
                    // The link holds where the call carries on and the register for its result.
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // A stub's lookup of the section laid for the function its call makes.
                OpCode::CallValueR => {
                    let to = self.enter_value(code, ins);
                    r = &mut self.regs[base..];
                    match to { Some(to) => goto!(to), None => p = unsafe { p.add(1) } }
                }
                // A section's return hands its value to the register the call's StoreTopR names and carries on past the call.
                OpCode::ReturnSectionR => {
                    let (link, v) = (reg!(ins.a).as_int() as usize, reg!(ins.b));
                    if v.is_undef() { break; }
                    set!(link & 0xFFFF, v);
                    goto!(link >> 16);
                }
                OpCode::ReturnBoundR => {
                    let link = reg!(ins.a).as_int() as usize;
                    set!(link & 0xFFFF, reg!(ins.b));
                    goto!(link >> 16);
                }
                // A return from inside a section's `try` leaves it through its handlers.
                OpCode::ReturnTriedR => {
                    let (link, v) = (reg!(ins.a).as_int() as usize, reg!(ins.b));
                    if v.is_undef() || self.exception_stack.last().is_some_and(|f| f.handler_ip & RAW_HANDLER != 0) { break; }
                    set!(link & 0xFFFF, v);
                    goto!(link >> 16);
                }
                // An op's dunder section call jumps in for its last class on the left with a number, that class or nothing to check on the right, anything else checked aside.
                OpCode::DunderSectionR => {
                    // SAFETY a DunderSectionR is followed by its StoreTopR and its SectionR, and the code holding it laid its block.
                    let (s, x) = unsafe { (*p.add(1), *p.add(2)) };
                    let sec = unsafe { code.block.as_deref().unwrap_unchecked().sections.get_unchecked(x.c as usize) };
                    let (epoch, held, ..) = sec.dunder.get();
                    let (a, b) = (reg!(ins.b), reg!(ins.c));
                    let heap = &self.heap;
                    let of_held = |v: Val| matches!(heap.try_get(v), Some(&HeapObj::Instance(c, _)) if c.0 == held.0);
                    let (seen, then) = sec.seen.get();
                    let left = then == self.class_epoch && a.0 == seen.0 || epoch == self.class_epoch && of_held(a);
                    if ins.a & !2 == 0 && self.budget > 0 && left && (ins.a == 2 || !b.is_undef() && (sec.lone.get() || !b.is_heap() || b.0 == a.0 || of_held(b))) {
                        self.budget -= 1;
                        let at = x.b as usize;
                        set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                        set!(at, a);
                        set!(at + 1, b);
                        goto!(x.a as usize);
                    } else {
                        match self.regs_aside(ins, n, idx!(), code, chunk) {
                            Some(next) => { goto!(next); base = self.base; r = &mut self.regs[base..]; }
                            None => break,
                        }
                    }
                }
                // A call back into the sections it came from keeps a recursion level first, looking aside once the stack passes its last stretch or frames opened or closed since.
                OpCode::TurnR => {
                    // SAFETY as for `ins`, its `x` read apart so the switch reads no more of each instruction than its opcode.
                    let x = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*p).x)) };
                    let len = self.stack.len();
                    if len.wrapping_sub(self.turn_low) >= self.turn_span || self.acts != self.turn_acts { break; }
                    self.turned += 1;
                    if x & lower::LONE_TURN != 0 {
                        // SAFETY a TurnR names the words its lowering laid past its call, and the stack holds room for the link and two values past where it may stand.
                        let (w, top) = unsafe { (*prog.get_unchecked(ins.a as usize), self.stack.as_mut_ptr().add(len)) };
                        let (mut at, b, c, link) = (0, reg!(w.b), reg!(w.c), reg!(w.a));
                        if w.x & 3 != 0 { unsafe { top.write(b) }; at = 1; }
                        if w.x & 3 == 2 { unsafe { top.add(1).write(c) }; at = 2; }
                        unsafe { top.add(at).write(link) };
                        unsafe { self.stack.set_len(len + at + 1) };
                    } else {
                        super::level::turn(r, &mut self.stack, unsafe { prog.get_unchecked(ins.a as usize..ins.a as usize + ins.b as usize) }, (x & lower::COUNT) as usize, ins.c);
                    }
                    p = unsafe { p.add(1) };
                }
                // Past that call the level comes back, every word of it at once.
                OpCode::UnturnR => {
                    // SAFETY as for `ins`.
                    let x = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*p).x)) };
                    let len = self.stack.len();
                    if x & lower::LONE != 0 && len >= 3 {
                        // SAFETY the level's values lie on the stack, three at most, a section noting no base keeping nothing past the link's own bits.
                        let (top, mut sp) = (self.stack.as_ptr(), len - 1);
                        set!(ins.a, unsafe { *top.add(sp) });
                        if x & 3 == 2 { sp -= 1; set!(ins.c, unsafe { *top.add(sp) }); }
                        if x & 3 != 0 { sp -= 1; set!(ins.b, unsafe { *top.add(sp) }); }
                        unsafe { self.stack.set_len(sp) };
                        // Every level taken back was counted as it was kept.
                        self.turned = self.turned.wrapping_sub(1);
                        p = unsafe { p.add(1) };
                    } else {
                        let at = idx!();
                        // SAFETY `at` lies inside `prog`.
                        let (start, _, k) = super::level::take_level(r, &self.stack, unsafe { prog.get_unchecked(at..) }, len);
                        self.stack.truncate(start);
                        self.turned = self.turned.saturating_sub(1);
                        goto!(at + k);
                    }
                }
                // A plain return to a frame of this loop carries on in the caller, a stack return taking its value off the stack.
                OpCode::ReturnR | OpCode::ReturnValue if self.acts > floor.depth => {
                    let v = if ins.op == OpCode::ReturnR { reg!(ins.b) } else {
                        if self.stack.len() > self.top_stack_base() { self.stack.pop().unwrap_or_else(Val::undef) } else { break }
                    };
                    let Some((c, k, next)) = self.leave_site(v, floor.code, floor.chunk, floor.pool, floor.depth) else {
                        if ins.op == OpCode::ReturnValue { self.stack.push(v); }
                        break
                    };
                    // SAFETY the caller runs code its pool or the floor keeps, and every chunk outlives the run.
                    (code, chunk) = unsafe { (&*c, &*k) };
                    (cache, prog, switched) = (&code.cache, &code.ins, true);
                    n = prog.len();
                    base = self.base; r = &mut self.regs[base..];
                    goto!(next);
                    debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
                }
                // A string the register owns grows in place, anything else takes the handler, which adds and may take ownership.
                OpCode::AppendR => {
                    let (sv, tv) = (reg!(ins.a), reg!(ins.b));
                    // Numbers add here as `InPlaceAddR` does.
                    let sum = if sv.is_int() && tv.is_int() { int_add(sv.as_int(), tv.as_int()) }
                        else if sv.is_float() && tv.is_float() { Some(Val::float(sv.as_float() + tv.as_float())) }
                        else { None };
                    if let Some(v) = sum { set!(ins.a, v); p = unsafe { p.add(1) }; continue; }
                    if sv.0 != self.owned_str.0 || self.owned_at != base + ins.a as usize || !self.heap.append_str(sv, tv) { break; }
                    p = unsafe { p.add(1) };
                }
                OpCode::OwnResetR => {
                    if self.owned_at == base + ins.a as usize { self.owned_str = Val::undef(); }
                    p = unsafe { p.add(1) };
                }
                // An unbound item leaves for the handler, which names it.
                OpCode::ListPushR => {
                    let v = reg!(ins.b);
                    let Some(HeapObj::List(rc)) = self.heap.try_get(reg!(ins.a)) else { break };
                    if v.is_undef() { break; }
                    rc.borrow_mut().push(v);
                    p = unsafe { p.add(1) };
                }
                // The blocks of a `try` and a `with`, and a `with` whose site knows its manager's class, stay here, the handler the compiler's index or for `x` 1 the code's own.
                OpCode::SetupExcept | OpCode::SetupFinally => {
                    self.exception_stack.push(ExceptionFrame {
                        kind: if ins.op == OpCode::SetupExcept { BlockKind::Except } else { BlockKind::Finally },
                        handler_ip: ins.a as usize | if ins.x == 1 { RAW_HANDLER } else { 0 },
                        stack_depth: self.stack.len(),
                        iter_depth: self.iter_stack.len(),
                        with_depth: self.with_stack.len(),
                        unwind_depth: self.unwind_stack.len(),
                        handling_depth: self.handling.len(),
                        turned: self.turned,
                    });
                    p = unsafe { p.add(1) };
                }
                OpCode::WithEnter if ins.x == 2 => {
                    let cm = reg!(ins.c);
                    let (heap, epoch) = (&self.heap, self.class_epoch);
                    let Some((func, exit)) = cache.with_site(idx!(), |site| with_func(heap, epoch, site, cm)) else { break };
                    self.with_stack.extend_from_slice(&[cm, exit, Val::none()]);
                    let at = ins.b as usize;
                    set!(at, func);
                    set!(at + 1, cm);
                    p = unsafe { p.add(1) };
                }
                // A fall-through stages `__exit__` with three Nones and skips the WithExit after, the handler of the other exits.
                OpCode::WithExit if ins.x == 2 => {
                    // SAFETY a fused exit is followed by its WithExit.
                    let next = unsafe { *p.add(1) };
                    // The `__exit__` its entry found, unless it names `super`.
                    let len = self.with_stack.len();
                    let Some(&[cm, func, tag]) = self.with_stack.get(len.wrapping_sub(3)..) else { break };
                    if !tag.is_none() { break; }
                    if let Some(f) = self.exception_stack.pop() { self.handling.truncate(f.handling_depth); }
                    self.unwind_stack.push(Unwind::Normal);
                    self.with_stack.truncate(len - 3);
                    let (at, to) = (next.b as usize, next.c as usize);
                    set!(at, func);
                    set!(at + 1, cm);
                    for k in 2..5 { set!(at + k, Val::none()); }
                    if to != 0 { set!(to, Val::none()); set!(to + 1, Val::none()); }
                    p = unsafe { p.add(2) };
                }
                // A fall-through's `__exit__` decides nothing, its mark ending with the EndFinally after.
                OpCode::WithJudge if ins.x == 1 => {
                    // SAFETY a judge from a register is followed by its EndFinally.
                    let len = self.unwind_stack.len();
                    if !matches!(self.unwind_stack.last(), Some(Unwind::Normal)) || unsafe { (*p.add(1)).op } != OpCode::EndFinally { break; }
                    // A normal mark holds nothing to drop.
                    self.unwind_stack.truncate(len - 1);
                    p = unsafe { p.add(2) };
                }
                // A generator a builtin runs hands it its value and runs on in place, or `next` takes it and ends.
                OpCode::Yield if self.acts > floor.depth && self.top_gather() && self.gen_yields(ins.x == 1) => {
                    let Some((c, k, next)) = self.gather_yield(ins, idx!(), code, floor) else { break };
                    // The frame running on in place gives no code.
                    if !c.is_null() {
                        // SAFETY the caller runs code its pool or the floor keeps, and every chunk outlives the run.
                        (code, chunk) = unsafe { (&*c, &*k) };
                        (cache, prog, switched) = (&code.cache, &code.ins, true);
                        n = prog.len();
                    }
                    base = self.base; r = &mut self.regs[base..];
                    goto!(next);
                }
                // A generator a `for` runs as a frame hands its value back here, its value position taking None.
                OpCode::Yield if self.acts > floor.depth && self.gen_yields_to_for(ins.x == 1) => {
                    let i = idx!();
                    let (v, resume) = if ins.x == 1 {
                        // SAFETY as for `reg!`, the frame's slice given up by the guard.
                        let v = unsafe { *self.regs.get_unchecked(base + ins.a as usize) };
                        if v.is_undef() { break; }
                        // The next step starts past the jump after it, which the lowering found.
                        (v, ins.b as usize)
                    } else {
                        let v = self.stack.pop().unwrap_or_else(Val::none);
                        let next_pops = prog.get(i + 1).is_some_and(|n| n.op == OpCode::PopTop);
                        if !next_pops { self.stack.push(Val::none()); }
                        (v, code.resume(i + 1 + next_pops as usize))
                    };
                    let Some((c, k, next)) = self.yield_site(v, resume, floor.code, floor.chunk, floor.pool, floor.depth) else { break };
                    // SAFETY the caller runs code its pool or the floor keeps, and every chunk outlives the run.
                    (code, chunk) = unsafe { (&*c, &*k) };
                    (cache, prog, switched) = (&code.cache, &code.ins, true);
                    n = prog.len();
                    base = self.base; r = &mut self.regs[base..];
                    goto!(next);
                    debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
                }
                // A plain function the stack calls with all its positionals opens its block in place, its arguments off the stack.
                OpCode::Call if ins.a <= 5 && let Some(at) = self.stack.len().checked_sub(ins.a as usize + 1)
                    && let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(self.stack[at])
                    && self.call_facts[fi].plain && opens_in_loop(&self.call_facts, &self.is_async, &self.pools, fi, ins.a as usize) => {
                    let (i, callee, k) = (idx!(), self.stack[at], ins.a as usize);
                    let mut args = [Val::undef(); 5];
                    args[..k].copy_from_slice(&self.stack[at + 1..]);
                    self.stack.truncate(at);
                    match self.enter_block(i, code, fi, callee, &args[..k], false) {
                        Some((c, ch)) => {
                            // Unlowered code learns what it calls, so a lowering after lays it.
                            if code.unchanged() { code.cache.set_site(i, Site::Call { callee, fi: fi as u32 }); }
                            // SAFETY the callee code lives in its pool and its chunk as long as the VM.
                            (code, chunk) = unsafe { (&*c, &*ch) };
                            (cache, prog, switched) = (&code.cache, &code.ins, true);
                            n = prog.len();
                            goto!(code.lowered(0));
                        }
                        None => {
                            self.stack.push(callee);
                            self.stack.extend_from_slice(&args[..k]);
                            match self.call_aside(ins, i, code, chunk) {
                                Some(next) => { goto!(next); if self.yielded { break; } }
                                None => break,
                            }
                        }
                    }
                    base = self.base; r = &mut self.regs[base..];
                    debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
                }
                // Calls stay in this loop, leaving when one fails or parks its callee.
                OpCode::Call | OpCode::CallMethod => match self.call_aside(ins, idx!(), code, chunk) {
                    Some(next) => { goto!(next); if self.yielded { break; } base = self.base; r = &mut self.regs[base..]; }
                    None => break,
                },
                // A stack loop over a range, a list or a ready builtin iterator steps here, inside a section too, the rest and an ended loop in the handler.
                OpCode::ForIter if self.budget > 0 && !self.heap.needs_gc() => {
                    let Some(f) = self.iter_stack.last_mut() else { break };
                    let item = match f.next_ready(&self.heap) {
                        Some(v) => v,
                        None => match f {
                            // A dict that changed fails here as the handler's step would, an ended walk ending there.
                            IterFrame::DictWalk { .. } => match f.next_item(&mut self.heap) {
                                Ok(Some(v)) => v,
                                Ok(None) => break,
                                Err(e) => { self.budget -= 1; self.reg_error = Some(e); break }
                            },
                            &mut IterFrame::UserDefined(it) => match self.ready_next(it) {
                                Some(Ok(v)) => v,
                                // A step that failed past taking its item raises here, as the handler's would.
                                Some(Err(e)) => { self.budget -= 1; self.reg_error = Some(e); break }
                                None => break,
                            },
                            _ => break,
                        },
                    };
                    self.budget -= 1;
                    self.stack.push(item);
                    r = &mut self.regs[base..];
                    p = unsafe { p.add(1) };
                }
                // Names and constants of code that lays no sections move between the stack and its registers here.
                OpCode::LoadName | OpCode::StoreName | OpCode::LoadConst if code.block.is_none() => {
                    if !self.stack_name(ins, code) { break; }
                    base = self.base; r = &mut self.regs[base..];
                    p = unsafe { p.add(1) };
                }
                op if aside_op(op) => match self.regs_aside(ins, n, idx!(), code, chunk) {
                    Some(next) => { goto!(next); base = self.base; r = &mut self.regs[base..]; }
                    None => break,
                },
                _ => break,
            }
        }
        (*ip, *code_at, *chunk_at) = (idx!(), code, chunk);
        switched
    }

    /* The next item of builtin iterator `it` as `iter_step` gives it, for one running no user code (no `map`, `filter` or callable iterator), else None, nothing taken. */
    #[inline(never)]
    fn ready_next(&mut self, it: Val) -> Option<Result<Val, VmErr>> {
        let frame = iter_cell(&self.heap, it)?;
        // SAFETY the iterator is live, and a step that runs no user code frees no object.
        let mut f = unsafe { &*frame }.try_borrow_mut().ok()?;
        match &*f {
            IterFrame::Map { .. } | IterFrame::Filter { .. } | IterFrame::Call { .. } => None,
            IterFrame::Zip { .. } | IterFrame::Enumerate { .. } => {
                let mut seen = ([core::ptr::null(); 8], 1);
                seen.0[0] = frame;
                if !self.tree_ready(&f, &mut seen, 0) { return None; }
                Some(self.take_tree(&mut f))
            }
            // A spent one ends in the handler, whose step ends it again.
            _ => match f.next_item(&mut self.heap) { Ok(Some(v)) => Some(Ok(v)), Ok(None) => None, Err(e) => Some(Err(e)) },
        }
    }

    /* Whether a step of `f`, a builtin iterator `level` steps deep, gives an item with no user code, end or failure, each iterator stepped once and kept in `seen`. */
    fn tree_ready(&self, f: &IterFrame, seen: &mut ([*const RefCell<IterFrame>; 8], usize), level: usize) -> bool {
        let leaf = |v: Val, seen: &mut ([*const RefCell<IterFrame>; 8], usize)| {
            let Some(g) = iter_cell(&self.heap, v) else { return false };
            if seen.1 == seen.0.len() || seen.0[..seen.1].contains(&g) { return false; }
            (seen.0[seen.1], seen.1) = (g, seen.1 + 1);
            // SAFETY as in `ready_next`.
            unsafe { &*g }.try_borrow().is_ok_and(|g| self.tree_ready(&g, seen, level + 1))
        };
        match f {
            IterFrame::Enumerate { it, n } => self.depth + level < self.max_calls && n.is_int() && n.as_int() < Val::INT_MAX && leaf(*it, seen),
            IterFrame::Zip { its, strict: false } => self.depth + level < self.max_calls && !its.is_empty() && its.iter().all(|&v| leaf(v, seen)),
            f => f.ready(&self.heap).is_some(),
        }
    }

    /* The step of `f` that `tree_ready` found ready, its iterators stepped in the handler's order. */
    fn take_tree(&mut self, f: &mut IterFrame) -> Result<Val, VmErr> {
        let next = |vm: &mut Self, v: Val| -> Result<Val, VmErr> {
            let g = iter_cell(&vm.heap, v).ok_or_else(|| cold_runtime("iterator gone"))?;
            // SAFETY as in `ready_next`, every iterator of the tree a distinct one.
            vm.take_tree(&mut unsafe { &*g }.borrow_mut())
        };
        match f {
            IterFrame::Enumerate { it, n } => {
                let item = next(self, *it)?;
                let count = *n;
                *n = Val::int(count.as_int() + 1);
                self.heap.alloc(HeapObj::Tuple(alloc::vec![count, item]))
            }
            IterFrame::Zip { its, .. } => {
                let its = its.clone();
                let mut items = Vec::with_capacity(its.len());
                for &v in its.iter() { items.push(next(self, v)?); }
                self.heap.alloc(HeapObj::Tuple(items))
            }
            f => Ok(f.next_ready(&self.heap).unwrap_or(Val::none())),
        }
    }

    /* Where the op's dunder section call at `i` goes, the section with its operands as parameters while they are as the stub found them, else the stub, None on a spent budget. */
    #[inline(always)]
    fn dunder_section(&mut self, code: &Code, chunk: &SSAChunk, i: usize) -> Option<usize> {
        debug_assert!(i + 3 < code.ins.len() && self.regs.len() >= self.base + code.frame);
        // SAFETY a DunderSectionR is followed by its StoreTopR, SectionR and its stub's SectionR, its code laid section `x.c`, and every register it names lies in the frame.
        let (ins, s, x, t, block) = unsafe { (*code.ins.get_unchecked(i), *code.ins.get_unchecked(i + 1), *code.ins.get_unchecked(i + 2), *code.ins.get_unchecked(i + 3), code.block.as_deref().unwrap_unchecked()) };
        // SAFETY as above.
        let (a, b) = unsafe { (*self.regs.get_unchecked(self.base + ins.b as usize), *self.regs.get_unchecked(self.base + ins.c as usize)) };
        // A `for`'s step calls the `__next__` of the user iterator it walks, collecting first in its handler.
        let (a, b) = if ins.a != 4 { (a, b) } else {
            match self.iter_stack.last() { Some(&IterFrame::UserDefined(it)) if !self.heap.needs_gc() => (it, Val::undef()), _ => return Some(t.a as usize) }
        };
        let sec = unsafe { block.sections.get_unchecked(x.c as usize) };
        if !self.dunder_holds_at(code, chunk, sec, ins.a, t, a, b) { let to = self.dunder_missed(code, chunk, i); return (to != GROW).then_some(to); }
        if self.budget == 0 { return None; }
        self.budget -= 1;
        let at = x.b as usize;
        let r = unsafe { self.regs.get_unchecked_mut(self.base..) };
        // SAFETY the section's link and parameters lie within the frame, as its outer register does.
        unsafe {
            *r.get_unchecked_mut(at - 1) = Val::int((s.b as i64) << 16 | s.a as i64);
            *r.get_unchecked_mut(at) = a;
            *r.get_unchecked_mut(at + 1) = b;
        }
        if ins.a == 4 { self.next_before = self.pending.exc_val; }
        Some(x.a as usize)
    }

    /* Whether the op's dunder whose head returns to `ret` declined where no head lays the reflected one, asking the code to grow, which the handler does once the op ran. */
    #[cold]
    #[inline(never)]
    fn declined_grows(&mut self, code: &Code, chunk: &SSAChunk, ret: usize) -> bool {
        if self.budget == 0 || self.growing.is_some() { return false; }
        let Some(h) = ret.checked_sub(lower::HEADER as usize) else { return false };
        let (head, stub) = (code.ins[h], code.ins[ret - 1]);
        if head.op != OpCode::DunderSectionR { return false; }
        // A declining that goes to the heads laid for it, or that gives identity, needs none.
        if head.a != 6 && stub.c != 0 && code.ins.get(stub.c as usize).is_some_and(|x| x.op == OpCode::DunderSectionR && x.a == 6) { return false; }
        let Some(&run) = code.ins.get(stub.b as usize) else { return false };
        if !lower::dunder_op(run.op) || matches!(run.op, OpCode::NotEqR | OpCode::EqR) && matches!(head.a, 1 | 6) || run.op == OpCode::NotEqR { return false; }
        let (l, r) = (self.regs[self.base + run.b as usize], self.regs[self.base + run.c as usize]);
        // Operands of one class try no reflected dunder.
        if self.same_class(l, r) { return false; }
        let Some(f) = self.reflected_met(run.op, l, r, true) else { return false };
        self.asks_growth(code, chunk, h, lower::Grow::Dunder, f)
    }

    /* Whether `l` and `r` are instances of one class. */
    fn same_class(&self, l: Val, r: Val) -> bool {
        matches!((self.heap.try_get(l), self.heap.try_get(r)), (Some(&HeapObj::Instance(a, _)), Some(&HeapObj::Instance(b, _))) if a.0 == b.0)
    }

    /* The reflected dunder of instance `r` right of op `op` that runs past `l`'s, once `l`'s declined or for a subclass of `l`'s class whose own goes first. */
    fn reflected_met(&self, op: OpCode, l: Val, r: Val, declined: bool) -> Option<Val> {
        let Some(&HeapObj::Instance(rc, _)) = self.heap.try_get(r) else { return None };
        if !declined && !matches!(self.heap.try_get(l), Some(&HeapObj::Instance(lc, _)) if lc.0 != rc.0 && self.heap.is_subclass(rc, lc)) { return None; }
        let (_, rn) = super::opcodes::dunder::op_names(op)?;
        // A `!=` of a class with no `__ne__` runs its `__eq__`.
        let (f, _) = self.lookup_class_member(rc, rn).or_else(|| if op == OpCode::NotEqR { self.lookup_class_member(rc, "__eq__") } else { None })?;
        matches!(self.heap.try_get(f), Some(HeapObj::Func(..))).then_some(f)
    }

    /* Where the op goes once the dunder returning at `rip` to `ret` declined (the reflected heads, the op skipping it, or identity for `==`), None for no op's call. */
    #[inline(never)]
    fn dunder_declined(&mut self, code: &Code, rip: usize, ret: usize) -> Option<usize> {
        let head = *code.ins.get(ret.checked_sub(lower::HEADER as usize)?)?;
        if head.op != OpCode::DunderSectionR { return None; }
        let (s, stub) = (section_at(code, rip)?.value.get(), code.ins[ret - 1]);
        // A property's, a string's or a step's NotImplemented is its value.
        if matches!(code.ins.get(stub.b as usize).map(|o| o.op), Some(OpCode::GetAttrR | OpCode::SetAttrR | OpCode::CallStr | OpCode::CallRepr | OpCode::ForIterR)) { return None; }
        if head.a != 6 && stub.c != 0 && code.ins.get(stub.c as usize).is_some_and(|h| h.op == OpCode::DunderSectionR && h.a == 6) { return Some(stub.c as usize); }
        // A comparison's reflected one declined after the other's, or past a number on the left (which answers no instance), is identity.
        if matches!(head.a, 1 | 6) && let Some(o) = code.ins.get(stub.b as usize) && matches!(o.op, OpCode::EqR | OpCode::NotEqR) {
            let (l, r) = (self.regs[self.base + head.c as usize], self.regs[self.base + head.b as usize]);
            // The identity is the result, which goes on past any inversion, where the op's run goes.
            self.regs[self.base + code.ins[ret - 3].a as usize] = Val::bool((l.0 == r.0) == (o.op == OpCode::EqR));
            return code.ins.get(stub.b as usize + 1).map(|j| j.a as usize);
        }
        self.declined = s;
        if head.a == 6 && let Some(f) = code.block.as_deref().and_then(|b| b.sections.get(stub.c as usize)) { self.declined_also = f.value.get(); }
        Some(stub.b as usize)
    }

    /* Where the op's dunder section call at `i` that does not hold goes, its stub's next head or the op, skipping the first dunder once it declined. */
    #[inline(never)]
    fn dunder_turned(&mut self, code: &Code, i: usize) -> usize {
        let (head, t) = (code.ins[i], code.ins[i + 3]);
        if head.a == 6 && !code.ins.get(t.a as usize).is_some_and(|h| h.op == OpCode::DunderSectionR && h.a == 6)
            && let Some(f) = code.block.as_deref().and_then(|b| b.sections.get(t.c as usize)) { self.declined = f.value.get(); }
        t.a as usize
    }

    /* `dunder_turned` for a head its operands turned down, the last head before the op asking the code to grow by the op's dunder, GROW then, left for the handler. */
    #[cold]
    #[inline(never)]
    fn dunder_missed(&mut self, code: &Code, chunk: &SSAChunk, i: usize) -> usize {
        let to = self.dunder_turned(code, i);
        if self.budget == 0 { return to; }
        // A builtin's stub takes its operand to the stack before it runs.
        let builtin = to + 1 == code.ins[i + 3].b as usize && code.ins.get(to).is_some_and(|p| p.op == OpCode::PushRegs);
        if to != code.ins[i + 3].b as usize && !builtin { return to; }
        let Some(&run) = code.ins.get(code.ins[i + 3].b as usize) else { return to };
        // A length, truth test, `bool`, `str`, `repr` or loop over an instance of another class, its operand the head's.
        if matches!(run.op, OpCode::LenR | OpCode::NotR | OpCode::CallBool | OpCode::CallStr | OpCode::CallRepr | OpCode::GetIter) {
            let o = self.regs[self.base + code.ins[i].b as usize];
            let form = if run.op == OpCode::NotR { OpCode::JumpIfFalseR } else { run.op };
            let f = match self.heap.try_get(o) { Some(&HeapObj::Instance(c, _)) => self.op_dunder(Ins { op: form, ..run }, c, 0, chunk), _ => None };
            return match f { Some(f) if self.asks_growth(code, chunk, i, lower::Grow::Dunder, f) => GROW, _ => to };
        }
        // A property of the class of another instance a load or a store of an attribute met.
        if matches!(run.op, OpCode::GetAttrR | OpCode::SetAttrR) {
            let o = self.regs[self.base + if run.op == OpCode::GetAttrR { run.b } else { run.a } as usize];
            let f = match self.heap.try_get(o) { Some(&HeapObj::Instance(c, _)) => self.op_dunder(run, c, 0, chunk), _ => None };
            return match f { Some(f) if self.asks_growth(code, chunk, i, lower::Grow::Dunder, f) => GROW, _ => to };
        }
        if !lower::dunder_op(run.op) { return to; }
        let (l, r) = (self.regs[self.base + run.b as usize], self.regs[self.base + run.c as usize]);
        // The reflected dunder of the instance on the right once the left one's declined or for a subclass, else the left one's.
        let declined = !self.declined.is_undef();
        let f = self.reflected_met(run.op, l, r, declined).filter(|_| !self.same_class(l, r)).or_else(|| match self.heap.try_get(l) {
            Some(&HeapObj::Instance(c, _)) if !declined => self.op_dunder(run, c, 0, chunk),
            _ if l.is_int() || l.is_float() || l.is_bool() => match self.heap.try_get(r) { Some(&HeapObj::Instance(c, _)) => self.op_dunder(run, c, 1, chunk), _ => None },
            _ => None,
        });
        match f { Some(f) if self.asks_growth(code, chunk, i, lower::Grow::Dunder, f) => GROW, _ => to }
    }

    /* Where a `for` goes, past the loop with its values and iterator dropped, once its sectioned `__next__` raised StopIteration at `rip`, None for one raised elsewhere. */
    #[cold]
    #[inline(never)]
    fn next_stop(&mut self, code: &Code, rip: usize) -> Option<usize> {
        let mut found = None;
        self.walk_levels(code, rip, false, 0, |vm, s, site, outer, sp| {
            if outer.is_some() { return true; }
            found = (|| {
                let head = *code.ins.get(site)?;
                if head.op != OpCode::DunderSectionR || head.a != 4 { return None; }
                let exit = code.ins.get(code.ins.get(site + 3)?.b as usize)?.a as usize;
                // The iterator the section was called on, past any its own loops pushed.
                let it = vm.regs.get(vm.base + s.link as usize + 1).copied()?;
                let k = vm.iter_stack.iter().rposition(|f| matches!(f, IterFrame::UserDefined(v) if v.0 == it.0))
                    .or_else(|| vm.iter_stack.iter().rposition(|f| matches!(f, IterFrame::UserDefined(_))))?;
                vm.stack.truncate(sp);
                vm.iter_stack.truncate(k);
                (vm.error_byte_pos, vm.yield_from_value) = (None, vm.swallow_stop(vm.next_before));
                vm.call_stack.clear();
                Some(exit)
            })();
            false
        });
        found
    }

    /* Whether dunder section `sec` (stub SectionR `t`, reflected for 1) runs for operands `a` and `b`, `a` of a class it held or failed for, `b` letting it run first. */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn dunder_holds_at(&self, code: &Code, chunk: &SSAChunk, sec: &lower::Section, reflected: u16, t: Ins, a: Val, b: Val) -> bool {
        // The class it held for last, with a number, the same instance or another of that class on the other side, is the most.
        let (epoch, held, ..) = sec.dunder.get();
        let of_held = |v: Val| matches!(self.heap.try_get(v), Some(&HeapObj::Instance(c, _)) if c.0 == held.0);
        if epoch == self.class_epoch && of_held(a) && match reflected { 1 => b.is_int() || b.is_float() || b.is_bool(), 2 | 4 | 6 => true, 3 => self.builtins_rebound == 0, 5 | 7 => false, _ => b.0 == a.0 || !b.is_heap() && !b.is_undef() || of_held(b) } {
            return true;
        }
        self.dunder_holds_apart(code, chunk, sec, reflected, t, a, b)
    }

    /* `dunder_holds_at` past the most common case. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn dunder_holds_apart(&self, code: &Code, chunk: &SSAChunk, sec: &lower::Section, reflected: u16, t: Ins, a: Val, b: Val) -> bool {
        let Some(&HeapObj::Instance(c, _)) = self.heap.try_get(a) else { return false };
        let (epoch, held, also, missed) = sec.dunder.get();
        if epoch != self.class_epoch || c.0 != held.0 && c.0 != also.0 && c.0 != missed.0 { return self.dunder_again(code, chunk, sec, reflected, t, a, c, b); }
        c.0 != missed.0 && self.dunder_other(code, sec, reflected, t, a, c, b)
    }

    /* `dunder_holds_at` for a class the section has not met since the last class change, whether it finds the dunder laid as the op would, kept for its next instance. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn dunder_again(&self, code: &Code, chunk: &SSAChunk, sec: &lower::Section, reflected: u16, t: Ins, a: Val, c: Val, b: Val) -> bool {
        let (epoch, held, also, missed) = sec.dunder.get();
        let kept = if epoch == self.class_epoch { (held, also, missed) } else { (Val::undef(), Val::undef(), Val::undef()) };
        let finds = self.op_dunder(code.ins[t.b as usize], c, reflected, chunk).is_some_and(|f| f.0 == sec.value.get().0);
        sec.dunder.set(match (finds, kept) {
            (false, (h, a, _)) => (self.class_epoch, h, a, c),
            (true, (h, _, m)) if h.is_undef() => {
                sec.lone.set(!self.mro_cache.iter().any(|(&k, m)| k != c.0 && m.iter().any(|x| x.0 == c.0)));
                sec.seen.set((a, self.class_epoch));
                (self.class_epoch, c, Val::undef(), m)
            }
            (true, (h, _, m)) => (self.class_epoch, h, c, m),
        });
        finds && self.dunder_other(code, sec, reflected, t, a, c, b)
    }

    /* Whether operand `b` lets an op's dunder on an instance of `c` run first, a number for a reflected one, else anything but a strict subclass with its own answer. */
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn dunder_other(&self, code: &Code, sec: &lower::Section, reflected: u16, t: Ins, a: Val, c: Val, b: Val) -> bool {
        // An op whose dunder checks no other operand (a negation, store or test) runs it, a builtin's call while the builtin is its own, a `__getattr__` while `b` is missing.
        let op = code.ins[t.b as usize];
        match reflected {
            1 => return b.is_int() || b.is_float() || b.is_bool(),
            2 | 4 | 6 => return true,
            // A subclass's reflected dunder goes first for an instance of a class it derives from on the left.
            7 => return matches!(super::opcodes::dunder::class_of(&self.heap, b), Some(lc) if lc.0 != c.0 && self.heap.is_subclass(c, lc) && !self.subclass_defers(op, lc, c)),
            3 => return crate::parser::fused_native(if op.op == OpCode::LenR { OpCode::CallLen } else { op.op }).is_none_or(|id| self.builtins_rebound & (1 << id as u32) == 0),
            5 => return self.lacks(a, b),
            _ => {}
        }
        if b.is_undef() { return false; }
        match super::opcodes::dunder::class_of(&self.heap, b) {
            // The other class it last let run first is kept while no class changed.
            Some(bc) if bc.0 != c.0 => {
                let (epoch, ok) = sec.other.get();
                if epoch == self.class_epoch && ok.0 == bc.0 { return true; }
                let lets = !self.heap.is_subclass(bc, c) || self.subclass_defers(op, c, bc);
                if lets { sec.other.set((self.class_epoch, bc)); }
                lets
            }
            _ => true,
        }
    }

    /* Whether instance `o` holds no attribute by the name string `name`. */
    #[inline(never)]
    fn lacks(&self, o: Val, name: Val) -> bool {
        let (Some(HeapObj::Instance(_, attrs)), Some(HeapObj::Str(n))) = (self.heap.try_get(o), self.heap.try_get(name)) else { return false };
        attrs.borrow().position_str(n, crate::value::hash_str(n), &self.heap).is_none()
    }

    /* Whether an op on `c` and its strict subclass `bc` runs the left's dunder first, always for an index, for arithmetic while `bc` reflects as `c` does, never to compare. */
    #[cold]
    #[inline(never)]
    fn subclass_defers(&self, op: Ins, c: Val, bc: Val) -> bool {
        if op.op == OpCode::GetItemR { return true; }
        let form = super::lower::stack_form(op.op);
        if super::opcodes::dunder::compare_dunder_names(form).is_some() { return false; }
        let Some((_, r)) = super::opcodes::dunder::op_names(op.op) else { return false };
        self.lookup_class_member(bc, r).map(|m| m.0.0) == self.lookup_class_member(c, r).map(|m| m.0.0)
    }

    /* Runs register forms from `ip`, entering and leaving the frames plain calls open above `floor`, true when another frame runs after. */
    #[inline(never)]
    fn run_frames(&mut self, code: &Code, chunk: &SSAChunk, ip: &mut usize, floor: &Floor) -> bool {
        let (mut code, mut chunk, mut switched) = (code, chunk, false);
        loop {
            switched |= self.run_regs(&mut code, &mut chunk, ip, floor);
            let i = *ip;
            if self.entered && !self.floor_req && self.reg_error.is_none() {
                // A call it let in runs next, in the frame it opened.
                (self.entered, self.yielded) = (false, false);
                // A result the next instruction would store goes straight to its register.
                let dest = super::act::dest_of(code.ins.get(i));
                let (c, k) = self.regs_enter(i, dest);
                // SAFETY the frame just opened runs code its pool keeps, and its chunk lives as long as the VM.
                (code, chunk) = unsafe { (&*c, &*k) };
                // A generator a builtin runs carries on where it last yielded.
                *ip = if self.top_gather() { self.gen_entry } else { code.lowered(0) };
            } else if self.acts > floor.depth && !self.yielded && let Some(&ins) = code.ins.get(i) && matches!(ins.op, OpCode::ReturnR | OpCode::ReturnValue) && let Some((ret, c, k)) = self.regs_return(ins, floor) {
                // SAFETY the caller runs code its pool or the floor keeps, and every chunk outlives the run.
                (code, chunk) = unsafe { (&*c, &*k) };
                *ip = ret;
            } else if i >= code.ins.len() && self.acts > floor.depth && !self.yielded && let Some((ret, c, k)) = self.regs_end(floor) {
                // SAFETY as above.
                (code, chunk) = unsafe { (&*c, &*k) };
                *ip = ret;
            } else {
                return switched;
            }
            switched = true;
        }
    }

    // Leaves the flat plain frames on top, none in code with sections, each noted for the traceback, the call in the frame below them.
    #[cold]
    #[inline(never)]
    fn unwind_plain(&mut self, floor: &Floor) -> Option<usize> {
        let plain = |vm: &Self| vm.acts > floor.depth && { let h = vm.head_at(vm.top_pool); vm.head(h).as_int() >> 40 == 0 && unsafe { &*(vm.head(h + 4).as_int() as usize as *const Code) }.block.is_none() };
        if !plain(self) { return None; }
        let mut at = 0;
        while plain(self) {
            let (pool, h) = (self.top_pool, self.head_at(self.top_pool));
            let (ret, below) = ((self.head(h).as_int() & 0xFF_FFFF) as usize, self.head(h + 1).as_int());
            let (fi, under) = ((below >> 24) as usize, (below & 0xFF_FFFF) as usize);
            (self.top_pool, self.acts) = (under, self.acts - 1);
            self.close_window(pool);
            let (c, k) = if self.acts > floor.depth {
                let h = self.head_at(under);
                (self.head(h + 4).as_int() as usize as *const Code, &self.functions[(self.head(h + 1).as_int() >> 24) as usize].1 as *const SSAChunk)
            } else { (floor.code, floor.chunk) };
            // SAFETY the caller runs code its pool or the floor keeps, and every chunk outlives the run.
            let (caller, k) = unsafe { (&*c, &*k) };
            at = ret - 1;
            // A call made inside a section was made in the function the section was laid from.
            match caller.block.as_deref().and_then(|b| b.holding(at)) {
                Some((s, j)) => { let o = s.orig.get(j - s.start as usize).copied(); self.note_frame(fi, o, &self.functions[s.fi].1); }
                None => self.note_frame(fi, Some(caller.orig(at)), k),
            }
        }
        Some(at)
    }

    /* Whether `f`, the running frame, is a plain one a `map` a `for` steps opened, which returns to that `for` in the frame below it. */
    #[inline(never)]
    fn mapped(&self, f: &super::act::Act) -> bool {
        let below = (self.head(self.head_at(self.top_pool) + 1).as_int() & 0xFF_FFFF) as usize;
        let code = self.head(self.head_at(below) + 4).as_int() as usize as *const Code;
        // SAFETY a frame's code lives in its pool as long as the VM.
        f.dest != super::act::NO_DEST && unsafe { &*code }.ins.get(f.ret).is_some_and(|i| i.op == OpCode::ForIterR) && matches!(self.iter_stack.last(), Some(IterFrame::Map { .. }))
    }

    /* A `yield` at `i` of the generator frame a builtin's job runs, its value taken and the frame running on, or for `next` suspended, None for the handler. */
    #[inline(never)]
    fn gather_yield(&mut self, ins: Ins, i: usize, code: &Code, floor: &Floor) -> Option<(*const Code, *const SSAChunk, usize)> {
        let v = if ins.x == 1 { self.regs[self.base + ins.a as usize] } else { self.stack.last().copied().unwrap_or_else(Val::undef) };
        if v.is_undef() { return None; }
        let next_pops = ins.x != 1 && code.ins.get(i + 1).is_some_and(|n| n.op == OpCode::PopTop);
        if self.gather_fast(v) {
            if ins.x != 1 { self.stack.pop(); if !next_pops { self.stack.push(Val::none()); } }
            return Some((core::ptr::null(), core::ptr::null(), i + 1 + next_pops as usize));
        }
        // `next` takes the value and ends, the generator suspended where it yielded.
        if !matches!(self.jobs.last(), Some(super::jobs::Job::NextGen(_))) || self.budget == 0 { return None; }
        let resume = if ins.x == 1 { ins.b as usize } else {
            self.stack.pop();
            if !next_pops { self.stack.push(Val::none()); }
            code.resume(i + 1 + next_pops as usize)
        };
        self.gather_leave(v, resume, floor.code, floor.chunk, floor.pool, floor.depth)
    }

    /* Makes the frame a call from the register loop just opened the running one, its code and chunk. */
    #[inline(always)]
    fn regs_enter(&mut self, ret: usize, dest: u16) -> (*const Code, *const SSAChunk) {
        self.set_ret(ret, dest);
        if self.acts == 0 { return (core::ptr::null(), core::ptr::null()) }
        // The record before the window holds the code the frame runs and its function, read alone.
        let (pool, at) = (self.top_pool, self.head_at(self.top_pool));
        let fi = (self.head(at + 1).as_int() >> 24) as usize;
        let (code, chunk) = (self.head(at + 4).as_int() as usize as *const Code, &self.functions[fi].1 as *const SSAChunk);
        self.run_in(pool);
        // SAFETY the frame just pushed holds its code, and its chunk lives as long as the VM.
        let (c, k): (&Code, &SSAChunk) = unsafe { (&*code, &*chunk) };
        if !self.laid(c, pool) { self.grow_frame(k, c, pool); }
        (code, chunk)
    }

    /* A PopIter `ins`, a generator section's mark of `a` own iterators pushed with `x` 2 and taken back with 1, or else a drop of the loop's iterator with any it marks. */
    #[inline(never)]
    pub(crate) fn pop_iter(&mut self, ins: Ins) {
        match ins.x {
            // A section with a `try` or a `with` notes where its stacks begin as it starts, in the registers from `a` on.
            3 => {
                let at = self.base + ins.a as usize;
                self.regs[at] = Val::int((self.exception_stack.len() | self.unwind_stack.len() << HELD_SHIFT) as i64);
                self.regs[at + 1] = Val::int((self.with_stack.len() | self.handling.len() << HELD_SHIFT) as i64);
            }
            2 if ins.c != 0 => self.hold(ins),
            // A return from its body drops what it left on the stacks above where it began.
            4 => {
                let at = self.base + ins.a as usize;
                let (e, w) = (self.regs[at].as_int() as usize, self.regs[at + 1].as_int() as usize);
                let mask = (1 << HELD_SHIFT) - 1;
                self.exception_stack.truncate(e & mask);
                self.unwind_stack.truncate(e >> HELD_SHIFT);
                self.with_stack.truncate(w & mask);
                self.handling.truncate(w >> HELD_SHIFT);
            }
            2 => self.iter_stack.push(IterFrame::Under(ins.a as u32)),
            1 => match self.iter_stack.last() {
                Some(IterFrame::Under(_)) => { self.iter_stack.pop(); }
                Some(IterFrame::Held(..)) => if let Some(IterFrame::Held(_, mut h)) = self.iter_stack.pop() {
                    // What it held goes back where it was, the loop's body having left the stacks as it found them.
                    if !h.excs.is_empty() { self.exception_stack.extend_from_slice(&h.excs); h.excs.clear(); }
                    if !h.withs.is_empty() { self.with_stack.extend_from_slice(&h.withs); h.withs.clear(); }
                    if !h.unwinds.is_empty() { self.unwind_stack.append(&mut h.unwinds); }
                    if !h.handling.is_empty() { self.handling.extend_from_slice(&h.handling); h.handling.clear(); }
                    self.held_pool.push(h);
                },
                _ => {}
            },
            _ => if let Some(IterFrame::Under(n) | IterFrame::Held(n, _)) = self.iter_stack.pop() { let n = self.iter_stack.len().saturating_sub(n as usize); self.iter_stack.truncate(n); },
        }
    }

    /* The mark of a generator section yielding in a `try` or `with` over its `ins.a` iterators, holding what it put on the stacks, a lone iterator left as the loop's. */
    fn hold(&mut self, ins: Ins) {
        let at = self.base + ins.b as usize;
        let (e, w) = (self.regs[at].as_int() as usize, self.regs[at + 1].as_int() as usize);
        let mask = (1 << HELD_SHIFT) - 1;
        let (exc, unwind, with, handling) = (e & mask, e >> HELD_SHIFT, w & mask, w >> HELD_SHIFT);
        if self.exception_stack.len() <= exc && self.with_stack.len() <= with && self.unwind_stack.len() <= unwind && self.handling.len() <= handling && ins.a == 1 { return; }
        let mut h = self.held_pool.pop().unwrap_or_default();
        if self.exception_stack.len() > exc { h.excs.extend_from_slice(&self.exception_stack[exc..]); self.exception_stack.truncate(exc); }
        if self.with_stack.len() > with { h.withs.extend_from_slice(&self.with_stack[with..]); self.with_stack.truncate(with); }
        if self.unwind_stack.len() > unwind { h.unwinds.extend(self.unwind_stack.drain(unwind..)); }
        if self.handling.len() > handling { h.handling.extend_from_slice(&self.handling[handling..]); self.handling.truncate(handling); }
        self.iter_stack.push(IterFrame::Held(ins.a as u32, h));
    }

    /* Ends the innermost frame on a plain return of a register or the stack top, where its caller carries on and the code and chunk it runs, None when the return needs its handler. */
    #[inline(always)]
    fn regs_return(&mut self, ins: Ins, floor: &Floor) -> Option<(usize, *const Code, *const SSAChunk)> {
        if self.top_job() { if self.top_gather() { return None; } return self.job_return(ins, floor); }
        let f = self.top().filter(|f| f.generator.is_undef())?;
        if self.top_linked() { return None; }
        let v = match ins.op {
            OpCode::ReturnR => self.regs[self.pools[f.pool].window + ins.b as usize],
            _ if self.stack.len() > f.stack_base => self.stack[self.stack.len() - 1],
            _ => return None,
        };
        // A return under a handler runs its cleanup first, which the slow path does.
        let b = self.bases_of(&f);
        if v.is_undef() || self.exception_stack.len() > b.exc { return None; }
        if ins.op == OpCode::ReturnValue { self.stack.pop(); }
        if self.unwind_stack.len() > b.unwind { self.unwind_stack.truncate(b.unwind); }
        let (ret, dest, v, dunder) = self.return_frame(v);
        let (code, chunk) = match self.top() {
            Some(f) if self.acts > floor.depth => { let (pool, at) = (f.pool, (f.code, &self.functions[f.fi].1 as *const SSAChunk)); self.run_in(pool); at }
            _ => { self.run_in(floor.pool); (floor.code, floor.chunk) }
        };
        let next = match dunder { Some(func) => self.dunder_back(v, ret, dest, func), None => self.deliver(v, ret, dest) };
        Some((next, code, chunk))
    }

    /* Ends the innermost frame once it runs past its last instruction, a return of None. */
    #[inline(never)]
    fn regs_end(&mut self, floor: &Floor) -> Option<(usize, *const Code, *const SSAChunk)> {
        // A plain frame hands back None as a register return does, anything else through the stack return.
        if let Some((c, k, next)) = self.leave_site(Val::none(), floor.code, floor.chunk, floor.pool, floor.depth) { return Some((next, c, k)); }
        let f = self.top().filter(|f| f.generator.is_undef() && !f.job)?;
        if self.exception_stack.len() > self.bases_of(&f).exc { return None; }
        self.push(Val::none());
        self.regs_return(Ins { op: OpCode::ReturnValue, x: 0, a: 0, b: 0, c: 0 }, floor)
    }

    /* An error at `rip` inside the sections of `code` points at its function's line, each section it rises through noting its frame, called where the one around it called. */
    #[cold]
    #[inline(never)]
    fn section_error(&mut self, code: &Code, chunk: &SSAChunk, rip: usize) {
        let Some(block) = code.block.as_deref() else { return };
        let Some((s, o)) = block.holding(rip) else { return };
        if self.error_byte_pos.is_none() && let Some(&o) = s.orig.get(o - s.start as usize) { self.error_byte_pos = self.functions[s.fi].1.resolve(o); }
        // Each section's link names the call into it, three before, a recursion level keeping the one below's link and counting once more on the frame it noted.
        let mut last: Option<(usize, usize, usize, u32)> = None;
        self.walk_levels(code, rip, false, 0, |vm, s, site, outer, _| {
            match outer {
                Some(outer) => {
                    let now = (vm.call_stack.len(), vm.call_stack.last().map_or(0, |f| f.repeats));
                    if last == Some((site, s.fi, now.0, now.1)) && let Some(f) = vm.call_stack.last_mut() { f.repeats += 1; } else {
                        // A head laid past the end for an op inside the section lies for it where the op does.
                        let at = outer.orig.get(site - outer.start as usize).or_else(|| outer.orig.get(block.origin(site) - outer.start as usize)).copied();
                        vm.note_frame(s.fi, at, &vm.functions[outer.fi].1);
                    }
                    let after = (vm.call_stack.len(), vm.call_stack.last().map_or(0, |f| f.repeats));
                    last = (after != now).then_some((site, s.fi, after.0, after.1));
                }
                // The call outside every section, or for a call whose result a generator's `yield` takes, past the generator that called it there.
                None => match tail_of(code, s, site) {
                    Some(g) => {
                        vm.note_frame(s.fi, g.tail.map(|t| t.0), &vm.functions[g.fi].1);
                        vm.note_frame(g.fi, Some(code.orig(site)), chunk);
                    }
                    None => vm.note_frame(s.fi, Some(code.orig(site)), chunk),
                },
            }
            true
        });
    }

    /* Where a run entering at `ip` a loop over a generator's section carries on, the suspended generator moved into the section, None to step it as an ordinary loop. */
    #[cold]
    #[inline(never)]
    fn adopt_gen(&mut self, code: &Code, ip: usize) -> Option<usize> {
        let block = code.block.as_deref()?;
        let &(from, _, k, head) = block.loops.iter().find(|l| l.0 as usize == ip)?;
        let s = block.sections.get(k as usize)?;
        // Where it carries on, where the loop's step comes back once the body ran past what the yield marked, its state and loop.
        let (taken, back, state, it, more, guards, co, pool) = {
                let &IterFrame::Coroutine(co) = self.iter_stack.last()? else { return None };
                let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return None };
                let BodyRef::Fn(fi) = c.body else { return None };
                if fi != s.fi || c.ip == super::globals::async_ops::FINISHED || c.running || c.layout != 0 || !c.syncs.is_empty() || !c.stack.is_empty() || !adoptable(s, code, &c.excs) { return None; }
                let step = *s.steps.iter().find(|t| t.0 as usize == c.ip || t.1 as usize == c.ip)?;
                let (names, pool) = (self.functions[fi].1.names.len(), self.call_facts[fi].pool);
                let state: alloc::vec::Vec<Val> = match self.pools[pool].win(self.pools[pool].active as usize) {
                    Some(w) if w.owner.0 == co.0 && w.len >= names => self.regs[w.base..w.base + names].to_vec(),
                    _ if c.slots.len() >= names => c.slots[..names].to_vec(),
                    _ => return None,
                };
                let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) else { return None };
                // Its iterators go on in order, the first in the loop's place, one over another generator stepped as an ordinary iterator, none leaving the place empty.
                let mut iters = core::mem::take(&mut c.iters);
                (c.ip, c.slots) = (super::globals::async_ops::FINISHED, alloc::vec::Vec::new());
                let guards = (core::mem::take(&mut c.excs), core::mem::take(&mut c.withs));
                let it = (!iters.is_empty()).then(|| iters.remove(0));
                (step.2 as usize, step.3 as usize, state, it, iters, guards, co, pool)
        };
        // Each other generator of its function a window holds gives its state back as the frames over them go, so each resumes wherever its frame opens next.
        self.evict_pool(pool);
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) { c.slots = alloc::vec::Vec::new(); }
        let base = self.base;
        for (r, &v) in state.iter().enumerate() {
            if let Some(&to) = s.slots.get(r).filter(|&&t| t != u16::MAX) { self.regs[base + to as usize] = v; }
        }
        // A loop of its own over another generator steps it as an ordinary iterator, its step sent back to the loop's entry as the entry sends it.
        let (start, end) = (s.start as usize, (s.end as usize).min(code.ins.len()));
        for g in start..end {
            if code.ins[g].op == OpCode::GetIter && code.ins[g].x == lower::ADOPT && let Some(mv) = code.ins.get(g + 2).filter(|m| m.op == OpCode::Move) {
                self.regs[base + mv.a as usize] = self.regs[base + mv.b as usize];
            }
        }
        let (store, step) = (code.ins[head as usize + 1], code.ins[from as usize + 3]);
        self.regs[base + s.link as usize] = Val::int((store.b as i64) << 16 | store.a as i64);
        // A body yielding at one place leaves its step to come back as that yield does, any other's next yield sets it.
        let back = if s.steps.len() == 1 { back } else { taken };
        self.regs[base + step.a as usize] = Val::int((back as i64) << 16 | step.a as i64);
        // The generator's stacks begin where they stand, its handlers and context managers back on them as the section names them.
        if s.held != u16::MAX { self.pop_iter(Ins { op: OpCode::PopIter, x: 3, a: s.held, b: 0, c: 0 }); }
        let iter_base = self.iter_stack.len() - 1;
        match it {
            Some(it) => if let Some(last) = self.iter_stack.last_mut() { *last = it; },
            None => { self.iter_stack.pop(); }
        }
        self.iter_stack.extend(more);
        self.adopt_handlers(s, code, guards, iter_base);
        Some(taken)
    }

    /* Puts back the handlers and context managers a generator taken into section `s` held, depths made absolute from the stacks and `iter_base`, each handler the section's own. */
    fn adopt_handlers(&mut self, s: &lower::Section, code: &Code, (excs, mut withs): (alloc::vec::Vec<ExceptionFrame>, alloc::vec::Vec<Val>), iter_base: usize) {
        if excs.is_empty() { return; }
        let (stack_base, with_base) = (self.stack.len(), self.with_stack.len());
        for f in excs {
            let Some(h) = handler_in(s, code, f.handler_ip) else { continue };
            self.exception_stack.push(ExceptionFrame { handler_ip: h, stack_depth: f.stack_depth + stack_base, iter_depth: f.iter_depth + iter_base, with_depth: f.with_depth + with_base, turned: self.turned, ..f });
        }
        self.with_stack.append(&mut withs);
    }

    /* Where in section `ins.c` a loop takes generator `obj`, unstarted or at a `yield`, its locals and iterator moved in and its link set, None for other iterators. */
    #[cold]
    #[inline(never)]
    fn take_gen(&mut self, code: &Code, obj: Val, ins: Ins) -> Option<usize> {
        let s = code.block.as_deref()?.sections.get(ins.c as usize)?;
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(obj) else { return None };
        let BodyRef::Fn(fi) = c.body else { return None };
        if fi != s.fi || c.ip == super::globals::async_ops::FINISHED || c.running || c.layout != 0 || !c.syncs.is_empty() || !c.stack.is_empty() || !adoptable(s, code, &c.excs) { return None; }
        let at = match (c.ip, c.iters.len()) {
            (0, 0) => s.start as usize,
            (ip, 1) => s.steps.iter().find(|t| t.0 as usize == ip || t.1 as usize == ip)?.2 as usize,
            _ => return None,
        };
        let (names, pool) = (self.functions[fi].1.names.len(), self.call_facts[fi].pool);
        let state: alloc::vec::Vec<Val> = match self.pools[pool].win(self.pools[pool].active as usize) {
            Some(w) if w.owner.0 == obj.0 && w.len >= names && c.ip != 0 => self.regs[w.base..w.base + names].to_vec(),
            _ if c.slots.len() >= names => c.slots[..names].to_vec(),
            _ => return None,
        };
        let base = self.base;
        for (r, &v) in state.iter().enumerate() {
            if let Some(&to) = s.slots.get(r).filter(|&&t| t != u16::MAX) { self.regs[base + to as usize] = v; }
        }
        self.regs[base + s.link as usize] = Val::int((ins.a as i64) << 16 | ins.b as i64);
        // The step comes back where a body yielding at one place always does, else where its next `yield` sends it.
        let back = if s.steps.len() == 1 { s.steps[0].3 as usize } else { at };
        self.regs[base + s.step_reg as usize] = Val::int((back as i64) << 16 | s.step_reg as i64);
        if s.held != u16::MAX { self.pop_iter(Ins { op: OpCode::PopIter, x: 3, a: s.held, b: 0, c: 0 }); }
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(obj) else { return None };
        let it = c.iters.pop();
        (c.ip, c.slots) = (super::globals::async_ops::FINISHED, alloc::vec::Vec::new());
        let guards = (core::mem::take(&mut c.excs), core::mem::take(&mut c.withs));
        let iter_base = self.iter_stack.len();
        if let Some(it) = it { self.iter_stack.push(it); }
        self.adopt_handlers(s, code, guards, iter_base);
        Some(at)
    }

    /* The chunk instruction `rip` of `code` reads names and constants from, its own or, inside a section, the one of the function it was laid from. */
    #[inline]
    fn chunk_at<'c>(&self, code: &Code, chunk: &'c SSAChunk, rip: usize) -> &'c SSAChunk where 'a: 'c { chunk_in(&self.functions, code, chunk, rip) }

    /* A bound builtin method in `vals[0]` called by the register call at `i` on `recv` and `n` arguments, rerun after a given key's `__hash__` and `__eq__`. */
    #[inline(never)]
    fn bound_keyed(&mut self, recv: Val, id: super::methods::BuiltinMethodId, vals: [Val; 3], n: usize, i: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        let armed = if self.user_keys(&vals[1..=n]) { self.redo_defer(i, super::redo::Rerun::Regs, self.stack.len() + n + 1, &vals[..=n]) } else { super::redo::Defer::No };
        let done = self.exec_bound_method(recv, id, &vals[1..=n], &[], chunk);
        self.redo_undefer(armed);
        done
    }

    /* `handle_comprehension` of a register form adding a key a class may hash, its `__hash__` and `__eq__` frames of the loop and the op run again after. */
    #[inline(never)]
    fn keyed_comprehension(&mut self, form: OpCode, rip: usize) -> Result<(), VmErr> {
        let (len, n) = (self.stack.len(), 1 + (form == OpCode::MapAdd) as usize);
        let ops = Args::of(&self.stack[len - n..]);
        let armed = self.user_keys(&ops) && self.redo_late(rip, super::redo::Rerun::Regs, len, &ops);
        let r = self.handle_comprehension(form);
        self.redo_done(armed);
        r
    }

    /* The builtin method of a register call at `i`, receiver and `n` arguments in `vals`, its calls going to the loop and the op rerun after, the next index or None on error. */
    #[inline(never)]
    fn bound_redone(&mut self, vals: [Val; 2], n: usize, id: super::methods::BuiltinMethodId, i: usize, chunk: &SSAChunk) -> Option<usize> {
        let armed = self.redo_defer(i, super::redo::Rerun::Regs, self.stack.len() + n + 1, &vals[..=n]);
        let done = self.exec_bound_method(vals[0], id, &vals[1..=n], &[], chunk);
        self.redo_undefer(armed);
        done.map(|()| i + 1).map_err(|e| self.reg_error = Some(e)).ok()
    }

    /* The builtin method `id` a register call's site knows on the receiver and argument in `vals`, its calls opening frames of this loop as through the stack. */
    #[inline(never)]
    fn bound_in_regs(&mut self, vals: [Val; 2], n: usize, id: super::methods::BuiltinMethodId, chunk: &SSAChunk) -> Result<(), VmErr> {
        self.job_ok = true;
        let done = self.exec_bound_method(vals[0], id, if n == 0 { &[] } else { &vals[1..2] }, &[], chunk);
        self.job_ok = false;
        done
    }

    /* Calls `callee` on `positional` and `kw_flat` from an op of this loop, a function opening as a frame of it. */
    #[inline(never)]
    fn call_in_loop_with(&mut self, callee: Val, positional: &[Val], kw_flat: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
        (self.inline_call, self.pending_exec_safe) = (true, self.frame_safe);
        let called = self.call_with(callee, None, positional, kw_flat, chunk);
        (self.inline_call, self.pending_exec_safe) = (false, false);
        called
    }

    /* A call from the register loop, its next index or None, the error kept. */
    #[inline(never)]
    fn call_aside(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        if matches!(ins.op, OpCode::CallR | OpCode::CallMethodR | OpCode::CallGlobalR) && lower::wide(ins) { return self.call_wide(ins, i, code, chunk); }
        let called = if matches!(ins.op, OpCode::CallR | OpCode::CallGlobalR) {
            self.pending.call_ip = Some(code.orig(i));
            let (r, n) = (&self.regs[self.base..], ins.x as usize);
            let callee = if ins.op == OpCode::CallGlobalR { global_callee(&self.scopes, code, ins) } else { r[ins.a as usize] };
            let vals = [callee, r[ins.b as usize], r[ins.c as usize]];
            // An unbound operand leaves for the handler, which names it.
            if vals[..=n].iter().any(|v| v.is_undef()) { return None; }
            let passed = Passed::Regs([vals[1], vals[2], Val::undef()]);
            // The function this call opened last opens again unchecked, while its body may still run as a frame.
            if let Site::Call { callee, fi } = code.cache.site(i) && callee.0 == vals[0].0 && self.call_facts[fi as usize].in_loop {
                return self.open_frame(fi as usize, callee, &passed, n, self.call_facts[fi as usize].closure as usize).map(|()| i + 1).map_err(|e| self.reg_error = Some(e)).ok();
            }
            // The callee read once, as a function or a class may open in the loop and a builtin runs on the registers.
            let (opens, native) = match self.heap.try_get(vals[0]) {
                Some(HeapObj::Func(..) | HeapObj::Class(..)) => (true, None),
                Some(&HeapObj::NativeFn(id)) => (false, Some(id)),
                // An instance called through its class's `__call__`, or held by a module binding, is noted so the code laid again lays that method.
                Some(&HeapObj::Instance(class, _)) => {
                    self.note_class(code, chunk, i, class, lower::CALLED);
                    if ins.op == OpCode::CallR && code.block.is_some() && self.called_grows(code, chunk, i, class) { return Some(i); }
                    (false, None)
                }
                // So is the function of a bound method, laid with its receiver first, by code a lowering may lay it in.
                Some(&HeapObj::BoundUserMethod(_, func, _)) => { if self.lays_later(code) { self.note_called(code, chunk, i, func, lower::BOUND); } (false, None) }
                _ => (false, None),
            };
            match if opens { self.open_in_loop(vals[0], passed, n) } else { Ok(false) } {
                Ok(true) => { self.learn_call(i, code, vals[0]); Ok(()) }
                // A bound builtin method, `xs.append` looked up before its argument ran, takes the registers as they are.
                Ok(false) if let Some(&HeapObj::BoundMethod(recv, id)) = self.heap.try_get(vals[0]) && !self.method_unbound(recv, id) => {
                    if self.user_hash { self.bound_keyed(recv, id, vals, n, i, chunk) } else {
                        self.job_ok = true;
                        let done = self.exec_bound_method(recv, id, &vals[1..=n], &[], chunk);
                        self.job_ok = false;
                        done
                    }
                }
                // Any other callee takes them off the stack as a stack call would.
                Ok(false) if let Some(id) = native => self.native_direct(id, &vals[1..=n], chunk, i),
                Ok(false) => { self.stack.extend_from_slice(&vals[..=n]); self.call_general(n as u16, chunk, (i, super::redo::Rerun::Regs)) }
                Err(e) => Err(e),
            }.map(|()| i + 1)
        } else if ins.op == OpCode::Call {
            // Unlowered code learns what it calls so a later lowering lays it, the operand counting positionals and, past the eighth bit, keyword name and value pairs.
            let callee = self.stack.len().checked_sub((ins.a & 0xFF) as usize + 2 * (ins.a >> 8) as usize + 1).map_or(Val::undef(), |k| self.stack[k]);
            // A keyword call no head lays asks for one laid for its function before it runs, a stub's for its head, and one its lowering missed counts toward laying it again.
            if (ins.a >> 8 != 0 || ins.a > 5) && code.block.is_some() && self.budget != 0 && matches!(self.heap.try_get(callee), Some(HeapObj::Func(..) | HeapObj::Class(..))) && self.site_grows(code, chunk, i, callee) { return Some(i); }
            if ins.a >> 8 != 0 && code.block.is_some() && code.misses.get() < lower::MISSES { self.kw_unlaid(code, chunk, i, ins.a); }
            match self.heap.try_get(callee) {
                Some(&HeapObj::Instance(class, _)) if ins.a < 0x100 => self.note_class(code, chunk, i, class, lower::CALLED),
                Some(&HeapObj::BoundUserMethod(_, func, _)) if ins.a < 0x100 && self.lays_later(code) => self.note_called(code, chunk, i, func, lower::BOUND),
                // So is a generator a register call makes in code that may lay a loop over it, noted only once that loop nears its lowering.
                Some(&HeapObj::Func(fi, ..)) if ins.a < 0x100 && code.unchanged() && code.hot.get() >= lower::HOT_LOOP / 2 && self.functions.get(fi).is_some_and(|f| f.1.is_generator) => self.note_called(code, chunk, i, callee, lower::GEN),
                // The stub of a call with keywords or many arguments counts the functions it meets, so the code is laid again with them.
                Some(HeapObj::Func(..)) if code.block.is_some() && code.misses.get() < lower::MISSES && is_stub(code, i) => note_alt(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, i, callee, false),
                // A class's, its constructor laid for the class as it was, counts toward laying the code again.
                Some(HeapObj::Class(..)) if code.block.is_some() && code.misses.get() < lower::MISSES && is_stub(code, i) => miss(&mut self.preempt_left, code),
                _ => {}
            }
            self.call_stacked(ins.a, code.orig(i), chunk, (i, super::redo::Rerun::Stack)).map(|()| { if self.entered && code.unchanged() { self.learn_call(i, code, callee); } i + 1 })
        } else if ins.op == OpCode::CallMethodR {
            let (r, n) = (&self.regs[self.base..], ins.x as usize);
            let vals = [r[ins.b as usize], r[ins.c as usize]];
            if vals[..=n].iter().any(|v| v.is_undef()) { return None; }
            // A builtin method the site knows takes its argument straight from the registers.
            if let Some(id) = self.site_builtin(code.cache.site(i), vals[0]) {
                if self.method_calls(id, &vals[1..=n]) { return self.bound_redone(vals, n, id, i, chunk); }
                // One whose table may be a user mapping runs its calls as frames of this loop, as through the stack.
                if self.heap.users && id.kind() == super::methods::MethodKind::Mapped { return self.bound_in_regs(vals, n, id, chunk).map(|()| i + 1).map_err(|e| self.reg_error = Some(e)).ok(); }
                return self.exec_bound_method(vals[0], id, if n == 0 { &[] } else { &vals[1..2] }, &[], chunk).map(|()| i + 1).map_err(|e| self.reg_error = Some(e)).ok();
            }
            // A method the site knows opens straight from the registers, the receiver first.
            if let Some(name) = chunk.names.get(ins.a as usize) && let Some((func, owner)) = self.site_method(code.cache.site(i), vals[0], name) {
                (self.pending.method_binding, self.pending.call_ip) = (Some((owner, vals[0])), Some(code.orig(i)));
                match self.open_in_loop(func, Passed::Regs([vals[0], vals[1], Val::undef()]), n + 1) {
                    Ok(true) => return Some(i + 1),
                    Ok(false) => self.pending.method_binding = None,
                    Err(e) => { self.reg_error = Some(e); return None; }
                }
            }
            // Any other receiver and argument go where the method call of the stack finds them.
            self.stack.extend_from_slice(&vals[..=n]);
            self.exec_call_method(ins.a, n as u16, i, code, chunk, super::redo::Rerun::Regs).map(|()| i + 1)
        } else {
            // An unlowered pair steps over its second half.
            let next = i + 1 + (ins.x == 1) as usize;
            self.exec_call_method(ins.a, ins.b, i, code, chunk, super::redo::Rerun::Stack).map(|()| next)
        };
        called.map_err(|e| self.reg_error = Some(e)).ok()
    }

    /* Enters the section laid for the function a register call at head `at` makes among the others it met, its arguments as the head writes them, None when there is none. */
    #[inline(never)]
    fn enter_value(&mut self, code: &Code, ins: Ins) -> Option<usize> {
        // SAFETY a lookup lies in the stub of a head, in code that laid its block, its table within `alt_tab`.
        let (block, at) = (unsafe { code.block.as_deref().unwrap_unchecked() }, ins.a as usize);
        let (call, store) = unsafe { (*code.ins.get_unchecked(at), *code.ins.get_unchecked(at + 1)) };
        if self.budget == 0 { return None; }
        let base = self.base;
        // SAFETY the frame running laid code covers every register it names, its sections' among them.
        let callee = unsafe { *self.regs.get_unchecked(base + call.a as usize) };
        let (start, mask) = (ins.b as usize, ins.c as usize);
        let k = table_find(&self.heap, block, start, mask, callee)?;
        let s = unsafe { block.sections.get_unchecked(k) };
        let p = base + s.link as usize;
        // A section any function of its body runs takes the cells of the one the call makes, and its defaults past the arguments.
        if s.by_body.is_some() && !body_fits(&self.heap, callee, s, call.x as usize) { return None; }
        let r = &mut self.regs;
        // SAFETY as above.
        unsafe {
            let (b, c) = (*r.get_unchecked(base + call.b as usize), *r.get_unchecked(base + call.c as usize));
            (*r.get_unchecked_mut(p + 1), *r.get_unchecked_mut(p + 2), *r.get_unchecked_mut(p)) = (b, c, Val::int((store.b as i64) << 16 | store.a as i64));
        }
        if call.x > 2 || (s.arity as usize) > call.x as usize { moved(r, base, &block.sections[code.ins[at + 2].c as usize], s, call.x as usize); }
        if s.by_body.is_some() { body_enter(&self.heap, &mut r[base..], callee, s, call.x as usize); }
        self.budget -= 1;
        Some(s.start as usize)
    }

    /* Notes that an instance may now hide a method by its own attribute, moving the class version on so every method's section checks the name again. */
    pub(crate) fn shadow(&mut self) {
        if !self.shadowed { self.class_epoch = self.class_epoch.wrapping_add(1); }
        self.shadowed = true;
    }

    /* Where a register call its head at `at` turned down goes, the section laid for the function the register holds among those met, entered as the head would, or the stub. */
    #[inline(never)]
    fn enter_value_alt(&mut self, code: &Code, at: usize) -> usize {
        // SAFETY a head lies in code that laid its block, its section and its stub within it.
        let block = unsafe { code.block.as_deref().unwrap_unchecked() };
        let (call, sect, stub) = unsafe { (*code.ins.get_unchecked(at), *code.ins.get_unchecked(at + 2), *code.ins.get_unchecked(at + 3)) };
        let (start, mask) = unsafe { block.sections.get_unchecked(sect.c as usize) }.table;
        if mask == u32::MAX || self.budget == 0 { return stub.a as usize; }
        let base = self.base;
        // SAFETY the frame running laid code covers every register it names, its sections' among them.
        let f = unsafe { *self.regs.get_unchecked(base + call.a as usize) };
        let mut slot = lower::alt_slot(f, mask as usize);
        let k = loop {
            // SAFETY a table holds `mask + 1` slots from `start`, at least one of them free.
            let (v, k) = unsafe { *block.alt_tab.get_unchecked(start as usize + slot) };
            if v.0 == f.0 { break k; }
            // One it holds none for may be a function of a body it lays a section for any function of, which the stub finds.
            if k == u32::MAX { return stub.a as usize; }
            slot = (slot + 1) & mask as usize;
        };
        // SAFETY a table names sections of its block, and the frame covers their registers.
        let s = unsafe { block.sections.get_unchecked(k as usize) };
        // The arguments past the second, which the moves before the head wrote to its own section's parameters, go to this one's.
        if call.x > 2 { moved(&mut self.regs, base, unsafe { block.sections.get_unchecked(sect.c as usize) }, s, call.x as usize); }
        let (p, store) = (base + s.link as usize, unsafe { *code.ins.get_unchecked(at + 1) });
        let r = &mut self.regs;
        unsafe {
            let (b, c) = (*r.get_unchecked(base + call.b as usize), *r.get_unchecked(base + call.c as usize));
            (*r.get_unchecked_mut(p + 1), *r.get_unchecked_mut(p + 2), *r.get_unchecked_mut(p)) = (b, c, Val::int((store.b as i64) << 16 | store.a as i64));
        }
        self.budget -= 1;
        s.start as usize
    }


    /* What the binding of the first section of `block` whose binding no longer holds its function holds now. */
    fn global_changed(&self, block: &lower::Block) -> Option<Val> {
        let s = block.sections.iter().find(|s| s.global != u16::MAX && self.scopes.get(s.bound).is_some_and(|g| g.read(s.global as u32).0 != s.value.get().0))?;
        Some(self.scopes[s.bound].read(s.global as u32))
    }

    /* Whether op `ins` at `rip`, outside the sections and laid as no head, asks for a head laid for the dunder `l` and `r` meet, the left one's or a reflected one past a number. */
    #[cold]
    #[inline(never)]
    pub(crate) fn op_grows(&mut self, code: &Code, chunk: &SSAChunk, rip: usize, ins: Ins) -> bool {
        if self.redo.keep { return false; }
        let Some(b) = code.block.as_deref().filter(|_| self.budget != 0 && !code.unchanged()) else { return false };
        let (l, r) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
        if !b.plain_at(rip) || code.ins.get(rip).is_none_or(|i| i.op != ins.op) || !lower::dunder_op(ins.op) { return false; }
        let f = match (self.heap.try_get(l), self.heap.try_get(r)) {
            (Some(&HeapObj::Instance(c, _)), _) => self.op_dunder(ins, c, 0, chunk),
            (_, Some(&HeapObj::Instance(c, _))) if l.is_int() || l.is_float() || l.is_bool() => self.op_dunder(ins, c, 1, chunk),
            _ => None,
        };
        f.is_some_and(|f| self.asks_growth(code, chunk, rip, lower::Grow::Op, f))
    }

    /* Whether the negation, membership test, item store or loop step at `rip`, outside the sections and laid as no head, asks for a head laid for `o`'s class dunder. */
    #[cold]
    #[inline(never)]
    fn op_dunder_grows(&mut self, code: &Code, chunk: &SSAChunk, rip: usize, ins: Ins, o: Val) -> bool {
        // An op run again past a call it handed the loop asks nothing, the call made already.
        if self.redo.keep { return false; }
        let Some(&HeapObj::Instance(c, _)) = self.heap.try_get(o) else { return false };
        let Some(b) = code.block.as_deref().filter(|_| self.budget != 0 && !code.unchanged()) else { return false };
        if !b.plain_at(rip) || code.ins.get(rip).is_none_or(|i| i.op != ins.op) { return false; }
        let f = if ins.op == OpCode::ForIterR {
            self.lookup_class_member(c, "__next__").map(|f| f.0).filter(|f| matches!(self.heap.try_get(*f), Some(HeapObj::Func(..))))
        } else { self.op_dunder(if ins.op == OpCode::NotInR { Ins { op: OpCode::InR, ..ins } } else { ins }, c, 0, chunk) };
        f.is_some_and(|f| self.asks_growth(code, chunk, rip, lower::Grow::Op, f))
    }

    /* Whether the stub at `i` of a head calling an instance through `__call__`, meeting an instance of `class`, asks the head to grow with that class's `__call__`. */
    #[cold]
    #[inline(never)]
    fn called_grows(&mut self, code: &Code, chunk: &SSAChunk, i: usize, class: Val) -> bool {
        if self.budget == 0 || self.redo.keep { return false; }
        let Some(h) = stub_head(code, i) else { return false };
        let Some(b) = code.block.as_deref() else { return false };
        // A head of a call through a register lays the `__call__` past itself, checked once its own and its table turn it down.
        let kind = match b.sections.get(code.ins[h + 2].c as usize) { Some(s) if s.on_class == 3 => lower::Grow::Method, Some(s) if s.on_class == 0 && code.ins[h + 2].x & (4 | 8) == 4 => lower::Grow::Chain, _ => return false };
        let Some((f, _)) = self.lookup_class_member(class, "__call__") else { return false };
        matches!(self.heap.try_get(f), Some(HeapObj::Func(..))) && self.asks_growth(code, chunk, h, kind, f)
    }

    /* Whether the truth test at `rip`, outside the sections and laid as no head, asks for a head laid for the `__bool__` or `__len__` its site found for its operand's class. */
    #[cold]
    #[inline(never)]
    fn truth_grows(&mut self, code: &Code, chunk: &SSAChunk, rip: usize, ins: Ins) -> bool {
        // An op run again past a call it handed the loop asks nothing, the call made already.
        if self.redo.keep { return false; }
        let o = self.regs[self.base + ins.b as usize];
        let Some(&HeapObj::Instance(c, _)) = self.heap.try_get(o) else { return false };
        let Some(b) = code.block.as_deref().filter(|_| self.budget != 0 && !code.unchanged()) else { return false };
        if !b.plain_at(rip) || code.ins.get(rip).is_none_or(|i| i.op != ins.op) { return false; }
        let func = match code.cache.site(rip) {
            Site::Dunder { class, func, epoch, .. } if epoch == self.class_epoch && class.0 == c.0 => func,
            _ => match self.op_dunder(ins, c, 0, chunk) { Some(f) => f, None => return false },
        };
        self.asks_growth(code, chunk, rip, lower::Grow::Op, func)
    }

    /* Whether the attribute load or store at `rip`, outside the sections and laid as no head, asks for a head laid for the property or `__getattr__` its site found. */
    #[cold]
    #[inline(never)]
    fn attr_grows(&mut self, code: &Code, chunk: &SSAChunk, rip: usize, ins: Ins, o: Val) -> bool {
        // An op run again past a call it handed the loop asks nothing, the call made already.
        if self.redo.keep { return false; }
        let Some(b) = code.block.as_deref().filter(|_| self.budget != 0 && !code.unchanged()) else { return false };
        if !b.plain_at(rip) || code.ins.get(rip).is_none_or(|i| i.op != ins.op) { return false; }
        // The dunder its site found for the class of `o`, or the property that class finds, an op meeting instances of several.
        let Some(&HeapObj::Instance(c, _)) = self.heap.try_get(o) else { return false };
        let func = match code.cache.site(rip) {
            Site::Dunder { class, func, epoch, .. } if epoch == self.class_epoch && class.0 == c.0 => func,
            _ => match self.op_dunder(ins, c, 0, chunk) { Some(f) => f, None => return false },
        };
        self.asks_growth(code, chunk, rip, lower::Grow::Op, func)
    }

    /* Whether the call at `i`, outside the sections and laid as no head, asks for a head laid for `callee`, a function the code may lay. */
    #[cold]
    #[inline(never)]
    fn site_grows(&mut self, code: &Code, chunk: &SSAChunk, i: usize, callee: Val) -> bool {
        let Some(b) = code.block.as_deref() else { return false };
        // A class's call asks only outside the sections, for its constructor, while a stub's has its head laid again with the class.
        let inside = b.sections.first().is_some_and(|s| i >= s.start as usize);
        if inside && matches!(self.heap.try_get(callee), Some(HeapObj::Class(..))) && !stub_head(code, i).is_some_and(|h| lower::kw_head(&code.ins, h) || code.ins[h + 2].x & 16 != 0) { return false; }
        // Until a code's calls turned down a few functions, each is only counted, and kept for the first ask to note.
        let g = b.grown();
        if g.turned.get() + 1 < lower::TURNS {
            g.turned.set(g.turned.get() + 1);
            if g.last.replace((i as u32, callee.0)) != (i as u32, callee.0) && g.pending.borrow().len() < 32 { g.pending.borrow_mut().push((i as u32, callee)); }
            return false;
        }
        if g.refuses(i as u32, callee.0) { return false; }
        if !g.pending.borrow().is_empty() { self.note_pending(code, chunk); }
        // A head's stub asks for its head, a register call for the function it met and a binding's for what the binding holds now.
        if inside {
            // A loop's stub calls the generator its head turned down as written, the loop then stepping it as any iterator.
            let looped = || b.loops.iter().find(|g| code.ins.get(g.3 as usize + 3).is_some_and(|s| s.op == OpCode::SectionR && s.x == lower::STUB && s.b as usize == i)).map(|g| g.3 as usize);
            let Some(h) = stub_head(code, i).or_else(looped) else {
                // A call of a section's body no head lays asks for one past the end as outside them, unless it calls the function whose section holds it, a recursion.
                let Some(s) = b.body_at(i) else { return false };
                if !matches!(self.heap.try_get(callee), Some(&HeapObj::Func(fi, ..)) if fi != s.fi && !self.functions.get(fi).is_some_and(|f| super::scope::is_comp(&f.0))) && !matches!(self.heap.try_get(callee), Some(HeapObj::BoundUserMethod(..)))
                    || self.redo.keep { return false; }
                self.refused = false;
                let asks = self.asks_growth(code, chunk, i, lower::Grow::Site, callee);
                if core::mem::take(&mut self.refused) { g.refuse(i as u32, callee.0); }
                return asks;
            };
            // A bound method's or instance's head meeting a function notes it where the head lies, for the code laid again.
            if code.ins[h].op == OpCode::CallMethodSectionR && b.sections.get(code.ins[h + 2].c as usize).is_some_and(|s| s.on_class & 3 == 3) {
                note_met(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, h, callee, false);
                return false;
            }
            let kind = match code.ins[h].op {
                // A constructor's meeting a class, for the `__init__` it finds now or the class its binding holds.
                OpCode::CallCtorSectionR | OpCode::CallSectionR if code.ins[h + 2].x & 16 != 0 && matches!(self.heap.try_get(callee), Some(HeapObj::Class(..))) => lower::Grow::Global,
                OpCode::CallValueSectionR => lower::Grow::Value,
                OpCode::CallSectionR if code.ins[h + 2].x & 4 != 0 => lower::Grow::Value,
                OpCode::CallGlobalSectionR | OpCode::CallSectionR if !sections_hold(&self.scopes, b) => lower::Grow::Global,
                _ => return false,
            };
            if kind == lower::Grow::Value && matches!(self.heap.try_get(callee), Some(&HeapObj::Func(fi, ..)) if self.functions.get(fi).is_some_and(|f| core::ptr::eq(&f.1, chunk))) { return false; }
            let func = if kind == lower::Grow::Global { self.global_changed(b).unwrap_or(callee) } else { callee };
            self.refused = false;
            let asks = self.asks_growth(code, chunk, h, kind, func);
            // What a binding holds may change, a head's own function not, and one its growth cannot lay counts toward laying the code again.
            if core::mem::take(&mut self.refused) {
                if kind != lower::Grow::Global { g.refuse(i as u32, callee.0); } else if code.misses.get() < lower::MISSES { miss(&mut self.preempt_left, code); }
            }
            return asks;
        }
        // A comprehension made again at each pass and a recursion the tier lays ask nothing, and a class's call lays its constructor.
        match self.heap.try_get(callee) { Some(&HeapObj::Func(fi, ..)) if !self.functions.get(fi).is_some_and(|f| super::scope::is_comp(&f.0) || core::ptr::eq(&f.1, chunk)) => {}, Some(HeapObj::Class(..) | HeapObj::BoundUserMethod(..)) => {}, _ => return false }
        self.refused = false;
        let asks = self.asks_growth(code, chunk, i, lower::Grow::Site, callee);
        if core::mem::take(&mut self.refused) { g.refuse(i as u32, callee.0); }
        asks
    }

    /* Notes the functions the stub calls and calls of `code` met before its first ask, each under the head it lies past. */
    #[cold]
    #[inline(never)]
    fn note_pending(&mut self, code: &Code, chunk: &SSAChunk) {
        let Some(b) = code.block.as_deref() else { return };
        let pending = core::mem::take(&mut *b.grown().pending.borrow_mut());
        for (i, f) in pending {
            let at = stub_head(code, i as usize).unwrap_or(i as usize);
            let (scope, cat) = lower::head_origin(code, at);
            let key = scope.map_or_else(|| VM::code_key_of(chunk, self.body_to_fi.get(&(chunk as *const SSAChunk)).copied()), |fi| fi as i64);
            let mut seen = self.learned.chunks(3).filter(|t| t[0].as_int() == key && t[1].as_int() == cat as i64);
            if matches!(self.heap.try_get(f), Some(HeapObj::Func(..))) && seen.clone().count() < 16 && !seen.any(|t| t[2].0 == f.0) { self.learned.extend_from_slice(&[Val::int(key), Val::int(cat as i64), f]); }
        }
    }

    /* Whether head `at`, turned down for `func`, asks the code to grow, once per head and function past a few calls, the handler growing it before the head runs again. */
    #[cold]
    #[inline(never)]
    fn asks_growth(&mut self, code: &Code, chunk: &SSAChunk, at: usize, kind: lower::Grow, func: Val) -> bool {
        if cfg!(feature = "coverage") || self.growing.is_some() { return false; }
        let Some(block) = code.block.as_deref() else { return false };
        let found = self.heap.try_get(func);
        // A function is asked for once per head by its body, a bound method by its function and a constructor's class by the `__init__` it finds.
        let pair = (at as u32, match found {
            Some(&HeapObj::Func(fi, ..)) => fi as u64 | 1 << 63,
            Some(&HeapObj::BoundUserMethod(_, f, _)) => f.0 | 1 << 62,
            Some(HeapObj::Class(..)) if kind == lower::Grow::Global => self.lookup_class_member(func, "__init__").map_or(func.0, |(f, _)| f.0 | 1 << 61),
            _ => func.0,
        });
        // A pair asked for once is turned down at once again, as it comes again and again.
        let grown = block.grown();
        let at_tried = grown.tried.borrow().binary_search(&pair);
        let Err(slot) = at_tried else { self.refused = true; return false };
        let class = matches!(kind, lower::Grow::Site | lower::Grow::Value) && matches!(found, Some(HeapObj::Class(..)))
            || kind == lower::Grow::Site && matches!(found, Some(HeapObj::BoundUserMethod(..)));
        // A comprehension, or a recursion inside a section of the very function it meets, is turned down for good as the tier lays it.
        let own = matches!(found, Some(&HeapObj::Func(fi, ..)) if section_at(code, at).is_some_and(|s| s.fi == fi));
        if kind != lower::Grow::Global && !class && !matches!(found, Some(HeapObj::Func(fi, ..)) if !self.functions.get(*fi).is_some_and(|f| super::scope::is_comp(&f.0))) || own {
            if grown.tried.borrow().len() < TRIED { grown.tried.borrow_mut().insert(slot, pair); }
            // One that met it after the tier laid the code, noted, has the code lowered again, its section keeping a level there.
            if own && kind == lower::Grow::Op && self.pools[self.pool_now].code.as_ref().is_some_and(|c| core::ptr::eq(&**c, code)) {
                let (scope, cat) = lower::head_origin(code, at);
                let (key, mark, f) = (scope.map_or(-1, |fi| fi as i64), (cat | lower::RELAID) as i64, self.pools[self.pool_now].func);
                if f != super::cache::NO_FN && !self.learned.chunks(3).any(|t| t[0].as_int() == key && t[1].as_int() == mark && t[2].0 == func.0) {
                    if !self.learned.chunks(3).any(|t| t[0].as_int() == key && t[1].as_int() == cat as i64 && t[2].0 == func.0) { self.learned.extend_from_slice(&[Val::int(key), Val::int(cat as i64), func]); }
                    self.learned.extend_from_slice(&[Val::int(key), Val::int(mark), func]);
                    self.pools[self.pool_now].laid = false;
                    self.relower(f as usize);
                }
            }
            self.refused = true;
            return false;
        }
        // A code asks for a few hundred pairs at most, past which its heads' tables are as full as they get.
        if grown.tried.borrow().len() >= TRIED { return false; }
        // Each function met is noted, sixteen at most per call, so the growth lays all a head met at once and code lowered again after a snapshot lays them too.
        if !matches!(kind, lower::Grow::Global | lower::Grow::Chain) && !class && grown.last.replace(pair) != pair {
            let (scope, cat) = lower::head_origin(code, at);
            let key = scope.map_or_else(|| VM::code_key_of(chunk, self.body_to_fi.get(&(chunk as *const SSAChunk)).copied()), |fi| fi as i64);
            let mut seen = self.learned.chunks(3).filter(|t| t[0].as_int() == key && t[1].as_int() == cat as i64);
            if seen.clone().count() < 16 && !seen.any(|t| t[2].0 == func.0) { self.learned.extend_from_slice(&[Val::int(key), Val::int(cat as i64), func]); }
        }
        grown.turned.set(grown.turned.get() + 1);
        if grown.turned.get() < lower::TURNS { return false; }
        grown.tried.borrow_mut().insert(slot, pair);
        (self.growing, self.yielded) = (Some(alloc::boxed::Box::new((code as *const Code, at, kind, func))), true);
        true
    }

    /* Where the method call its head at `at` turned down goes, the section the table lays for the receiver's method entered with the head's arguments, or the stub for none. */
    #[inline(never)]
    fn enter_method_alt(&mut self, code: &Code, chunk: &SSAChunk, at: usize) -> usize {
        // SAFETY a head lies in code that laid its block, its section within it, and its table within `alt_tab`.
        let block = unsafe { code.block.as_deref().unwrap_unchecked() };
        let (call, store, sect) = unsafe { (*code.ins.get_unchecked(at), *code.ins.get_unchecked(at + 1), *code.ins.get_unchecked(at + 2)) };
        // SAFETY the frame running laid code covers every register it names, its sections' among them.
        let recv = unsafe { *self.regs.get_unchecked(self.base + call.b as usize) };
        let (start, mask) = unsafe { block.sections.get_unchecked(sect.c as usize) }.table;
        if mask != u32::MAX && !self.shadowed && self.budget != 0 && let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(recv) {
            let (c, epoch, k) = unsafe { block.by_class.get_unchecked(start as usize + lower::alt_slot(class, mask as usize)) }.get();
            // A call that keeps values keeps them as it enters, as its head would.
            if c.0 == class.0 && epoch == self.class_epoch { return self.enter_alt(block, k as usize, sect.c as usize, call, store, recv); }
        }
        self.enter_method_found(code, chunk, at, recv)
    }

    /* `enter_method_alt` for a receiver whose class the table holds no section for, found by method name, noting one laid for none so the code is laid again with it. */
    #[inline(never)]
    fn enter_method_found(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> usize {
        // SAFETY as for `enter_method_alt`.
        let block = unsafe { code.block.as_deref().unwrap_unchecked() };
        let (call, store, sect, stub) = (code.ins[at], code.ins[at + 1], code.ins[at + 2], code.ins[at + 3]);
        let head = &block.sections[sect.c as usize];
        if head.on_class != 0 { return self.enter_on_class(code, chunk, at, recv).unwrap_or_else(|| self.enter_held(code, chunk, at, recv)); }
        let (start, mask) = head.table;
        let Some(func) = self.method_found(code, chunk, at, recv) else { return self.enter_held(code, chunk, at, recv) };
        let k = if func.0 == head.value.get().0 { sect.c as usize } else { alt_of(block, start, mask, func) };
        if k == u32::MAX as usize || self.budget == 0 {
            // The head's own method is noted first, so the code laid again keeps it beside the one met.
            if k == u32::MAX as usize {
                note_alt(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, stub.b as usize, head.value.get(), false);
                note_alt(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, stub.b as usize, func, false);
                if self.budget != 0 && self.asks_growth(code, chunk, at, lower::Grow::Method, func) { return GROW; }
            }
            return stub.a as usize;
        }
        // A function a receiver holds past a method head takes its arguments where its own parameters lie.
        if block.sections[k].on_class == 4 { return self.enter_mixed(block, k, sect.c as usize, call, store, recv); }
        let class = match self.heap.try_get(recv) { Some(&HeapObj::Instance(c, _)) => c, _ => Val::undef() };
        if !class.is_undef() && !self.shadowed { block.sections[k].guard.set((class, self.class_epoch, recv)); }
        if mask != u32::MAX { block.by_class[start as usize + lower::alt_slot(class, mask as usize)].set((class, self.class_epoch, k as u32)); }
        self.enter_alt(block, k, sect.c as usize, call, store, recv)
    }

    /* `enter_method_found` for a receiver holding a function by the called name or a method its class finds, entering the table's section or the stub, noting it. */
    #[cold]
    #[inline(never)]
    fn enter_held(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> usize {
        // SAFETY as for `enter_method_alt`.
        let block = unsafe { code.block.as_deref().unwrap_unchecked() };
        let (call, store, sect, stub) = (code.ins[at], code.ins[at + 1], code.ins[at + 2], code.ins[at + 3]);
        let head = &block.sections[sect.c as usize];
        if matches!(head.on_class, 1 | 2) && self.chain_grows(code, chunk, at, recv) { return GROW; }
        if !matches!(head.on_class, 0 | 4) { return stub.a as usize; }
        let held = self.held_func(code, chunk, at, recv);
        let found = if held.is_none() && head.on_class == 4 { self.method_found(code, chunk, at, recv) } else { None };
        let Some(func) = held.or(found) else { return stub.a as usize };
        let (start, mask) = head.table;
        let k = if func.0 == head.value.get().0 && held.is_some() { sect.c as usize } else { alt_of(block, start, mask, func) };
        if k == u32::MAX as usize {
            if held.is_some() {
                // A function the receiver holds is noted apart, and the head asks for the code to grow with it.
                self.note_called(code, chunk, stub.b as usize, func, lower::FIELD);
                if self.budget != 0 && self.asks_growth(code, chunk, at, lower::Grow::Method, func) { return GROW; }
            } else {
                note_alt(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, stub.b as usize, func, false);
            }
            return stub.a as usize;
        }
        if self.budget == 0 { return stub.a as usize; }
        self.enter_mixed(block, k, sect.c as usize, call, store, recv)
    }

    /* Whether the static or class method head at `at`, turning down `recv`, asks for a head laid past the end for what the receiver or its class holds by the call's name. */
    #[cold]
    #[inline(never)]
    fn chain_grows(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> bool {
        if self.budget == 0 || self.redo.keep || code.unchanged() { return false; }
        let name = chunk_in(&self.functions, code, chunk, at).names.get(code.ins[at].a as usize).map_or("", |n| crate::parser::ssa_strip(n));
        let held = match self.heap.try_get(recv) {
            Some(HeapObj::Instance(_, attrs)) => { let a = attrs.borrow(); a.position_str(name, crate::value::hash_str(name), &self.heap).map(|i| a.value_at(i)) }
            _ => None,
        };
        let class = match self.heap.try_get(recv) { Some(&HeapObj::Instance(c, _)) => c, Some(HeapObj::Class(..)) => recv, _ => return false };
        let (v, mark) = match held { Some(f) => (f, lower::FIELD), None => match self.lookup_class_member(class, name) { Some((v, _)) => (v, lower::STATIC), None => return false } };
        let func = match self.heap.try_get(v) { Some(&HeapObj::StaticMethod(f) | &HeapObj::ClassMethod(f)) if mark == lower::STATIC => f, Some(HeapObj::Func(..)) => v, _ => return false };
        if !matches!(self.heap.try_get(func), Some(HeapObj::Func(..))) { return false; }
        // Noted where the growth finds it, as what kind of call it is.
        self.note_called(code, chunk, at, v, mark);
        self.asks_growth(code, chunk, at, lower::Grow::Chain, func)
    }

    /* Whether the instance `recv` holds anything by the name the method head at `at` calls. */
    fn holds_name(&self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> bool {
        let Some(HeapObj::Instance(_, attrs)) = self.heap.try_get(recv) else { return false };
        let name = chunk_in(&self.functions, code, chunk, at).names.get(code.ins[at].a as usize).map_or("", |n| crate::parser::ssa_strip(n));
        attrs.borrow().position_str(name, crate::value::hash_str(name), &self.heap).is_some()
    }

    // The function the instance or the module `recv` holds by the name the method head at `at` calls.
    fn held_func(&self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> Option<Val> {
        let name = chunk_in(&self.functions, code, chunk, at).names.get(code.ins[at].a as usize).map_or("", |n| crate::parser::ssa_strip(n));
        let f = match self.heap.try_get(recv) {
            Some(HeapObj::Instance(_, attrs)) => { let a = attrs.borrow(); a.position_str(name, crate::value::hash_str(name), &self.heap).map(|i| a.value_at(i)) }
            Some(HeapObj::Module(_, attrs)) => attrs.iter().find(|(n, _)| n == name).map(|&(_, v)| v),
            _ => None,
        }?;
        matches!(self.heap.try_get(f), Some(HeapObj::Func(..))).then_some(f)
    }

    /* `enter_alt` for a head or section `k` laid for a function the receiver holds, the arguments the head wrote moved to `k`'s parameters with no receiver first. */
    #[inline(never)]
    fn enter_mixed(&mut self, block: &lower::Block, k: usize, head: usize, call: Ins, store: Ins, recv: Val) -> usize {
        let (h, s, base) = (&block.sections[head], &block.sections[k], self.base);
        let (from, to, n) = (base + h.link as usize + 1, base + s.link as usize + 1, call.x as usize);
        let r = &mut self.regs;
        let args: [Val; 5] = core::array::from_fn(|j| if j >= n { Val::undef() } else if h.on_class == 4 { r[from + j] } else if j == 0 { r[base + call.c as usize] } else { r[from + 1 + j] });
        let first = if s.on_class == 4 { to } else { r[to] = recv; to + 1 };
        r[first..first + n].copy_from_slice(&args[..n]);
        r[to - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
        self.budget -= 1;
        s.start as usize
    }

    /* Enters section `k` of `block` for the method head `call` on `recv`, its result to the register `store` names. */
    #[inline(always)]
    fn enter_alt(&mut self, block: &lower::Block, k: usize, head: usize, call: Ins, store: Ins, recv: Val) -> usize {
        // SAFETY `k` and `head` are sections of `block`, and the frame running it covers their registers.
        let (s, base) = (unsafe { block.sections.get_unchecked(k) }, self.base);
        let p = base + s.link as usize;
        let r = &mut self.regs;
        unsafe {
            let c = *r.get_unchecked(base + call.c as usize);
            (*r.get_unchecked_mut(p + 1), *r.get_unchecked_mut(p + 2), *r.get_unchecked_mut(p)) = (recv, c, Val::int((store.b as i64) << 16 | store.a as i64));
        }
        if k != head && (call.x > 1 || s.arity as usize > call.x as usize + 1) { moved(r, base, unsafe { block.sections.get_unchecked(head) }, s, call.x as usize + 1); }
        self.budget -= 1;
        s.start as usize
    }

    /* Notes the class `class` a method call at `rip` was made on, or with `mark` CALLED an instance's class a register call called, so code laid again lays its method. */
    #[cold]
    #[inline(never)]
    fn note_class(&mut self, code: &Code, chunk: &SSAChunk, rip: usize, class: Val, mark: u32) {
        if !matches!(self.heap.try_get(class), Some(HeapObj::Class(..))) { return; }
        self.note_called(code, chunk, rip, class, mark);
    }

    /* Counts a keyword call its lowering did not lay, made before its binding held its callee, toward laying the code again, a function's noted as a stub's is. */
    #[cold]
    #[inline(never)]
    fn kw_unlaid(&mut self, code: &Code, chunk: &SSAChunk, i: usize, op: u16) {
        if is_stub(code, i) { return; }
        let n = (op & 0xFF) as usize + 2 * (op >> 8) as usize;
        let Some(&callee) = self.stack.len().checked_sub(n + 1).and_then(|k| self.stack.get(k)) else { return };
        match self.heap.try_get(callee) {
            Some(HeapObj::Func(..)) => note_alt(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, i, callee, false),
            Some(HeapObj::Class(..)) => miss(&mut self.preempt_left, code),
            _ => {}
        }
    }

    /* Whether `code` may be lowered again with what its calls note, a function's or a loop's that turned already. */
    fn lays_later(&self, code: &Code) -> bool { code.block.is_some() || code.hot.get() != 0 || self.pools[self.pool_now].func != super::cache::NO_FN }

    /* `note_class` for any value `v`. */
    fn note_called(&mut self, code: &Code, chunk: &SSAChunk, rip: usize, class: Val, mark: u32) {
        // A stub's call of an instance or a bound method is noted where its head lies, inside the section that laid it.
        let (key, at) = match code.block.as_deref().and_then(|b| b.holding(rip).or_else(|| if mark & (lower::CALLED | lower::BOUND) == 0 { None } else { b.holding(stub_head(code, rip)?) })) {
            Some((s, rip)) => (s.fi as i64, s.orig[rip - s.start as usize] | mark),
            None => (VM::code_key_of(chunk, self.body_to_fi.get(&(chunk as *const SSAChunk)).copied()), code.orig(rip) | mark),
        };
        // A stub's call counts toward laying the code again with it.
        if is_stub(code, rip) && code.misses.get() < lower::MISSES {
            code.misses.set(code.misses.get() + 1);
            if code.misses.get() == lower::MISSES && self.preempt_left == 0 { self.preempt_left = RELAY; }
        }
        let mut seen = self.learned.chunks(3).filter(|t| t[0].as_int() == key && t[1].as_int() == at as i64);
        if seen.clone().count() >= 16 || seen.any(|t| t[2].0 == class.0) { return; }
        self.learned.extend_from_slice(&[Val::int(key), Val::int(at as i64), class]);
    }

    /* Enters method head `at`'s section for a method called on its class `recv` that still finds it, a static one with the arguments alone, a class one class first. */
    fn enter_on_class(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> Option<usize> {
        let block = code.block.as_deref()?;
        let (call, store, sect) = (code.ins[at], code.ins[at + 1], code.ins[at + 2]);
        let s = &block.sections[sect.c as usize];
        if self.budget == 0 { return None; }
        // An instance called through `__call__` enters it as a method, and a function the instance holds by the call's name enters with the arguments alone.
        if s.on_class == 4 {
            let name = chunk_in(&self.functions, code, chunk, at).names.get(call.a as usize).map_or("", |n| crate::parser::ssa_strip(n));
            let held = match self.heap.try_get(recv) {
                Some(HeapObj::Instance(_, attrs)) => { let a = attrs.borrow(); a.position_str(name, crate::value::hash_str(name), &self.heap).map(|i| a.value_at(i)) }
                Some(HeapObj::Module(_, attrs)) => attrs.iter().find(|(n, _)| n == name).map(|&(_, v)| v),
                _ => return None,
            };
            if held.is_none_or(|f| f.0 != s.value.get().0) { return None; }
            self.budget -= 1;
            self.regs[self.base + s.link as usize] = Val::int((store.b as i64) << 16 | store.a as i64);
            return Some(s.start as usize);
        }
        // A bound method of the head's function enters it with its receiver first.
        if s.on_class == 7 {
            let Some(&HeapObj::BoundUserMethod(own, func, _)) = self.heap.try_get(recv) else { return None };
            if func.0 != s.value.get().0 { return None; }
            return Some(self.enter_alt(block, sect.c as usize, sect.c as usize, call, store, own));
        }
        if s.on_class == 3 {
            let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(recv) else { return None };
            let (func, _) = self.lookup_class_member(class, "__call__")?;
            // Another class's `__call__` enters the section the head's table names for it.
            let k = if func.0 == s.value.get().0 { sect.c as usize } else {
                let (start, mask) = s.table;
                if mask == u32::MAX { return None; }
                let mut slot = lower::alt_slot(func, mask as usize);
                loop {
                    let &(v, k) = block.alt_tab.get(start as usize + slot)?;
                    if v.0 == func.0 || v.0 == lower::with_defaults(func).0 { break (k & !lower::DEFAULTS) as usize; }
                    if k == u32::MAX { return None; }
                    slot = (slot + 1) & mask as usize;
                }
            };
            if !self.shadowed { block.sections[k].guard.set((class, self.class_epoch, recv)); }
            return Some(self.enter_alt(block, k, sect.c as usize, call, store, recv));
        }
        // An instance finds a static or a class method in its class as the class does.
        if !matches!(self.heap.try_get(recv), Some(HeapObj::Class(..))) { return self.on_class_of(code, chunk, at, recv); }
        // Marked apart from an instance's class, so the head's own check never takes an instance of the class for it.
        let held = (recv, self.class_epoch | 1 << 31, Val::undef());
        if s.guard.get() != held {
            let name = chunk_in(&self.functions, code, chunk, at).names.get(call.a as usize).map_or("", |n| crate::parser::ssa_strip(n));
            let (v, _) = self.lookup_class_member(recv, name)?;
            let func = match *self.heap.try_get(v)? {
                HeapObj::StaticMethod(f) if s.on_class == 1 => f,
                HeapObj::ClassMethod(f) if s.on_class == 2 => f,
                HeapObj::Func(..) if s.on_class == 1 => v,
                _ => return None,
            };
            if func.0 != s.value.get().0 { return None; }
            s.guard.set(held);
        }
        self.budget -= 1;
        let (base, p) = (self.base, self.base + s.link as usize);
        if s.on_class == 2 { (self.regs[p + 1], self.regs[p + 2]) = (recv, self.regs[base + call.c as usize]); }
        self.regs[p] = Val::int((store.b as i64) << 16 | store.a as i64);
        Some(s.start as usize)
    }

    /* `enter_on_class` for an instance whose class holds the head's static or class method, run as its class's call does, unless the instance holds the name itself. */
    #[cold]
    #[inline(never)]
    fn on_class_of(&mut self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> Option<usize> {
        let block = code.block.as_deref()?;
        let (call, store, sect) = (code.ins[at], code.ins[at + 1], code.ins[at + 2]);
        let s = &block.sections[sect.c as usize];
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(recv) else { return None };
        if !matches!(s.on_class, 1 | 2) || self.holds_name(code, chunk, at, recv) { return None; }
        // Marked apart from the class's own call, which a plain function the class holds also takes.
        let held = (class, self.class_epoch | 1 << 30, Val::undef());
        if s.guard.get() != held {
            let name = chunk_in(&self.functions, code, chunk, at).names.get(call.a as usize).map_or("", |n| crate::parser::ssa_strip(n));
            let (v, _) = self.lookup_class_member(class, name)?;
            let func = match *self.heap.try_get(v)? {
                HeapObj::StaticMethod(f) if s.on_class == 1 => f,
                HeapObj::ClassMethod(f) if s.on_class == 2 => f,
                _ => return None,
            };
            if func.0 != s.value.get().0 { return None; }
            s.guard.set(held);
        }
        self.budget -= 1;
        let (base, p) = (self.base, self.base + s.link as usize);
        if s.on_class == 2 { (self.regs[p + 1], self.regs[p + 2]) = (class, self.regs[base + call.c as usize]); }
        self.regs[p] = Val::int((store.b as i64) << 16 | store.a as i64);
        Some(s.start as usize)
    }

    /* The plain method the receiver `recv` of the method head at `at` finds by the name its call names, None for any other. */
    #[inline(always)]
    fn method_found(&self, code: &Code, chunk: &SSAChunk, at: usize, recv: Val) -> Option<Val> {
        let (call, stub) = (code.ins[at], code.ins[at + 3]);
        let Some(&HeapObj::Instance(class, ref attrs)) = self.heap.try_get(recv) else { return None };
        let name = chunk_in(&self.functions, code, chunk, at).names.get(call.a as usize).map_or("", |n| n.as_str());
        if let Some(f) = super::sites::site_method_func(&self.heap, self.class_epoch, self.shadowed, &code.cache.site(stub.b as usize), recv, name) { return Some(f); }
        if self.shadowed && attrs.borrow().position_str(name, crate::value::hash_str(name), &self.heap).is_some() { return None; }
        let (f, _) = self.lookup_class_member(class, name)?;
        matches!(self.heap.try_get(f), Some(HeapObj::Func(..))).then_some(f)
    }

    /* A name loaded or stored on the stack, or a constant read, as the handler does for a local, an unshadowed global or a constant, false for any other. */
    #[inline(never)]
    fn stack_name(&mut self, ins: Ins, code: &Code) -> bool {
        let at = ins.a as usize;
        match ins.op {
            OpCode::LoadConst => match self.pools[self.pool_now].consts.as_deref().and_then(|c| c.get(at)) {
                Some(&v) => self.stack.push(v),
                None => return false,
            },
            OpCode::LoadName => {
                let v = match code.kinds.get(at) {
                    Some(&Kind::Global(g)) => match self.scopes.get(code.module) { Some(s) => match s.at(g) { v if v.is_undef() => s.fallback(g), v => v }, None => return false },
                    Some(Kind::Cell) => return false,
                    _ => self.regs.get(self.base + at).copied().unwrap_or(Val::undef()),
                };
                if v.is_undef() { return false; }
                self.stack.push(v);
            }
            _ => match code.kinds.get(at) {
                Some(&Kind::Global(g)) => {
                    let Some(scope) = self.scopes.get_mut(code.module) else { return false };
                    if scope.names_builtin(g).is_some() { return false; }
                    let Some(v) = self.stack.pop() else { return false };
                    scope.set_at(g, v);
                }
                Some(Kind::Cell) => return false,
                _ => {
                    if self.base + at >= self.regs.len() { return false; }
                    let Some(v) = self.stack.pop() else { return false };
                    self.regs[self.base + at] = v;
                }
            },
        }
        true
    }

    /* A register call past two operands, its values put on the stack as a stack call takes them. */
    #[inline(never)]
    fn call_wide(&mut self, ins: Ins, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        let (regs, count) = lower::call_regs(ins, *code.ins.get(i + 1)?);
        // A builtin method the site knows takes its arguments straight from the registers.
        if ins.op == OpCode::CallMethodR && count <= 4 && let Some(id) = self.site_builtin(code.cache.site(i), self.regs[self.base + regs[0] as usize]) {
            let r = &self.regs[self.base..];
            let vals = [r[regs[0] as usize], r[regs[1] as usize], r[regs[2] as usize], r[regs[3] as usize]];
            if vals[..count].iter().any(|v| v.is_undef()) { return None; }
            return self.exec_bound_method(vals[0], id, &vals[1..count], &[], chunk).map(|()| i + 2).map_err(|e| self.reg_error = Some(e)).ok();
        }
        // A builtin of a few arguments takes them from the registers, past no stack.
        if ins.op != OpCode::CallMethodR && count <= 9 {
            let r = &self.regs[self.base..];
            let callee = if ins.op == OpCode::CallGlobalR { self.scopes[code.module].read(ins.a as u32) } else { r[regs[0] as usize] };
            if let Some(&HeapObj::NativeFn(id)) = self.heap.try_get(callee) {
                let mut vals = [Val::undef(); 8];
                for (v, &k) in vals.iter_mut().zip(&regs[1..count]) { *v = r[k as usize]; }
                if vals[..count - 1].iter().any(|v| v.is_undef()) { return None; }
                self.pending.call_ip = Some(code.orig(i));
                return self.native_direct(id, &vals[..count - 1], chunk, i).map(|()| i + 2).map_err(|e| self.reg_error = Some(e)).ok();
            }
        }
        let (start, r) = (self.stack.len(), &self.regs[self.base..]);
        for &k in &regs[..count] {
            // An unbound operand leaves for the handler, which names it.
            let v = r[k as usize];
            if v.is_undef() { self.stack.truncate(start); return None; }
            self.stack.push(v);
        }
        if ins.op == OpCode::CallGlobalR {
            let callee = self.scopes[code.module].read(ins.a as u32);
            if callee.is_undef() { self.stack.truncate(start); return None; }
            self.stack[start] = callee;
        }
        let called = if ins.op != OpCode::CallMethodR {
            let callee = self.stack[start];
            self.call_stacked(ins.x as u16, code.orig(i), chunk, (i, super::redo::Rerun::Regs)).map(|()| if self.entered { self.learn_call(i, code, callee); })
        } else {
            self.exec_call_method(ins.a, ins.x as u16, i, code, chunk, super::redo::Rerun::Regs)
        };
        called.map(|()| i + 2).map_err(|e| self.reg_error = Some(e)).ok()
    }

    /* Calls the builtin `id` on `args` from the register loop as a stack call would, its result left on the stack. */
    #[inline(never)]
    fn native_direct(&mut self, id: super::types::NativeFnId, args: &[Val], chunk: &SSAChunk, rip: usize) -> Result<(), VmErr> {
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.charge_step()?;
        // One given a user object hands the calls it makes to the loop, the register call run again after each.
        if self.heap.users && super::opcodes::function::native_redoable(id) { return self.native_redone(id, args, chunk, rip); }
        // It may run its callback as a frame of this loop, as a call through the stack does.
        self.job_ok = true;
        let done = self.dispatch_native(id, args, &[], chunk);
        self.job_ok = false;
        done
    }

    /* `native_direct` of a builtin that may call Python, its op armed once such a call needs it. */
    #[inline(never)]
    fn native_redone(&mut self, id: super::types::NativeFnId, args: &[Val], chunk: &SSAChunk, rip: usize) -> Result<(), VmErr> {
        self.job_ok = true;
        let armed = self.redo_defer(rip, super::redo::Rerun::Regs, self.stack.len() + args.len(), args);
        let done = self.dispatch_native(id, args, &[], chunk);
        self.redo_undefer(armed);
        self.job_ok = false;
        done
    }

    /* Keeps the callee of the frame a call at `i` just opened in its site, a plain function or a class with its `__init__`. */
    fn learn_call(&self, i: usize, code: &Code, callee: Val) {
        // A function called once or twice keeps its sites as they are, its first code replaced at its second call.
        if !self.entered || self.acts == 0 || self.pools[self.top_pool].calls <= 2 { return; }
        let Some(f) = self.top() else { return };
        if f.callee.0 == callee.0 && f.instance.is_undef() && matches!(self.heap.try_get(callee), Some(HeapObj::Func(..))) {
            code.cache.set_site(i, Site::Call { callee, fi: f.fi as u32 });
        } else if !f.instance.is_undef() && let Some(&super::Ctor { epoch, init: Some((init, owner)), exception: false, attrs, native: None, special: false }) = self.ctors.get(&callee.0)
            && init.0 == f.callee.0 && epoch == self.class_epoch {
            code.cache.set_site(i, Site::Ctor { class: callee.as_heap(), init, owner, fi: f.fi as u32, attrs: attrs.min(u16::MAX as usize) as u16, epoch });
        }
    }

    /* A call whose callee and `count` positionals are on the stack, made at compiler instruction `orig`, opening a plain function as a frame of the loop. */
    #[inline(never)]
    fn call_stacked(&mut self, count: u16, orig: u32, chunk: &SSAChunk, site: (usize, super::redo::Rerun)) -> Result<(), VmErr> {
        self.pending.call_ip = Some(orig);
        if self.call_in_loop(count)? { return Ok(()); }
        self.call_general(count, chunk, site)
    }

    /* The general call of a callee and `count` positionals on the stack, which may still run in the loop. */
    #[inline(always)]
    fn call_general(&mut self, count: u16, chunk: &SSAChunk, site: (usize, super::redo::Rerun)) -> Result<(), VmErr> {
        // Before the program makes a class with dunders no builtin's call runs Python.
        if self.heap.users { return self.call_sited(count, chunk, site); }
        self.pending_exec_safe = self.frame_safe;
        self.inline_call = true;
        let called = self.exec_call(count, chunk);
        self.pending_exec_safe = false;
        called
    }

    /* `call_general` where the builtin the call may dispatch knows its op, to run it again after a call it makes. */
    #[inline(never)]
    fn call_sited(&mut self, count: u16, chunk: &SSAChunk, site: (usize, super::redo::Rerun)) -> Result<(), VmErr> {
        self.pending_exec_safe = self.frame_safe;
        self.inline_call = true;
        self.redo.set_site((site.0 as u32, site.1));
        let called = self.exec_call(count, chunk);
        self.pending_exec_safe = false;
        self.redo.end_site();
        called
    }

    /* Register-loop opcodes kept apart from the hot ones, the next index or None. */
    #[inline(never)]
    fn regs_aside(&mut self, ins: Ins, n: usize, i: usize, code: &Code, chunk: &SSAChunk) -> Option<usize> {
        match ins.op {
            // An op's call of its dunder's section jumps to it while the operands are as the site of the op's stub found them.
            OpCode::DunderSectionR => return self.dunder_section(code, chunk, i),
            // A length or truth the dunder gave goes on as it is, anything else to the handler.
            OpCode::DunderCheckR => {
                let v = self.regs[self.base + ins.a as usize];
                let fine = match ins.x { 2 => v.is_bool(), 4 | 5 => matches!(self.heap.try_get(v), Some(HeapObj::Str(_))), _ => v.is_int() && v.as_int() >= 0 || ins.x == 3 && v.is_bool() };
                if !fine { return None; }
            }
            // The return of a section an op calls, NotImplemented handing the op to its stub through the handler.
            OpCode::ReturnDunderR => {
                let (link, v) = (self.regs[self.base + ins.a as usize].as_int() as usize, self.regs[self.base + ins.b as usize]);
                // One from inside the section's `try` runs its handlers first.
                if v.is_undef() || ins.x != 0 && self.exception_stack.last().is_some_and(|f| f.handler_ip & RAW_HANDLER != 0) { return None; }
                if self.heap.is_not_implemented(v) { return if self.declined_grows(code, chunk, link >> 16) { None } else { self.dunder_declined(code, i, link >> 16) }; }
                self.regs[self.base + (link & 0xFFFF)] = v;
                return Some(link >> 16);
            }
            // The blocks of a `try` and a `with`, as the handler keeps them, an exit carrying anything but a fall-through left to it.
            OpCode::PopExcept => if let Some(f) = self.exception_stack.pop() { self.handling.truncate(f.handling_depth); },
            OpCode::BeginFinally => self.unwind_stack.push(Unwind::Normal),
            OpCode::EndFinally => match self.unwind_stack.last() { None | Some(Unwind::Normal) => { self.unwind_stack.pop(); } _ => return None },
            // A `with` staged off the stack or for an exit that carries something, while its site knows the class.
            OpCode::WithEnter | OpCode::WithExit if ins.x == 1 => if !self.with_hit(ins, i, code) { return None },
            // What `__exit__` returned decides nothing unless an exception leaves.
            OpCode::WithJudge => if matches!(self.unwind_stack.last(), Some(Unwind::Reraise(..))) || ins.x == 0 && self.stack.pop().is_none() { return None },
            OpCode::PowR => self.regs[self.base + ins.a as usize] = numeric_binop(OpCode::Pow, self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize])?,
            // A stack value tested by plain truth, a short-circuit keeping it to jump.
            OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop => if self.stack_jump(ins, n)? { return Some(ins.a as usize) },
            OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR => if !self.accumulate(ins, self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]) { return None },
            OpCode::UnpackR => {
                let fits = seq_items(&self.heap, *self.stack.last()?, |items| {
                    if items.len() != ins.x as usize { return false; }
                    for (&k, &v) in [ins.a, ins.b, ins.c].iter().zip(items) { self.regs[self.base + k as usize] = v; }
                    true
                });
                if fits != Some(true) { return None; }
                self.stack.pop();
            }
            // Common stack opcodes stay in the loop, leaving untouched when they miss.
            OpCode::UnbindR => self.regs[self.base + ins.a as usize] = Val::undef(),
            OpCode::SuperR => { let v = self.super_of_method(self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]).ok()?; self.regs[self.base + ins.a as usize] = v; }
            OpCode::WrapCellR => { let v = self.heap.alloc(HeapObj::Cell(self.regs[self.base + ins.a as usize])).ok()?; self.regs[self.base + ins.a as usize] = v; }
            // Another module's binding, read or written by a section laid from that module.
            OpCode::LoadGlobalAtR => {
                let scope = self.scopes.get(ins.c as usize)?;
                let v = match scope.at(ins.b as u32) { v if v.is_undef() => scope.fallback(ins.b as u32), v => v };
                if v.is_undef() { return None; }
                self.regs[self.base + ins.a as usize] = v;
            }
            OpCode::StoreGlobalAtR if ins.x == 0 => {
                let v = self.regs[self.base + ins.b as usize];
                if v.is_undef() { return None; }
                self.scopes.get_mut(ins.c as usize)?.set_at(ins.a as u32, v);
            }
            _ => if !self.stack_fast(ins) { return None },
        }
        Some(i + 1)
    }

    /* Whether a stack-tested branch jumps, None for user code or a spent budget. */
    #[inline]
    fn stack_jump(&mut self, ins: Ins, n: usize) -> Option<bool> {
        let t = self.plain_truth(*self.stack.last()?)?;
        let jump = t == (ins.op == OpCode::JumpIfTrueOrPop);
        if jump && !(self.budget > 0 && (ins.a as usize) <= n) { return None; }
        if !jump || ins.op == OpCode::JumpIfFalse { self.stack.pop(); }
        if jump { self.budget -= 1; }
        Some(jump)
    }

    /* A value's truth when no user code decides it. */
    #[inline(always)]
    pub(crate) fn plain_truth(&self, v: Val) -> Option<bool> {
        if v.is_bool() { return Some(v.as_bool()); }
        if v.is_int() { return Some(v.as_int() != 0); }
        if v.is_none() { return Some(false); }
        match self.heap.try_get(v) { Some(HeapObj::Str(s)) => Some(!s.is_empty()), _ => None }
    }

    /* A stack opcode's common case done in place, false and untouched on a miss. */
    #[inline]
    fn stack_fast(&mut self, ins: Ins) -> bool {
        let len = self.stack.len();
        let n = ins.a as usize;
        let v = match ins.op {
            OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildString => {
                if len < n { return false; }
                let items = &self.stack[len - n..];
                let obj = match ins.op {
                    OpCode::BuildList => HeapObj::List(Rc::new(RefCell::new(items.to_vec()))),
                    OpCode::BuildTuple => HeapObj::Tuple(items.to_vec()),
                    _ => {
                        let mut s = String::new();
                        for &p in items {
                            match self.heap.try_get(p) {
                                Some(HeapObj::Str(part)) => s.push_str(part),
                                // A bare field leaves an inline value, rendered only here.
                                None if !p.is_undef() => s.push_str(&self.display(p)),
                                _ => return false,
                            }
                        }
                        HeapObj::Str(s)
                    }
                };
                let Ok(v) = self.heap.alloc(obj) else { return false };
                self.stack.truncate(len - n);
                v
            }
            OpCode::UnpackSequence => {
                let Some(&top) = self.stack.last() else { return false };
                let stack = &mut self.stack;
                return seq_items(&self.heap, top, |items| {
                    if items.len() != n { return false; }
                    stack.pop();
                    stack.extend(items.iter().rev());
                    true
                }) == Some(true);
            }
            OpCode::ListAppend | OpCode::SetAdd | OpCode::MapAdd => {
                let arity = if ins.op == OpCode::MapAdd { 2 } else { 1 };
                if len < arity + 1 { return false; }
                let (key, value) = (self.stack[len - arity], self.stack[len - 1]);
                self.stack.truncate(len - arity);
                let form = match ins.op { OpCode::ListAppend => OpCode::ListAppendR, OpCode::SetAdd => OpCode::SetAddR, _ => OpCode::MapAddR };
                if self.accumulate(Ins { op: form, ..ins }, key, value) { return true; }
                self.stack.extend_from_slice(&[key, value][2 - arity..]);
                return false;
            }
            // Two numbers on the stack, as the register forms compute them.
            OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Div | OpCode::Mod | OpCode::FloorDiv | OpCode::InPlaceAdd | OpCode::InPlaceSub
            | OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::LtEq | OpCode::Gt | OpCode::GtEq => {
                if len < 2 { return false; }
                let (x, y) = (self.stack[len - 2], self.stack[len - 1]);
                let v = match numeric_binop(ins.op, x, y) {
                    Some(v) => v,
                    // Strings joined or one repeated run the op itself, as the handler would.
                    None if matches!(ins.op, OpCode::Add | OpCode::Mul) && matches!(self.heap.try_get(x), Some(HeapObj::Str(_))) && (y.is_int() || matches!(self.heap.try_get(y), Some(HeapObj::Str(_)))) => {
                        match if ins.op == OpCode::Add { self.add_vals(x, y) } else { self.mul_vals(x, y) } { Ok(v) => v, Err(_) => return false }
                    }
                    None => return false,
                };
                self.stack.truncate(len - 2);
                v
            }
            OpCode::FormatValue | OpCode::CallStr | OpCode::CallInt | OpCode::CallFloat | OpCode::CallBool
            | OpCode::CallAbs | OpCode::CallChr | OpCode::CallOrd | OpCode::CallLen => {
                // A fused builtin takes one bare argument, a rebound name runs the binding.
                let unary = if ins.op == OpCode::FormatValue { n == 0 } else { n == 1 && !self.fused_rebound(ins.op) };
                let Some(&x) = self.stack.last().filter(|_| unary) else { return false };
                let Some(v) = self.unary_fast(ins.op, x) else { return false };
                self.stack.pop();
                v
            }
            _ => return false,
        };
        self.stack.push(v);
        true
    }

    /* Adds `key` and `value` to the stack-top accumulator, false when the handler must check the key. */
    fn accumulate(&mut self, ins: Ins, key: Val, value: Val) -> bool {
        let Some(&acc) = self.stack.last() else { return false };
        if value.is_undef() { return false; }
        let plain = self.plain_key(key);
        match (ins.op, self.heap.try_get(acc)) {
            (OpCode::ListAppendR, Some(HeapObj::List(rc))) => rc.borrow_mut().push(value),
            (OpCode::SetAddR, Some(HeapObj::Set(rc))) if plain => { rc.borrow_mut().insert(key, &self.heap); }
            (OpCode::MapAddR, Some(HeapObj::Dict(rc))) if plain => rc.borrow_mut().insert(key, value, &self.heap),
            _ => return false,
        }
        true
    }

    /* A one-argument builtin or bare format on a plain value, else None. */
    fn unary_fast(&mut self, op: OpCode, x: Val) -> Option<Val> {
        let text = |vm: &mut Self, x: Val| match vm.heap.try_get(x) {
            Some(HeapObj::Str(_)) => Some(x),
            None if !x.is_undef() && !x.is_foreign() => { let s = vm.display(x); vm.heap.alloc(HeapObj::Str(s)).ok() }
            _ => None,
        };
        match op {
            // A bare field keeps an inline value, the string that holds it renders it.
            OpCode::FormatValue if !x.is_heap() && !x.is_undef() && !x.is_foreign() => Some(x),
            OpCode::FormatValue | OpCode::CallStr => text(self, x),
            OpCode::CallInt if x.is_int() => Some(x),
            OpCode::CallInt if x.is_bool() => Some(Val::int(x.as_bool() as i64)),
            OpCode::CallInt if x.is_float() && x.as_float().is_finite() && x.as_float().abs() < Val::INT_MAX as f64 => Some(Val::int(x.as_float() as i64)),
            OpCode::CallFloat if x.is_float() => Some(x),
            OpCode::CallFloat if x.is_int() => Some(Val::float(x.as_int() as f64)),
            OpCode::CallBool => self.plain_truth(x).map(Val::bool),
            OpCode::CallAbs if x.is_int() => Val::int_checked(x.as_int().abs()),
            OpCode::CallAbs if x.is_float() => Some(Val::float(x.as_float().abs())),
            OpCode::CallChr if x.is_int() => {
                let c = u32::try_from(x.as_int()).ok().and_then(char::from_u32)?;
                self.heap.alloc(HeapObj::Str(c.into())).ok()
            }
            OpCode::CallOrd => match self.heap.try_get(x) {
                Some(HeapObj::Str(s)) => { let mut cs = s.chars(); let c = cs.next()?; cs.next().is_none().then(|| Val::int(c as i64)) }
                _ => None,
            },
            OpCode::CallLen => self.plain_len(x).map(|n| Val::int(n as i64)),
            _ => None,
        }
    }

    /* A key that is hashable by its kind alone, so a fast path skips the hashability check. */
    #[inline(always)]
    fn plain_key(&self, k: Val) -> bool { plain_key_in(&self.heap, k) }

    /* A builtin container's length or an ASCII string's, None when a dunder may decide. */
    #[inline(always)]
    fn plain_len(&self, v: Val) -> Option<usize> {
        Some(match self.heap.plain(v)? {
            HeapObj::List(v) => v.borrow().len(),
            HeapObj::Tuple(v) => v.len(),
            HeapObj::Dict(d) | HeapObj::FrozenDict(d) => d.borrow().len(),
            HeapObj::Set(s) => s.borrow().len(),
            HeapObj::Str(s) if s.is_ascii() => s.len(),
            _ => return None,
        })
    }

    /* The fast path for a subscript of a plain list, tuple or dict, false when it misses. */
    #[inline(never)]
    fn item_fast(&mut self, ins: Ins) -> bool {
        let r = &self.regs[self.base..];
        if ins.op == OpCode::GetItemR {
            let (o, k) = (r[ins.b as usize], r[ins.c as usize]);
            let hit = match self.heap.plain(o) {
                Some(HeapObj::List(v)) if k.is_int() => { let b = v.borrow(); index_of(k.as_int(), b.len()).map(|j| b[j]) }
                Some(HeapObj::Tuple(v)) if k.is_int() => index_of(k.as_int(), v.len()).map(|j| v[j]),
                Some(HeapObj::Dict(d) | HeapObj::FrozenDict(d)) => dict_get(&self.heap, d, k),
                _ => None,
            };
            let Some(v) = hit else { return false };
            self.regs[self.base + ins.a as usize] = v;
            return true;
        }
        let (o, k, v) = (r[ins.a as usize], r[ins.b as usize], r[ins.c as usize]);
        match self.heap.plain(o) {
            Some(HeapObj::List(l)) if k.is_int() => {
                let mut b = l.borrow_mut();
                let Some(j) = index_of(k.as_int(), b.len()) else { return false };
                b[j] = v;
            }
            Some(HeapObj::Dict(d)) => if !dict_set(&self.heap, d, k, v) { return false },
            _ => return false,
        }
        true
    }

    /* The fast path for a key, length or cell probe, false when it misses. */
    #[inline(never)]
    fn heap_fast(&mut self, ins: Ins) -> bool {
        match ins.op {
            OpCode::InR | OpCode::NotInR => {
                let (item, container) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
                // A plain key probes a plain dict or set, anything else runs the protocol.
                if !self.plain_key(item) { return false; }
                let hit = match self.heap.try_get(container) {
                    Some(HeapObj::Set(rc)) => rc.borrow().contains(item, &self.heap),
                    Some(HeapObj::Dict(rc) | HeapObj::FrozenDict(rc)) => rc.borrow().contains_key(&item, &self.heap),
                    Some(HeapObj::FrozenSet(s)) => s.contains(item, &self.heap),
                    _ => return false,
                };
                self.regs[self.base + ins.a as usize] = Val::bool(hit == (ins.op == OpCode::InR));
            }
            OpCode::LenR => {
                if self.builtins_rebound & (1 << NativeFnId::Len as u32) != 0 { return false; }
                let Some(n) = self.plain_len(self.regs[self.base + ins.b as usize]) else { return false };
                self.regs[self.base + ins.a as usize] = Val::int(n as i64);
            }
            OpCode::LoadCellR => {
                let Some(&HeapObj::Cell(v)) = self.heap.try_get(self.regs[self.base + ins.b as usize]) else { return false };
                if v.is_undef() { return false; }
                self.regs[self.base + ins.a as usize] = v;
            }
            OpCode::StoreCellR => {
                let v = self.regs[self.base + ins.b as usize];
                if v.is_undef() { return false; }
                let Some(HeapObj::Cell(inner)) = self.heap.try_get_mut(self.regs[self.base + ins.a as usize]) else { return false };
                *inner = v;
            }
            _ => return false,
        }
        true
    }

    /* Register forms whose fast path missed, run as their stack opcode. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn exec_reg(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, cache: &OpcodeCache, code: &Code, ip: &mut usize, exc_base: usize) -> Result<Option<Val>, VmErr> {
        let n = code.ins.len();
        let op = ins.a;
        match ins.op {
            // A jump to or from a head laid past the end of code that grew, no step of the program's.
            OpCode::JumpR => *ip = ins.a as usize,
            // A comparison keeps what a user dunder returned, which need not be a bool.
            OpCode::AddR | OpCode::InPlaceAddR | OpCode::SubR | OpCode::InPlaceSubR | OpCode::MulR | OpCode::DivR | OpCode::ModR | OpCode::FloorDivR | OpCode::PowR
            | OpCode::EqR | OpCode::NotEqR | OpCode::LtR | OpCode::LtEqR | OpCode::GtR | OpCode::GtEqR => {
                let (l, r) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
                let inst = |v: Val| matches!(self.heap.try_get(v), Some(HeapObj::Instance(..)));
                // Only an instance's op meets a dunder.
                let open = inst(l) || inst(r);
                let met = open && !code.unchanged();
                let declined = met && !self.declined.is_undef();
                // One outside the sections laid as no head asks for one for the dunder it meets before it runs.
                if self.reg_binop(ins, rip, cache, chunk, open.then_some(code))? { *ip = rip; return Ok(None); }
                // An op's dunder stub notes another dunder it met for code laid again, the left's or the right's reflected once the left declined or a subclass went first.
                if met {
                    if let Site::Dunder { func, .. } = cache.site(rip) { note_dunder(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, rip, func); }
                    if let Some(f) = self.reflected_met(ins.op, l, r, declined) { note_dunder(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, rip, f); }
                }
            }
            OpCode::JumpUnlessEq | OpCode::JumpUnlessNotEq | OpCode::JumpUnlessLt | OpCode::JumpUnlessLtEq | OpCode::JumpUnlessGt | OpCode::JumpUnlessGtEq
                => if !self.reg_test(ins, rip, cache, chunk)? { *ip = self.checked_jump(op as usize, n)?; },
            OpCode::JumpIfFalseR => {
                if code.block.is_some() && code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.truth_grows(code, chunk, rip, ins) { *ip = rip; return Ok(None); }
                if self.reg_truthy(chunk, ins.b, rip)? == (ins.x != 0) { *ip = self.checked_jump(op as usize, n)?; }
                self.learn_truth(rip, cache, code, chunk, self.regs[self.base + ins.b as usize]);
            }
            // What a dunder gave, which the register loop found no length or truth.
            OpCode::DunderCheckR => {
                let (v, o) = (self.regs[self.base + ins.a as usize], self.regs[self.base + ins.b as usize]);
                self.regs[self.base + ins.a as usize] = self.dunder_checked(ins.x, v, o)?;
            }
            // A dict subscript the register loop left skips the generic protocol.
            OpCode::GetItemR | OpCode::StoreItemR if self.item_fast(ins) => {}
            // An instance whose class's `__getitem__` the site learned runs it as the next frame of this loop.
            OpCode::GetItemR if self.open_reg_dunder(ins, rip, cache)? => if let Site::Dunder { func, .. } = cache.site(rip) {
                note_dunder(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, rip, func);
            },
            OpCode::LenR | OpCode::MinusR | OpCode::NotR | OpCode::GetItemR | OpCode::InR | OpCode::NotInR | OpCode::IsR | OpCode::IsNotR
            | OpCode::BitAndR | OpCode::BitOrR | OpCode::BitXorR | OpCode::ShlR | OpCode::ShrR => {
                let (b, c) = (self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize]);
                if matches!(ins.op, OpCode::BitAndR | OpCode::BitOrR | OpCode::BitXorR | OpCode::ShrR) && let Some(v) = self.wide_fast(super::lower::stack_form(ins.op), b, c)? {
                    self.regs[self.base + ins.a as usize] = v;
                    return Ok(None);
                }
                if ins.op == OpCode::LenR { self.pending.call_ip = Some(code.orig(rip)); }
                if matches!(ins.op, OpCode::MinusR | OpCode::InR | OpCode::NotInR | OpCode::LenR) && code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.op_dunder_grows(code, chunk, rip, ins, if matches!(ins.op, OpCode::MinusR | OpCode::LenR) { b } else { c }) { *ip = rip; return Ok(None); }
                if ins.op == OpCode::NotR && code.block.is_some() && code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.truth_grows(code, chunk, rip, ins) { *ip = rip; return Ok(None); }
                self.regs[self.base + ins.a as usize] = self.reg_stack(ins, rip, cache, chunk)?;
                // An instance's `__contains__` is learned as an op's dunder is, a negation's as its handler learned it.
                if matches!(ins.op, OpCode::InR | OpCode::NotInR) { self.learn_dunder(rip, cache, code, chunk, c, "__contains__", 2); }
                if ins.op == OpCode::LenR { self.learn_dunder(rip, cache, code, chunk, b, "__len__", 1); }
                if ins.op == OpCode::NotR { self.learn_truth(rip, cache, code, chunk, b); }
                if ins.op == OpCode::MinusR && let Site::Dunder { func, .. } = cache.site(rip) { note_dunder(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, rip, func); }
            }
            OpCode::UnpackR => {
                self.unpack_iterable(ins.x as usize, None, None, chunk)?;
                for &t in [ins.a, ins.b, ins.c].iter().take(ins.x as usize) { self.regs[self.base + t as usize] = self.pop()?; }
            }
            OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR => {
                let v = self.reg(chunk, ins.c)?;
                if ins.op == OpCode::MapAddR { let k = self.reg(chunk, ins.b)?; self.push(k); }
                self.push(v);
                let form = match ins.op { OpCode::ListAppendR => OpCode::ListAppend, OpCode::SetAddR => OpCode::SetAdd, _ => OpCode::MapAdd };
                if form == OpCode::ListAppend || !self.user_hash { self.handle_comprehension(form)?; } else { self.keyed_comprehension(form, rip)?; }
            }
            OpCode::Move => self.regs[self.base + ins.a as usize] = self.reg(chunk, ins.b)?,
            OpCode::PushRegs => self.push_regs(ins, chunk)?,
            OpCode::CallR | OpCode::CallMethodR | OpCode::CallGlobalR => {
                let wide = lower::wide(ins);
                let (regs, count) = lower::call_regs(ins, if wide { code.ins[rip + 1] } else { ins });
                // A global callee is read here, its error naming the binding as LoadGlobalR's would.
                let first = if ins.op == OpCode::CallGlobalR { let v = self.global_at(code.module, ins.a as u32)?; self.push(v); 1 } else { 0 };
                for &r in &regs[first..count] { let v = self.reg(chunk, r)?; self.push(v); }
                *ip += wide as usize;
                if ins.op != OpCode::CallMethodR { self.call_stacked(ins.x as u16, code.orig(rip), chunk, (rip, super::redo::Rerun::Regs))?; }
                else { self.exec_call_method(ins.a, ins.x as u16, rip, code, chunk, super::redo::Rerun::Regs)?; }
            }
            // The items of the sequence spread onto the stack as an unpacked argument list leaves them, then called.
            OpCode::CallSpreadR => {
                let (f, seq) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                // A spread head's stub asks for its head laid for the function it met, which its table then finds.
                if code.block.is_some() && self.budget != 0 && !self.redo.keep && matches!(self.heap.try_get(f), Some(HeapObj::Func(..))) && let Some(h) = stub_head(code, rip)
                    && code.ins[h + 2].x & 72 == 72 && self.asks_growth(code, chunk, h, lower::Grow::Value, f) { *ip = rip; return Ok(None); }
                if let Some(&HeapObj::Func(fi, ..)) = self.heap.try_get(f) { cache.set_site(rip, Site::Call { callee: f, fi: fi as u32 }); }
                // A tuple or list of a few items spread into a function that opens in the loop are its positionals.
                if ins.x != 2 && let Some(args) = self.few_items(seq) {
                    let n = args.1;
                    self.pending.call_ip = Some(code.orig(rip));
                    if self.open_in_loop(f, Passed::Regs(args.0), n)? { return Ok(None); }
                    self.pending.call_ip = None;
                }
                self.push(f);
                self.pending.delta_save.push((self.pending.pos_delta, self.pending.kw_delta));
                (self.pending.pos_delta, self.pending.kw_delta) = (0, 0);
                self.push(seq);
                self.unpack_args(if ins.x == 2 { 2 } else { 1 }, chunk, None)?;
                self.pending.call_ip = Some(code.orig(rip));
                self.pending_exec_safe = self.frame_safe;
                let called = self.handle_function(OpCode::CallSpread, 1, chunk);
                self.pending_exec_safe = false;
                called?;
            }
            // Read by the call before it, never run alone.
            OpCode::ArgsR | OpCode::CallValueR => {}
            // A level of a recursion through sections, which raises past the frames' share of memory.
            OpCode::TurnR => if !self.turn(&code.ins[ins.a as usize..ins.a as usize + ins.b as usize], ins.x, ins.c) { return Err(cold_depth()); },
            OpCode::UnturnR => *ip = rip + self.unturn(&code.ins[rip..]),
            // A section's return the loop gave way on, its value unbound.
            OpCode::ReturnSectionR | OpCode::ReturnTriedR | OpCode::ReturnBoundR | OpCode::ReturnDunderR => {
                let (link, v) = (self.regs[self.base + ins.a as usize].as_int() as usize, self.regs[self.base + ins.b as usize]);
                if v.is_undef() { return Err(VmErr::Raised(alloc::string::String::from("UnboundLocalError: cannot access a local variable that is not associated with a value"))); }
                // An op's dunder that declined hands the op to its stub, which goes on with the reflected dunder.
                let ret = link >> 16;
                // A `finally` of the section runs first, its end returning.
                if ins.x != 0 && let Some(s) = section_at(code, rip) && let Some(h) = self.section_cleanup(s) {
                    self.unwind_stack.push(Unwind::Return(v));
                    *ip = h & !RAW_HANDLER;
                    return Ok(None);
                }
                if ins.op == OpCode::ReturnDunderR && self.heap.is_not_implemented(v) && let Some(to) = self.dunder_declined(code, rip, ret) {
                    *ip = to;
                    return Ok(None);
                }
                self.regs[self.base + (link & 0xFFFF)] = v;
                *ip = link >> 16;
            }
            // Past an ordinary call in a section call's place, what was pending under it comes back off the stack.
            OpCode::SectionR => if ins.x != lower::STUB && let under = code.ins[rip - 1].c && under > 0 {
                let v = self.pop()?;
                if ins.x & 2 == 0 { self.regs[self.base + under as usize - 1] = v; }
            },
            OpCode::AppendR => {
                let at = self.base + ins.a as usize;
                let (sv, tv) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                if sv.0 == self.owned_str.0 && self.owned_at == at && self.heap.append_str(sv, tv) { return Ok(None); }
                self.reg_binop(Ins { op: OpCode::InPlaceAddR, x: 0, a: ins.a, b: ins.a, c: ins.b }, rip, cache, chunk, None)?;
                // A string the add just made belongs to this register alone, so the next appends grow it in place.
                let made = self.regs[at];
                let strs = matches!(self.heap.plain(sv), Some(HeapObj::Str(_))) && matches!(self.heap.plain(tv), Some(HeapObj::Str(_)));
                if strs && made.0 != sv.0 && made.0 != tv.0 && self.heap.growable_str(made) { (self.owned_str, self.owned_at) = (made, at); }
            }
            OpCode::OwnResetR => if self.owned_at == self.base + ins.a as usize { self.owned_str = Val::undef(); },
            OpCode::ListPushR => {
                let (l, v) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                let Some(HeapObj::List(rc)) = self.heap.try_get(l) else { return Err(cold_type("append: receiver is not a list")) };
                rc.borrow_mut().push(v);
            }
            OpCode::StoreItemR => {
                let (o, k, v) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?, self.reg(chunk, ins.c)?);
                if code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.op_dunder_grows(code, chunk, rip, ins, o) { *ip = rip; return Ok(None); }
                self.store_item_r(o, k, v, rip, chunk)?;
                // An instance's `__setitem__` is learned, so lowered again the store calls its section.
                self.learn_dunder(rip, cache, code, chunk, o, "__setitem__", 3);
            }
            OpCode::GetAttrR => {
                let o = self.reg(chunk, ins.b)?;
                if code.block.is_some() && code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.attr_grows(code, chunk, rip, ins, o) { *ip = rip; return Ok(None); }
                let Some(name) = chunk.names.get(ins.c as usize) else { return Err(VmErr::Runtime("LoadAttr: bad name index")) };
                (self.ran, self.prop_at) = (0, Some((rip, ins.a)));
                self.load_attr(o, name, chunk)?;
                // A getter opened as a frame is learned as one run in place is, so lowered again the load calls its section.
                if self.ran >= 3 {
                    let getattr = core::mem::take(&mut self.ran) == 4;
                    let held = matches!(cache.site(rip), Site::Dunder { class, epoch, .. } if epoch == self.class_epoch
                        && matches!(self.heap.try_get(o), Some(&HeapObj::Instance(c, _)) if c.0 == class.0));
                    if !held && let Some(site) = if getattr { self.learn_getattr(o, name) } else { self.learn_property(o, name, false) } { cache.set_site(rip, site); }
                    // Noted each time, as a function run in place is, so the code is laid again calling its section.
                    if let Site::Dunder { func, .. } = cache.site(rip) { note_dunder(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, rip, func); }
                    return Ok(None);
                }
                let v = self.pop()?;
                self.regs[self.base + ins.a as usize] = v;
                let site = match self.heap.try_get(v) {
                    Some(&HeapObj::BoundMethod(recv, id)) if recv.0 == o.0 => self.learn_builtin(o, id),
                    // A property's getter or the `__getattr__` the name fell back to is learned as a dunder so the load lowered again calls its section, once a load ran one.
                    _ => match core::mem::take(&mut self.ran) {
                        1 | 2 if let held @ Site::Dunder { class, epoch, .. } = cache.site(rip) && epoch == self.class_epoch
                            && matches!(self.heap.try_get(o), Some(&HeapObj::Instance(c, _)) if c.0 == class.0) => Some(held),
                        1 => self.learn_property(o, name, false),
                        2 => self.learn_getattr(o, name),
                        _ => None,
                    }.unwrap_or_else(|| self.learn_get(o, name)),
                };
                cache.set_site(rip, site);
                if let Site::Dunder { func, .. } = site { note_dunder(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, rip, func); }
            }
            OpCode::SetAttrR => {
                let (o, v) = (self.reg(chunk, ins.a)?, self.reg(chunk, ins.b)?);
                if code.block.is_some() && code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.attr_grows(code, chunk, rip, ins, o) { *ip = rip; return Ok(None); }
                self.ran = 0;
                self.store_attr_at(o, ins.c, v, rip, cache, chunk)?;
                // So is a property's setter.
                if core::mem::take(&mut self.ran) == 1 && let Some(name) = chunk.names.get(ins.c as usize) && let Some(site) = self.learn_property(o, name, true) {
                    cache.set_site(rip, site);
                    if let Site::Dunder { func, .. } = site { note_dunder(&self.heap, &mut self.learned, &self.body_to_fi, &self.functions, &mut self.preempt_left, code, chunk, rip, func); }
                }
            }
            OpCode::LoadGlobalR => self.regs[self.base + ins.a as usize] = self.global_at(code.module, ins.b as u32)?,
            OpCode::LoadGlobalAtR => self.regs[self.base + ins.a as usize] = self.global_at(ins.c as usize, ins.b as u32)?,
            OpCode::UnbindR => self.regs[self.base + ins.a as usize] = Val::undef(),
            OpCode::SuperR => { let v = self.super_of_method(self.regs[self.base + ins.b as usize], self.regs[self.base + ins.c as usize])?; self.regs[self.base + ins.a as usize] = v; }
            OpCode::WrapCellR => { let v = self.heap.alloc(HeapObj::Cell(self.regs[self.base + ins.a as usize]))?; self.regs[self.base + ins.a as usize] = v; }
            OpCode::ExcArgsR => {
                let at = self.base + ins.b as usize;
                let args = self.regs[at..at + ins.c as usize].to_vec();
                self.set_exc_args(self.regs[self.base + ins.a as usize], args)?;
            }
            OpCode::StoreGlobalAtR => {
                let (v, m) = (self.reg(chunk, ins.b)?, ins.c as usize);
                self.scopes[m].set_at(ins.a as u32, v);
                if ins.x & STORE_NOTES_BUILTIN != 0 && let Some(id) = self.scopes[m].names_builtin(ins.a as u32) { self.rebind_builtin(id); }
            }
            OpCode::StoreTopR => {
                let v = self.pop()?;
                if ins.x == 0 { self.regs[self.base + ins.a as usize] = v; } else { self.scopes[code.module].set_at(ins.a as u32, v); }
            }
            OpCode::StoreGlobalR => {
                let v = self.reg(chunk, ins.b)?;
                self.scopes[code.module].set_at(ins.a as u32, v);
                if ins.x & STORE_NOTES_BUILTIN != 0 && let Some(id) = self.scopes[code.module].names_builtin(ins.a as u32) { self.rebind_builtin(id); }
            }
            OpCode::LoadCellR => {
                let v = self.deref(self.regs[self.base + ins.b as usize]);
                if v.is_undef() { return Err(self.unbound_err(chunk, ins.b as usize)); }
                self.regs[self.base + ins.a as usize] = v;
            }
            OpCode::StoreCellR => {
                let v = self.reg(chunk, ins.b)?;
                if !self.set_cell(self.regs[self.base + ins.a as usize], v) { self.regs[self.base + ins.a as usize] = v; }
            }
            // A generator the register loop left this `for` to, coverage's unlowered code among them, opens as a frame of this loop.
            OpCode::ForIterR if let Some(&IterFrame::Coroutine(co)) = self.iter_stack.last() && let Some((_, _, at)) = self.enter_gen(co, rip) => (self.gen_entry, self.entered, self.yielded) = (at, true, true),
            OpCode::ForIterR if self.for_job(rip, None).is_some() => (self.entered, self.yielded) = (true, true),
            // A builtin iterator over a generator runs the generator to its item as a frame of this loop first.
            OpCode::ForIterR if self.lazy_over() && self.lazy_prefetch(rip, code)? => {}
            OpCode::ForIterR if let Some(&IterFrame::UserDefined(it)) = self.iter_stack.last() => {
                if code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.op_dunder_grows(code, chunk, rip, ins, it) { *ip = rip; return Ok(None); }
                // A user iterator's `__next__` is learned, so lowered again the step calls its section.
                self.learn_dunder(rip, cache, code, chunk, it, "__next__", 1);
                // An instance's runs as a frame of the loop, its item going straight to the loop's binding.
                let step = if self.heap.user(it) { self.user_step(ins, rip, code, chunk)? } else { self.for_step(chunk)? };
                match step {
                    Some(item) if ins.x == 0 => self.regs[self.base + ins.b as usize] = item,
                    Some(item) => self.scopes[code.module].set_at(ins.b as u32, item),
                    None => {
                        if op as usize > n { return Err(cold_runtime("jump target out of bounds")); }
                        *ip = op as usize;
                    }
                }
            }
            OpCode::ForIterR => match self.for_step(chunk)? {
                Some(item) if item.is_undef() => *ip = rip,
                Some(item) if ins.x == 0 => self.regs[self.base + ins.b as usize] = item,
                Some(item) => self.scopes[code.module].set_at(ins.b as u32, item),
                None => {
                    if op as usize > n { return Err(cold_runtime("jump target out of bounds")); }
                    *ip = op as usize;
                }
            },
            OpCode::ReturnR => {
                let result = self.reg(chunk, ins.b)?;
                if self.exception_stack.len() > exc_base
                    && let Some(h) = self.next_cleanup_handler(exc_base)
                {
                    self.unwind_stack.push(Unwind::Return(result));
                    *ip = handler_at(code, h);
                    return Ok(None);
                }
                return Ok(Some(result));
            }
            _ => return Err(cold_runtime("not a register form")),
        }
        Ok(None)
    }

    /* `obj[k] = v` from registers as its stack op, a user object's dunder handed to the loop. */
    #[inline(never)]
    fn store_item_r(&mut self, o: Val, k: Val, v: Val, rip: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        self.redone_if(self.heap.user(o) || self.heap.user(k), rip, 0, |vm| { vm.stack.extend_from_slice(&[o, k, v]); vm.store_item(chunk) })
    }

    /* A register `for`'s step of a user instance, its `__next__` a frame of the loop whose item goes to the loop's binding. */
    #[inline(never)]
    fn user_step(&mut self, ins: Ins, rip: usize, code: &Code, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        let then = if ins.x == 0 { super::redo::Then::Reg(ins.b) } else { u16::try_from(code.module).map_or(super::redo::Then::Again, |m| super::redo::Then::Global(m, ins.b as u32)) };
        self.redone_to(rip, 0, then, |vm| vm.for_step(chunk))
    }

    /* Stack opcodes, apart from the loop so a frame's entry stays cheap. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn dispatch(&mut self, ins: Ins, rip: usize, chunk: &SSAChunk, cache: &OpcodeCache, code: &Code, pool: usize, ip: &mut usize, exc_base: usize) -> Result<Option<Val>, VmErr> {
        // A function made inside a section takes its captures from where the section lays their slots.
        if matches!(ins.op, OpCode::MakeFunction | OpCode::MakeCoroutine) && code.block.is_some() { self.cell_map = section_at(code, rip).map(|s| s.slots.clone()).filter(|m| !m.is_empty()); }
        let n = code.ins.len();
        let op = ins.a;
        match ins.op {
            // A `try` laid inside a section names its handler's own index.
            OpCode::SetupExcept | OpCode::SetupFinally if ins.x == 1 => self.setup_raw(ins),
            // A generator's section marks the `a` iterators of its own it holds as its loop's body runs, the one its loop drops.
            OpCode::PopIter if ins.x != 0 => self.pop_iter(ins),
            // A `yield` of a register, its result unused, hands the register's value.
            OpCode::Yield if ins.x == 1 => {
                let v = self.reg(chunk, ins.a)?;
                self.push(v);
                self.yielded = true;
            }
            // A `with` whose dunder the call after reads from registers.
            OpCode::WithEnter | OpCode::WithExit if ins.x == 1 || ins.x == 2 && ins.op == OpCode::WithEnter => self.with_stage(ins, rip, cache, code, chunk, ip)?,
            // A fall-through out of a `with`, the WithExit after staging its `__exit__`.
            OpCode::WithExit if ins.x == 2 => self.with_leave(),
            OpCode::WithJudge if ins.x == 1 => self.with_judge(self.regs[self.base + op as usize]),
            // A `finally` of a section ends its return in the section's return.
            OpCode::EndFinally if matches!(self.unwind_stack.last(), Some(Unwind::Return(_))) && section_at(code, rip).is_some() => self.end_section_finally(code, rip, ip),
            // Short-circuit jumps, instance `__bool__` / `__len__` may run via `truthy_op`.
            OpCode::JumpIfFalseOrPop => {
                let v = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                if !self.truth_at(v, rip, 0, chunk)? { *ip = op as usize; }
                else { self.pop()?; }
            }
            OpCode::JumpIfTrueOrPop => {
                let v = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                if self.truth_at(v, rip, 0, chunk)? { *ip = op as usize; }
                else { self.pop()?; }
            }

            // Hot opcodes.
            OpCode::LoadName => {
                let v = match code.kinds.get(op as usize) {
                    Some(&Kind::Global(g)) => self.global_at(code.module, g)?,
                    Some(Kind::Cell) => {
                        let v = self.deref(self.regs.get(self.base + op as usize).copied().unwrap_or(Val::undef()));
                        if v.is_undef() { return Err(self.unbound_err(chunk, op as usize)); }
                        v
                    }
                    // Malformed bytecode can carry an out-of-range slot, treat it as unbound.
                    _ => match self.regs.get(self.base + op as usize).copied().filter(|v| !v.is_undef()) {
                        Some(v) => v,
                        None => self.unbound(chunk, op)?,
                    },
                };
                self.push(v);
            }
            OpCode::StoreName => match code.kinds.get(op as usize) {
                Some(&Kind::Global(g)) => {
                    let v = self.pop()?;
                    self.scopes[code.module].set_at(g, v);
                    if let Some(id) = self.scopes[code.module].names_builtin(g) { self.rebind_builtin(id); }
                }
                Some(Kind::Cell) => {
                    let v = self.pop()?;
                    if !self.set_cell(self.regs[self.base + op as usize], v) { self.regs[self.base + op as usize] = v; }
                }
                _ => self.handle_store(op)?,
            },
            OpCode::LoadGlobal => {
                let v = match code.kinds.get(op as usize) {
                    Some(&Kind::Global(g)) => self.global_at(code.module, g)?,
                    _ => {
                        let name = chunk.names.get(op as usize).ok_or_else(|| cold_runtime("LoadGlobal: name index out of bounds"))?;
                        self.scopes[code.module].get(name).or_else(|| self.global(name)).ok_or_else(|| VmErr::Name(name.clone()))?
                    }
                };
                self.push(v);
            }
            OpCode::StoreGlobal => {
                let v = self.pop()?;
                let name = chunk.names.get(op as usize).ok_or_else(|| cold_runtime("StoreGlobal: name index out of bounds"))?;
                self.note_builtin_binding(name);
                self.scopes[code.module].set(name, v);
            }
            // A section's delete of binding `b` of module `c`, its name `a` in the chunk of the function it was laid from.
            OpCode::Del if ins.x == 1 => {
                let (g, m) = (ins.b as u32, ins.c as usize);
                if self.scopes[m].at(g).is_undef() { return Err(VmErr::Name(ssa_strip(&chunk_in(&self.functions, code, chunk, rip).names[op as usize]).into())); }
                self.scopes[m].set_at(g, Val::undef());
                let name = ssa_strip(&chunk_in(&self.functions, code, chunk, rip).names[op as usize]).to_string();
                self.note_builtin_binding(&name);
            }
            OpCode::Del => match code.kinds.get(op as usize) {
                Some(&Kind::Global(g)) => {
                    let name = ssa_strip(&chunk.names[op as usize]);
                    if self.scopes[code.module].at(g).is_undef() { return Err(VmErr::Name(name.into())); }
                    self.scopes[code.module].set_at(g, Val::undef());
                    // A builtin name falls back.
                    self.note_builtin_binding(name);
                }
                Some(Kind::Cell) => {
                    let cell = self.regs[self.base + op as usize];
                    if self.deref(cell).is_undef() { return Err(self.unbound_err(chunk, op as usize)); }
                    self.set_cell(cell, Val::undef());
                }
                _ => self.handle_side(OpCode::Del, op, chunk)?,
            },
            OpCode::LoadConst => {
                // Constants are pre-materialised at exec entry, so this is a single bounds-checked index instead of a Value->Val conversion.
                let v = *self.pools[pool].consts.as_deref().and_then(|c| c.get(op as usize))
                    .ok_or_else(|| cold_runtime("constant index out of bounds"))?;
                self.push(v);
            }

            // Extracted to exec_arith_or_compare so VM::dispatch doesn't fuse the IC/deopt cycle into its own symbol.
            OpCode::Add | OpCode::Sub | OpCode::Mul
            | OpCode::Mod | OpCode::FloorDiv
            | OpCode::Eq | OpCode::Lt | OpCode::NotEq
            | OpCode::Gt | OpCode::LtEq | OpCode::GtEq
            | OpCode::Div | OpCode::Pow | OpCode::Minus | OpCode::Pos | OpCode::InPlaceAdd | OpCode::InPlaceSub => {
                self.exec_arith_or_compare(ins.op, op, rip, cache, chunk, true)?;
            }

            OpCode::Jump => {
                let target = self.checked_jump(op as usize, n)?;
                // Backward jumps are loop back-edges, charge them so `while` is bounded like `for`.
                if target <= rip && ins.x == 0 {
                    self.charge_step()?;
                    if self.heap.needs_gc() { self.collect_point()?; }
                    // A count left by `note_alt` only sends the back-edge here.
                    if self.preempt_left == RELAY { self.preempt_left = 0; }
                    // Back-edges are the only preempt sampling point.
                    if self.preempt_left != 0 {
                        self.preempt_left -= 1;
                        if self.preempt_left == 0 {
                            if self.frame_safe && self.gen_frames == 0 && self.jobs.is_empty() && self.frames_park() {
                                self.preempt_left = self.preempt_every;
                                self.pending.preempt_request = true;
                                self.yielded = true;
                            } else {
                                // Unpreemptible here, retry next back-edge.
                                self.preempt_left = 1;
                            }
                        }
                    }
                    // Code whose calls met other functions often enough is laid again with them, unless a loop over or inside a section holds its state in the section's registers.
                    let inside = code.block.as_deref().and_then(|b| b.sections.first()).is_some_and(|s| target >= s.start as usize);
                    if code.misses.get() == super::lower::MISSES && !self.yielded && !inside && !code.ins.get(target).is_some_and(|i| i.op == OpCode::ReturnSectionR) {
                        code.misses.set(super::lower::MISSES + 1);
                        let p = &mut self.pools[pool];
                        // Code lowered without sections grows a block, each call that met a function moved to a head past its end and each op left to grow its own, nothing moved.
                        if code.block.is_none() && p.code.as_ref().is_some_and(|c| core::ptr::eq(Rc::as_ptr(c), code)) {
                            (self.growing, self.yielded) = (Some(alloc::boxed::Box::new((code as *const Code, 1, super::lower::Grow::All, Val::undef()))), true);
                        // Laid again only while that lays more than the time before, which is all a laying meets laid.
                        } else if p.code.as_ref().is_some_and(|c| core::ptr::eq(Rc::as_ptr(c), code)) && p.relaid != code.ins.len() as u32 {
                            (p.laid, p.relaid) = (false, code.ins.len() as u32);
                            (self.tier_up, self.yielded, self.relaying) = (true, true, true);
                        }
                    }
                    #[cfg(not(feature = "coverage"))]
                    if code.unchanged() && !self.yielded {
                        code.hot.set(code.hot.get() + 1);
                        if code.hot.get() == super::lower::HOT_LOOP { self.tier_up = true; self.yielded = true; }
                    }
                }
                *ip = target;
            }
            OpCode::JumpIfFalse => {
                let v = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                let t = self.truth_at(v, rip, 1, chunk)?;
                self.stack.pop();
                if !t { *ip = self.checked_jump(op as usize, n)?; }
                self.learn_truth(rip, cache, code, chunk, v);
            }
            // A generator a `yield from` or an unlowered `for` steps opens as a frame of this loop too.
            OpCode::ForIter => {
                let co = match self.iter_stack.last() { Some(&IterFrame::Coroutine(co)) => Some(co), _ => None };
                match co.and_then(|co| self.enter_gen(co, rip)) {
                    Some((_, _, at)) => (self.gen_entry, self.entered, self.yielded) = (at, true, true),
                    // A `map` or `filter` calls its function as a frame of this loop too.
                    None if self.for_job(rip, None).is_some() => (self.entered, self.yielded) = (true, true),
                    // A builtin iterator over a generator runs the generator to its item as a frame of this loop first.
                    None if self.lazy_over() && self.lazy_prefetch(rip, code)? => {}
                    None => {
                        if let Some(&IterFrame::UserDefined(it)) = self.iter_stack.last() {
                            self.learn_dunder(rip, cache, code, chunk, it, "__next__", 1);
                            if self.heap.user(it) { self.redone_to(rip, 0, super::redo::Then::Push, |vm| vm.exec_for_iter(op, ip, n, chunk))? } else { self.exec_for_iter(op, ip, n, chunk)? }
                        } else { self.exec_for_iter(op, ip, n, chunk)? }
                    }
                }
            }
            OpCode::PopTop => { self.pop()?; }
            OpCode::ReturnValue => {
                let result = if self.stack.is_empty() { Val::none() } else { self.pop()? };
                // Run any enclosing finally/with cleanup before the value leaves the frame.
                if self.exception_stack.len() > exc_base
                    && let Some(h) = self.next_cleanup_handler(exc_base)
                {
                    self.unwind_stack.push(Unwind::Return(result));
                    *ip = handler_at(code, h);
                    return Ok(None);
                }
                return Ok(Some(result));
            }

            // Warm opcodes.
            OpCode::GetItem => self.get_item_op(rip, chunk, cache, true)?,

            OpCode::CallSpread | OpCode::CallPrint | OpCode::CallLen | OpCode::CallAbs
            | OpCode::CallStr | OpCode::CallInt | OpCode::CallFloat | OpCode::CallBool
            | OpCode::CallType | OpCode::CallChr | OpCode::CallOrd
            | OpCode::CallList | OpCode::CallTuple | OpCode::CallEnumerate | OpCode::CallIsInstance
            | OpCode::CallRange | OpCode::CallRound | OpCode::CallMin | OpCode::CallMax
            | OpCode::CallSum | OpCode::CallZip | OpCode::CallDict | OpCode::CallSet
            | OpCode::CallInput | OpCode::MakeFunction | OpCode::MakeCoroutine
            | OpCode::CallAll | OpCode::CallAny | OpCode::CallBin | OpCode::CallOct
            | OpCode::CallHex | OpCode::CallDivmod | OpCode::CallPow | OpCode::CallRepr
            | OpCode::CallReversed | OpCode::CallCallable
            | OpCode::CallExtern => {
                // Snapshot call-site byte_pos for the new CallFrame, falls back to enclosing stmt.
                if matches!(ins.op, OpCode::CallStr | OpCode::CallRepr | OpCode::CallBool) && op == 1 && let Some(&o) = self.stack.last()
                    && code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.op_dunder_grows(code, chunk, rip, ins, o) { *ip = rip; return Ok(None); }
                self.pending.call_ip = Some(code.orig(rip));
                // Only plain user calls stage frames.
                self.pending_exec_safe = self.frame_safe && matches!(ins.op, OpCode::Call | OpCode::CallSpread);
                // A `min` or `max` here may run its key as frames of this loop, and a `type` making a class its hooks.
                self.job_ok = matches!(ins.op, OpCode::CallMin | OpCode::CallMax | OpCode::CallList | OpCode::CallTuple | OpCode::CallSet | OpCode::CallDict | OpCode::CallSum | OpCode::CallAny | OpCode::CallAll | OpCode::CallRepr | OpCode::CallStr | OpCode::CallPrint | OpCode::CallBool | OpCode::CallLen | OpCode::CallAbs | OpCode::CallInt | OpCode::CallFloat | OpCode::CallType | OpCode::CallExtern);
                // `str` or `repr` of an instance learns the dunder it runs, so lowered again the call lays its section.
                let recv = if matches!(ins.op, OpCode::CallStr | OpCode::CallRepr | OpCode::CallLen | OpCode::CallBool) && op == 1 { self.stack.last().copied() } else { None };
                // A builtin a module rebound, or one given a user object, hands the calls it makes to the loop.
                let dispatched = if self.heap.users && self.call_arms(ins.op, op) { self.call_redone(ins.op, op, rip, chunk) } else { self.handle_function(ins.op, op, chunk) };
                // Cleared so `true` cannot leak onward.
                (self.pending_exec_safe, self.job_ok) = (false, false);
                dispatched?;
                if let Some(o) = recv { self.learn_fused(ins.op, o, rip, cache, code, chunk); }
            }

            // A loop over a generator laid as a section takes the generator into it, or steps any other iterator as it would.
            OpCode::GetIter if ins.x == lower::ADOPT => {
                let obj = self.pop()?;
                match self.take_gen(code, obj, ins) {
                    Some(at) => *ip = at,
                    None if self.heap.users && self.instance_class(obj).is_some() => {
                        self.push(obj);
                        let frame = self.redone_if(true, rip, 1, |vm| vm.make_iter_frame(obj, chunk))?;
                        self.stack.pop();
                        self.iter_stack.push(frame);
                    }
                    None => { let frame = self.make_iter_frame(obj, chunk)?; self.iter_stack.push(frame); }
                }
            }
            OpCode::GetIter if op == crate::parser::ITER_VALUE => {
                let o = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                // A list reads live and the others cannot change, so each iterates the same handed over as is.
                if !matches!(self.heap.plain(o), Some(HeapObj::List(_) | HeapObj::Range(..) | HeapObj::Tuple(_) | HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::FrozenSet(_))) {
                    // An instance's `__iter__` runs as a frame of the loop, the op run again after it.
                    let it = if self.heap.users { self.redone_if(self.instance_class(o).is_some(), rip, 1, |vm| vm.iter_of(o, chunk))? } else { self.iter_of(o, chunk)? };
                    if let Some(top) = self.stack.last_mut() { *top = it; }
                }
            }
            OpCode::GetIter => {
                let obj = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                // A loop over an instance outside the sections asks for a head of its own for the `__iter__` it met.
                if op == 0 && code.block.as_deref().and_then(|b| b.grown.get()).is_some_and(|g| g.meets(rip as u32)) && self.op_dunder_grows(code, chunk, rip, ins, obj) { *ip = rip; return Ok(None); }
                let user = op == crate::parser::ITER_AS_IS && matches!(self.heap.try_get(obj), Some(HeapObj::Instance(..)));
                // `async for` steps what `__aiter__` gives through its `__anext__` awaitables, an instance's `__aiter__` a frame of the loop with the op run again after.
                let frame = if op == crate::parser::ITER_ASYNC { IterFrame::AsyncFor { it: self.redone_if(self.instance_class(obj).is_some(), rip, 1, |vm| vm.aiter_of(obj, true, chunk))?, aw: Val::undef(), out: 0 } } else if user { self.lazy_seen = obj; IterFrame::UserDefined(obj) } else if self.heap.users && self.instance_class(obj).is_some() { self.redone_if(true, rip, 1, |vm| vm.make_iter_frame(obj, chunk))? } else { self.make_iter_frame(obj, chunk)? };
                self.stack.pop();
                self.iter_stack.push(frame);
                // An instance's `__iter__` is learned, so lowered again the loop lays its section.
                if op == 0 { self.learn_dunder(rip, cache, code, chunk, obj, "__iter__", 1); }
            }
            OpCode::LoadTrue => self.push(Val::bool(true)),
            OpCode::LoadFalse => self.push(Val::bool(false)),
            OpCode::LoadNone => self.push(Val::none()),
            OpCode::Not => self.redone(rip, 1, |vm| vm.handle_logic(OpCode::Not, chunk))?,

            OpCode::Phi => {
                // The phi tables index the compiler instructions, not the lowered ones.
                self.exec_phi(op, code.orig(rip) as usize, &chunk.phi_map, &chunk.phi_sources);
            }

            OpCode::LoadAttr => {
                let obj = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                match self.site_get(cache.site(rip), obj) {
                    Some(v) => { self.pop()?; self.push(v); }
                    None => {
                        self.prop_at = Some((rip, super::act::PUSH));
                        self.redone(rip, 1, |vm| vm.handle_load_attr(op, chunk))?;
                        if let Some(name) = chunk.names.get(op as usize) { cache.set_site(rip, self.learn_get(obj, name)); }
                    }
                }
            }

            // Fused method call, the call's counts in the second operand.
            OpCode::CallMethod => {
                if ins.x == 1 { *ip += 1; }
                self.exec_call_method(op, ins.b, rip, code, chunk, super::redo::Rerun::Stack)?
            }
            OpCode::CallMethodArgs => {
                // Always folded into CallMethod, reaching here is a bytecode bug.
                return Err(cold_runtime("CallMethodArgs reached dispatch unpaired"));
            }

            // Cold opcodes.
            OpCode::And | OpCode::Or => {
                // Parser should short-circuit these via JumpIf*OrPop, reaching here is a codegen bug.
                return Err(cold_runtime("And/Or reached VM dispatch (should be short-circuited)"));
            }

            OpCode::MakeClass => self.exec_make_class(op, code.orig(rip) as usize + 1, chunk)?,
            OpCode::StoreAttr => {
                let value = self.pop()?;
                let obj = self.pop()?;
                self.store_attr_at(obj, op, value, rip, cache, chunk)?;
            }

            OpCode::LoadModule => {
                let entry = chunk.imports.get(op as usize).ok_or_else(|| cold_runtime("LoadModule: import index out of range"))?;
                let v = *self.module_table.get(&entry.spec).ok_or_else(|| cold_runtime("LoadModule: module not initialised"))?;
                self.push(v);
            }

            // A store or a test on an instance learns its dunder, so the code lowered from this lays it.
            OpCode::StoreItem | OpCode::In | OpCode::NotIn if self.stack.len() >= 2 => {
                let recv = self.stack[self.stack.len() - if ins.op == OpCode::StoreItem { 3.min(self.stack.len()) } else { 1 }];
                // A store calls Python through its container or key, never the value it stores.
                let go = self.heap.users && if ins.op == OpCode::StoreItem { let l = self.stack.len(); l >= 3 && (self.heap.user(self.stack[l - 3]) || self.heap.user(self.stack[l - 2])) } else { self.redo_top(2) };
                // A test over containers arms only once an item's dunder runs.
                let d = if !go && self.heap.users && ins.op != OpCode::StoreItem { self.contains_defer(rip) } else { super::redo::Defer::No };
                let r = self.redone_if(go, rip, if ins.op == OpCode::StoreItem { 3 } else { 2 }, |vm| vm.dispatch_generic(ins.op, op, chunk, code, ip, exc_base));
                self.redo_undefer(d);
                if r.is_ok() { self.learn_dunder(rip, cache, code, chunk, recv, if ins.op == OpCode::StoreItem { "__setitem__" } else { "__contains__" }, 2 + (ins.op == OpCode::StoreItem) as u8); }
                return r;
            }
            other => {
                let armed = self.heap.users && self.redo_enter_op(other, op, rip);
                let r = self.dispatch_generic(other, op, chunk, code, ip, exc_base);
                self.redo_done(armed);
                return r;
            }
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch_generic(&mut self, opcode: OpCode, operand: u16, chunk: &SSAChunk, code: &Code, ip: &mut usize, exc_base: usize) -> Result<Option<Val>, VmErr> {
        match opcode {
            OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor
            | OpCode::BitNot | OpCode::Shl | OpCode::Shr
            | OpCode::InPlaceBitOr | OpCode::InPlaceBitAnd | OpCode::InPlaceBitXor => self.handle_bitwise(opcode, operand, chunk)?,
            OpCode::MatMul => self.handle_matmul(operand, chunk)?,
            OpCode::MakeTypeAlias => {
                let value = self.pop()?;
                let name = chunk.names.get(operand as usize).ok_or_else(|| cold_runtime("MakeTypeAlias: bad name index"))?.clone();
                let alias = self.heap.alloc(HeapObj::TypeAlias(name, value))?;
                self.push(alias);
            }
            OpCode::MakeTypeVar => {
                let name = chunk.names.get(operand as usize).ok_or_else(|| cold_runtime("MakeTypeVar: bad name index"))?;
                let v = self.heap.alloc(HeapObj::TypeVar(name.trim_start_matches('#').into()))?;
                self.push(v);
            }
            OpCode::In | OpCode::NotIn | OpCode::Is | OpCode::IsNot => self.handle_identity(opcode, chunk)?,

            OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildDict
            | OpCode::BuildString | OpCode::BuildSet | OpCode::BuildSlice => self.handle_build(opcode, operand)?,

            OpCode::StoreItem if self.slice_gen() && self.retry_gen(chunk)? => {}
            OpCode::StoreItem => { self.store_item(chunk)?; }
            OpCode::DelItem => { self.del_item(chunk)?; }
            OpCode::DelAttr => self.del_attr_at(operand, *ip - 1, chunk)?,
            // The op's own index, which a job of the loop the op starts runs it again at.
            OpCode::UnpackSequence | OpCode::UnpackEx | OpCode::FormatValue => self.handle_container(opcode, operand, chunk, *ip - 1)?,

            OpCode::ListAppend | OpCode::SetAdd | OpCode::MapAdd => self.handle_comprehension(opcode)?,
            // A generator spread or unpacked runs as a frame of the loop, the instruction run again on its items.
            OpCode::SetUpdate | OpCode::ListExtend if self.top_may_gen() && self.retry_gen(chunk)? => {}
            OpCode::DictUpdate | OpCode::SetUpdate | OpCode::ListExtend => self.handle_spread_merge(opcode, chunk, *ip - 1)?,

            // The yielded value stays on the stack for the resumer.
            OpCode::Yield => self.yielded = true,
            OpCode::LoadYieldFrom => self.push(self.yield_from_value),
            OpCode::LoadEllipsis => {
                let v = self.heap.alloc(HeapObj::Ellipsis)?;
                self.push(v);
            }
            OpCode::Dup => {
                let v = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                self.push(v);
            }
            OpCode::MatchSeq => {
                // Sequence patterns match only list/tuple, not str/bytes.
                let v = self.pop()?;
                let is_seq = v.is_heap() && matches!(self.heap.get(v), HeapObj::List(_) | HeapObj::Tuple(_));
                self.push(Val::bool(is_seq));
            }
            OpCode::MatchClass => self.match_class(operand as usize, chunk)?,
            OpCode::MatchMap => {
                let v = self.pop()?;
                let is_map = self.heap.dict_cell(v).is_some();
                self.push(Val::bool(is_map));
            }
            OpCode::Dup2 => {
                let b = self.pop()?; let a = self.pop()?;
                self.push(a); self.push(b); self.push(a); self.push(b);
            }
            OpCode::Swap => {
                let b = self.pop()?; let a = self.pop()?;
                self.push(b); self.push(a);
            }
            OpCode::Rot3 => {
                let c = self.pop()?; let b = self.pop()?; let a = self.pop()?;
                self.push(b); self.push(c); self.push(a);
            }
            OpCode::Assert | OpCode::Del | OpCode::Global | OpCode::Nonlocal
            | OpCode::Raise | OpCode::RaiseFrom => {
                self.handle_side(opcode, operand, chunk)?;
            }
            OpCode::GetAwaitable => {
                let x = *self.stack.last().ok_or_else(|| cold_runtime("stack underflow"))?;
                // An instance's `__await__` runs as a frame of the loop, the op run again after it.
                let it = self.redone_if(self.instance_class(x).is_some(), *ip - 1, 1, |vm| vm.awaitable_of(x, chunk))?;
                if let Some(top) = self.stack.last_mut() { *top = it; }
                self.push(Val::none());
            }
            // Sends the value on top into the awaitable under it, a yield passing up through this frame, which runs the step again on resume.
            OpCode::Await => {
                let sent = self.pop()?;
                let at = self.stack.len().checked_sub(1).ok_or_else(|| cold_runtime("stack underflow"))?;
                match self.await_on(at, sent, chunk)? {
                    Some(super::globals::async_ops::Step::Yield(v)) => {
                        self.push(v);
                        self.yielded = true;
                        *ip -= 1;
                    }
                    Some(super::globals::async_ops::Step::Return(v)) => self.stack[at] = v,
                    // The frame it opened comes back to this op.
                    None => *ip -= 1,
                }
            }
            // A finally frame runs on every exit path, its handler the finally body or WithExit.
            OpCode::SetupExcept | OpCode::SetupFinally => {
                self.exception_stack.push(ExceptionFrame {
                    kind: if opcode == OpCode::SetupExcept { BlockKind::Except } else { BlockKind::Finally },
                    handler_ip: operand as usize,
                    stack_depth: self.stack.len(),
                    iter_depth: self.iter_stack.len(),
                    with_depth: self.with_stack.len(),
                    unwind_depth: self.unwind_stack.len(),
                    handling_depth: self.handling.len(),
                    turned: self.turned,
                });
            }
            OpCode::WithEnter => {
                let cm = self.pop()?;
                let (enter_fn, class) = self.with_push(cm, operand == 1)?;
                if self.names_super(enter_fn) { self.with_noted(code, chunk, *ip, enter_fn, cm); }
                self.pending.method_binding = Some((class, cm));
                self.push(enter_fn);
                self.push(cm);
            }
            // Marks a normal fall-through into a finally body so EndFinally balances its pop.
            OpCode::BeginFinally => self.unwind_stack.push(Unwind::Normal),
            // Stages `__exit__` for the plain Call behind it, parking-safe.
            OpCode::WithExit => {
                let (cm, exit_fn, class) = self.with_pop()?;
                // Reraise selects `__exit__(type, exc, None)`, other exits pass three Nones.
                let (exc_type, exc) = if matches!(self.unwind_stack.last(), Some(Unwind::Reraise(..))) {
                    let exc = self.pending.exc_val.unwrap_or(Val::none());
                    let exc_name = self.exc_type_name(exc);
                    (self.heap.alloc(HeapObj::Type(exc_name))?, exc)
                } else {
                    (Val::none(), Val::none())
                };
                if !class.is_none() { self.with_noted(code, chunk, *ip, exit_fn, cm); }
                self.pending.method_binding = (!class.is_none()).then_some((class, cm));
                self.push(exit_fn);
                self.push(cm);
                self.push(exc_type);
                self.push(exc);
                self.push(Val::none());
            }
            OpCode::WithJudge => { let r = self.pop()?; self.with_judge(r); }
            // End of a finally body / WithExit, pop its reason and resume the exit it carried.
            OpCode::EndFinally => {
                match self.unwind_stack.pop() {
                    None | Some(Unwind::Normal) => {}
                    Some(Unwind::Return(v)) => {
                        if let Some(h) = self.next_cleanup_handler(exc_base) {
                            self.unwind_stack.push(Unwind::Return(v));
                            *ip = handler_at(code, h);
                        } else {
                            return Ok(Some(v));
                        }
                    }
                    Some(Unwind::Goto { target, remaining }) => {
                        if remaining > 0 && let Some(h) = self.next_cleanup_handler(exc_base) {
                            self.unwind_stack.push(Unwind::Goto { target, remaining: remaining - 1 });
                            *ip = handler_at(code, h);
                        } else {
                            *ip = code.lowered(target);
                        }
                    }
                    Some(Unwind::Reraise(e, at)) => {
                        self.error_byte_pos = at;
                        return Err(e);
                    }
                }
            }
            // break/continue across N finally/with blocks, the following Jump completes the transfer.
            OpCode::UnwindFinally => {
                let target = code.resume(*ip);
                if operand > 0 && let Some(h) = self.next_cleanup_handler(exc_base) {
                    self.unwind_stack.push(Unwind::Goto { target, remaining: operand - 1 });
                    *ip = handler_at(code, h);
                }
            }
            OpCode::BeginArgs => {
                self.pending.delta_save.push((self.pending.pos_delta, self.pending.kw_delta));
                self.pending.pos_delta = 0;
                self.pending.kw_delta = 0;
            }
            OpCode::UnpackArgs if operand & 0x3 == 1 && self.top_may_gen() && self.retry_gen(chunk)? => {}
            OpCode::UnpackArgs => self.unpack_args(operand, chunk, Some(*ip - 1))?,
            OpCode::PopExcept => if let Some(f) = self.exception_stack.pop() { self.handling.truncate(f.handling_depth); },
            // Emitted by `break` to drop the abandoned for-loop iterator.
            OpCode::PopIter => if let Some(IterFrame::Under(n) | IterFrame::Held(n, _)) = self.iter_stack.pop() { let n = self.iter_stack.len().saturating_sub(n as usize); self.iter_stack.truncate(n); },
            _ => return Err(cold_runtime("unexpected opcode in generic dispatch")),
        }
        Ok(None)
    }

    /* Enters the section for the call at `rip` that any function of its body runs, budget paid and parameters set, the index it carries on at, None for other callees. */
    fn enter_section_of_body(&mut self, code: &Code, rip: usize) -> Option<usize> {
        let (ins, store, x) = (code.ins[rip], code.ins[rip + 1], code.ins[rip + 2]);
        let sections = &code.block.as_deref()?.sections;
        if x.x & 32 == 0 || self.budget == 0 { return None; }
        // The section itself, or for a closure's the twin laid after it.
        let own = sections.get(x.c as usize)?;
        let s = if own.by_body.is_some() { own } else { sections.get(x.c as usize + 1).filter(|t| t.by_body.is_some() && t.fi == own.fi)? };
        let base = self.base;
        let (f, b, c, n) = (self.regs[base + ins.a as usize], self.regs[base + ins.b as usize], self.regs[base + ins.c as usize], ins.x as usize);
        if !body_enter(&self.heap, &mut self.regs[base..], f, s, n) { return None; }
        self.budget -= 1;
        // A function with no defaults or cells, which the section's registers hold none of, enters it again as the head checks, the same each call.
        if core::ptr::eq(s, own) && matches!(self.heap.try_get(f), Some(HeapObj::Func(_, d, c, _)) if d.is_empty() && c.is_empty()) { own.value.set(f); }
        let (at, from) = (base + s.link as usize + 1, base + own.link as usize + 1);
        if n > 0 { self.regs[at] = b; }
        if n > 1 { self.regs[at + 1] = c; }
        // Past the first two the arguments were moved into the section's own parameters, which the twin takes.
        if !core::ptr::eq(s, own) { for p in 2..n { self.regs[at + p] = self.regs[from + p]; } }
        self.regs[at - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
        Some(s.start as usize)
    }

    /* Pushes the handler a `try` laid inside a section names by its own index. */
    #[inline(never)]
    fn setup_raw(&mut self, ins: Ins) {
        self.exception_stack.push(ExceptionFrame {
            kind: if ins.op == OpCode::SetupExcept { BlockKind::Except } else { BlockKind::Finally },
            handler_ip: ins.a as usize | RAW_HANDLER,
            stack_depth: self.stack.len(),
            iter_depth: self.iter_stack.len(),
            with_depth: self.with_stack.len(),
            unwind_depth: self.unwind_stack.len(),
            handling_depth: self.handling.len(),
            turned: self.turned,
        });
    }

    /* Ends a `finally` of the section at `rip` that a return runs, the next one or the section's return. */
    #[inline(never)]
    fn end_section_finally(&mut self, code: &Code, rip: usize, ip: &mut usize) {
        let (Some(s), Some(Unwind::Return(v))) = (section_at(code, rip), self.unwind_stack.pop()) else { return };
        match self.section_cleanup(s) {
            Some(h) => { self.unwind_stack.push(Unwind::Return(v)); *ip = h & !RAW_HANDLER; }
            None => {
                let link = self.regs[self.base + s.link as usize].as_int() as usize;
                // An op's dunder that declined past its `finally` hands the op on as its return would.
                if self.heap.is_not_implemented(v) && let Some(to) = self.dunder_declined(code, rip, link >> 16) { *ip = to; return; }
                self.regs[self.base + (link & 0xFFFF)] = v;
                *ip = link >> 16;
            }
        }
    }

    /* The `super()` proxy for `method` called on `recv`, past the class in the instance's order that holds the method. */
    fn super_of_method(&mut self, recv: Val, method: Val) -> Result<Val, VmErr> {
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(recv) else { return Err(VmErr::Runtime("super() must be called inside a method")) };
        let order: alloc::vec::Vec<Val> = match self.mro_cache.get(&class.0) { Some(m) => m.to_vec(), None => alloc::vec![class] };
        let owner = order.iter().copied().find(|&c| matches!(self.heap.try_get(c), Some(HeapObj::Class(_, _, m)) if m.borrow().iter().any(|(_, v)| v.0 == method.0)))
            .ok_or(VmErr::Runtime("super() must be called inside a method"))?;
        self.heap.alloc(HeapObj::Super(owner, recv))
    }

    /* Enters the section of a spreading call at `rip` while its register holds the function laid, the items its parameters or a tuple for `*args` alone, None for other calls. */
    fn enter_spread(&mut self, code: &Code, rip: usize) -> Option<usize> {
        let (ins, store, x) = (code.ins[rip], code.ins[rip + 1], code.ins[rip + 2]);
        if x.x & 72 != 72 || self.budget == 0 { return None; }
        let sections = &code.block.as_deref()?.sections;
        let (mut s, base) = (sections.get(x.c as usize)?, self.base);
        let f = self.regs[base + ins.a as usize];
        if s.via_super.is_some() { return None; }
        // Another function its head met runs the section its table names for it.
        let found = if f.0 != s.value.get().0 && s.table.1 != u32::MAX { sections.get(alt_of(code.block.as_deref()?, s.table.0, s.table.1, f)) } else { None };
        if let Some(t) = found { s = t; }
        // Another closure of the body runs the twin laid after the section, its cells filled from it.
        let twin = f.0 != s.value.get().0;
        if twin {
            s = sections.get(x.c as usize + 1).filter(|t| x.x & 32 != 0 && t.by_body.is_some() && t.fi == s.fi)?;
            if !body_enter(&self.heap, &mut self.regs[base..], f, s, s.arity as usize) { return None; }
        }
        let seq = self.regs[base + ins.b as usize];
        // A mapping a function taking `**kwargs` alone takes is copied, its keys strings as keywords are.
        if s.packs == 2 {
            if ins.x != 2 { return None; }
            let Some(HeapObj::Dict(d)) = self.heap.try_get(seq) else { return None };
            let d = d.borrow().clone();
            if !d.iter().all(|(k, _)| matches!(self.heap.try_get(k), Some(HeapObj::Str(_)))) { return None; }
            let kw = self.heap.alloc(HeapObj::Dict(Rc::new(core::cell::RefCell::new(d)))).ok()?;
            self.budget -= 1;
            let at = base + s.link as usize + 1;
            self.regs[at] = kw;
            self.regs[at - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
            return Some(s.start as usize);
        }
        if ins.x == 2 { return None; }
        let items: alloc::vec::Vec<Val> = match self.heap.try_get(seq) {
            Some(HeapObj::Tuple(v)) => v.clone(),
            Some(HeapObj::List(v)) => v.borrow().clone(),
            _ => return None,
        };
        let args = if s.packs == 1 {
            let tuple = if matches!(self.heap.try_get(seq), Some(HeapObj::Tuple(_))) { seq } else { self.heap.alloc(HeapObj::Tuple(items)).ok()? };
            alloc::vec![tuple]
        } else { items };
        if args.len() != s.arity as usize { return None; }
        self.budget -= 1;
        let at = base + s.link as usize + 1;
        self.regs[at..at + args.len()].copy_from_slice(&args);
        self.regs[at - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
        Some(s.start as usize)
    }

    /* `*a` or `**a` of an argument list, the value off the stack spread onto it as its positionals or its keyword pairs. */
    fn unpack_args(&mut self, operand: u16, chunk: &SSAChunk, rip: Option<usize>) -> Result<(), VmErr> {
        let val = self.pop()?;
        let kw_before = (operand >> 2) as usize;
        match operand & 0x3 {
            1 => {
                let val = match rip { Some(rip) if matches!(self.heap.try_get(val), Some(HeapObj::Iter(..))) => match self.lazy_items(val, rip)? { Some(v) => v, None => return Ok(()) }, _ => val };
                let items = self.iterable_items(val, chunk)?;
                let n = items.len() as i32;
                // Insert below preceding kw pairs so positionals stay contiguous.
                let at = self.stack.len() - 2 * kw_before;
                for (off, v) in items.into_iter().enumerate() { self.stack.insert(at + off, v); }
                self.pending.pos_delta += n - 1;
            }
            2 => {
                let pairs = self.mapping_to_kw_pairs(val)?;
                let n = pairs.len() as i32;
                for (k, v) in pairs { self.push(k); self.push(v); }
                self.pending.pos_delta -= 1;
                self.pending.kw_delta += n;
            }
            _ => return Err(cold_runtime("UnpackArgs: bad operand")),
        }
        Ok(())
    }

    /* Enters the section of a `super()` call at `rip` while the class still finds the laid method past the proxy's class, None for another, the head when it asks for one laid. */
    #[inline(never)]
    fn enter_super(&mut self, code: &Code, chunk: &SSAChunk, rip: usize) -> Option<usize> {
        let (ins, store, x) = (code.ins[rip], code.ins[rip + 1], code.ins[rip + 2]);
        let s = code.block.as_deref()?.sections.get(x.c as usize)?;
        let (_, name) = s.via_super.as_ref()?;
        if self.budget == 0 { return None; }
        let base = self.base;
        let Some(&HeapObj::Super(owner, recv)) = self.heap.try_get(self.regs[base + ins.b as usize]) else { return None };
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(recv) else { return None };
        let found = self.lookup_class_member_after(class, owner, name);
        if found.is_none_or(|(f, _)| f.0 != s.value.get().0) {
            // Another method in its place, from an assignment to the class, is laid past the end and checked past this head, one no head lays counting toward laying again.
            self.refused = false;
            if let Some((f, _)) = found && matches!(self.heap.try_get(f), Some(HeapObj::Func(..))) && !self.redo.keep && self.asks_growth(code, chunk, rip, lower::Grow::Chain, f) { return Some(rip); }
            if !core::mem::take(&mut self.refused) && code.misses.get() < lower::MISSES {
                code.misses.set(code.misses.get() + 1);
                if code.misses.get() == lower::MISSES && self.preempt_left == 0 { self.preempt_left = RELAY; }
            }
            return None;
        }
        self.budget -= 1;
        let at = base + s.link as usize + 1;
        let c = self.regs[base + ins.c as usize];
        self.regs[at] = recv;
        if ins.x > 0 { self.regs[at + 1] = c; }
        self.regs[at - 1] = Val::int((store.b as i64) << 16 | store.a as i64);
        Some(s.start as usize)
    }

    /* Takes back, for an exception a handler at depth `depth` catches, the registers each recursion level through sections kept, top down as `section_error` walks. */
    #[cold]
    #[inline(never)]
    fn unwind_levels(&mut self, code: &Code, rip: usize, depth: usize) { self.walk_levels(code, rip, true, depth, |_, _, _, _, _| true); }

    /* `next_cleanup_handler` for a return out of section `s`, down to the frames its own `try`s pushed at this recursion level, none lower than the stack stands now. */
    fn section_cleanup(&mut self, s: &lower::Section) -> Option<usize> {
        let level = self.stack.len();
        let own = |f: &ExceptionFrame| f.handler_ip & RAW_HANDLER != 0 && (s.start as usize..s.end as usize).contains(&(f.handler_ip & !RAW_HANDLER)) && f.stack_depth >= level;
        let base = self.exception_stack.iter().rposition(|f| !own(f)).map_or(0, |k| k + 1);
        self.next_cleanup_handler(base)
    }

    /* Pops Except frames and the next Finally frame, returns its handler IP, or None at base. */
    fn next_cleanup_handler(&mut self, exc_base: usize) -> Option<usize> {
        while self.exception_stack.len() > exc_base {
            let frame = self.exception_stack.pop().unwrap();
            self.stack.truncate(frame.stack_depth);
            self.turned = frame.turned;
            self.iter_stack.truncate(frame.iter_depth);
            self.with_stack.truncate(frame.with_depth);
            // Discard reasons from finally bodies skipped while seeking this handler.
            self.unwind_stack.truncate(frame.unwind_depth);
            self.handling.truncate(frame.handling_depth);
            if frame.kind == BlockKind::Finally {
                return Some(frame.handler_ip);
            }
        }
        None
    }

    /* Judges what `__exit__` returned, a truthy result suppressing the exception. */
    fn with_judge(&mut self, r: Val) {
        if matches!(self.unwind_stack.last(), Some(Unwind::Reraise(..))) && self.truthy(r) {
            // Suppress, turn the re-raise into a normal exit and drop the exc identity.
            if let Some(top) = self.unwind_stack.last_mut() { *top = Unwind::Normal; }
            if let (Some(&(h, _)), Some(e)) = (self.handling.last(), self.pending.exc_val.take()) && h.0 == e.0 { self.handling.pop(); }
        }
    }

    /* A `with` its body falls out of, its block popped and its exit marked normal as PopExcept and BeginFinally do. */
    #[inline]
    fn with_leave(&mut self) {
        if let Some(f) = self.exception_stack.pop() { self.handling.truncate(f.handling_depth); }
        self.unwind_stack.push(Unwind::Normal);
    }

    /* Resolves both dunders of `cm` up front as Python does, keeping its `__exit__` and, when that names `super`, its class, and gives the `__enter__` and its class. */
    fn with_push(&mut self, cm: Val, is_async: bool) -> Result<(Val, Val), VmErr> {
        // `async with` takes the awaitable pair.
        let (enter, exit) = if is_async { ("__aenter__", "__aexit__") } else { ("__enter__", "__exit__") };
        let (enter, class) = self.with_dunder(cm, enter)?;
        let (exit, owner) = self.with_dunder(cm, exit)?;
        let tag = if self.names_super(exit) { owner } else { Val::none() };
        self.with_stack.extend_from_slice(&[cm, exit, tag]);
        Ok((enter, class))
    }

    /* The context manager of the innermost `with`, its `__exit__` and the class to bind it to, None for an `__exit__` that names no `super`. */
    fn with_pop(&mut self) -> Result<(Val, Val, Val), VmErr> {
        let len = self.with_stack.len();
        let &[cm, exit, tag] = self.with_stack.get(len.wrapping_sub(3)..).ok_or_else(|| cold_runtime("WithExit without matching WithEnter"))? else { unreachable!() };
        self.with_stack.truncate(len - 3);
        Ok((cm, exit, tag))
    }

    /* Whether `f` is a function whose body names `super`, so a `with` calls it bound. */
    fn names_super(&mut self, f: Val) -> bool {
        let fi = match self.heap.try_get(f) { Some(&HeapObj::Func(fi, ..)) => Some(fi), _ => None };
        fi.is_some_and(|fi| self.uses_super(fi))
    }

    /* Stages a `with`'s `__enter__` or `__exit__` from register `ins.b`, the method, the manager and for `__exit__` its exception, an `__enter__` learning both at `rip`. */
    #[inline(never)]
    fn with_stage(&mut self, ins: Ins, rip: usize, cache: &OpcodeCache, code: &Code, chunk: &SSAChunk, ip: &mut usize) -> Result<(), VmErr> {
        let at = self.base + ins.b as usize;
        let (func, cm, owner) = if ins.op == OpCode::WithEnter {
            let cm = if ins.x == 2 { self.regs[self.base + ins.c as usize] } else { self.pop()? };
            let (func, owner) = self.with_push(cm, false)?;
            let (exit, tag) = (self.with_stack[self.with_stack.len() - 2], self.with_stack[self.with_stack.len() - 1]);
            // A dunder whose body names `super` is called as a method, bound to its owner, which the site leaves to here.
            let bound = self.names_super(func) || !tag.is_none();
            if let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(cm) { cache.set_site(rip, Site::Dunder { class, func, owner: exit, arity: bound as u8, epoch: self.class_epoch }); }
            (func, cm, if self.names_super(func) { owner } else { Val::none() })
        } else {
            let (cm, func, tag) = self.with_pop()?;
            // Reraise selects `__exit__(type, exc, None)`, other exits pass three Nones.
            let (exc_type, exc) = if matches!(self.unwind_stack.last(), Some(Unwind::Reraise(..))) {
                let exc = self.pending.exc_val.unwrap_or(Val::none());
                let exc_name = self.exc_type_name(exc);
                (self.heap.alloc(HeapObj::Type(exc_name))?, exc)
            } else {
                (Val::none(), Val::none())
            };
            self.regs[at + 2..at + 5].copy_from_slice(&[exc_type, exc, Val::none()]);
            // The last two go straight to the parameters of the section the call jumps to, as well.
            if ins.c != 0 { let c = self.base + ins.c as usize; self.regs[c..c + 2].copy_from_slice(&[exc, Val::none()]); }
            (func, cm, tag)
        };
        (self.regs[at], self.regs[at + 1]) = (func, cm);
        if !owner.is_none() { self.with_bound(rip, owner, code, chunk, ip)?; }
        Ok(())
    }

    /* Notes the `super`-naming dunder `func` the call at `j` makes and the class of manager `cm`, so the code laid again lays it for that class. */
    #[cold]
    #[inline(never)]
    fn with_noted(&mut self, code: &Code, chunk: &SSAChunk, j: usize, func: Val, cm: Val) {
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(cm) else { return };
        if !self.lays_later(code) { return; }
        self.note_called(code, chunk, j, func, 0);
        self.note_called(code, chunk, j, class, lower::WITH);
        // Lowered code that met it only after its lowering is laid again with it.
        if code.block.is_some() && code.misses.get() < lower::MISSES { miss(&mut self.preempt_left, code); }
    }

    /* Makes the call after the `with` at `rip` with its method bound to `owner`, carrying on where its result is stored, past the call or for a section's call past its stub. */
    #[inline(never)]
    fn with_bound(&mut self, rip: usize, owner: Val, code: &Code, chunk: &SSAChunk, ip: &mut usize) -> Result<(), VmErr> {
        let (w, call) = (code.ins[rip], code.ins[rip + 1]);
        let j = if lower::section_call(call.op) { code.ins[rip + 4].a as usize } else { rip + 1 };
        let n = if w.op == OpCode::WithEnter { 2 } else { 5 };
        let at = self.base + w.b as usize;
        let class = match self.heap.try_get(self.regs[at + 1]) { Some(&HeapObj::Instance(c, _)) => c, _ => Val::undef() };
        // The section laid for the dunder on this class runs it with its `super()` found for that class, any other call noting the class.
        if lower::section_call(call.op) && let Some(s) = code.block.as_deref().and_then(|b| b.sections.get(code.ins[rip + 3].c as usize))
            && s.value.get().0 == self.regs[at].0 && s.class.0 == class.0 && !class.is_undef() { return Ok(()); }
        self.with_noted(code, chunk, j, self.regs[at], self.regs[at + 1]);
        let staged: [Val; 5] = core::array::from_fn(|k| if k < n { self.regs[at + k] } else { Val::undef() });
        self.stack.extend_from_slice(&staged[..n]);
        self.pending.method_binding = Some((owner, staged[1]));
        self.call_stacked(n as u16 - 1, code.orig(j), chunk, (0, super::redo::Rerun::No))?;
        *ip = j + 1 + lower::wide(code.ins[j]) as usize;
        Ok(())
    }

    /* `with_stage` of a stack manager whose class its site learned with no class changed, or of an exit carrying something, false untouched for a leaving exception or `super`. */
    #[inline]
    fn with_hit(&mut self, ins: Ins, i: usize, code: &Code) -> bool {
        let at = self.base + ins.b as usize;
        if ins.op == OpCode::WithEnter {
            let Some(&cm) = self.stack.last() else { return false };
            let (heap, epoch) = (&self.heap, self.class_epoch);
            let Some((func, exit)) = code.cache.with_site(i, |site| with_func(heap, epoch, site, cm)) else { return false };
            self.stack.pop();
            self.with_stack.extend_from_slice(&[cm, exit, Val::none()]);
            (self.regs[at], self.regs[at + 1]) = (func, cm);
        } else {
            let len = self.with_stack.len();
            let Some(&[cm, func, tag]) = self.with_stack.get(len.wrapping_sub(3)..) else { return false };
            if !tag.is_none() || matches!(self.unwind_stack.last(), Some(Unwind::Reraise(..))) { return false; }
            self.with_stack.truncate(len - 3);
            self.regs[at..at + 5].copy_from_slice(&[func, cm, Val::none(), Val::none(), Val::none()]);
            if ins.c != 0 { let c = self.base + ins.c as usize; self.regs[c..c + 2].fill(Val::none()); }
        }
        true
    }

    /* The function a context manager binds to `name` and the class holding it. */
    fn with_member(&self, cm: Val, name: &str) -> Option<(Val, Val)> {
        let Some(&HeapObj::Instance(cls, _)) = self.heap.try_get(cm) else { return None };
        self.lookup_class_member(cls, name).filter(|&(func, _)| matches!(self.heap.try_get(func), Some(HeapObj::Func(..))))
    }

    /* MRO-bound dunder pair for a context manager, Err when unmet. */
    fn with_dunder(&mut self, cm: Val, name: &str) -> Result<(Val, Val), VmErr> {
        if let Some(found) = self.with_member(cm, name) { return Ok(found); }
        // The missing half is named, the exit one when both are.
        let (enter, exit, kind) = if name.starts_with("__a") { ("__aenter__", "__aexit__", "asynchronous context manager") } else { ("__enter__", "__exit__", "context manager") };
        let missed = if name == enter && self.with_member(cm, exit).is_some() { enter } else { exit };
        Err(VmErr::TypeMsg(s!("'", str self.type_name(cm), "' object does not support the ", str kind, " protocol (missed ", str missed, " method)")))
    }

    /* Exception class name for a raised value, defaults to "Exception" for non-instances. */
    pub(crate) fn exc_type_name(&self, exc: Val) -> String {
        if !exc.is_heap() { return "Exception".into(); }
        match self.heap.get(exc) {
            HeapObj::ExcInstance(n, ..) => n.clone(),
            HeapObj::Instance(cls, _) => {
                if cls.is_heap() && let HeapObj::Class(name, _, _) = self.heap.get(*cls) { name.clone() } else { "Exception".into() }
            }
            _ => "Exception".into(),
        }
    }

    /* What a `len`, `bool`, `str` or `repr` of `o` learns of the dunder it ran, so lowered again the call lays its section. */
    #[inline(never)]
    fn learn_fused(&mut self, opcode: OpCode, o: Val, rip: usize, cache: &OpcodeCache, code: &Code, chunk: &SSAChunk) {
        if opcode == OpCode::CallLen { self.learn_dunder(rip, cache, code, chunk, o, "__len__", 1); }
        else if opcode == OpCode::CallBool { self.learn_truth(rip, cache, code, chunk, o); }
        else if let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(o) {
            let name = if opcode == OpCode::CallStr && self.lookup_class_member(class, "__str__").is_some() { "__str__" } else { "__repr__" };
            self.learn_dunder(rip, cache, code, chunk, o, name, 1);
        }
    }

    /* Readies the `in` at `rip`, its item and container on the stack, to arm once a dunder of what the container holds runs. */
    #[inline(never)]
    fn contains_defer(&mut self, rip: usize) -> super::redo::Defer {
        let l = self.stack.len();
        let holds = |v: Val| matches!(self.heap.plain(v), Some(HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Dict(_) | HeapObj::Set(_) | HeapObj::FrozenSet(_)));
        if l < 2 || !(holds(self.stack[l - 1]) || holds(self.stack[l - 2])) { return super::redo::Defer::No; }
        self.redo_defer_top(rip, 2)
    }

    /* A builtin's op whose native may call Python, its calls going to the loop and its stack arguments taken as it runs again once such a call comes back. */
    #[inline(never)]
    fn call_redone(&mut self, opcode: OpCode, op: u16, rip: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        // A builtin the module rebound calls its binding as a plain call does, its function a frame of this loop.
        if self.fused_rebound(opcode) && !self.redo.keep {
            (self.pending_exec_safe, self.inline_call) = (self.frame_safe, true);
            let r = self.handle_function(opcode, op, chunk);
            self.inline_call = false;
            return r;
        }
        let n = if matches!(opcode, OpCode::CallPrint | OpCode::CallDict | OpCode::CallMin | OpCode::CallMax | OpCode::CallEnumerate) { (op & 0xFF) as usize + 2 * (op >> 8 & 0x3F) as usize } else { op as usize & 0xFF };
        // A spread call's items were merged by the ops before it, so it never runs again.
        let again = n <= super::redo::OPS && op & crate::parser::SPREAD_ARGS == 0 && !matches!(opcode, OpCode::CallSpread | OpCode::MakeFunction | OpCode::MakeCoroutine | OpCode::CallExtern);
        // An `isinstance`, `type` or `callable` asks the class and calls nothing, nor do min, max, sum, any and all, whose jobs take the items' dunders.
        if !again || self.builtins_rebound == 0 && matches!(opcode, OpCode::CallIsInstance | OpCode::CallType | OpCode::CallCallable | OpCode::CallMin | OpCode::CallMax | OpCode::CallSum | OpCode::CallAny | OpCode::CallAll) { return self.handle_function(opcode, op, chunk); }
        // One argument that is no user object calls nothing, but a container's items a print or a text writes.
        if !self.redo.keep && self.builtins_rebound == 0 && (n == 0 || n == 1 && !matches!(opcode, OpCode::CallPrint | OpCode::CallStr | OpCode::CallRepr) && !self.heap.user(self.stack[self.stack.len() - 1])
            // Nor does a print or a text of numbers and texts alone.
            || matches!(opcode, OpCode::CallPrint | OpCode::CallStr | OpCode::CallRepr) && self.stack[self.stack.len() - n..].iter().all(|&v| !v.is_heap() || matches!(self.heap.plain(v), Some(HeapObj::Str(_))))) { return self.handle_function(opcode, op, chunk); }
        // It arms only once a call it makes needs it.
        let d = self.redo_defer_top(rip, n);
        let r = self.handle_function(opcode, op, chunk);
        self.redo_undefer(d);
        r
    }

    /* Whether fused builtin `opcode` of `op` arms its calls, not for class queries, min, max, sum, any and all, or a lone non-user argument a print or text writes. */
    #[inline(always)]
    fn call_arms(&self, opcode: OpCode, op: u16) -> bool {
        if self.builtins_rebound != 0 || self.redo.keep { return true; }
        if matches!(opcode, OpCode::CallIsInstance | OpCode::CallType | OpCode::CallCallable | OpCode::CallMin | OpCode::CallMax | OpCode::CallSum | OpCode::CallAny | OpCode::CallAll) { return false; }
        !(op & 0xFF == 1 && !matches!(opcode, OpCode::CallPrint | OpCode::CallStr | OpCode::CallRepr | OpCode::CallDict | OpCode::CallMin | OpCode::CallMax | OpCode::CallEnumerate)
            && self.stack.last().is_none_or(|&v| !self.heap.user(v)))
    }

    /* Heavy arms extracted out of `dispatch` so wasm-opt can dedup prologues and the dispatcher itself stays compact. */

    #[inline(never)]
    pub(crate) fn exec_arith_or_compare(&mut self, opcode: OpCode, operand: u16, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk, open: bool) -> Result<(), VmErr> {
        // A stack op runs again with its operands put back, a register op's stack form with them in their registers.
        let rerun = if open { super::redo::Rerun::Stack } else { super::redo::Rerun::Regs };
        let n = self.stack.len();
        if n >= 2 && let Some(v) = self.wide_fast(opcode, self.stack[n - 2], self.stack[n - 1])? {
            self.stack.truncate(n - 2);
            self.stack.push(v);
            return Ok(());
        }
        // The register loop already tried two plain numbers.
        let site = cache.site(rip);
        // An op run again for the calls its handler made takes them through the handler, which made them.
        if matches!(site, Site::Dunder { .. }) && !self.redo_replays() {
            // A stack op whose result the next instruction takes runs the dunder as a frame of this loop.
            if open && self.open_dunder(rip, site)? { return Ok(()); }
            if self.exec_dunder(site, rip, rerun, chunk)? { return Ok(()); }
            cache.set_site(rip, Site::Empty);
        } else if open && matches!(site, Site::Empty) && !self.redo_replays() && self.pools[self.pool_now].func != super::cache::NO_FN && self.open_first(opcode, rip, cache, chunk)? { return Ok(()); }
        // A list extended in place by a generator runs it as a frame of this loop, the op run again on its items.
        if open && opcode == OpCode::InPlaceAdd && n >= 2 && self.top_may_gen()
            && self.stack[n - 2].is_heap() && matches!(self.heap.get(self.stack[n - 2]), HeapObj::List(_)) && self.retry_gen(chunk)? { return Ok(()); }
        let r = if matches!(opcode, OpCode::Eq | OpCode::Lt | OpCode::NotEq | OpCode::Gt | OpCode::LtEq | OpCode::GtEq) {
            self.handle_compare(opcode, rip, cache, chunk, rerun)
        } else {
            self.handle_arith(opcode, operand, rip, cache, chunk, rerun)
        };
        (self.declined, self.declined_also) = (Val::undef(), Val::undef());
        r
    }

    /* The dunder an op meeting an instance on its left first learns before it runs, so it opens as a frame of this loop, its own operator found first, None for the handler's. */
    #[cold]
    #[inline(never)]
    pub(crate) fn first_dunder(&self, opcode: OpCode, a: Val, inplace: bool, rip: usize, cache: &OpcodeCache) -> Option<Site> {
        if !matches!(self.heap.try_get(a), Some(HeapObj::Instance(..))) { cache.set_site(rip, Site::Plain); return None; }
        let compare = matches!(opcode, OpCode::Eq | OpCode::Lt | OpCode::NotEq | OpCode::Gt | OpCode::LtEq | OpCode::GtEq);
        let name = if compare { super::opcodes::dunder::compare_dunder_names(opcode)?.0 } else { self.site_dunder_name(opcode, a, inplace)? };
        // A `!=` of a class without `__ne__` inverts its `__eq__`, which the handler runs.
        let class = self.instance_class(a)?;
        let (func, owner) = self.lookup_class_member(class, name)?;
        if !matches!(self.heap.try_get(func), Some(HeapObj::Func(..))) { return None; }
        let site = Site::Dunder { class, func, owner, arity: 2, epoch: self.class_epoch };
        cache.set_site(rip, site);
        Some(site)
    }

    /* `a op b` for two ints, one boxed and both within i128, without the generic handler, None to fall back. */
    #[inline]
    pub(crate) fn wide_fast(&mut self, op: OpCode, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
        if !(a.is_foreign() || b.is_foreign()) { return Ok(None); }
        let int = |v: Val| if v.is_int() { Some(v.as_int() as i128) } else { self.heap.foreign.wide_value(v) };
        let (Some(x), Some(y)) = (int(a), int(b)) else { return Ok(None) };
        let r = match op {
            OpCode::Add | OpCode::InPlaceAdd => x.checked_add(y),
            OpCode::Sub | OpCode::InPlaceSub => x.checked_sub(y),
            OpCode::Mul => x.checked_mul(y),
            OpCode::FloorDiv if y != 0 => int_divmod(x, y).map(|(q, _)| q),
            OpCode::Mod if y != 0 => int_divmod(x, y).map(|(_, r)| r),
            OpCode::BitAnd => Some(x & y),
            OpCode::BitOr => Some(x | y),
            OpCode::BitXor => Some(x ^ y),
            OpCode::Shr if y >= 0 => Some(x >> y.min(127)),
            OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::LtEq | OpCode::Gt | OpCode::GtEq => {
                return Ok(Some(Val::bool(match op { OpCode::Eq => x == y, OpCode::NotEq => x != y, OpCode::Lt => x < y, OpCode::LtEq => x <= y, OpCode::Gt => x > y, _ => x >= y })));
            }
            _ => None,
        };
        r.map(|r| self.heap.int(r)).transpose()
    }

    /* `obj[idx]` on the stack, a monomorphic `__getitem__` site skipping the class lookup. */
    pub(crate) fn get_item_op(&mut self, rip: usize, chunk: &SSAChunk, cache: &OpcodeCache, open: bool) -> Result<(), VmErr> {
        let rerun = if open { super::redo::Rerun::Stack } else { super::redo::Rerun::Regs };
        let site = cache.site(rip);
        if matches!(site, Site::Dunder { .. }) && !self.redo_replays() {
            if open && self.open_dunder(rip, site)? { return Ok(()); }
            if self.exec_dunder(site, rip, rerun, chunk)? { return Ok(()); }
            cache.set_site(rip, Site::Empty);
        }
        let r = self.get_item(rip, chunk, cache, rerun);
        (self.declined, self.declined_also) = (Val::undef(), Val::undef());
        r
    }

    /* One unit and the native work the heap counted, for native loops that bypass the back-edge counter. */
    #[inline]
    pub(crate) fn charge_step(&mut self) -> Result<(), VmErr> {
        self.charge_steps(1 + self.heap.take_work())
    }

    /* The native work counted since the last charge, paid before the next instruction runs. */
    #[inline]
    pub(crate) fn pay_work(&mut self) -> Result<(), VmErr> {
        let w = self.heap.take_work();
        if w == 0 { Ok(()) } else { self.charge_steps(w) }
    }

    /* Charge `n` units at once for native builtins (sort, materialise) whose cost scales with input size. */
    #[inline]
    pub(crate) fn charge_steps(&mut self, n: usize) -> Result<(), VmErr> {
        let n = n as u64;
        if self.budget < n { self.budget = 0; return Err(cold_budget()); }
        self.budget -= n;
        self.heap.set_budget(self.budget);
        Ok(())
    }

    #[inline(never)]
    fn exec_for_iter(&mut self, op: u16, ip: &mut usize, n: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        match self.for_step(chunk)? {
            // Parked with its generator, the `for` runs again on resume.
            Some(item) if item.is_undef() => *ip -= 1,
            Some(item) => self.push(item),
            None => {
                if op as usize > n { return Err(cold_runtime("jump target out of bounds")); }
                *ip = op as usize;
            }
        }
        Ok(())
    }

    /* The next item of the innermost iterator, None once it ended and left the iterator stack, undef while it waits with its generator. */
    #[inline(never)]
    fn for_step(&mut self, chunk: &SSAChunk) -> Result<Option<Val>, VmErr> {
        self.charge_step()?;
        if self.heap.needs_gc() { self.collect_point()?; }
        // The next item, or the value a `yield from` over this frame evaluates to once it ends.
        let step = match self.iter_stack.last() {
            Some(IterFrame::AsyncFor { .. }) => return self.async_for_step(chunk),
            // Resume directly so `yielded` distinguishes a yielded value (even None) from exhaustion.
            Some(&IterFrame::Coroutine(_)) if self.too_deep() => return Err(crate::vm::cold_depth()),
            Some(&IterFrame::Coroutine(cv)) => {
                self.for_steps = true;
                let result = self.step_coroutine(cv);
                self.for_steps = false;
                let result = result?;
                // A generator parked on a wait leaves the `for` waiting in its place, a placeholder taking what wakes it.
                if self.yielded && self.waits() {
                    if !self.pending.preempt_request { self.push(Val::none()); }
                    return Ok(Some(Val::undef()));
                }
                if core::mem::take(&mut self.yielded) { Ok(result) } else { Err(result) }
            }
            // A user iterator steps `__next__`, `StopIteration` ends the loop and `raise StopIteration(v)` carries `v`.
            Some(&IterFrame::UserDefined(iter)) => {
                let before = self.pending.exc_val;
                match self.iter_next_proto(iter, chunk) {
                    Ok(Some(item)) => Ok(item),
                    Ok(None) => Err(Val::none()),
                    Err(VmErr::Raised(m)) if m == "StopIteration" || m.starts_with("StopIteration:") => Err(self.swallow_stop(before)),
                    Err(e) => return Err(e),
                }
            }
            // A `map` or `filter` calls its function, a StopIteration out of it ending the loop.
            Some(f @ (IterFrame::Map { .. } | IterFrame::Filter { .. })) => {
                let (state, before) = (f.clone(), self.pending.exc_val);
                // A filter may leave a generator's next item to the `for` to run as a frame.
                self.step_soft = matches!(state, IterFrame::Filter { .. });
                let stepped = self.called_step(state);
                self.step_soft = false;
                match stepped {
                    Ok(Some(item)) => Ok(item),
                    Ok(None) => Err(Val::none()),
                    Err(e) if is_stop(&e) => Err(self.swallow_stop(before)),
                    Err(e) => return Err(e),
                }
            }
            // Split-borrow heap so the Range step can box a wide int.
            _ => match self.iter_stack.last_mut() {
                Some(f) => f.next_item(&mut self.heap)?.ok_or(Val::none()),
                None => Err(Val::none()),
            },
        };
        Ok(match step {
            Ok(item) => Some(item),
            Err(done) => {
                self.yield_from_value = done;
                self.iter_stack.pop();
                None
            }
        })
    }

    /* The pre-registered value for a genuine builtin name, None for user names, so deleted user bindings stay deleted. */
    pub(crate) fn builtin_binding(&self, bare: &str) -> Option<Val> {
        if NativeFnId::from_name(bare).is_none() && crate::parser::builtin_type(bare).is_none() {
            return None;
        }
        self.global(bare)
    }

    #[inline(never)]
    fn exec_make_class(&mut self, op: u16, ip: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        // The operand is the class chunk index, the body keeps its base count.
        let class_idx = op as usize;
        let (num_bases, num_kw) = chunk.classes.get(class_idx).map_or((0, 0), |b| (b.bases as usize, b.class_kw as usize));
        // Pop keywords and bases first so a misencoded operand fails before we touch the body.
        let keywords = self.pop_n(2 * num_kw)?;
        let mut bases = if num_bases == crate::parser::SPREAD_BASES as usize {
            let list = self.pop()?;
            match self.heap.try_get(list) { Some(HeapObj::List(l)) => l.borrow().clone(), _ => return Err(cold_runtime("spread bases corrupted")) }
        } else { self.pop_n(num_bases)? };
        self.check_bases(&mut bases)?;
        let Some(body) = chunk.classes.get(class_idx) else {
            return Err(cold_runtime("class index out of range"));
        };
        // The cells around the class, its defining function's or its enclosing class's.
        let around: Vec<(String, Val)> = match self.body_to_fi.get(&(chunk as *const SSAChunk)) {
            Some(&fi) => chunk.names.iter().enumerate()
                .filter(|&(i, _)| self.fn_scope[fi].kinds.get(i) == Some(&Kind::Cell))
                .filter_map(|(i, n)| self.regs.get(self.base + i).filter(|&&c| matches!(self.heap.try_get(c), Some(HeapObj::Cell(_)))).map(|&c| (String::from(ssa_strip(n)), c)))
                .collect(),
            None if self.class_chunks.contains(&(chunk as *const SSAChunk)) => self.class_cells.last().cloned().unwrap_or_default(),
            None => Vec::new(),
        };
        // Members are exactly the slots the body itself stores, loads of builtins or injected globals never leak into the class namespace.
        let mut member = alloc::vec![false; body.names.len()];
        for ins in body.instructions.iter().filter(|i| matches!(i.opcode, OpCode::StoreName | OpCode::Phi)) {
            if let Some(m) = member.get_mut(ins.operand as usize) { *m = true; }
        }
        // A defining function lends its plain locals by value, the body runs before they can change.
        let lender = self.body_to_fi.get(&(chunk as *const SSAChunk)).copied();
        let local = |bare: &str| -> Option<Val> {
            let fi = lender?;
            chunk.names.iter().enumerate()
                .filter(|&(i, n)| self.fn_scope[fi].kinds.get(i) == Some(&Kind::Local) && ssa_strip(n) == bare)
                .find_map(|(i, _)| self.regs.get(self.base + i).copied().filter(|v| !v.is_undef()))
        };
        // A class body reads enclosing variables first, then its module, then the builtins.
        let module = self.chunk_module_id(body);
        let mut class_slots = alloc::vec![Val::undef(); body.names.len()];
        let reads = !around.is_empty() || lender.is_some();
        for (i, name) in body.names.iter().enumerate() {
            let bare = ssa_strip(name);
            // A slot starts from the builtin its name or its version-0 name finds, a bare name from the entry module's binding first, never a versioned one.
            let first = if bare.len() == name.len() { self.global(name) } else {
                self.builtins.get(name.as_str()).or_else(|| name.strip_suffix("_0").and_then(|b| self.builtins.get(b))).copied()
            };
            if let Some(v) = first { class_slots[i] = v; }
            // A name the body assigns is its own, read before then it falls to the module.
            let enclosing = if member[i] || !reads { None } else {
                around.iter().find(|(n, _)| n == bare).map(|&(_, c)| self.deref(c)).or_else(|| local(bare))
            };
            if let Some(v) = enclosing {
                class_slots[i] = v;
            } else if class_slots[i].is_undef() {
                // A bare name of the entry module was looked for just above, and the entry module is its own module there.
                let gv = if module == 0 { if bare.len() == name.len() { None } else { self.global(bare) } }
                    else { self.scopes[module].get(bare).or_else(|| self.global(bare)) };
                if let Some(gv) = gv { class_slots[i] = gv; }
            }
        }
        // It nests as deep as its own run would.
        if self.depth >= self.max_calls { return Err(cold_depth()); }
        self.class_cells.push(around);
        // Name comes from the StoreName target, skip any decorator `Call`s emitted between.
        let written = &chunk.instructions;
        let mut j = ip;
        while matches!(written.get(j).map(|i| i.opcode), Some(OpCode::Call)) { j += 1; }
        let name = written.get(j)
            .filter(|i| i.opcode == OpCode::StoreName)
            .and_then(|i| chunk.names.get(i.operand as usize))
            .map(|n| ssa_strip(n))
            // A generic class returns from the scope of its parameters, so its qualified name gives the name.
            .unwrap_or_else(|| body.qualname.rsplit('.').next().unwrap_or("?"))
            .to_string();
        // The body runs as a floor of this loop, `class_end` making the class once it ends.
        let run = super::globals::compile::FloorRun::Class(super::globals::compile::ClassRun { bases, member, name, keywords });
        // One floor's room at first, a block small enough for the allocator's pool.
        if self.floors.capacity() == 0 { self.floors.reserve_exact(1); }
        self.floors.push(super::globals::compile::Floored::new(body as *const SSAChunk, class_slots, 1, run));
        (self.floor_req, self.yielded) = (true, true);
        Ok(())
    }

    /* Whether a class of `bases` and `methods` is plain, as `class_end` tells. */
    #[inline(never)]
    fn plain_class(&self, bases: &[Val], methods: &[(String, Val)]) -> bool {
        for &b in bases {
            if !(self.heap.is_plain(b) || matches!(self.heap.try_get(b), Some(HeapObj::Type(_)))) { return false; }
        }
        for (n, v) in methods {
            if !plain_name(n.as_bytes()) { return false; }
            if v.is_heap() && matches!(self.heap.get(*v), HeapObj::Property(..) | HeapObj::ClassMethod(_) | HeapObj::StaticMethod(_) | HeapObj::Instance(..)) { return false; }
        }
        true
    }

    /* Ends the class body atop the floors set aside, its code giving `ran` (`yielded` if suspended), the class made from its slots, hooks as frames when `frames`. */
    pub(crate) fn class_end(&mut self, ran: Result<Val, VmErr>, yielded: bool, frames: bool) -> Result<Val, VmErr> {
        let Some(f) = self.floors.pop() else { return ran };
        let super::globals::compile::FloorRun::Class(super::globals::compile::ClassRun { bases, member, name: name_str, keywords }) = f.run else { return ran };
        self.class_cells.pop();
        if yielded {
            self.resume_ip = 0;
            self.pending.clear_waits();
            return Err(VmErr::Runtime("a class body cannot suspend, await outside it"));
        }
        ran?;
        // SAFETY the VM keeps every chunk it runs until it drops.
        let body: &SSAChunk = unsafe { &*f.chunk };
        let class_slots = f.slots;
        let mut methods: Vec<(String, Val)> = Vec::new();
        for (i, name) in body.names.iter().enumerate() {
            if !member[i] { continue; }
            if let Some(&v) = class_slots.get(i)
                && !v.is_undef() {
                    let base = ssa_strip(name);
                    if let Some(pos) = methods.iter().position(|(n, _)| n == base) {
                        methods[pos].1 = v;
                    } else {
                        methods.push((base.to_string(), v));
                    }
                }
        }
        let qualname = (body.qualname != name_str).then(|| body.qualname.clone());
        // A docstring the body opens with is the `__doc__` of the class.
        let base = self.temp_roots.len();
        if !methods.iter().any(|(n, _)| n == "__doc__") && let Some(doc) = super::opcodes::attr_lookup::docstring(body) {
            let v = self.heap.alloc(HeapObj::Str(doc))?;
            self.temp_roots.push(v);
            methods.insert(0, (String::from("__doc__"), v));
        }
        // SAFETY the chunk that holds the class statement lives as long as the VM.
        let cls = self.finish_class(name_str, qualname, bases, methods, &keywords, frames);
        self.temp_roots.truncate(base);
        cls
    }

    /* Refuses a base no class can derive from or two builtin layouts at once, an `object` base staying where it was listed. */
    pub(crate) fn check_bases(&self, bases: &mut [Val]) -> Result<(), VmErr> {
        // An alias base, `Box[int]` or `list[int]`, derives from the class it parametrizes.
        for b in bases.iter_mut() { if let Some(&HeapObj::GenericAlias(origin, _)) = self.heap.try_get(*b) { *b = origin; } }
        // A builtin exception joins as a base, so the class enters its tree, and so do the types in `NATIVE_BASES`.
        for &b in bases.iter() {
            let ok = match self.heap.try_get(b) {
                Some(HeapObj::Class(..)) => true,
                Some(HeapObj::Type(n)) if n == "object" || crate::vm::globals::is_exc_type(n) || super::opcodes::attr_lookup::NATIVE_BASES.contains(&n.as_str()) => true,
                Some(HeapObj::Type(n)) => return Err(VmErr::TypeMsg(s!("type '", str n, "' is not an acceptable base type"))),
                _ => false,
            };
            if !ok { return Err(cold_type("base class must be a class object")); }
        }
        // An instance takes one builtin layout at most, an exception or one builtin type.
        let mut layouts: Vec<Val> = bases.iter().filter_map(|&b| if self.exc_base(b).is_some() { Some(Val::none()) } else { self.native_base(b) }).collect();
        layouts.dedup_by_key(|v| v.0);
        if layouts.len() > 1 { return Err(cold_type("multiple bases have instance lay-out conflict")); }
        Ok(())
    }

    /* Allocates the class from its checked bases and members, caches its order and runs a new class's hooks, as frames of the loop when `frames`. */
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_class(&mut self, name: String, qualname: Option<String>, bases: Vec<Val>, members: Vec<(String, Val)>, keywords: &[Val], frames: bool) -> Result<Val, VmErr> {
        // C3 linearize before allocating, an inconsistent hierarchy raises here, so no half-built class escapes.
        let mro_tail = self.c3_merge(&bases)?;
        // A class of plain methods with such bases or builtin types gives the ops no dunder, property or descriptor to run, and once one is not none needs telling apart.
        let plain = !self.heap.users && self.plain_class(&bases, &members);
        let cls = self.heap.alloc(HeapObj::Class(name, bases, alloc::rc::Rc::new(core::cell::RefCell::new(members))))?;
        if plain { self.heap.set_plain(cls); } else { self.heap.users = true; }
        // A site keyed on the slot of a swept class must not match its successor, nor its qualified name.
        self.class_epoch = self.class_epoch.wrapping_add(1);
        match qualname { Some(q) => { self.class_qualnames.insert(cls.0, q); } None => { self.class_qualnames.remove(&cls.0); } }
        let mut mro = Vec::with_capacity(mro_tail.len() + 1);
        mro.push(cls);
        mro.extend(mro_tail);
        self.mro_cache.insert(cls.0, alloc::rc::Rc::new(mro));
        self.with_roots(keywords.iter().copied().chain([cls]), |vm| vm.class_created(cls, keywords, frames))?;
        Ok(cls)
    }

    /* Tells each member of a new class its name with `__set_name__`, then runs its bases' `__init_subclass__` (as frames when `frames`), true when the first opened. */
    #[inline(never)]
    pub(crate) fn class_created(&mut self, cls: Val, keywords: &[Val], frames: bool) -> Result<bool, VmErr> {
        let members = match self.heap.get(cls) { HeapObj::Class(_, _, m) => m.borrow().clone(), _ => Vec::new() };
        // The dunders that make ops ask a class, looked for in one pass over names that are dunders.
        for (n, _) in members.iter().filter(|(n, _)| n.starts_with("__")) {
            match n.as_str() { "__hash__" => self.user_hash = true, "__new__" => self.user_new = true, "__setattr__" | "__delattr__" => self.attr_hooks = true, _ => {} }
        }
        // A slot name may not also be a class variable.
        if let Some(&(_, slots)) = members.iter().find(|(n, _)| n == "__slots__") {
            self.slotted = true;
            if let Some(n) = self.slot_names(slots).into_iter().find(|n| members.iter().any(|(m, _)| m == n)) {
                return Err(VmErr::Raised(s!("ValueError: '", str &n, "' in __slots__ conflicts with class variable")));
            }
        }
        // A class no hook runs for makes no job.
        let next = members.iter().position(|&(_, m)| self.set_name_of(m).is_some());
        if next.is_none() && keywords.is_empty() && self.lookup_class_member_after(cls, cls, "__init_subclass__").is_none() { return Ok(false); }
        let hooks = alloc::boxed::Box::new(ClassHooks { cls, keywords: keywords.to_vec(), members, next: next.unwrap_or(usize::MAX), subclassed: false, getting: Val::undef() });
        Ok(self.calls_run(hooks, frames)?.is_none())
    }

    /* The `__set_name__` the class of `member` holds and the class holding it. */
    fn set_name_of(&self, member: Val) -> Option<(Val, Val)> {
        if member.0 == self.plain_recv.0 { return None; }
        self.instance_class(member).and_then(|c| self.lookup_class_member(c, "__set_name__"))
    }

    /* `obj.name = value` at a site, which keeps where an instance's attribute went. */
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_attr_at(&mut self, obj: Val, name_idx: u16, value: Val, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<(), VmErr> {
        if self.site_store(cache.site(rip), obj, value) { return Ok(()); }
        let name = chunk.names.get(name_idx as usize).ok_or_else(|| cold_runtime("StoreAttr: bad name index"))?;
        let before = match self.heap.try_get(obj) { Some(HeapObj::Instance(_, a)) => a.borrow().shape(), _ => crate::value::NO_SHAPE };
        self.prop_at = Some((rip, super::act::DISCARD));
        let stored = self.store_attr(obj, name, value, chunk);
        self.prop_at = None;
        stored?;
        cache.set_site(rip, self.learn_store(obj, name, before));
        Ok(())
    }

    /* `obj.name = value`, shared by StoreAttr and `setattr()`, through a `__setattr__` the class defines. */
    pub(crate) fn store_attr(&mut self, obj: Val, name: &str, value: Val, chunk: &SSAChunk) -> Result<(), VmErr> {
        if self.attr_hooks && self.has_hook(obj, "__setattr__") {
            let attr = self.heap.intern_str(name)?;
            // A store of the loop opens it as a frame of the loop, its value dropped.
            let at = self.prop_at.take();
            if !self.hook_in_loop(obj, "__setattr__", &[attr, value], at)? { self.call_hook(obj, "__setattr__", &[attr, value], chunk)?; }
            return Ok(());
        }
        self.store_attr_raw(obj, name, value, chunk)
    }

    /* The error of a store on a value that keeps no attributes, worded by what `name` already is on it. */
    #[cold]
    fn no_store(&self, obj: Val, name: &str) -> VmErr {
        let ty = self.type_name(obj);
        if let Some(HeapObj::Type(t)) = self.heap.try_get(obj) { return VmErr::TypeMsg(s!("cannot set '", str name, "' attribute of immutable type '", str t, "'")); }
        match self.resolve_attr(obj, name) {
            Ok(AttrLookup::BuiltinMethod(_) | AttrLookup::BoundBuiltin(..)) => VmErr::Attribute(s!("'", str ty, "' object attribute '", str name, "' is read-only")),
            Ok(_) if matches!(self.heap.try_get(obj), Some(HeapObj::Range(..) | HeapObj::BigRange(..) | HeapObj::Slice(..))) => VmErr::Attribute("readonly attribute".into()),
            Ok(_) => VmErr::Attribute(s!("attribute '", str name, "' of '", str ty, "' objects is not writable")),
            Err(_) => VmErr::Attribute(s!("'", str ty, "' object has no attribute '", str name, "' and no __dict__ for setting new attributes")),
        }
    }

    /* `object.__setattr__`, the store past any `__setattr__` hook. */
    pub(crate) fn store_attr_raw(&mut self, obj: Val, name: &str, value: Val, chunk: &SSAChunk) -> Result<(), VmErr> {
        if !obj.is_heap() { return Err(self.no_store(obj, name)); }
        let member = self.instance_class(obj).and_then(|c| self.lookup_class_member(c, name));
        if let Some((m, _)) = member && self.store_through_class(obj, m, name, value, chunk)? { return Ok(()); }
        // Class attribute, insert or replace in the mutable members store.
        if let HeapObj::Class(_, _, members) = self.heap.get(obj) {
            // A method or property set on a class may share a name with an instance attribute.
            self.shadowed |= self.hides(value);
            self.user_hash |= name == "__hash__";
            self.user_new |= name == "__new__";
            self.attr_hooks |= name == "__setattr__" || name == "__delattr__";
            set_member(members, name, value);
            self.class_epoch = self.class_epoch.wrapping_add(1);
            return Ok(());
        }
        if let HeapObj::Func(_, _, _, attrs) = self.heap.get(obj) {
            set_member(attrs, name, value);
            return Ok(());
        }
        self.check_slot(obj, name)?;
        let key = self.heap.intern_str(name)?;
        let attrs = match self.heap.get(obj) {
            HeapObj::Instance(_, attrs) => attrs,
            _ => match self.heap.classed(obj) { Some((_, attrs)) => attrs, None => return Err(self.no_store(obj, name)) },
        };
        if member.is_some_and(|(m, _)| self.hides(m)) && !core::mem::replace(&mut self.shadowed, true) { self.class_epoch = self.class_epoch.wrapping_add(1); }
        let mut a = attrs.borrow_mut();
        let (shape, count) = (a.shape(), a.entry_count());
        a.insert(key, value, &self.heap);
        // A new key moves the dict along its shapes, a replaced value keeps the shape it had.
        if shape != crate::value::NO_SHAPE && a.entry_count() > count {
            drop(a);
            let next = self.shape_after(shape, key);
            if let Some(HeapObj::Instance(_, attrs)) = self.heap.try_get(obj) { attrs.borrow_mut().set_shape(next); }
        }
        Ok(())
    }
}

/* The hooks of a new class run as a job, the members told their name with `__set_name__`, the next of them, and whether its bases ran `__init_subclass__`. */
struct ClassHooks { cls: Val, keywords: Vec<Val>, members: Vec<(String, Val)>, next: usize, subclassed: bool, getting: Val }

impl VM<'_> {
    /* Opens the method a job's `Open` or `Bound` step names in place of its frame, carrying on at `ret` into `dest`, None once opened or the value a nested run gives. */
    #[inline(never)]
    fn job_bound(&mut self, step: super::jobs::Step, ret: usize, dest: u16) -> Result<Option<Val>, VmErr> {
        // The job goes with this frame's place on the jobs, which leaving the frame takes off.
        let job = self.jobs.last_mut().map(|j| core::mem::replace(j, super::jobs::Job::IterOf));
        self.leave_frame();
        let Some(job) = job else { return Err(cold_runtime("a job frame without its job")) };
        let opened = match step {
            super::jobs::Step::Open(g, class, x, y) => { if !self.open_new(g, x, &[y], &[], (class, x), job)? { return Err(cold_runtime("a job's next function opens no frame")); } true }
            super::jobs::Step::Bound(g, class, x) => { if !self.open_init(g, class, x, job)? { return Ok(Some(x)); } true }
            _ => false,
        };
        if opened { (self.entered, self.yielded) = (false, false); self.set_ret_apart(ret, dest); }
        Ok(None)
    }

    /* `del obj.name` at `rip` through a `__delattr__`, deleter or `__delete__` opened as a frame of the loop, its value dropped. */
    #[inline(never)]
    fn del_attr_at(&mut self, op: u16, rip: usize, chunk: &SSAChunk) -> Result<(), VmErr> {
        self.prop_at = Some((rip, super::act::DISCARD));
        let r = self.exec_del_attr(op, chunk);
        self.prop_at = None;
        r
    }

    /* A method call through the descriptor `desc`, its `__get__` and then what it gave run as frames of a job that gives the call's value. */
    #[inline(never)]
    fn desc_call(&mut self, desc: Val, recv: Val, owner: Val, pos: &[Val], kw: &[Val]) -> Result<(), VmErr> {
        let call = alloc::boxed::Box::new(DescCall { desc, recv, owner, pos: pos.to_vec(), kw: kw.to_vec(), got: false });
        if let Some(v) = self.calls_run(call, true)? { self.push(v); }
        Ok(())
    }
}

/* A method call through a descriptor, its `__get__` on the receiver and owner, the value it gives called on the arguments, the descriptor asked once `got`. */
struct DescCall { desc: Val, recv: Val, owner: Val, pos: Vec<Val>, kw: Vec<Val>, got: bool }

impl super::jobs::Calls for DescCall {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<super::jobs::Call, VmErr> {
        use super::jobs::Call;
        let Some(r) = got else {
            let hook = vm.instance_class(self.desc).and_then(|c| vm.lookup_class_member(c, "__get__"));
            return Ok(match hook {
                Some((func, defining)) => Call::Frame(func, defining, [self.desc, self.recv, self.owner]),
                None => Call::Give(vm.call_hook(self.desc, "__get__", &[self.recv, self.owner], vm.chunk)?),
            });
        };
        let v = r?;
        if self.got { return Ok(Call::Give(v)); }
        self.got = true;
        // What the descriptor gave is called on the arguments, as a frame when three positionals or one and keywords carry them.
        Ok(match (self.pos.as_slice(), self.kw.is_empty()) {
            (p, true) if p.len() <= 3 => { let mut a = [Val::undef(); 3]; a[..p.len()].copy_from_slice(p); Call::Frame(v, Val::undef(), a) }
            ([x], false) => Call::Named(v, Val::undef(), *x, self.kw.clone()),
            _ => { vm.call_with(v, None, &self.pos, &self.kw, vm.chunk)?; Call::Give(vm.pop()?) }
        })
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) { f(self.desc); f(self.recv); f(self.owner); for &v in self.pos.iter().chain(&self.kw) { f(v) } }
}

impl super::jobs::Calls for ClassHooks {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<super::jobs::Call, VmErr> {
        use super::jobs::Call;
        if let Some(Err(e)) = got { return Err(e); }
        let cls = self.cls;
        // A property's getter gave the hook, which runs next on the name kept.
        let name = core::mem::replace(&mut self.getting, Val::undef());
        if let Some(Ok(hook)) = got && !name.is_undef() { return Ok(Call::Frame(hook, Val::undef(), [cls, name, Val::undef()])); }
        while let Some(&(ref name, member)) = self.members.get(self.next) {
            self.next += 1;
            let Some((func, defining)) = vm.set_name_of(member) else { continue };
            let name = vm.heap.alloc(HeapObj::Str(name.clone()))?;
            match vm.heap.try_get(func) {
                Some(HeapObj::Func(..)) => return Ok(Call::Frame(func, defining, [member, cls, name])),
                Some(&HeapObj::Property(getter, ..)) if !getter.is_none() => { self.getting = name; return Ok(Call::Frame(getter, Val::undef(), [member, Val::undef(), Val::undef()])); }
                _ => {}
            }
            // Any other hook is what reading it off the member gives, a static or a class method unwrapped.
            let chunk = vm.chunk;
            let hook = vm.with_roots([name], |vm| { vm.load_attr(member, "__set_name__", chunk)?; vm.pop() })?;
            return Ok(Call::Frame(hook, Val::undef(), [cls, name, Val::undef()]));
        }
        if core::mem::replace(&mut self.subclassed, true) { return Ok(Call::Give(cls)); }
        // The hook comes from above the class, and a base without one takes no keywords.
        let Some((hook, defining)) = vm.lookup_class_member_after(cls, cls, "__init_subclass__") else {
            if self.keywords.is_empty() { return Ok(Call::Give(cls)); }
            let HeapObj::Class(name, ..) = vm.heap.get(cls) else { return Ok(Call::Give(cls)) };
            return Err(VmErr::TypeMsg(s!(str name, ".__init_subclass__() takes no keyword arguments")));
        };
        let hook = match vm.heap.try_get(hook) { Some(&HeapObj::ClassMethod(f)) => f, _ => hook };
        Ok(if self.keywords.is_empty() { Call::Frame(hook, defining, [cls, Val::undef(), Val::undef()]) } else { Call::Named(hook, defining, cls, self.keywords.clone()) })
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) {
        f(self.cls);
        for &v in &self.keywords { f(v) }
        for &(_, v) in &self.members { f(v) }
        f(self.getting);
    }
}
