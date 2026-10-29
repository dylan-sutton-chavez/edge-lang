use alloc::rc::Rc;
use core::cell::RefCell;
use crate::code::OpCode;
use crate::syntax::chunk::{SITE_FAST, SITE_PINNED, SITE_TABLE, SITE_VOID, SSAChunk};
use super::{VM, lower};
use crate::value::*;
use super::sites::Site;
use super::lower::Code;
use crate::code::Ins;
use super::scope::Kind;
use super::registers::{int_add, int_sub, int_mul, int_mod, int_div, int_floordiv, float_div, int_bits, arith_int, arith_float, ordered};
use super::frame::{Floor, chunk_in};
use super::slow::item::{index_of, dict_get, dict_set};
use super::slow::build::{bare_field, seq_items};
use super::exc::RAW_HANDLER;
use super::sections::{GROW, RELAY, note_alt};
use crate::lang::{Language, OpTable};
use crate::r#unsafe::{CodePtr, get_in, get_in_mut, index_in, known, past, peek, put, read, reg, turn_lone, untake_lone, word, words};

// back-edges a due preempt waits for the run to leave sections before parking in them
const PREEMPT_PATIENCE: u32 = 64;

/* the top yield `ins` of `code` parks with at `resume` or none over nothing */
#[inline(always)]
pub(crate) fn yield_top(code: &Code, ins: Ins, resume: usize, vm: &VM) -> u16 {
    match ins.x {
        2 => ins.a + 1,
        x if x & lower::KEPT != 0 => vm.parked_top(code, resume) as u16,
        _ => 0,
    }
}

/* whether the name of native `id` holds a binding by the bits `rebound` keeps */
#[inline(always)]
pub(crate) fn rebound_bit(rebound: &[u64], id: u16) -> bool { rebound.get(id as usize / 64).is_some_and(|w| w & 1 << (id % 64) != 0) }

// the rebound state while no name an opcode fuses is bound and while one is
const OPS_FREE: u8 = 1;
const OPS_BOUND: u8 = 2;
/* the state a rebinding leaves so the next runtime opcode asks `ops_bound` again */
pub(crate) const OPS_STALE: u8 = 3;

/* whether a name an opcode of `L` fuses is bound which `state` keeps between rebindings */
#[cold]
#[inline(never)]
fn ops_bound<L: Language>(state: &mut u8, rebound: &[u64], natives: &crate::lang::Natives) -> bool {
    let bound = <L::Ops as OpTable>::SET.ops.iter().filter_map(|o| o.fuse.and_then(|f| natives.function_id(f))).any(|id| rebound_bit(rebound, id.0));
    *state = if bound { OPS_BOUND } else { OPS_FREE };
    bound
}

/* the module binding a `CallGlobalR` calls which is read outside the register loop */
#[inline(never)]
pub(crate) fn global_callee(scopes: &[super::scope::Globals], code: &Code, ins: Ins) -> Val { scopes[code.module].read(ins.a as u32) }

/* whether every section still matches its binding noted so the next call checks none */
#[cold]
#[inline(never)]
pub(crate) fn sections_hold(scopes: &[super::scope::Globals], block: &lower::Block) -> bool {
    let now = super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed);
    // method sections bind nothing and check receivers per call and builtins what their name finds
    let hold = block.sections.iter().all(|s| s.global == u16::MAX || scopes.get(s.bound).is_some_and(|g| g.read(s.global as u32).0 == s.value.get().0));
    if hold { block.binds.set(now); }
    hold
}

/* whether section `s` still finds the init it was laid from noted per class version */
#[cold]
#[inline(never)]
fn ctor_holds(heap: &crate::value::HeapPool, mro: &crate::util::hash::FxHashMap<u64, Rc<alloc::vec::Vec<Val>>>, s: &lower::Section, epoch: u32) -> bool {
    let Some((at, _, init)) = &s.ctor else { return false };
    let hold = crate::vm::slow::attr::class_member(heap, mro, s.value.get(), heap.names().constructor).is_some_and(|(f, _)| f.0 == init.0);
    if hold { at.set(epoch); }
    hold
}

/* a fresh `class` instance with room for `attrs` attributes or none past the memory limit */
#[cold]
#[inline(never)]
fn new_object(heap: &mut crate::value::HeapPool, class: Val, attrs: usize) -> Option<Val> {
    let mut dict = crate::value::DictMap::with_capacity(attrs);
    dict.set_shape(super::sites::ROOT_SHAPE);
    heap.new_instance(class, dict).ok()
}

/* marks instruction `rip` of `code` as one `chunk` ran */
#[cfg(feature = "coverage")]
#[inline(never)]
fn mark_ran(executed: &mut crate::util::hash::FxHashMap<*const SSAChunk, alloc::vec::Vec<u64>>, code: &Code, chunk: &SSAChunk, rip: usize) {
    let at = code.orig(rip) as usize;
    let bits = executed.entry(chunk as *const _).or_insert_with(|| alloc::vec![0; chunk.instructions.len().div_ceil(64)]);
    if let Some(word) = bits.get_mut(at / 64) { *word |= 1 << (at % 64); }
}

/* what a call site holds as far as the register loop's call needs to know */
#[derive(Clone, Copy)]
enum Known { Call(Val, u32), Ctor(Val), Method, Other }

/* the VM's `opens_in_loop` read off fields alone so the loop keeps its frame borrowed */
#[inline(always)]
fn opens_in_loop(facts: &[super::act::CallFacts], is_async: &[bool], pools: &[super::cache::CachePool], fi: usize, n: usize) -> bool {
    let f = &facts[fi];
    f.in_loop && f.arity.is_some_and(|k| n <= k && n >= f.required as usize) && !is_async.get(fi).copied().unwrap_or(false)
        && pools[f.pool].code.is_some() && !pools[f.pool].cold && pools[f.pool].consts.is_some()
}

impl<'a> VM<'a> {

    /* runs from `ip` to its driver's next turn and is true once it switched frames */
    #[inline(always)]
    pub(crate) fn run_regs<'c, L: Language>(&mut self, code_at: &mut &'c Code, chunk_at: &mut &'c SSAChunk, ip: &mut usize, floor: &Floor) -> bool {
        let (mut code, mut chunk, mut switched) = (*code_at, *chunk_at, false);
        let mut cache = &code.cache;
        // the running code as a raw slice of the Vec whose provenance reaches the sentinel
        macro_rules! whole { () => { core::ptr::slice_from_raw_parts(code.ins.as_ptr(), code.ins.as_slice().len()) } }
        let mut prog: *const [Ins] = whole!();
        let mut n = prog.len();
        debug_assert!(*ip <= n);
        // callers keep `*ip` within the length so `p` is in `prog` or the sentinel
        let mut p: *const Ins = past(prog.cast(), *ip);
        // the index of the running instruction
        macro_rules! idx { () => { index_in(prog, p) } }
        // carries on at `$k` which jumps and calls and returns keep at most `prog`'s length
        macro_rules! goto { ($k:expr) => {{ let k = $k; debug_assert!(k <= n); p = past(prog.cast(), k); }} }
        let mut base = self.base;
        // the frame as a local slice retaken only after calls that may grow the stack
        let mut r = &mut self.regs[base..];
        // registers read unchecked as all lie below `code.frame` per `reg_bound` and `grow_frame` keeps that length
        debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
        macro_rules! reg { ($k:expr) => { reg(r, $k as usize) } }
        macro_rules! set { ($k:expr, $v:expr) => {{ let v = $v; put(r, $k as usize, v); }} }
        // code over rows keeps its depth here while the loop runs it
        let (mut s0, mut sp) = (0usize, usize::MAX);
        // the first row and depth of the running code over rows read once
        macro_rules! over {
            () => {{
                if sp == usize::MAX {
                    debug_assert!(code.rows != 0, "an op over rows in lowered code");
                    s0 = code.rows as usize;
                    sp = (self.top as usize).wrapping_sub(s0);
                    debug_assert!(sp <= code.frame - s0, "a depth past the rows of the frame");
                }
            }};
        }
        // the running word's `a` read alone so the loop head never splits a whole word
        macro_rules! arg { () => { read(p, |w| &w.a) } }
        // pushes onto the rows and runs on past the word
        macro_rules! push {
            ($v:expr) => {{
                let v = $v;
                if s0 + sp >= code.frame { sync!(); self.reg_error = Some(cold_runtime("a stack deeper than the rows of its frame")); break; }
                set!(s0 + sp, v);
                sp += 1;
                p = past(p, 1);
            }};
        }
        // the top of code over rows written back for code past the loop
        macro_rules! sync { () => { self.top = (s0 + sp) as u32; } }
        // the window taken back past a call out of line with the depth read later
        macro_rules! refill { () => {{ base = self.base; r = &mut self.regs[base..]; sp = usize::MAX; }} }
        // a value op its arm leaves runs out of line and steps past its word
        macro_rules! valued {
            () => {{
                sync!();
                let ran = self.value_rows(idx!(), code, chunk);
                refill!();
                if !ran { break; }
                p = past(p, 1);
                if self.yielded { break; }
            }};
        }
        // a binary op over the top two rows with its int and float forms inline
        macro_rules! binary {
            ($int:expr, $float:expr) => {{
                over!();
                debug_assert!(sp >= 2);
                let (x, y) = (reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                let v = if x.is_int() && y.is_int() { $int(x.as_int(), y.as_int()) }
                    else if x.is_float() && y.is_float() { $float(x.as_float(), y.as_float()) }
                    else { None };
                match v {
                    Some(v) => { sp -= 1; set!(s0 + sp - 1, v); p = past(p, 1); }
                    None => valued!(),
                }
            }};
        }
        // a binary op whose strings join or repeat at once as the stack forms do
        macro_rules! joining {
            ($repeat:expr, $int:expr, $float:expr) => {{
                over!();
                debug_assert!(sp >= 2);
                let (x, y) = (reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                let v = if x.is_int() && y.is_int() { $int(x.as_int(), y.as_int()) }
                    else if x.is_float() && y.is_float() { $float(x.as_float(), y.as_float()) }
                    else if x.is_heap() { let v = self.joined($repeat, x, y); r = &mut self.regs[base..]; v }
                    else { None };
                match v {
                    Some(v) => { sp -= 1; set!(s0 + sp - 1, v); p = past(p, 1); }
                    None => valued!(),
                }
            }};
        }
        // two equal texts compare their bytes inline as the register forms do
        macro_rules! equal {
            ($same:expr) => {{
                over!();
                debug_assert!(sp >= 2);
                let (x, y) = (reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                let eq = if x.is_int() && y.is_int() { Some(x.as_int() == y.as_int()) }
                    else if x.is_float() && y.is_float() { Some(x.as_float() == y.as_float()) }
                    else { self.heap.str_eq(x, y) };
                match eq {
                    Some(eq) => { sp -= 1; set!(s0 + sp - 1, Val::bool(eq == $same)); p = past(p, 1); }
                    None => valued!(),
                }
            }};
        }
        // a comparison over the top two rows with ints and floats compared inline
        macro_rules! ordered {
            ($cmp:tt) => {{ binary!(|a: i64, b: i64| Some(Val::bool(a $cmp b)), |a: f64, b: f64| Some(Val::bool(a $cmp b))) }};
        }
        // a branch over rows taking plain truths inline as the loop's own branches do
        macro_rules! tested {
            ($op:expr) => {{
                over!();
                debug_assert!(sp >= 1);
                let (v, to) = (reg!(s0 + sp - 1), arg!() as usize);
                let f = L::POLICY.falsy;
                let t = if v.is_bool() { Some(f.bool_truth(v.as_bool())) } else if v.is_int() { Some(f.int_truth(v.as_int())) } else if v.is_none() { Some(f.nil_truth()) }
                    else { match self.heap.try_get(v) { Some(HeapObj::Str(s)) => Some(f.text_truth(s.is_empty())), _ => None } };
                match t {
                    Some(t) if t != ($op == OpCode::JumpIfTrueOrPop) => { sp -= 1; p = past(p, 1); }
                    Some(_) if self.budget > 0 && to <= n => {
                        self.budget -= 1;
                        if $op == OpCode::JumpIfFalse { sp -= 1; }
                        goto!(to);
                    }
                    _ => {
                        sync!();
                        let went = self.test_rows($op, to, n);
                        refill!();
                        match went {
                            Ok(true) => goto!(to),
                            Ok(false) => p = past(p, 1),
                            Err(e) => { self.reg_error = Some(e); break }
                        }
                    }
                }
            }};
        }
        // a pair past numbers leaves unless a text or a mixed number answers apart
        macro_rules! miss {
            ($x:expr, $miss:lifetime) => {{
                if $x.is_heap() && !matches!(self.heap.try_get($x), Some(HeapObj::Str(_))) { break }
                break $miss true
            }};
        }
        // the generated arm of an arithmetic form through the kernels of its stack op
        macro_rules! arith {
            ($ins:ident, $op:expr, $miss:lifetime) => {{
                let (x, y) = (reg!($ins.b), reg!($ins.c));
                let v = if x.is_int() && y.is_int() { arith_int($op, x.as_int(), y.as_int(), &L::POLICY) }
                    else if x.is_float() && y.is_float() { arith_float($op, x.as_float(), y.as_float(), &L::POLICY) }
                    else { None };
                let Some(v) = v else { miss!(x, $miss) };
                set!($ins.a, v);
                p = past(p, 1);
            }};
        }
        // integer bit operations stay in the loop and anything else leaves for the handler
        macro_rules! bits {
            ($ins:ident, $miss:lifetime) => {{
                let (x, y) = (reg!($ins.b), reg!($ins.c));
                let v = if x.is_int() && y.is_int() { int_bits($ins.op, x.as_int(), y.as_int()) } else { None };
                let Some(v) = v else { miss!(x, $miss) };
                set!($ins.a, v);
                p = past(p, 1);
            }};
        }
        macro_rules! compare {
            ($ins:ident, $op:expr) => {{
                let (x, y) = (reg!($ins.b), reg!($ins.c));
                let v = if x.is_int() && y.is_int() { ordered($op, x.as_int(), y.as_int()) }
                    else if x.is_float() && y.is_float() { ordered($op, x.as_float(), y.as_float()) }
                    else { break };
                set!($ins.a, Val::bool(v));
                p = past(p, 1);
            }};
        }
        // a taken branch costs one step of the budget
        macro_rules! branch {
            ($ins:ident, $holds:expr) => {{
                if $holds { p = past(p, 1); }
                else if self.budget > 0 && ($ins.a as usize) <= n { self.budget -= 1; goto!($ins.a as usize); }
                else { break; }
            }};
        }
        macro_rules! test {
            ($ins:ident, $op:expr) => {{
                let (x, y) = (reg!($ins.b), reg!($ins.c));
                let holds = if x.is_int() && y.is_int() { ordered($op, x.as_int(), y.as_int()) }
                    else if x.is_float() && y.is_float() { ordered($op, x.as_float(), y.as_float()) }
                    else { break };
                branch!($ins, holds)
            }};
        }
        macro_rules! moved {
            ($ins:ident) => {{
                let v = reg!($ins.b);
                if v.is_undef() { break; }
                set!($ins.a, v);
                p = past(p, 1);
            }};
        }
        // a call through the site to `$callee` which a method call never reads
        macro_rules! call {
            ($ins:ident, $callee:expr) => {{
                // the site read in place for only the few fields a call needs
                let i = idx!();
                // a section's method call names its method in the chunk its section was laid from
                let named = if $ins.op == OpCode::CallMethodR && code.block.is_some() { chunk_in(&self.functions, code, chunk, i) } else { chunk };
                let known = cache.with_site(i, |site| match *site {
                    Site::Call { callee, fi } => Known::Call(callee, fi),
                    Site::Ctor { class, .. } => Known::Ctor(Val::heap(class)),
                    Site::Method { .. } => Known::Method,
                    _ => Known::Other,
                });
                // calls meeting unlaid functions ask for a head first and stubs for their own head
                if code.block.is_some() && ($ins.op == OpCode::CallR || $ins.op == OpCode::CallGlobalR) && self.budget != 0 && let c = $callee && match self.heap.try_get(c) { Some(HeapObj::Func { .. }) => true, Some(HeapObj::Object { meta, .. }) => meta.is_class_meta(), Some(&HeapObj::Bound { func, .. }) => func.is_heap() && $ins.op == OpCode::CallR, _ => false } {
                    let grows = self.site_grows(code, chunk, i, c);
                    r = &mut self.regs[base..];
                    if grows { break; }
                }
                // stubs outside sections count met functions toward laying them beside the first except recursion
                if ($ins.op == OpCode::CallR || $ins.op == OpCode::CallGlobalR && code.block.is_some()) && code.misses.get() < lower::MISSES && !code.unchanged()
                    && !matches!(known, Known::Call(c, fi) if c.0 == $callee.0 && self.functions.get(fi as usize).is_some_and(|f| core::ptr::eq(&f.1, chunk))) { note_alt(&self.heap, &mut self.learned, &self.bodies, &self.functions, &mut self.preempt_left, code, chunk, i, $callee, matches!(known, Known::Call(c, _) if c.0 == $callee.0)); }
                let opened = match known {
                    Known::Call(callee, fi) if $ins.op != OpCode::CallMethodR && $callee.0 == callee.0 => {
                        if lower::wide($ins) { self.enter_wide(i, code, chunk, fi as usize, callee) }
                        else { let args = [reg!($ins.b), reg!($ins.c)]; self.enter_block(i, code, fi as usize, callee, &args[..$ins.x as usize], false) }
                    }
                    // another function than the site's opens alike with its body read off the function itself
                    Known::Call(..) if $ins.op != OpCode::CallMethodR && let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get($callee) && opens_in_loop(&self.call_facts, &self.is_async, &self.pools, fi, $ins.x as usize) => {
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
                        (code, chunk) = (c.get(), k.get());
                        (cache, prog, switched) = (&code.cache, whole!(), true);
                        n = prog.len();
                        // the callee starts at its first row as its record keeps the caller's top
                        (self.top, sp) = (u32::from(code.rows), usize::MAX);
                        goto!(code.lowered(0));
                    }
                    None => match self.call_reg($ins, i, code, named) {
                        Some(next) => { goto!(next); if self.yielded { break; } }
                        None => break,
                    },
                }
                base = self.base; r = &mut self.regs[base..];
                debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
            }};
        }
        'regs: loop {
        let missed = 'miss: {
        loop {
            // `p` lies in `prog` or on the sentinel and the Vec's provenance covers both
            let ins = *word(p);
            // coverage builds mark each instruction the loop meets
            #[cfg(feature = "coverage")]
            if idx!() < n { mark_ran(&mut self.executed, code, chunk, idx!()); }
            // debug builds hold code over rows to the depth its chunk settles at each instruction
            #[cfg(debug_assertions)]
            if code.rows != 0 && idx!() < n { let d = (self.top as usize).wrapping_sub(code.rows as usize); debug_assert_eq!(if sp == usize::MAX { d } else { sp }, code.depths[idx!()] as usize, "the depth of code over rows at {}", idx!()); }
            // the forms the table generates an arm for go between the arms written here
            macro_rules! loop_arms {
                (arith [$(($a:ident $as:ident))*] order [$(($o:ident $os:ident))*] test [$(($t:ident $ts:ident))*] bits [$(($b:ident $bs:tt))*] move [$(($v:ident $vs:tt))*]
                    { $miss:lifetime { $($before:tt)* } { $($equal:tt)* } { $($same:tt)* } { $($truth:tt)* } { $($after:tt)* } }) => {
                    match read(p, |w| &w.op) {
                        $($before)*
                        $( OpCode::$a => arith!(ins, OpCode::$as, $miss), )*
                        $( OpCode::$b )|* => bits!(ins, $miss),
                        $($equal)*
                        $( OpCode::$o => compare!(ins, OpCode::$os), )*
                        $($same)*
                        $( OpCode::$t => test!(ins, OpCode::$ts), )*
                        $($truth)*
                        $( OpCode::$v )|* => moved!(ins),
                        $($after)*
                    }
                };
            }
            crate::code::simple!(loop_arms { 'miss {
                // values over rows start with a constant read where its pool keeps it
                OpCode::LoadConst => {
                    over!();
                    let k = arg!() as usize;
                    debug_assert!(k < chunk.constants.len());
                    push!(code.consts.get(k))
                }
                // a local or module binding reads in place and other names out of line
                OpCode::LoadName => {
                    over!();
                    let k = arg!() as usize;
                    let v = match code.kinds.get(k) {
                        Some(&Kind::Global(g)) => {
                            let scope = get_in(&self.scopes, code.module);
                            match scope.at(g) { v if v.is_undef() => scope.fallback(g), v => v }
                        }
                        Some(Kind::Local) if k < s0 => reg!(k),
                        _ => Val::undef(),
                    };
                    if v.is_undef() { valued!() } else { push!(v) }
                }
                // a local or a module binding no builtin names takes the top row in place
                OpCode::StoreName => {
                    over!();
                    let (k, v) = (arg!() as usize, reg!(s0 + sp - 1));
                    match code.kinds.get(k) {
                        Some(Kind::Local) if k < s0 => { set!(k, v); sp -= 1; p = past(p, 1); }
                        Some(&Kind::Global(g)) if get_in(&self.scopes, code.module).names_native(g).is_none() => {
                            get_in_mut(&mut self.scopes, code.module).set_at(g, v);
                            sp -= 1;
                            p = past(p, 1);
                        }
                        _ => valued!(),
                    }
                }
                // the constants every frame shares push at once
                OpCode::LoadTrue => { over!(); push!(Val::bool(true)) }
                OpCode::LoadFalse => { over!(); push!(Val::bool(false)) }
                OpCode::LoadNone => { over!(); push!(Val::none()) }
                // the top rows move in place
                OpCode::PopTop => { over!(); debug_assert!(sp >= 1); sp -= 1; p = past(p, 1); }
                OpCode::Dup => { over!(); push!(reg!(s0 + sp - 1)) }
                OpCode::Dup2 => {
                    over!();
                    if s0 + sp + 2 > code.frame { sync!(); self.reg_error = Some(cold_runtime("a stack deeper than the rows of its frame")); break; }
                    let (a, b) = (reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                    set!(s0 + sp, a);
                    set!(s0 + sp + 1, b);
                    sp += 2;
                    p = past(p, 1);
                }
                OpCode::Swap => {
                    over!();
                    let (a, b) = (reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                    set!(s0 + sp - 2, b);
                    set!(s0 + sp - 1, a);
                    p = past(p, 1);
                }
                OpCode::Rot3 => {
                    over!();
                    let (a, b, c) = (reg!(s0 + sp - 3), reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                    set!(s0 + sp - 3, b);
                    set!(s0 + sp - 2, c);
                    set!(s0 + sp - 1, a);
                    p = past(p, 1);
                }
                // two ints or two floats compute in place as the register forms do
                OpCode::Add | OpCode::InPlaceAdd => joining!(false, int_add, |a: f64, b: f64| Some(Val::float(a + b))),
                OpCode::Sub | OpCode::InPlaceSub => binary!(int_sub, |a: f64, b: f64| Some(Val::float(a - b))),
                OpCode::Mul => joining!(true, int_mul, |a: f64, b: f64| Some(Val::float(a * b))),
                OpCode::Div => binary!(|a, b| int_div(a, b, &L::POLICY), |a, b| float_div(a, b, &L::POLICY)),
                OpCode::Mod => binary!(|a, b| int_mod(a, b, &L::POLICY), |_, _| None),
                OpCode::FloorDiv => binary!(|a, b| int_floordiv(a, b, &L::POLICY), |_, _| None),
                OpCode::BitAnd | OpCode::InPlaceBitAnd => binary!(|a, b| int_bits(OpCode::BitAndR, a, b), |_, _| None),
                OpCode::BitOr | OpCode::InPlaceBitOr => binary!(|a, b| int_bits(OpCode::BitOrR, a, b), |_, _| None),
                OpCode::BitXor | OpCode::InPlaceBitXor => binary!(|a, b| int_bits(OpCode::BitXorR, a, b), |_, _| None),
                OpCode::Shl => binary!(|a, b| int_bits(OpCode::ShlR, a, b), |_, _| None),
                OpCode::Shr => binary!(|a, b| int_bits(OpCode::ShrR, a, b), |_, _| None),
                OpCode::Eq => equal!(true),
                OpCode::NotEq => equal!(false),
                OpCode::Lt => ordered!(<),
                OpCode::LtEq => ordered!(<=),
                OpCode::Gt => ordered!(>),
                OpCode::GtEq => ordered!(>=),
                // identity compares the two rows' bits
                OpCode::Is => { over!(); let v = Val::bool(reg!(s0 + sp - 2).0 == reg!(s0 + sp - 1).0); sp -= 1; set!(s0 + sp - 1, v); p = past(p, 1); }
                OpCode::IsNot => { over!(); let v = Val::bool(reg!(s0 + sp - 2).0 != reg!(s0 + sp - 1).0); sp -= 1; set!(s0 + sp - 1, v); p = past(p, 1); }
                // a bool an int or None negates in place
                OpCode::Not => {
                    over!();
                    let x = reg!(s0 + sp - 1);
                    let f = L::POLICY.falsy;
                    let t = if x.is_bool() { Some(f.bool_truth(x.as_bool())) } else if x.is_int() { Some(f.int_truth(x.as_int())) } else if x.is_none() { Some(f.nil_truth()) } else { None };
                    match t {
                        Some(t) => { set!(s0 + sp - 1, Val::bool(!t)); p = past(p, 1); }
                        None => valued!(),
                    }
                }
                OpCode::Minus => {
                    over!();
                    let x = reg!(s0 + sp - 1);
                    let v = if x.is_int() { Val::int_checked(-x.as_int()) } else if x.is_float() { Some(Val::float(-x.as_float())) } else { None };
                    match v {
                        Some(v) => { set!(s0 + sp - 1, v); p = past(p, 1); }
                        None => valued!(),
                    }
                }
                // a true value or a nonzero int passes at once
                OpCode::Assert => {
                    over!();
                    let x = reg!(s0 + sp - 1);
                    if x.is_true() || x.is_int() && L::POLICY.falsy.int_truth(x.as_int()) { sp -= 1; p = past(p, 1); } else { valued!() }
                }
                // the rarer value ops run out of line
                OpCode::LoadGlobal | OpCode::StoreGlobal | OpCode::LoadEllipsis | OpCode::LoadModule
                | OpCode::Pow | OpCode::MatMul | OpCode::In | OpCode::NotIn | OpCode::Pos | OpCode::BitNot | OpCode::Len | OpCode::ListToTuple | OpCode::MatchExc => { over!(); debug_assert!(s0 + sp <= r.len()); valued!() }
                // containers over rows start with a tuple built of the top rows
                OpCode::BuildTuple => {
                    over!();
                    let k = arg!() as usize;
                    debug_assert!(sp >= k);
                    let items = r[s0 + sp - k..s0 + sp].to_vec();
                    match self.heap.alloc(HeapObj::Tuple(items)) {
                        Ok(v) => { sp -= k; push!(v) }
                        Err(e) => { sync!(); self.reg_error = Some(e); break }
                    }
                }
                // a list of the top rows as the tuple above
                OpCode::BuildList => {
                    over!();
                    let k = arg!() as usize;
                    debug_assert!(sp >= k);
                    let items = r[s0 + sp - k..s0 + sp].to_vec();
                    match self.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items)))) {
                        Ok(v) => { sp -= k; push!(v) }
                        Err(e) => { sync!(); self.reg_error = Some(e); break }
                    }
                }
                // strings sets dicts and slices of rows that need no check build in place
                OpCode::BuildString | OpCode::BuildSet | OpCode::BuildDict | OpCode::BuildSlice => {
                    over!();
                    let op = read(p, |w| &w.op);
                    let k = arg!() as usize * if op == OpCode::BuildDict { 2 } else { 1 };
                    debug_assert!(sp >= k && s0 + sp <= r.len());
                    match self.build_reg(Ins { op: OpCode::BuildR, x: op as u8, a: 0, b: (s0 + sp - k) as u16, c: k as u16 }) {
                        Some(v) => { r = &mut self.regs[base..]; sp -= k; push!(v) }
                        None => {
                            sync!();
                            let ran = self.held_rows(idx!(), code, chunk);
                            refill!();
                            if !ran { break; }
                            p = past(p, 1);
                        }
                    }
                }
                // a bare field stays in its row for the string that holds it to render
                OpCode::FormatValue => {
                    over!();
                    if arg!() == 0 && bare_field(&self.heap, reg!(s0 + sp - 1)) { p = past(p, 1); continue; }
                    sync!();
                    let ran = self.held_rows(idx!(), code, chunk);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                }
                // list tuple and plain dict items read in place and strings out of line
                OpCode::GetItem => {
                    over!();
                    let (o, k) = (reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                    let hit = match self.heap.try_get(o) {
                        Some(HeapObj::List(v)) if k.is_int() => v.try_borrow().ok().and_then(|b| index_of(k.as_int(), b.len(), L::POLICY.neg_index).map(|j| b[j])),
                        Some(HeapObj::Tuple(v)) if k.is_int() => index_of(k.as_int(), v.len(), L::POLICY.neg_index).map(|j| v[j]),
                        Some(HeapObj::Dict(d)) => dict_get(&self.heap, d, k),
                        _ => None,
                    };
                    if let Some(v) = hit { sp -= 1; set!(s0 + sp - 1, v); p = past(p, 1); continue; }
                    sync!();
                    let ran = self.held_rows(idx!(), code, chunk);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                }
                // an item of a list or a plain key of a dict stores in place
                OpCode::StoreItem => {
                    over!();
                    let (o, k, v) = (reg!(s0 + sp - 3), reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                    let done = match self.heap.try_get(o) {
                        Some(HeapObj::List(l)) if k.is_int() => match l.try_borrow_mut() {
                            Ok(mut b) => match index_of(k.as_int(), b.len(), L::POLICY.neg_index) { Some(j) => { b[j] = v; true } None => false },
                            Err(_) => false,
                        },
                        Some(HeapObj::Dict(d)) => dict_set(&self.heap, d, k, v),
                        _ => false,
                    };
                    if done { sp -= 3; p = past(p, 1); continue; }
                    sync!();
                    let ran = self.held_rows(idx!(), code, chunk);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                    if self.yielded { break; }
                }
                // an attribute the site knows reads in place
                OpCode::LoadAttr => {
                    over!();
                    let o = reg!(s0 + sp - 1);
                    let (heap, epoch) = (&self.heap, self.layout_epoch);
                    if let Some(v) = cache.with_site(idx!(), |site| super::sites::site_value(heap, epoch, site, o)) { set!(s0 + sp - 1, v); p = past(p, 1); continue; }
                    sync!();
                    let ran = self.held_rows(idx!(), code, chunk);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                }
                // an attribute the site knows stores in place
                OpCode::StoreAttr => {
                    over!();
                    let (o, v) = (reg!(s0 + sp - 2), reg!(s0 + sp - 1));
                    let (heap, epoch) = (&self.heap, self.layout_epoch);
                    if cache.with_site(idx!(), |site| super::sites::site_put(heap, epoch, site, o, v)) { sp -= 2; p = past(p, 1); continue; }
                    sync!();
                    let ran = self.held_rows(idx!(), code, chunk);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                }
                // a list accumulator takes its item in place
                OpCode::ListAppend => {
                    over!();
                    if let Some(HeapObj::List(l)) = self.heap.try_get(reg!(s0 + sp - 2)) { l.borrow_mut().push(reg!(s0 + sp - 1)); sp -= 1; p = past(p, 1); continue; }
                    sync!();
                    let ran = self.held_rows(idx!(), code, chunk);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                }
                // a tuple or list of the right length spreads over its row and those above
                OpCode::UnpackSequence => {
                    over!();
                    let (n, at) = (arg!() as usize, s0 + sp - 1);
                    let fits = at + n <= code.frame && seq_items(&self.heap, reg!(at), |items| {
                        if items.len() != n { return false; }
                        for (j, &v) in items.iter().rev().enumerate() { r[at + j] = v; }
                        true
                    }) == Some(true);
                    if fits { sp = sp - 1 + n; p = past(p, 1); continue; }
                    sync!();
                    let ran = self.held_rows(idx!(), code, chunk);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                    if self.yielded { break; }
                }
                // the rarer container ops run out of line over the rows
                OpCode::SetAdd | OpCode::MapAdd | OpCode::ListExtend | OpCode::SetUpdate | OpCode::DictUpdate | OpCode::UnpackEx
                | OpCode::DelItem | OpCode::DelAttr | OpCode::MatchSeq | OpCode::MatchMap | OpCode::MatchClass | OpCode::Cut | OpCode::DelCut => {
                    if sp != usize::MAX { sync!(); }
                    let ran = self.held_rows(idx!(), code, chunk);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                    if self.yielded { break; }
                }
                // calls over rows open plain functions in place and run any other out of line
                OpCode::Call => {
                    over!();
                    let (op, i) = (arg!(), idx!());
                    let k = (op & 0xFF) as usize + 2 * (op >> 8) as usize;
                    debug_assert!(sp > k);
                    let callee = reg!(s0 + sp - k - 1);
                    if op <= 5 && let Some(&HeapObj::Func { fi, .. }) = self.heap.try_get(callee) && self.call_facts[fi].plain && opens_in_loop(&self.call_facts, &self.is_async, &self.pools, fi, k)
                        && let Some((c, ch)) = self.enter_rows(i, s0 + sp, k, fi, callee, code) {
                        (code, chunk) = (c.get(), ch.get());
                        (cache, prog, switched) = (&code.cache, whole!(), true);
                        n = prog.len();
                        sp = usize::MAX;
                        base = self.base; r = &mut self.regs[base..];
                        goto!(code.lowered(0));
                        continue;
                    }
                    sync!();
                    let next = self.call_over(ins, i, code, chunk);
                    refill!();
                    match next {
                        Some(next) => { goto!(next); if self.yielded { break; } }
                        None => break,
                    }
                }
                // a return over rows hands back its top row or None with no rows
                OpCode::ReturnValue => {
                    over!();
                    let v = if sp == 0 { Val::none() } else { reg!(s0 + sp - 1) };
                    if self.acts <= floor.depth { sync!(); break; }
                    let Some((c, k, next)) = self.leave_rows(v, floor) else { sync!(); break };
                    (code, chunk) = (c.get(), k.get());
                    (cache, prog, switched) = (&code.cache, whole!(), true);
                    n = prog.len();
                    sp = usize::MAX;
                    base = self.base; r = &mut self.regs[base..];
                    goto!(next);
                }
                // the other calls and makers over rows run out of line from their word
                OpCode::CallMethod | OpCode::CallMethodArgs | OpCode::BeginArgs | OpCode::CallExtern | OpCode::MakeFunction | OpCode::MakeCoroutine | OpCode::MakeClass
                | OpCode::UnpackArgs | OpCode::CallSpread => {
                    if sp != usize::MAX { sync!(); }
                    let next = self.calls_rows(idx!(), code, chunk);
                    refill!();
                    match next {
                        Some(next) => { goto!(next); if self.yielded { break; } }
                        None => break,
                    }
                }
                // natives over rows take their arguments off the rows and leave their value there
                OpCode::CallNative => {
                    let w = read(p, |w| w);
                    let (id, k) = crate::syntax::chunk::site_in_place(w.a);
                    // a heap op answers one or two bare rows unless a binding redirects it
                    if w.a & (SITE_TABLE | SITE_FAST) == SITE_FAST && (w.a & SITE_PINNED != 0 || self.any_rebound == 0 || !rebound_bit(&self.rebound, id)) {
                        over!();
                        let fast = match (k, self.natives.get(crate::lang::NativeId(id))) {
                            (1, Some(&crate::lang::Native { fast1: Some(f), .. })) => f(&mut self.heap, reg!(s0 + sp - 1)),
                            (2, Some(&crate::lang::Native { fast2: Some(f), .. })) => f(&mut self.heap, reg!(s0 + sp - 2), reg!(s0 + sp - 1)),
                            _ => None,
                        };
                        if let Some(v) = fast {
                            if self.heap.has_work() {
                                sync!();
                                if let Err(e) = self.pay_work() { self.reg_error = Some(e); break; }
                                r = &mut self.regs[base..];
                            }
                            if w.a & SITE_VOID != 0 { sp -= k; p = past(p, 1); }
                            else if lower::drops(Some(word(past(p, 1)))) { sp -= k; p = past(p, 2); } else { sp = sp + 1 - k; set!(s0 + sp - 1, v); p = past(p, 1); }
                            continue;
                        }
                    }
                    if sp != usize::MAX { sync!(); }
                    let went = self.native_rows(idx!(), code, chunk);
                    refill!();
                    match went {
                        Ok(step) => p = past(p, step),
                        Err(e) => { self.reg_error = Some(e); break; }
                    }
                    if self.yielded { break; }
                }
                // a runtime's own opcode answers off the top rows what its fast path takes
                OpCode::RuntimeOp if <L::Ops as OpTable>::COUNT != 0 => {
                    over!();
                    let (op, k) = (arg!() as u8, usize::from(arg!() >> 8));
                    // only rebinding a name an opcode fuses sends every op to its slow path
                    let bound = self.any_rebound > OPS_FREE && (self.any_rebound == OPS_BOUND || ops_bound::<L>(&mut self.any_rebound, &self.rebound, self.natives));
                    let v = if bound || sp < k { None } else { r.get(s0 + sp - k..s0 + sp).and_then(|args| <L::Ops as OpTable>::fast(op, args, &mut self.heap)) };
                    match v {
                        Some(v) => {
                            if self.heap.has_work() {
                                sync!();
                                if let Err(e) = self.pay_work() { self.reg_error = Some(e); break; }
                                r = &mut self.regs[base..];
                            }
                            sp -= k;
                            push!(v)
                        }
                        None => valued!(),
                    }
                }
            } {
                // two strings compare their text here and any other non-number pair leaves for the handler
                OpCode::EqR | OpCode::NotEqR => {
                    let (x, y) = (reg!(ins.b), reg!(ins.c));
                    let eq = if x.is_int() && y.is_int() { x.as_int() == y.as_int() }
                        else if x.is_float() && y.is_float() { x.as_float() == y.as_float() }
                        else { match self.heap.str_eq(x, y) { Some(e) => e, None => break } };
                    set!(ins.a, Val::bool(eq == (ins.op == OpCode::EqR)));
                    p = past(p, 1);
                }
            } {
                OpCode::JumpUnlessEq | OpCode::JumpUnlessNotEq => {
                    let (x, y) = (reg!(ins.b), reg!(ins.c));
                    let eq = if x.is_int() && y.is_int() { x.as_int() == y.as_int() }
                        else if x.is_float() && y.is_float() { x.as_float() == y.as_float() }
                        else { match self.heap.str_eq(x, y) { Some(e) => e, None => break } };
                    branch!(ins, eq == (ins.op == OpCode::JumpUnlessEq))
                }
            } {
                // `x` set jumps on a true value instead
                OpCode::JumpIfFalseR => {
                    let v = reg!(ins.b);
                    let holds = if v.is_bool() { L::POLICY.falsy.bool_truth(v.as_bool()) } else if v.is_int() { L::POLICY.falsy.int_truth(v.as_int()) } else { break };
                    branch!(ins, holds != (ins.x != 0))
                }
            } {
                // a take pops the row a wait or call left leaving lowered code no top
                OpCode::StoreTopR => {
                    // a value a job handed its register at once leaves nothing to take
                    if self.top == 0 { p = past(p, 1); continue; }
                    let Some(&v) = r.get(self.top as usize - 1) else { break };
                    self.top = 0;
                    if ins.x == 0 { set!(ins.a, v); } else { self.scopes[code.module].set_at(ins.a as u32, v); }
                    p = past(p, 1);
                }
                OpCode::IsR | OpCode::IsNotR => {
                    let (x, y) = (reg!(ins.b), reg!(ins.c));
                    if x.is_undef() || y.is_undef() { break; }
                    set!(ins.a, Val::bool((x.0 == y.0) == (ins.op == OpCode::IsR)));
                    p = past(p, 1);
                }
                OpCode::LoadGlobalR => {
                    let scope = get_in(&self.scopes, code.module);
                    let v = match scope.at(ins.b as u32) { v if v.is_undef() => scope.fallback(ins.b as u32), v => v };
                    if v.is_undef() { break; }
                    set!(ins.a, v);
                    p = past(p, 1);
                }
                OpCode::StoreGlobalR if ins.x == 0 => {
                    let v = reg!(ins.b);
                    if v.is_undef() { break; }
                    self.scopes[code.module].set_at(ins.a as u32, v);
                    p = past(p, 1);
                }
                // a store into a native's name notes it once apart and stays in the loop
                OpCode::StoreGlobalR => {
                    let v = reg!(ins.b);
                    if v.is_undef() { break; }
                    self.store_noted(code.module, ins.a as u32, v);
                    r = &mut self.regs[base..];
                    p = past(p, 1);
                }
                OpCode::GetItemR => {
                    // dict keys and slices the arm misses answer in place and the rest leave
                    macro_rules! missed { () => {{ if !self.item_missed(idx!(), code, chunk) { break; } r = &mut self.regs[base..]; p = past(p, 1); continue; }} }
                    let w = read(p, |w| w);
                    let (o, k) = (reg!(w.b), reg!(w.c));
                    if !o.is_heap() || !k.is_int() { missed!() }
                    let hit = match self.heap.try_get(o) {
                        Some(HeapObj::List(v)) => match peek(v) { Some(b) => index_of(k.as_int(), b.len(), L::POLICY.neg_index).map(|j| b[j]), None => None },
                        Some(HeapObj::Tuple(v)) => index_of(k.as_int(), v.len(), L::POLICY.neg_index).map(|j| v[j]),
                        Some(HeapObj::Str(_)) => self.heap.str_char(o, k.as_int()),
                        _ => None,
                    };
                    let Some(v) = hit else { missed!() };
                    set!(w.a, v);
                    p = past(p, 1);
                }
                OpCode::StoreItemR => {
                    // a plain dict key the arm misses stores in place and the rest leaves
                    macro_rules! missed { () => {{ if !self.item_missed(idx!(), code, chunk) { break; } r = &mut self.regs[base..]; p = past(p, 1); continue; }} }
                    let (o, k) = (reg!(ins.a), reg!(ins.b));
                    if !o.is_heap() || !k.is_int() { missed!() }
                    let Some(HeapObj::List(v)) = self.heap.try_get(o) else { missed!() };
                    let mut b = v.borrow_mut();
                    let Some(j) = index_of(k.as_int(), b.len(), L::POLICY.neg_index) else { break };
                    b[j] = reg!(ins.c);
                    drop(b);
                    base = self.base; r = &mut self.regs[base..];
                    p = past(p, 1);
                }
                // a loop's iterator and a maker or a value of the vm run here
                OpCode::GetIterR | OpCode::OpR => {
                    if !self.op_in_loop(ins, idx!(), code, chunk) { break; }
                    base = self.base; r = &mut self.regs[base..];
                    p = past(p, 1);
                    // a job its handler began runs next as a frame of this loop
                    if self.yielded { break; }
                }
                // a generator a `for` steps runs as this loop's next frame yielding values straight back
                OpCode::ForIterR if let Some(&IterFrame::Coroutine(co)) = self.iter_stack.last() => {
                    let Some((c, k, at)) = self.enter_gen(co, idx!()) else { break };
                    // one waiting inside helpers opens them first as its driver enters it
                    if self.chain.next { (self.gen_entry, self.thrown, self.yielded) = (at, true, true); break; }
                    (code, chunk) = (c.get(), k.get());
                    (cache, prog, switched) = (&code.cache, whole!(), true);
                    n = prog.len();
                    base = self.base; r = &mut self.regs[base..];
                    goto!(at);
                    debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
                }
                // ranges and lists step here and the rest or an ended loop in the handler
                OpCode::ForIterR if self.budget > 0 && !self.heap.needs_gc() => {
                    let Some(f) = self.iter_stack.last_mut() else { break };
                    let item = match f.next_ready(&self.heap) {
                        Some(v) => v,
                        None => match f {
                            // a failure sticks for the handler to raise and an uninterned character is made here
                            IterFrame::DictWalk { .. } | IterFrame::Chars { .. } => match f.next_item(&mut self.heap) {
                                Ok(Some(v)) => v,
                                _ => break,
                            },
                            // a builtin iterator the program holds steps in place
                            &mut IterFrame::UserDefined(it) => match self.ready_next(it) {
                                Some(Ok(v)) => { r = &mut self.regs[base..]; v }
                                Some(Err(e)) => { self.budget -= 1; self.reg_error = Some(e); break }
                                None => break,
                            },
                            // `map` and `filter` call their function as the next frame the loop enters on leaving
                            IterFrame::Map { .. } | IterFrame::Filter { .. } => { self.step_called(idx!(), ins); break }
                            _ => break,
                        },
                    };
                    self.budget -= 1;
                    if ins.x == 0 { set!(ins.b, item); } else { self.scopes[code.module].set_at(ins.b as u32, item); }
                    p = past(p, 1);
                }
                OpCode::GetAttrR => {
                    let o = reg!(ins.b);
                    let (heap, epoch) = (&self.heap, self.layout_epoch);
                    let (found, method) = cache.with_site(idx!(), |site| match super::sites::site_value(heap, epoch, site, o) {
                        Some(v) => (Some(v), None),
                        None => (None, super::sites::site_bound(heap, site, o)),
                    });
                    let v = match found {
                        Some(v) => v,
                        // a builtin type's method binds its receiver as the same bound object each time
                        None => match method {
                            Some(bound) => match self.heap.alloc(bound.of(o)) { Ok(v) => v, Err(_) => break },
                            // a read the site misses runs in place and learns
                            None => { if !self.attr_missed(idx!(), code, chunk) { break; } r = &mut self.regs[base..]; p = past(p, 1); continue }
                        },
                    };
                    set!(ins.a, v);
                    p = past(p, 1);
                }
                OpCode::SetAttrR => {
                    let (o, v) = (reg!(ins.a), reg!(ins.b));
                    let (heap, epoch) = (&self.heap, self.layout_epoch);
                    if v.is_undef() || !cache.with_site(idx!(), |site| super::sites::site_put(heap, epoch, site, o, v)) { break; }
                    p = past(p, 1);
                }
                // jumps to or from heads laid past grown code's end are no program step
                OpCode::JumpR => goto!(ins.a as usize),
                // flow over rows starts with a jump and its safe point
                OpCode::Jump if (ins.a as usize) <= n && self.budget >= 2 => {
                    let (to, back) = (ins.a as usize, (ins.a as usize) <= idx!() && ins.x == 0);
                    if back && (self.preempt_left != 0 || self.heap.needs_gc() || code.unchanged() && code.hot.get() < super::lower::HOT_LOOP) {
                        if sp != usize::MAX { sync!(); }
                        match self.regs_edge(code, floor, to) {
                            Some(next) => { goto!(next); if self.yielded { break; } base = self.base; r = &mut self.regs[base..]; }
                            None => break,
                        }
                    } else {
                        self.budget -= 1 + back as u64;
                        goto!(to);
                    }
                }
                OpCode::JumpIfFalse => tested!(OpCode::JumpIfFalse),
                OpCode::JumpIfFalseOrPop => tested!(OpCode::JumpIfFalseOrPop),
                OpCode::JumpIfTrueOrPop => tested!(OpCode::JumpIfTrueOrPop),
                // a loop over rows takes its iterator from the top row apart
                OpCode::GetIter => {
                    if sp != usize::MAX { sync!(); }
                    let ran = self.iter_rows(arg!());
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                }
                // a range a list or a ready iterator steps inline and any other apart
                OpCode::ForIter => {
                    over!();
                    if self.budget > 0 && !self.heap.needs_gc() && let Some(v) = self.iter_stack.last_mut().and_then(|f| f.next_ready(&self.heap)) {
                        self.budget -= 1;
                        push!(v)
                    } else {
                        sync!();
                        // a generator runs next in this loop above the rows its yields hand back
                        if let Some(&IterFrame::Coroutine(co)) = self.iter_stack.last() && let Some((c, k, at)) = self.enter_gen(co, idx!()) {
                            // one waiting inside helpers opens them first as its driver enters it
                            if self.chain.next { (self.gen_entry, self.thrown, self.yielded) = (at, true, true); break; }
                            (code, chunk) = (c.get(), k.get());
                            (cache, prog, switched) = (&code.cache, whole!(), true);
                            n = prog.len();
                            refill!();
                            goto!(at);
                            continue;
                        }
                        let went = self.for_rows(idx!(), code, n);
                        refill!();
                        match went { Ok(next) => goto!(next), Err(at) => { goto!(at); break } }
                    }
                }
                // a finally that fell through ends here and other reasons take the loop's path
                OpCode::EndFinally | OpCode::UnwindFinally => {
                    if sp != usize::MAX { sync!(); }
                    let on = self.end_rows(idx!(), code);
                    refill!();
                    if !on { break; }
                    p = past(p, 1);
                }
                // a raise leaves with its error and the sentinel past the end raises nothing
                OpCode::Raise if idx!() < n => {
                    if sp != usize::MAX { sync!(); }
                    self.raise_rows(idx!(), code, chunk);
                    break;
                }
                OpCode::MinusR | OpCode::NotR | OpCode::UnaryR => {
                    let x = reg!(ins.b);
                    let v = match ins.op {
                        OpCode::MinusR if x.is_int() => Val::int_checked(-x.as_int()),
                        OpCode::MinusR if x.is_float() => Some(Val::float(-x.as_float())),
                        OpCode::NotR if x.is_bool() => Some(Val::bool(!L::POLICY.falsy.bool_truth(x.as_bool()))),
                        OpCode::NotR if x.is_int() => Some(Val::bool(!L::POLICY.falsy.int_truth(x.as_int()))),
                        OpCode::NotR if x.is_none() => Some(Val::bool(!L::POLICY.falsy.nil_truth())),
                        OpCode::UnaryR if ins.x == OpCode::Pos as u8 && (x.is_int() || x.is_float()) => Some(x),
                        OpCode::UnaryR if ins.x == OpCode::BitNot as u8 && x.is_int() => Val::int_checked(!x.as_int()),
                        _ => None,
                    };
                    let Some(v) = v else { break };
                    set!(ins.a, v);
                    p = past(p, 1);
                }
                // dropping a loop's iterator or a generator section's mark over its own runs apart
                OpCode::PopIter => {
                    self.pop_iter(ins);
                    base = self.base; r = &mut self.regs[base..];
                    p = past(p, 1);
                }
                // key length and cell probes run apart to keep this loop small
                OpCode::InR | OpCode::NotInR | OpCode::LenR | OpCode::LoadCellR | OpCode::StoreCellR => {
                    if !self.heap_fast(ins) { break; }
                    base = self.base; r = &mut self.regs[base..];
                    p = past(p, 1);
                }
                // a call its site knows opens its frame here and any other leaves aside
                OpCode::CallR | OpCode::CallMethodR => call!(ins, reg!(ins.a)),
                OpCode::CallGlobalR => call!(ins, global_callee(&self.scopes, code, ins)),
                // one or two plain arguments a native answers here and the rest run apart
                OpCode::CallNativeR => {
                    let fast = if ins.x & lower::PINNED == 0 && self.any_rebound != 0 && rebound_bit(&self.rebound, ins.a) { None } else {
                        match (ins.x & lower::ARGC, self.natives.get(crate::lang::NativeId(ins.a))) {
                            (1, Some(&crate::lang::Native { fast1: Some(f), .. })) => { let x = reg!(ins.b); if x.is_undef() { None } else { f(&mut self.heap, x) } }
                            (2, Some(&crate::lang::Native { fast2: Some(f), .. })) => { let (x, y) = (reg!(ins.b), reg!(ins.c)); if x.is_undef() || y.is_undef() { None } else { f(&mut self.heap, x, y) } }
                            _ => None,
                        }
                    };
                    match fast {
                        Some(v) => {
                            // native work pays before the loop goes on as the stack form pays it
                            if self.heap.has_work() {
                                if let Err(e) = self.pay_work() { self.reg_error = Some(e); break; }
                                r = &mut self.regs[base..];
                            }
                            // a statement's call keeps nothing and another's take follows it in `prog`
                            if ins.x & lower::VOID != 0 { p = past(p, 1); } else {
                                let take = *word(past(p, 1));
                                debug_assert!(take.op == OpCode::StoreTopR && take.x == 0, "a native call without its take");
                                set!(take.a, v);
                                p = past(p, 2);
                            }
                        }
                        None => {
                            let next = self.native_regs(idx!(), code, chunk);
                            base = self.base; r = &mut self.regs[base..];
                            match next {
                                Some(next) => { goto!(next); if self.yielded { break; } }
                                None => break,
                            }
                        }
                    }
                }
                // a runtime's own opcode writes `a` from its fast path and leaves the rest apart
                OpCode::RuntimeR if <L::Ops as OpTable>::COUNT != 0 => {
                    let (b, k) = (usize::from(ins.b), usize::from(ins.c));
                    let bound = self.any_rebound > OPS_FREE && (self.any_rebound == OPS_BOUND || ops_bound::<L>(&mut self.any_rebound, &self.rebound, self.natives));
                    let v = if bound { None } else { r.get(b..b + k).and_then(|args| <L::Ops as OpTable>::fast(ins.x, args, &mut self.heap)) };
                    let Some(v) = v else { break };
                    if self.heap.has_work() {
                        if let Err(e) = self.pay_work() { self.reg_error = Some(e); break; }
                        r = &mut self.regs[base..];
                    }
                    set!(ins.a, v);
                    p = past(p, 1);
                }
                // the most common plain call of a binding's section checks only that no binding changed
                OpCode::CallGlobalSectionR => {
                    let (s, x) = (*word(past(p, 1)), *word(past(p, 2)));
                    let block = known(code.block.as_deref());
                    if super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed) != block.binds.get() && !sections_hold(&self.scopes, block) {
                        goto!(word(past(p, 3)).a as usize);
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
                // a register call enters the section of the function it holds and others the stub
                OpCode::CallValueSectionR => {
                    let (s, x) = (*word(past(p, 1)), *word(past(p, 2)));
                    let block = known(code.block.as_deref());
                    // another function the call met enters what its head's table finds and others the stub
                    if reg!(ins.a).0 != get_in(&block.sections, x.c as usize).value.get().0 { let to = self.enter_value_alt(code, idx!()); r = &mut self.regs[base..]; goto!(to); continue; }
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    let at = x.b as usize;
                    set!(at, reg!(ins.b));
                    set!(at + 1, reg!(ins.c));
                    self.budget = left;
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // a method's section holds on receivers of its last class while no class changed
                OpCode::CallMethodSectionR => {
                    let (s, x) = (*word(past(p, 1)), *word(past(p, 2)));
                    let block = known(code.block.as_deref());
                    let sec = get_in(&block.sections, x.c as usize);
                    let (class, epoch, last) = sec.guard.get();
                    let recv = reg!(ins.b);
                    let held = epoch == self.layout_epoch && (recv.0 == last.0 || matches!(self.heap.try_get(recv), Some(HeapObj::Object { meta: c, .. }) if c.0 == class.0));
                    // any other receiver enters the section laid for the method it finds or the stub
                    if !held { let to = self.enter_method_alt(code, chunk, idx!()); r = &mut self.regs[base..]; if to == GROW { break; } goto!(to); continue; }
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    let at = x.b as usize;
                    set!(at, recv);
                    set!(at + 1, reg!(ins.c));
                    self.budget = left;
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // a class call passes its new instance first while binding and class stay as laid
                OpCode::CallCtorSectionR => {
                    let (s, x) = (*word(past(p, 1)), *word(past(p, 2)));
                    let block = known(code.block.as_deref());
                    if super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed) != block.binds.get() && !sections_hold(&self.scopes, block) {
                        goto!(word(past(p, 3)).a as usize);
                        continue;
                    }
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    let sec = get_in(&block.sections, x.c as usize);
                    let holds = sec.ctor.as_ref().is_some_and(|c| c.0.get() == self.layout_epoch || ctor_holds(&self.heap, &self.mro_cache, sec, self.layout_epoch));
                    let Some(inst) = sec.ctor.as_ref().filter(|_| holds).and_then(|c| new_object(&mut self.heap, sec.value.get(), c.1)) else { goto!(word(past(p, 3)).a as usize); continue; };
                    let at = x.b as usize;
                    set!(at, inst);
                    set!(at + 1, reg!(ins.b));
                    self.budget = left;
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // likewise with a module binding's receiver set in its register only for checks aside
                OpCode::CallMethodGlobalSectionR => {
                    let (s, x, t) = (*word(past(p, 1)), *word(past(p, 2)), *word(past(p, 3)));
                    let block = known(code.block.as_deref());
                    let sec = get_in(&block.sections, x.c as usize);
                    let (class, epoch, last) = sec.guard.get();
                    let recv = get_in(&self.scopes, code.module).at(t.c as u32 - 1);
                    let held = epoch == self.layout_epoch && (recv.0 == last.0 || matches!(self.heap.try_get(recv), Some(HeapObj::Object { meta: c, .. }) if c.0 == class.0));
                    // an unbound one goes to the stub which reads it again
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
                // a section call jumps in while its binding holds the function it was laid from
                OpCode::CallSectionR => {
                    let (s, x) = (*word(past(p, 1)), *word(past(p, 2)));
                    let block = known(code.block.as_deref());
                    if x.x & 8 != 0 {
                        // a spreading call is checked apart
                        if x.x & 64 != 0 { break; }
                        // a method section holds for its unchanged class unless an instance may shadow the name
                        let sec = get_in(&block.sections, x.c as usize);
                        let (class, epoch, _) = sec.guard.get();
                        let class_of = |heap: &HeapPool, v: Val| match heap.try_get(v) { Some(HeapObj::Object { meta: c, .. }) => *c, _ => Val::undef() };
                        // any other receiver enters the section laid for the method it finds or the stub
                        if epoch != self.layout_epoch || self.shadowed || class_of(&self.heap, reg!(ins.b)).0 != class.0 {
                            let to = self.enter_method_alt(code, chunk, idx!());
                            r = &mut self.regs[base..];
                            if to == GROW { break; }
                            goto!(to);
                            continue;
                        }
                    } else if x.x & 4 != 0 {
                        // any function of the section's body enters it apart filling cells and defaults from itself
                        if reg!(ins.a).0 != get_in(&block.sections, x.c as usize).value.get().0 {
                            let to = if x.x & 32 != 0 { self.enter_section_of_body(code, idx!()) } else { None };
                            r = &mut self.regs[base..];
                            goto!(to.unwrap_or(word(past(p, 3)).a as usize));
                            continue;
                        }
                    } else if super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed) != block.binds.get() && !sections_hold(&self.scopes, block) {
                        goto!(word(past(p, 3)).a as usize);
                        continue;
                    }
                    // each call pays a step so recursion through sections stays within the budget
                    let Some(left) = self.budget.checked_sub(1) else { break };
                    // every section has two parameters at least and an unpassed argument names its own register
                    let at = x.b as usize;
                    if x.x & 16 == 0 {
                        set!(at, reg!(ins.b));
                        set!(at + 1, reg!(ins.c));
                    } else {
                        // a class call passes its new instance first while the class stays as laid
                        let s = get_in(&block.sections, x.c as usize);
                        let holds = s.ctor.as_ref().is_some_and(|c| c.0.get() == self.layout_epoch || ctor_holds(&self.heap, &self.mro_cache, s, self.layout_epoch));
                        let Some(inst) = s.ctor.as_ref().filter(|_| holds).and_then(|c| new_object(&mut self.heap, s.value.get(), c.1)) else { goto!(word(past(p, 3)).a as usize); continue; };
                        set!(at, inst);
                        set!(at + 1, reg!(ins.b));
                    }
                    self.budget = left;
                    // the link holds where the call carries on and the register for its result
                    set!(at - 1, Val::int((s.b as i64) << 16 | s.a as i64));
                    goto!(x.a as usize);
                }
                // a stub's lookup of the section laid for the function its call makes
                OpCode::CallValueR => {
                    let to = self.enter_value(code, ins);
                    r = &mut self.regs[base..];
                    match to { Some(to) => goto!(to), None => p = past(p, 1) }
                }
                // a section return writes the register the call's `StoreTopR` names and resumes past the call
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
                // a return from inside a section's `try` leaves it through its handlers
                OpCode::ReturnTriedR => {
                    let (link, v) = (reg!(ins.a).as_int() as usize, reg!(ins.b));
                    if v.is_undef() || self.exception_stack.last().is_some_and(|f| f.handler_ip & RAW_HANDLER != 0) { break; }
                    set!(link & 0xFFFF, v);
                    goto!(link >> 16);
                }
                // a call back into its sections first keeps a recursion level on the levels
                OpCode::TurnR => {
                    let x = read(p, |w| &w.x);
                    let len = self.levels.len();
                    if len.wrapping_sub(self.turn_low) >= self.turn_span || self.acts != self.turn_acts { break; }
                    self.turned += 1;
                    if x & lower::LONE_TURN != 0 {
                        // a TurnR names words its lowering laid and the levels have room for three
                        let w = word(past(prog.cast(), ins.a as usize));
                        turn_lone(&mut self.levels, len, w.x & 3, (reg!(w.b), reg!(w.c), reg!(w.a)));
                    } else {
                        super::level::turn(r, &mut self.levels, words(prog, ins.a as usize, ins.b as usize));
                    }
                    p = past(p, 1);
                }
                // past that call the level comes back with every word of it at once
                OpCode::UnturnR => {
                    let x = read(p, |w| &w.x);
                    let len = self.levels.len();
                    if x & lower::LONE != 0 && len >= 3 {
                        // the level's values lie on the levels top three at most
                        untake_lone(&mut self.levels, len, x & 3, |k, v| match k { 0 => set!(ins.a, v), 2 => set!(ins.c, v), _ => set!(ins.b, v) });
                        self.turned = self.turned.saturating_sub(1);
                        p = past(p, 1);
                    } else if x & lower::LONE != 0 {
                        let at = idx!();
                        let k = self.unturn_apart(words(prog, at, n - at));
                        r = &mut self.regs[base..];
                        goto!(at + k);
                    } else {
                        let at = idx!();
                        let (start, _, k) = super::level::take_level(r, &self.levels, words(prog, at, n - at), len);
                        self.levels.truncate(start);
                        self.turned = self.turned.saturating_sub(1);
                        goto!(at + k);
                    }
                }
                // a plain return to a frame of this loop carries on in the caller
                OpCode::ReturnR if self.acts > floor.depth => {
                    let v = reg!(ins.b);
                    let Some((c, k, next)) = self.leave_site(v, floor.code, floor.chunk, floor.pool, floor.depth) else { break };
                    (code, chunk) = (c.get(), k.get());
                    (cache, prog, switched) = (&code.cache, whole!(), true);
                    n = prog.len();
                    base = self.base; r = &mut self.regs[base..];
                    goto!(next);
                    debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
                }
                // owned strings grow in place and the handler adds the rest and may take ownership
                OpCode::AppendR => {
                    let (sv, tv) = (reg!(ins.a), reg!(ins.b));
                    // numbers add here as `InPlaceAddR` does
                    let sum = if sv.is_int() && tv.is_int() { int_add(sv.as_int(), tv.as_int()) }
                        else if sv.is_float() && tv.is_float() { Some(Val::float(sv.as_float() + tv.as_float())) }
                        else { None };
                    if let Some(v) = sum { set!(ins.a, v); p = past(p, 1); continue; }
                    if sv.0 != self.owned_str.0 || self.owned_at != base + ins.a as usize || !self.heap.append_str(sv, tv) { break; }
                    p = past(p, 1);
                }
                OpCode::OwnResetR => {
                    if self.owned_at == base + ins.a as usize { self.owned_str = Val::undef(); }
                    p = past(p, 1);
                }
                // an unbound item leaves for the handler which names it
                OpCode::ListPushR => {
                    let v = reg!(ins.b);
                    let Some(HeapObj::List(rc)) = self.heap.try_get(reg!(ins.a)) else { break };
                    if v.is_undef() { break; }
                    rc.borrow_mut().push(v);
                    p = past(p, 1);
                }
                // a stack op of lowered code runs over its row as rows code runs it
                OpCode::RowsR => {
                    let w = read(p, |w| w);
                    let next = self.rows_op(w, idx!(), code, chunk);
                    refill!();
                    match next {
                        Some(next) => { goto!(next); if self.yielded { break; } }
                        None => break,
                    }
                }
                // a row op's value lies in its row and lowered code keeps no top
                OpCode::RowsBackR => {
                    self.top = 0;
                    p = past(p, 1);
                }
                // a build makes its object apart and leaves for the handler when it may fail
                OpCode::BuildR => {
                    let op = OpCode::ALL[(ins.x & !lower::TAKEN) as usize];
                    let v = if matches!(op, OpCode::BuildList | OpCode::BuildTuple) { self.seq_reg(op == OpCode::BuildList, base + ins.b as usize, ins.c as usize) } else { self.build_reg(ins) };
                    let Some(v) = v else { break };
                    r = &mut self.regs[base..];
                    set!(ins.a, v);
                    p = past(p, 1);
                }
                // a bare field keeps a plain value or a string for the build to render
                OpCode::FormatR => {
                    let w = read(p, |w| w);
                    let v = reg!(w.b);
                    // any other formats in place by its stack form's handler
                    if w.x != 0 || !bare_field(&self.heap, v) { if !self.form_missed(idx!(), code, chunk) { break; } r = &mut self.regs[base..]; p = past(p, 1); continue; }
                    set!(w.a, v);
                    p = past(p, 1);
                }
                // a `try`'s blocks stay here with the handler index `x` picks
                OpCode::SetupExcept | OpCode::SetupFinally => {
                    // a try over rows takes no row as each statement starts on an empty stack
                    debug_assert!(code.rows == 0 || super::rows::flow::tried(code.rows as usize, sp, self.top), "a try over rows above its first row");
                    self.exception_stack.push(ExceptionFrame {
                        kind: if ins.op == OpCode::SetupExcept { BlockKind::Except } else { BlockKind::Finally },
                        handler_ip: ins.a as usize | if ins.x == 1 { RAW_HANDLER } else { 0 },
                        levels: self.levels.len(),
                        iter_depth: self.iter_stack.len(),
                        unwind_depth: self.unwind_stack.len(),
                        handling_depth: self.handling.len(),
                        turned: self.turned,
                        slot: ins.b,
                    });
                    p = past(p, 1);
                }
                // a `try`'s blocks as the handler keeps them
                OpCode::PopExcept => {
                    if let Some(f) = self.exception_stack.pop() { self.handling.truncate(f.handling_depth); }
                    p = past(p, 1);
                }
                OpCode::BeginFinally => {
                    self.unwind_stack.push(Unwind::Normal);
                    p = past(p, 1);
                }
                // a generator a `for` runs as a frame hands its value back here
                OpCode::Yield if ins.x != 0 && self.acts > floor.depth && self.gen_yields_to_for() => {
                    let v = reg(&self.regs, base + ins.a as usize);
                    if v.is_undef() { break; }
                    // a value yield's row takes the none a `for` sends
                    if ins.x == 2 { put(&mut self.regs, base + ins.a as usize, Val::none()); }
                    // the next step starts past the jump after it which the lowering found
                    let resume = ins.b as usize;
                    let top = yield_top(code, ins, resume, self);
                    let Some((c, k, next)) = self.yield_site(v, (resume, top), floor.code, floor.chunk, floor.pool, floor.depth) else { break };
                    (code, chunk) = (c.get(), k.get());
                    (cache, prog, switched) = (&code.cache, whole!(), true);
                    n = prog.len();
                    base = self.base; r = &mut self.regs[base..];
                    goto!(next);
                    debug_assert!(r.len() >= code.frame, "a frame shorter than its code");
                }
                // a dropped register yield a builtin takes in place runs on past it
                OpCode::Yield if ins.x == 1 && self.acts > floor.depth && self.top_gather() && self.gather_reg(ins.a) => {
                    base = self.base; r = &mut self.regs[base..];
                    p = past(p, 1);
                }
                // a builtin running the generator takes its value in place or `next` ends it
                OpCode::Yield if ins.x != 0 && self.acts > floor.depth && self.top_gather() && self.gen_yields() => {
                    let Some((c, k, next)) = self.gather_yield(ins, idx!(), code, floor) else { break };
                    // the frame running on in place gives no code
                    if c != CodePtr::NONE {
                        (code, chunk) = (c.get(), k.get());
                        (cache, prog, switched) = (&code.cache, whole!(), true);
                        n = prog.len();
                    }
                    base = self.base; r = &mut self.regs[base..];
                    goto!(next);
                }
                // a yield over rows hands its value on or parks its generator
                OpCode::Yield if ins.x == 0 => {
                    if sp != usize::MAX { sync!(); }
                    let went = self.yield_rows(idx!(), code, (floor.code, floor.chunk, floor.pool, floor.depth));
                    refill!();
                    match went {
                        Ok((c, k, next)) => {
                            // a builtin taking the value in place leaves the generator running
                            if c != CodePtr::NONE {
                                (code, chunk) = (c.get(), k.get());
                                (cache, prog, switched) = (&code.cache, whole!(), true);
                                n = prog.len();
                            }
                            goto!(next);
                        }
                        Err(at) => { goto!(at); break }
                    }
                }
                OpCode::LoadYieldFrom => {
                    if sp != usize::MAX { sync!(); }
                    let ran = self.delegated_rows(code);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                }
                OpCode::GetAwaitable => {
                    if sp != usize::MAX { sync!(); }
                    let ran = self.awaitable_rows(code);
                    refill!();
                    if !ran { break; }
                    p = past(p, 1);
                }
                // an await over rows leaves for its driver which steps the awaitable
                OpCode::Await => {
                    if sp != usize::MAX { sync!(); }
                    break;
                }
                // a join a delete or a declaration runs apart raising for what is unbound
                OpCode::Phi | OpCode::Del | OpCode::Global | OpCode::Outer => {
                    if let Err(e) = self.side_op(ins, idx!(), code, chunk) { self.reg_error = Some(e); break; }
                    r = &mut self.regs[base..];
                    p = past(p, 1);
                }
                // register ops kept apart from the hot ones that answer their common case there
                OpCode::PowR | OpCode::ListAppendR | OpCode::SetAddR | OpCode::MapAddR | OpCode::UnpackR | OpCode::MatchExcR | OpCode::ListToTupleR
                | OpCode::LoadGlobalAtR | OpCode::StoreGlobalAtR | OpCode::UnbindR | OpCode::WrapCellR => {
                    if !self.reg_apart(ins) { break; }
                    base = self.base; r = &mut self.regs[base..];
                    p = past(p, 1);
                }
                _ => break,
            } });
        }
        false
        };
        // `p` is the operation whose numbers missed and strings or wide ones answer here
        if missed && let Some(v) = self.binop_apart(*word(p)) {
            self.regs[self.base + word(p).a as usize] = v;
            p = past(p, 1);
            base = self.base; r = &mut self.regs[base..];
            continue 'regs;
        }
        // an op its arm left runs out of line unless the driver takes it
        let at = idx!();
        if self.reg_error.is_none() && !self.yielded && !self.entered && at < n {
            if sp != usize::MAX { sync!(); }
            let next = self.op_apart(at, code, chunk, floor);
            (base, sp) = (self.base, usize::MAX);
            r = &mut self.regs[base..];
            if let Some(next) = next {
                goto!(next);
                if !self.yielded { continue 'regs; }
            }
        }
        break;
        }
        (*ip, *code_at, *chunk_at) = (idx!(), code, chunk);
        switched
    }

    /* a register loop back-edge's safe point which stays at the jump when it fails */
    #[cold]
    #[inline(never)]
    fn regs_edge(&mut self, code: &Code, floor: &Floor, target: usize) -> Option<usize> {
        // the safe point pays the second step and the native work
        self.budget -= 1;
        match self.back_edge(code, floor, target) { Ok(()) => Some(target), Err(e) => { self.reg_error = Some(e); None } }
    }

    /* whether a preempt now parks inside sections at `at` or where a caller waits */
    #[cold]
    #[inline(never)]
    fn parks_inside(&mut self, code: &Code, at: usize, floor: &Floor) -> bool {
        // a run that waited long enough parks wherever it stands
        if self.preempt_waited >= PREEMPT_PATIENCE { return false; }
        let mut inside = super::coro::parks_raw(code, at);
        // each frame's caller carries on at the frame's return point in the code below it
        let (mut pool, mut k) = (self.top_pool, self.acts);
        while !inside && k > floor.depth && pool != super::act::NO_POOL {
            let h = self.head_at(pool);
            let (ret, below) = ((self.head(h).as_int() & 0xFF_FFFF) as usize, (self.head(h + 1).as_int() & 0xFF_FFFF) as usize);
            let caller = if k - 1 > floor.depth { crate::r#unsafe::CodePtr::from_word(self.head(self.head_at(below) + 4)) } else { floor.code };
            inside = super::coro::parks_raw(caller.get(), ret);
            (pool, k) = (below, k - 1);
        }
        if inside { self.preempt_waited += 1; }
        inside
    }

    /* a back-edge's safe point whose caller leaves at the target once yielded is set */
    #[cold]
    #[inline(never)]
    pub(crate) fn back_edge(&mut self, code: &Code, floor: &Floor, target: usize) -> Result<(), VmErr> {
        self.charge_step()?;
        if self.heap.needs_gc() { self.collect_point()?; }
        // a count `note_alt` left only sends the back-edge here
        if self.preempt_left == RELAY { self.preempt_left = 0; }
        // back-edges are the only preempt sampling point
        if self.preempt_left != 0 {
            self.preempt_left -= 1;
            if self.preempt_left == 0 {
                if self.frame_safe && self.gen_frames == 0 && self.jobs.is_empty() && self.frames_park() && !self.parks_inside(code, target, floor) {
                    (self.preempt_left, self.preempt_waited) = (self.preempt_every, 0);
                    self.pending.preempt_request = true;
                    self.yielded = true;
                } else {
                    // unpreemptible here so the next back-edge tries again
                    self.preempt_left = 1;
                }
            }
        }
        // calls that met other functions often relay the code unless section registers hold the loop
        let inside = || code.block.as_deref().and_then(|b| b.sections.first()).is_some_and(|s| target >= s.start as usize);
        if code.misses.get() == super::lower::MISSES && !self.yielded && !inside() && !code.ins.get(target).is_some_and(|i| i.op == OpCode::ReturnSectionR) {
            code.misses.set(super::lower::MISSES + 1);
            let pool = if self.acts > floor.depth { self.top_pool } else { floor.pool };
            let p = &mut self.pools[pool];
            // code lowered without sections grows a block heading each call that met a function
            if code.block.is_none() && p.code.as_ref().is_some_and(|c| core::ptr::eq(Rc::as_ptr(c), code)) {
                (self.growing, self.yielded) = (Some(alloc::boxed::Box::new((CodePtr::of_code(code), 1, super::lower::Grow::All, Val::undef()))), true);
            // laid again only while each laying lays more than the one before
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
        Ok(())
    }
}
