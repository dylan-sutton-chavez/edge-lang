use crate::code::OpCode;
use crate::syntax::chunk::{Instruction, NativeSite, SSAChunk, native_site};
use crate::syntax::ssa::ssa_strip;
use alloc::{borrow::Cow, boxed::Box, rc::Rc, vec, vec::Vec};
use core::cell::{Cell, OnceCell};

use super::scope::Kind;
use crate::value::Val;
use crate::code::Ins;

mod extend;
mod sections;
mod bound;
use bound::*;
pub(crate) use self::{extend::*, sections::*};


// the `x` bit of a CallNativeR no binding of its name redirects
pub(crate) const PINNED: u8 = 0x40;
// the `x` bit of a statement's CallNativeR which takes no value
pub(crate) const VOID: u8 = 0x20;
// the `x` bit of a register yield whose park keeps rows under its value
pub(crate) const KEPT: u8 = 4;
// the `x` of a RaiseR standing for code no rows hold
pub(crate) const TOO_DEEP: u8 = 2;
// the bits of a CallNativeR's `x` that count its arguments
pub(crate) const ARGC: u8 = 7;
// the most arguments a CallNativeR passes in its own word and one ArgsR
pub(crate) const NATIVE_ARGS: usize = 5;

/* whether the word after a native call is a statement's PopTop its call drops into */
#[inline(always)]
pub(crate) fn drops(next: Option<&Ins>) -> bool { !cfg!(feature = "coverage") && next.is_some_and(|n| n.op == OpCode::PopTop) }

/* what a chunk's lowering may assume about the slots of its frame */
#[derive(Clone)]
pub(crate) struct Facts {
    /* names lower to registers which a class body's namespace never does */
    pub regs: bool,
    /* slots bound before the body runs as bits below 128 and listed past them */
    pub bound: u128,
    pub bound_far: Vec<usize>,
    /* whether each slot is a local or a closure cell or a module binding */
    pub kinds: Rc<[Kind]>,
    /* the module whose bindings the globals index */
    pub module: usize,
    /* what the code holds as `shape` gives it */
    pub shape: u8,
    /* the code is a module's which runs once */
    pub once: bool,
    /* the compiler instruction a module's hot loop carries on from or 0 for the whole */
    pub from: usize,
    /* the function calls itself so each level's frame keeps no rows */
    pub recurs: bool,
    /* the runtime's natives naming the builtins a store rebinds */
    pub natives: &'static crate::lang::Natives,
    /* each native the module rebinds with the slot of the binding its calls call */
    pub rebound: Vec<(u16, u16)>,
}

impl Facts {
    /* the module binding a call of native `id` calls once the module binds its name */
    pub(crate) fn binding_of(&self, id: u16) -> Option<u16> {
        self.rebound.iter().find(|r| r.0 == id).map(|r| r.1)
    }

    /* sets the bits of the slots bound before the body runs in the words `out` */
    fn bind(&self, out: &mut [u64]) {
        for (w, o) in out.iter_mut().take(2).enumerate() { *o |= (self.bound >> (64 * w)) as u64; }
        for &k in &self.bound_far { if let Some(o) = out.get_mut(k / 64) { *o |= 1 << (k % 64); } }
    }
}

// what a chunk's code holds that its lowering looks for read in one pass
pub(crate) const CALLS: u8 = 1;
pub(crate) const LOOPS: u8 = 2;
pub(crate) const ATTRS: u8 = 4;
pub(crate) const APPENDS: u8 = 8;
pub(crate) const DELETES: u8 = 16;
// an await which steps over rows only in the compiler's own code
pub(crate) const AWAITS: u8 = 32;
// a jump or handler or an early end of flow means more than one block
pub(crate) const JUMPS: u8 = 64;
// set on every shape read so a pool knows it read one
pub(crate) const READ: u8 = 128;

/* what `src` holds as bits of the constants above */
pub(crate) fn shape(src: &[Instruction]) -> u8 {
    let mut shape = READ;
    for (k, i) in src.iter().enumerate() {
        if i.opcode.is_jump() || i.opcode == OpCode::UnwindFinally || ends_flow(i.opcode) && k + 1 < src.len() { shape |= JUMPS; }
        shape |= match i.opcode {
            OpCode::Call | OpCode::CallMethod | OpCode::CallSpread => CALLS,
            OpCode::ForIter => LOOPS,
            OpCode::Jump if i.operand as usize <= k => LOOPS,
            OpCode::LoadAttr => ATTRS,
            OpCode::InPlaceAdd => APPENDS,
            OpCode::Del => DELETES,
            OpCode::GetAwaitable | OpCode::Await => AWAITS,
            _ => 0,
        };
    }
    shape
}

/* a chunk's code as the vm runs it mapped back to the compiler's instructions */
pub(crate) struct Code {
    pub ins: Vec<Ins>,
    /* the compiler instruction each one came from and empty for unchanged code */
    orig: Vec<u16>,
    /* where each compiler instruction starts and past bit 16 the rows a resume finds */
    at: Vec<u32>,
    /* the compiler instruction a run stopped before `ins[i]` resumes from */
    back: Vec<u16>,
    /* frame length covering the names then the constants then the temporaries */
    pub frame: usize,
    pub kinds: Rc<[Kind]>,
    pub module: usize,
    /* back-edges an unlowered frame took and its loop lowers once they reach `HOT_LOOP` */
    pub hot: Cell<u32>,
    /* the sites its instructions learned which every frame running it shares */
    pub cache: super::cache::OpcodeCache,
    /* whether a call may lay it as a section and past bit 2 its `pushes` */
    pure: Cell<u8>,
    /* the sections laid in it and none for most code */
    pub block: Option<Box<Block>>,
    /* the same body without sections which a caller lays when this one laid its own */
    pub plain: OnceCell<Option<Rc<Code>>>,
    /* a unique number so a window knows the code it is laid out for */
    pub layout: u32,
    /* calls from outside its sections missing their head's function which at `MISSES` lay it again */
    pub misses: Cell<u8>,
    /* how many site entries lowering read which snapshots remake from and which lay nothing new */
    pub read: Cell<u32>,
    // the first row of code over rows and zero for lowered code
    pub rows: u16,
    // where its constants start in its frame past the rows lowered code keeps
    pub cbase: u16,
    // the first row of lowered code keeping its values in rows and zero otherwise
    pub slots: u16,
    // the pool constants a load over rows reads where lowered code reads its window
    pub consts: crate::r#unsafe::Consts,
    // each instruction's static depth which debug builds hold code over rows to
    #[cfg(debug_assertions)]
    pub depths: Vec<u16>,
}

/* calls before a chunk lowers again with callee sections since only hot code pays off */
pub(crate) const TIER: u32 = 64;
// concurrent frames of one body past which it lowers again with recursion levels as sections
pub(crate) const LAY_DEPTH: u32 = 32;

/* back-edges after which an unlowered loop carries on as lowered code */
pub(crate) const HOT_LOOP: u32 = 16;

/* turned-down calls before laying again beside the first and the functions a register call checks */
pub(crate) const MISSES: u8 = 64;
// turn-downs for a function missing from the table before the code grows to lay it
pub(crate) const TURNS: u32 = 24;
const ALTS: u32 = 16;

impl Code {
    /* whether this is the compiler's code as written with each ip its own */
    #[inline]
    pub fn unchanged(&self) -> bool { self.at.is_empty() }

    /* where compiler instruction `ip` starts or the end past the last one */
    #[inline]
    pub fn lowered(&self, ip: usize) -> usize {
        if self.at.is_empty() { ip.min(self.ins.len()) } else { self.at.get(ip).map_or(self.ins.len(), |&i| (i & 0xFFFF) as usize) }
    }

    /* the compiler instruction a run stopped before `ip` resumes from */
    #[inline]
    pub fn resume(&self, ip: usize) -> usize {
        if self.back.is_empty() { ip.min(self.ins.len()) } else { self.back.get(ip).map_or(self.at.len() - 1, |&i| usize::from(i)) }
    }

    /* whether it pushes nothing on any stack so a frame running it keeps no bases */
    #[inline(always)]
    pub fn flat(&self) -> bool { self.pushes() == 1 }

    /* whether it pushes only loop iterators so a generator finds its stepping iterator under them */
    #[inline(always)]
    pub fn loops_only(&self) -> bool { self.pushes() <= 2 }

    /* 1 when it pushes on no stack and 2 when only iterators and 3 otherwise */
    #[inline(always)]
    fn pushes(&self) -> u8 {
        // the count shares its byte with what `pure` found
        if self.pure.get() >> 2 == 0 { self.pure.set(self.pure.get() & 3 | pushes_of(&self.ins) << 2); }
        self.pure.get() >> 2
    }

    /* whether a caller may lay it as a section which the block's `pure` decides */
    pub fn pure(&self) -> bool {
        if self.pure.get() & 3 == 0 { self.pure.set(self.pure.get() | if super::block::pure(&self.ins) { 1 } else { 2 }); }
        self.pure.get() & 3 == 1
    }

    /* the compiler instruction `ins[i]` came from */
    #[inline]
    pub fn orig(&self, i: usize) -> u32 { self.orig.get(i).map_or(i as u32, |&o| u32::from(o)) }

    /* the module whose bindings `ins[i]` names which a section keeps as its function's */
    pub(crate) fn module_at(&self, i: usize) -> usize { self.block.as_deref().and_then(|b| b.holding(i)).map_or(self.module, |(s, _)| s.module) }

    /* the rows a resume at compiler instruction `ip` finds under the store it enters */
    #[inline]
    pub fn entry_rows(&self, ip: usize) -> usize { (self.at.get(ip).copied().unwrap_or(0) >> 16) as usize }
}

/* the frame of `ins` grown past `len` from `old` */
fn grown_frame(ins: &[Ins], len: usize, frame: usize, old: &Code) -> usize { frame.max(reg_bound(&ins[len..])).max(old.frame) }

/* one past the top register so a frame that long needs no bounds checks */
pub(crate) fn reg_bound(ins: &[Ins]) -> usize { ins.iter().fold(0, |m, i| m.max(reg_top(i))) }

/* one past the highest register `i` names */
#[inline(always)]
fn reg_top(i: &Ins) -> usize {
    use OpCode::*;
    if !i.op.is_register() { return 0; }
    // operands naming jump targets or module slots or cells or names stay out
    let (a, b, c) = match i.op {
        // no SectionR place nor lookup head or table nor TurnR word is a register
        SectionR | CallValueR | JumpR | TurnR => return 0,
        OpR | GetIterR | BuildR | RowsR | RowsBackR | RuntimeR => return reg_apart(i),
        JumpIfFalseR | JumpUnlessEq | JumpUnlessNotEq | JumpUnlessLt | JumpUnlessLtEq | JumpUnlessGt | JumpUnlessGtEq | StoreGlobalR
        | CallMethodR | CallNativeR | CallGlobalR | CallSectionR | CallGlobalSectionR | CallMethodSectionR | CallValueSectionR | CallCtorSectionR | CallMethodGlobalSectionR => (false, true, true),
        StoreGlobalAtR => (false, true, false),
        LoadGlobalR | LoadCellR | StoreCellR => (true, false, true),
        LoadGlobalAtR => (true, false, false),
        LenR => (true, true, false),
        // a section call's StoreTopR keeps its return point in `b`
        StoreTopR => (i.x == 0, false, true),
        // a loop step's exit lies in `a` and a register step's item in `b`
        ForIterR => (false, i.x == 0, true),
        _ => (true, true, true),
    };
    let top = |on: bool, r: u16| if on { r as usize + 1 } else { 0 };
    top(a, i.a).max(top(b, i.b)).max(top(c, i.c))
}

/* `reg_top` of an instruction whose fields name registers by its other fields */
fn reg_apart(i: &Ins) -> usize {
    use OpCode::*;
    match i.op {
        // an OpR keeps its operand in `c` and only a value GetIterR writes `a`
        OpR => (i.a as usize + 1).max(if i.b == NO_ROW { 0 } else { i.b as usize + 1 }),
        GetIterR => (i.b as usize + 1).max(if i.x as u16 == crate::syntax::parser::ITER_VALUE { i.a as usize + 1 } else { 0 }),
        // a build or runtime op names its row from `b` and its length in `c`
        BuildR | RuntimeR => (i.a as usize + 1).max(i.b as usize + i.c as usize),
        // a row op names its row by its first register and counts it in `c`
        RowsR => { let (pops, gives) = row_op_counts(i); i.a as usize + usize::from(pops.max(gives)) }
        RowsBackR => i.a as usize + 1,
        _ => 0,
    }
}

/* `pushes` of `ins` with the first stack past the values it meets settling it */
#[cold]
#[inline(never)]
fn pushes_of(ins: &[Ins]) -> u8 {
    let mut pushes = 1;
    for i in ins {
        match i.op {
            OpCode::GetIter | OpCode::GetIterR => pushes = 2,
            OpCode::SetupExcept | OpCode::SetupFinally | OpCode::BeginArgs | OpCode::BeginFinally | OpCode::EndFinally | OpCode::UnwindFinally => return 3,
            _ => {}
        }
    }
    pushes
}

/* the compiler's own code unchanged so each ip it marks is that instruction */
pub(crate) fn identity(src: &[Instruction], names: usize, kinds: Rc<[Kind]>, module: usize) -> Code {
    // room for the sentinel past the end
    let mut ins: Vec<Ins> = Vec::with_capacity(src.len() + 1);
    ins.extend(src.iter().map(|&i| Ins::of(i)));
    // a fused call keeps both halves with the first carrying the counts past the second
    for k in 1..ins.len() {
        if ins[k].op == OpCode::CallMethodArgs && ins[k - 1].op == OpCode::CallMethod { ins[k - 1].b = ins[k].a; ins[k - 1].x = 1; }
    }
    let cache = super::cache::OpcodeCache::new(ins.len());
    // only the lowering writes register forms so this code names no register past its names
    debug_assert!(reg_bound(&ins) <= names, "a register form in unlowered code");
    let ins = seal(ins, names);
    let layout = next_layout();
    Code { ins, orig: Vec::new(), at: Vec::new(), back: Vec::new(), frame: names, kinds, module, hot: Cell::new(0), cache, pure: Cell::new(2), block: None, plain: OnceCell::new(), layout, misses: Cell::new(0), read: Cell::new(0), rows: 0, cbase: names as u16, slots: 0, consts: crate::r#unsafe::Consts::NONE, #[cfg(debug_assertions)] depths: Vec::new() }
}

// the most parameters a section takes with those past five moved from the call's registers
const MAX_ARGS: usize = 12;

// past this many pending values the lowering spills them so temporaries stay few
pub(crate) const MAX_PENDING: usize = 16;
// most values under a register call kept in registers that its stub moves through rows
pub(crate) const MAX_UNDER: usize = 6;

// a keyword stub's head and call and the row its operands move into
type KwStub = (usize, Vec<u16>, u16, Option<(u16, bool)>);

/* a stack value kept in a register until the stack needs it */
#[derive(Clone, Copy, PartialEq)]
struct Entry {
    r: u16,
    // a local read put off until its use and bound by then
    name: bool,
}

impl Entry {
    const fn reg(r: u16) -> Self { Self { r, name: false } }
}




/* what a section's body holds that its laying looks for read in one pass */
struct Marks { makes: bool, deletes: bool, builds: bool, tried: bool, stacked: bool, yields: usize, yields_stack: bool }

fn body_marks(ins: &[Ins]) -> Marks {
    use OpCode::*;
    let mut m = Marks { makes: false, deletes: false, builds: false, tried: false, stacked: false, yields: 0, yields_stack: false };
    for i in ins {
        match i.op {
            MakeFunction => { m.makes = true; m.builds = true; }
            MakeCoroutine => m.makes = true,
            OpR if i.x == MakeFunction as u8 => { m.makes = true; m.builds = true; }
            OpR if i.x == MakeCoroutine as u8 => m.makes = true,
            Del => m.deletes = true,
            BuildList | BuildDict | BuildSet | CallNative | CallNativeR | ListToTuple | ListToTupleR | RuntimeR => m.builds = true,
            BuildR if matches!(OpCode::ALL[(i.x & !TAKEN) as usize], BuildList | BuildDict | BuildSet) => m.builds = true,
            SetupExcept | SetupFinally => m.tried = true,
            ReturnValue | StoreName => m.stacked = true,
            Yield => { m.yields += 1; m.yields_stack |= i.x == 0; }
            _ => {}
        }
    }
    m
}

/* lowers fused stack code to registers with `sections` giving the function a call may lay */
pub(crate) fn lower<'p>(src: &[Instruction], chunk: &SSAChunk, facts: &Facts, sections: &dyn Fn(Want) -> Option<Callee<'p>>) -> Option<Code> {
    // class bodies and awaits and chunks past what sixteen bits count run unlowered
    if !facts.regs || facts.shape & AWAITS != 0 || src.len() >= usize::from(u16::MAX) { return None; }
    let mut more = false;
    let code = lower_in(src, chunk, facts, sections, false, &mut more);
    if !more { return code; }
    // a spill the bound missed lays a row for every value the code holds
    debug_assert!(false, "the rows bound of {:?} missed a spill", chunk.qualname);
    lower_in(src, chunk, facts, sections, true, &mut more)
}

/* `lower` keeping values in rows with `more` set when a spill needs more */
fn lower_in<'p>(src: &[Instruction], chunk: &SSAChunk, facts: &Facts, sections: &dyn Fn(Want) -> Option<Callee<'p>>, wide: bool, more: &mut bool) -> Option<Code> {
    let (names, consts) = (chunk.names.len(), chunk.constants.len());
    let len = src.len();
    // a loop's step stores its item into a local or a binding no builtin redirects
    let stores = |s: u16| (s as usize) < names && match facts.kinds.get(s as usize) {
        Some(Kind::Local) => true,
        Some(Kind::Global(_)) => facts.natives.function_id(chunk.names.get(s as usize).map_or("", |n| ssa_strip(n))).is_none(),
        _ => false,
    };
    // code of one block marks no instruction
    let blocks = facts.shape & JUMPS != 0;
    let mut flags = if blocks { vec![0u32; len + 1] } else { Vec::new() };
    // each instruction's depth one past until the lowering writes where it starts
    let mut at = vec![0u32; len + 1];
    let settled = if blocks || facts.regs { blocks_and_depths(src, chunk, facts, &stores, &mut at, &mut flags) } else { None };
    // a module's hot loop lowers from the statement opening its outermost loop
    let skip = if facts.once && facts.regs && facts.from > 0 && facts.from < len && settled.is_some() {
        src.iter().enumerate().filter(|&(j, i)| i.opcode.is_jump() && (i.operand as usize) <= facts.from && j >= facts.from).map(|(_, i)| i.operand as usize).min()
            .and_then(|h| (0..h).rev().find(|&k| at[k] == 1)).unwrap_or(0)
    } else { 0 };
    // what never runs again starts at the trap laid first
    at[..skip].fill(0);
    let rows = match settled {
        Some(_) if wide => at.iter().filter(|&&a| a != 0).map(|&a| a - 1).max().unwrap_or(0) as u16,
        Some(need) => need,
        None => 0,
    };
    // code whose depths settle nowhere runs as the compiler wrote it
    settled?;
    if let Some(f) = flags.first_mut() { *f |= START; }
    let mut deleted = Vec::new();
    for ins in src.iter().filter(|i| facts.shape & DELETES != 0 && i.opcode == OpCode::Del) {
        deleted.resize(names, false);
        if let Some(s) = deleted.get_mut(ins.operand as usize) { *s = true; }
    }
    // code of one block binds at its start only what its call binds
    let one = flags.is_empty() || !flags[1..len.max(1)].iter().any(|f| f & START != 0);
    let bound_at = (facts.regs && !one).then(|| bound_on_entry(src, chunk, names, facts, &mut flags));
    let (appends, resets) = if facts.regs && facts.shape & APPENDS != 0 { append_loops(src, facts) } else { (Vec::new(), Vec::new()) };
    // code keeping values in rows lays them past its names and its constants past them
    let s0 = super::rows::first_row(names);
    let cbase = if rows > 0 { s0 + rows as usize } else { names };
    let tbase = cbase + consts + 3;
    // a module's instructions no loop reruns lay no calls since what they call runs once
    let once = if facts.once && facts.regs { runs_once(src) } else { Vec::new() };
    let mut l = Lower {
        src, chunk, facts, flags: &flags, deleted, names, cbase, consts, tbase,
        out: Vec::with_capacity(len), orig: Vec::with_capacity(len + 1), at, back: Vec::with_capacity(len + 1),
        pending: Vec::with_capacity(MAX_PENDING), patches: Vec::new(), temps: 0, last: None, cur: 0, quiet: 0, bound: vec![0; names.div_ceil(64)],
        appends, lists: core::cell::OnceCell::new(), sections, wanted: Vec::new(), heads: Vec::new(), prep: Vec::new(), seeds: Vec::new(), dest: None, resume: None,
        gaps: 0, kwcall: None, kwstubs: Vec::new(), stub_row: (0, 0), kwrow: None, alts: Vec::new(), lookups: Vec::new(), chain_heads: Vec::new(), yields: Vec::new(),
        genfors: Vec::new(), gen_next: None, gen_args: Vec::new(), ret_to: None, nested: Vec::new(), gen_loops: Vec::new(), gen_steps: Vec::new(),
        keyed: false, more_args: Vec::new(), wide: false, once: false, old: None, first: 0, alts_first: 0, only: None, calls: OnceCell::new(), displaced: Vec::new(), tern: None,
        s0, rows: rows as usize, overflow: false, entry: Vec::new(), pops: 0, resumes: Vec::new(), prefer: None, spread_depths: None, refused: false,
    };
    let mut i = skip;
    // what never runs again raises if anything enters it
    if skip > 0 { l.emit(Ins { op: OpCode::RaiseR, x: 1, a: 0, b: 0, c: 0 }); }
    let terns = chunk.ternaries();
    // whether a path reaches the block being lowered
    let mut live = true;
    while i < len {
        // what falls into a block lands in its rows
        if flags.get(i).map_or(i == 0, |f| f & START != 0) {
            // a ternary's arms keep what was pending and its join takes either value
            match l.tern.as_deref() {
                Some(t) if terns && (t.else_at == i || t.start_a == i) => {}
                Some(t) if terns && t.join == i && !t.bail => l.join_tern(),
                _ => live = l.start(i, live),
            }
            // a block no path reaches lowers to nothing up to the next block
            if !live {
                let next = (i + 1..len).find(|&k| flags.get(k).is_some_and(|f| f & START != 0)).unwrap_or(len);
                l.at[i..next].fill(l.out.len() as u32);
                i = next;
                continue;
            }
            l.quiet = i as u32;
            match &bound_at {
                Some(b) => b.entry(&flags, i, &mut l.bound),
                None => {
                    l.bound.fill(0);
                    if facts.regs { facts.bind(&mut l.bound); }
                }
            }
            // entering a loop that appends to a local gives up its owned string
            for &(_, slot) in resets.iter().filter(|r| r.0 == i) { l.emit(Ins { op: OpCode::OwnResetR, x: 0, a: slot, b: 0, c: 0 }); }
        }
        l.cur = i as u32;
        l.once = once.get(i).copied().unwrap_or(false);
        let entered = l.resume.take();
        debug_assert!(l.tern.is_some() || l.at[i] == 0 || l.pending.len() + 1 == l.at[i] as usize, "{:?} lowered at {i} at another depth", chunk.qualname);
        // past bit 16 the rows a resume there finds under the store it enters
        let pops = if entered.is_some() { core::mem::take(&mut l.pops).max(1) as usize } else { 0 };
        l.at[i] = entered.unwrap_or(l.out.len()) as u32 | (l.pending.len().saturating_sub(pops) as u32) << 16;
        let before = l.out.len();
        let step = l.one(i);
        // a fused group's later instructions start where its last one does
        let under = (l.pending.len() as u32) << 16;
        for k in 1..step { l.at[i + k] = before.max(l.out.len().saturating_sub(1)) as u32 | under; }
        // only stores binds and deletes change what is bound
        for (k, &ins) in src[i..i + step].iter().enumerate().filter(|(_, s)| matches!(s.opcode, OpCode::StoreName | OpCode::Phi | OpCode::Del)) { transfer(&mut l.bound, ins, i + k, chunk); }
        i += step;
        if l.settled() { l.quiet = i as u32; }
    }
    l.flush();
    if l.overflow { *more = true; return None; }
    if l.refused { return None; }
    if terns && l.tern.is_some() { return None; }
    l.at[len] = l.out.len() as u32;
    for &p in &l.patches {
        let t = l.out[p].a as usize;
        // a jump back to a loop over a generator's section goes to its step
        l.out[p].a = match l.genfors.iter().find(|g| g.at == t) { Some(g) => g.step as u16, None if t <= len => l.at[t] as u16, None => u16::MAX };
    }
    let frame = if facts.regs { tbase + l.temps } else { names };
    // wanted sections lie past the code behind a jump and their registers past the temporaries
    let (block, frame) = l.lay(frame);
    if l.out.len() >= u16::MAX as usize { return None; }
    // the register loop reads registers unchecked so the frame covers every one it names
    debug_assert!(reg_bound(&l.out) <= frame, "a register past the frame");
    let frame = frame.max(reg_bound(&l.out));
    // registers are u16 operands
    if frame >= u16::MAX as usize { return None; }
    l.orig.push(len as u16);
    l.back.push(len as u16);
    // a register `yield` keeps its next step's start in `c` and that instruction in `b`
    for &k in &l.yields {
        let next = match l.out.get(k + 1) { Some(j) if j.op == OpCode::Jump && (j.a as usize) < l.out.len() => j.a as usize, _ => k + 1 };
        (l.out[k].b, l.out[k].c) = (l.back[next], next as u16);
        // a loop step's `c` of 1 marks a loop stepped in place
        if next + 1 == k && l.out[next].op == OpCode::ForIterR && l.out[next].x == 0 && l.out[next].b == l.out[k].a { l.out[next].c = 1; }
    }
    let cache = super::cache::OpcodeCache::new(l.out.len());
    for &(at, site) in l.seeds.iter().filter(|s| s.0 & SEEDED != 0) { cache.set_site(at & !SEEDED, site); }
    // section calls and returns take their kind's instruction after lowering so none checks flags
    if block.is_some() { specialize(&mut l.out, 0); }
    // sectioned code is never laid and others are judged once a call would lay them
    let pure = Cell::new(if block.is_none() { 0 } else { 2 });
    let layout = next_layout();
    let mut block = block;
    if let Some(b) = block.as_mut() { b.lineage = layout; }
    let at = l.at;
    let code = Code { ins: seal(l.out, frame), orig: l.orig, at, back: l.back, frame, kinds: facts.kinds.clone(), module: facts.module, hot: Cell::new(0), cache, pure, block, plain: OnceCell::new(), layout, misses: Cell::new(0), read: Cell::new(0), rows: 0, cbase: cbase as u16, slots: l.s0 as u16, consts: crate::r#unsafe::Consts::NONE, #[cfg(debug_assertions)] depths: Vec::new() };
    Some(code)
}


/* marks blocks in `flags` and depths in `at` giving the rows the code lays */
#[inline(never)]
fn blocks_and_depths(src: &[Instruction], chunk: &SSAChunk, facts: &Facts, stores: &impl Fn(u16) -> bool, at: &mut [u32], flags: &mut [u32]) -> Option<u16> {
    use super::depth::{BIAS, BINARY, CLOSER, NONE, PENDING, STEPS};
    let len = src.len();
    let (at, blocks) = (&mut at[..=len], !flags.is_empty());
    // a loop's back-edge checks the depth its head was first entered at
    let loops = facts.shape & LOOPS != 0;
    // code whose depth settles keeps the values its joins calls and parks cross in rows
    let (mut d, mut need, mut i) = (0usize, 0usize, 0usize);
    // rows for joins a stack op takes whole which only code laying rows anyway lays
    let mut joins = 0usize;
    // code that keeps no rows only marks its blocks
    let keeps = facts.regs;
    let settled = keeps && loop {
        if i == len { break true; }
        let ins = src[i];
        let e = STEPS[ins.opcode as usize];
        if loops {
            // a target a jump ahead noted and the flow falling in must agree
            let noted = at[i] as usize;
            if noted != 0 && noted != d + 1 { break false; }
            at[i] = d as u32 + 1;
        }
        // an underflow wraps past the bound the next closer look takes to the worklist
        if e < BINARY { d = (d + usize::from(e)).wrapping_sub(usize::from(BIAS)); i += 1; continue; }
        if e == BINARY && ins.operand < 0x80 && d < PENDING { d = d.wrapping_sub(1); i += 1; continue; }
        let t = ins.operand as usize;
        match ins.opcode {
            // the commonest jumps and the return step here and the rest take the closer look
            OpCode::JumpIfFalse | OpCode::Jump if (1..PENDING).contains(&d) && t <= len => {
                let (e, back) = (if ins.opcode == OpCode::Jump { d } else { d - 1 }, t <= i);
                let noted = at[t] as usize;
                if back && noted != e + 1 || !back && noted != 0 && noted != e + 1 { break false; }
                if !back { at[t] = e as u32 + 1; }
                // a jump to a return returns its value at once and needs no row
                if ins.opcode == OpCode::Jump && src.get(t).is_some_and(|r| takes_all(r, e)) { if src[t].opcode != OpCode::ReturnValue { joins = joins.max(e); } } else { need = need.max(e); }
                d = if ins.opcode == OpCode::Jump { NONE } else { e };
                if blocks { mark_block(src, i, flags); }
            }
            OpCode::Jump if d == 0 && t <= len => {
                let noted = at[t] as usize;
                if t <= i && noted != 1 || t > i && noted > 1 { break false; }
                if t > i { at[t] = 1; }
                d = NONE;
                if blocks { mark_block(src, i, flags); }
            }
            OpCode::ReturnValue if d < PENDING => {
                d = NONE;
                if blocks { flags[i + 1] |= START; }
            }
            // a loop's step notes its exit and the block its unstored item starts
            OpCode::ForIter if d < PENDING && t > i && t <= len => {
                let noted = at[t] as usize;
                if noted != 0 && noted != d + 1 { break false; }
                at[t] = d as u32 + 1;
                need = need.max(if super::depth::stepped(src, i, stores) { d } else { d + 1 });
                d += 1;
                if blocks { mark_block(src, i, flags); }
            }
            OpCode::GetIter if (1..PENDING).contains(&d) => d = d - 1 + usize::from(ins.operand == crate::syntax::parser::ITER_VALUE),
            // a call lays what lies under it in rows and a build takes its items
            OpCode::Call | OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildSet | OpCode::BuildString if d < PENDING => {
                let pops = if ins.opcode == OpCode::Call { 1 + (t & 0xFF) + 2 * (t >> 8) } else { t };
                if pops > d { break false; }
                // a call past five positionals or with keywords runs over its row
                if ins.opcode == OpCode::Call && t > 5 { need = need.max(d); } else if matches!(ins.opcode, OpCode::Call | OpCode::BuildSet) { need = need.max(d - pops); }
                d = d - pops + 1;
            }
            OpCode::CallNative if d < PENDING && let Some(site) = native_site(ins.operand, &chunk.native_sites) && !site.spread => {
                let pops = usize::from(site.pos) + 2 * usize::from(site.kw);
                if pops > d { break false; }
                // a length's register form leaves what lies under it where it is
                let len = site.pos == 1 && site.kw == 0 && !site.void && facts.natives.get(crate::lang::NativeId(site.id)).is_some_and(|f| f.intrinsic == crate::lang::Intrinsic::Len);
                if site.kw != 0 || site.pos > 5 { need = need.max(d); } else if !len { need = need.max(d - pops); }
                d = d - pops + usize::from(!site.void);
            }
            _ => {
                let Some(next) = super::depth::closer(src, i, chunk, stores, at, d, need) else { break false };
                (d, need) = next;
                if e == CLOSER && blocks { mark_block(src, i, flags); }
            }
        }
        i += 1;
        // what follows a jump away runs only where a jump ahead noted its depth
        while d == NONE && i < len {
            if at[i] != 0 { d = at[i] as usize - 1; break; }
            if blocks && STEPS[src[i].opcode as usize] == CLOSER { mark_block(src, i, flags); }
            i += 1;
        }
        if d == NONE { break true; }
    };
    // the blocks past where the pass stopped still start where their jumps say
    if blocks { for k in i..len { if STEPS[src[k].opcode as usize] == CLOSER { mark_block(src, k, flags); } } }
    if !keeps { return None; }
    if settled && d != NONE {
        if d > 4 * len + 4 || at[len] != 0 && at[len] as usize != d + 1 { return super::depth::kept_rows(src, chunk, stores, at); }
        at[len] = d as u32 + 1;
    }
    if !settled { return super::depth::kept_rows(src, chunk, stores, at); }
    Some(need.max(joins).min(usize::from(u16::MAX)) as u16)
}

/* whether `ins` at a join takes all `e` values at once as its stack form */
fn takes_all(ins: &Instruction, e: usize) -> bool {
    match ins.opcode {
        OpCode::ReturnValue => e == 1,
        OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildSet | OpCode::BuildString => usize::from(ins.operand) == e,
        _ => false,
    }
}

/* marks the blocks the jump or end of flow at `i` starts */
#[inline]
fn mark_block(src: &[Instruction], i: usize, flags: &mut [u32]) {
    let (ins, len) = (src[i], src.len());
    if ins.opcode.is_jump() && (ins.operand as usize) <= len {
        let forward = ins.opcode == OpCode::Jump && ins.operand as usize > i + 1;
        flags[ins.operand as usize] |= TARGET | START | if forward { FORWARD } else { ENTERED };
    }
    // an unwind `Goto` resumes at the instruction after UnwindFinally
    if ins.opcode == OpCode::UnwindFinally { flags[i + 1] |= TARGET | START | ENTERED; }
    // a handler a catch enters takes its exception off the stack
    if ins.opcode == OpCode::SetupExcept && (ins.operand as usize) <= len { flags[ins.operand as usize] |= CAUGHT; }
    if ins.opcode.is_jump() || ends_flow(ins.opcode) { flags[i + 1] |= START; }
}

/* `at` with the rows under each resume entry of `entry` past bit 16 */
fn packed(mut at: Vec<u32>, entry: &[u16]) -> Vec<u32> {
    for (a, &e) in at.iter_mut().zip(entry) { *a |= (e as u32) << 16; }
    at
}

/* instructions a jump or handler or finished `finally` enters which no fusion may swallow */
fn entered(src: &[Instruction]) -> Vec<bool> {
    let mut entered = vec![false; src.len() + 1];
    for (k, ins) in src.iter().enumerate() {
        if ins.opcode.is_jump() && (ins.operand as usize) <= src.len() { entered[ins.operand as usize] = true; }
        // an unwind `Goto` resumes at the instruction after UnwindFinally
        if ins.opcode == OpCode::UnwindFinally { entered[k + 1] = true; }
    }
    entered
}

/* no instruction after it runs from it */
fn ends_flow(op: OpCode) -> bool { op.ends_flow() }

/* a build a call takes at once which laying reads as its stack form */
pub(crate) const TAKEN: u8 = 128;

/* the values build ins pops with two for each pair of a dict */
fn build_len(ins: Instruction) -> usize { ins.operand as usize * if ins.opcode == OpCode::BuildDict { 2 } else { 1 } }

// instruction flags for entries and block starts with the block's row past bit `ROW`
const TARGET: u32 = 1;
const START: u32 = 2;
const FORWARD: u32 = 4;
const ENTERED: u32 = 8;
// a handler's start where its catch hands the exception on the stack
const CAUGHT: u32 = 16;
const ROW: u32 = 5;

/* slots bound entering block `i` of `rows` and none when no path reaches it */
#[inline]
fn entry(rows: &[u64], words: usize, flags: &[u32], i: usize, out: &mut [u64]) {
    let f = flags.get(i).copied().unwrap_or(0);
    let at = (f >> ROW) as usize * (words + 1);
    if f & START == 0 || rows[at + words] == 0 { out.fill(0); return; }
    for (o, &b) in out.iter_mut().zip(&rows[at..at + words]) { *o = b; }
}

/* narrows block `t` of `rows` to what `s` binds and tells whether that changed it */
#[inline]
fn merge(rows: &mut [u64], words: usize, flags: &[u32], t: usize, s: &[u64]) -> bool {
    let f = flags.get(t).copied().unwrap_or(0);
    if f & START == 0 { return false; }
    let at = (f >> ROW) as usize * (words + 1);
    let fresh = core::mem::replace(&mut rows[at + words], 1) == 0;
    let mut changed = fresh;
    for (o, &n) in rows[at..at + words].iter_mut().zip(s) {
        let m = if fresh { n } else { *o & n };
        changed |= m != *o;
        *o = m;
    }
    changed
}

/* whether a body calls binding `g` which means recursion when it was called through `g` */
fn recurs(ins: &[Ins], g: u16) -> bool {
    g != u16::MAX && ins.iter().enumerate().any(|(j, &i)| matches!(i.op, OpCode::CallGlobalR | OpCode::CallR) && super::block::global_of(ins, j, i) == Some(g))
}

/* for each instruction whether no jump back from past it lands at or before it */
fn runs_once(src: &[Instruction]) -> Vec<bool> {
    let mut once = vec![true; src.len()];
    // each back-edge's loop runs again from its head to the jump
    for (j, ins) in src.iter().enumerate().filter(|(j, i)| i.opcode.is_jump() && (i.operand as usize) <= *j) { once[ins.operand as usize..=j].fill(false); }
    once
}

struct Lower<'c, 'p> {
    src: &'c [Instruction],
    chunk: &'c SSAChunk,
    facts: &'c Facts,
    // each instruction's entry and block start flags with the block's row past them
    flags: &'c [u32],
    deleted: Vec<bool>,
    names: usize,
    cbase: usize,
    consts: usize,
    tbase: usize,
    out: Vec<Ins>,
    orig: Vec<u16>,
    at: Vec<u32>,
    back: Vec<u16>,
    pending: Vec<Entry>,
    // lowered jumps whose target is still a compiler index
    patches: Vec<usize>,
    temps: usize,
    // the last instruction when it wrote a fresh temporary a store may take over
    last: Option<usize>,
    cur: u32,
    // the InPlaceAdd instructions that append to a local their loop only ever appends to
    appends: Vec<bool>,
    // the locals that only ever hold the list they were made with as `list_locals` finds
    lists: core::cell::OnceCell<Vec<bool>>,
    // the first compiler instruction whose work no emitted instruction has finished
    quiet: u32,
    bound: Vec<u64>,
    sections: &'c dyn Fn(Want) -> Option<Callee<'p>>,
    // functions to lay as sections by calling binding and each call's place and section
    wanted: Vec<(u16, Callee<'p>)>,
    heads: Vec<(usize, usize, Ins)>,
    // section call operands set once sections are laid as parameter registers or constant slots
    prep: Vec<(usize, u8, Prep)>,
    // sites method stubs start with by head or once laid by their call marked `SEEDED`
    seeds: Vec<(usize, super::sites::Site)>,
    // a call's StoreTopR whose register a store after it may take over
    dest: Option<usize>,
    // the section call StoreTopR a resume enters after the last call left its value stacked
    resume: Option<usize>,
    // the next head's defaulted keyword parameters and the registers and operand its stub calls with
    gaps: u8,
    kwcall: Option<(Vec<u16>, u16)>,
    // the keyword calls among the heads by the head's place
    kwstubs: Vec<KwStub>,
    // the row every keyword stub moves its operands into and its length
    stub_row: (u16, usize),
    // the row the next keyword stub's operands move into and whether they lie there already
    kwrow: Option<(u16, bool)>,
    // each head's section with plain sections of at most two parameters its call checks next
    alts: Vec<(usize, usize)>,
    // lookups stubs lay by place with their head's section and given their table once made
    lookups: Vec<(usize, usize)>,
    // where each `yield` of a register lies
    yields: Vec<usize>,
    // loops stepping a generator's section and the one whose step comes next
    genfors: Vec<GenFor>,
    gen_next: Option<GenNext>,
    // calls through a generator's parameter given a laid function by section and place and callee
    gen_args: Vec<(usize, usize, usize)>,
    // the next head returns past its call unless a generator `yield` returns into the loop
    ret_to: Option<u16>,
    // loops over a parameter given a generator result by section and PushRegs and stepped section
    nested: Vec<(usize, usize, usize)>,
    // each generator section's loop by exit and variable and body start and whether own code
    gen_loops: Vec<(usize, u16, Option<u16>, u16, bool)>,
    // a growth's generator section for another's loop and the step register that loop always reads
    gen_steps: Vec<(usize, u16)>,
    // the next section wanted is for a keyword or packing call filling past the positionals
    keyed: bool,
    // the instruction being lowered runs once as a module's outside its loops does
    once: bool,
    // the registers of the next head's arguments past the fifth
    more_args: Vec<u16>,
    // the next section wanted is for a call naming arguments past the fifth
    wide: bool,
    // heads a growth chains past a turned-down one by place with the section each checks
    chain_heads: Vec<(usize, usize)>,
    // a growth's block whose first `first` sections and `alts_first` pairs of `alts` are laid already
    old: Option<&'c Block>,
    first: usize,
    alts_first: usize,
    // a growth's one function the head met which alone it lays beside those it has
    only: Option<Val>,
    // where each head laid past the end for a grown call returns and lies
    displaced: Vec<(u32, u32)>,
    // while sections are laid the calls each makes of the others found when first asked
    calls: OnceCell<Vec<OnceCell<Calls>>>,
    /* the ternary lowered with pending values in registers and one open at the end fails */
    tern: Option<alloc::boxed::Box<Tern>>,
    // the first row of code keeping its values in rows or zero
    s0: usize,
    // the rows its frame lays which no value it spills reaches past
    rows: usize,
    // a spill past the rows laid so the code is lowered again with more
    overflow: bool,
    // each compiler instruction's rows under the store a resume there enters
    entry: Vec<u16>,
    // the values the store the next resume enters takes off the stack
    pops: u16,
    // each head with temporaries under it and the row each comes back from
    resumes: Vec<(usize, u16, u16)>,
    // the register the next call's value lands in when free
    prefer: Option<u16>,
    // the chunk's depths once a spread call needs them for its packed operands
    spread_depths: Option<Vec<u16>>,
    // a spread call whose packed operands no word names so the code is not lowered
    refused: bool,
}


/* calls in a section's body that jump to sections each with its target section */
type Calls = Rc<[(usize, usize)]>;

impl<'p> Lower<'_, 'p> {
    /* the checks a call or a spread takes before `one` matches its op */
    fn call_first(&mut self, i: usize, ins: Instruction, regs: bool) -> Option<usize> {
        let op = ins.operand;
        if ins.opcode == OpCode::Call && regs && op >> 8 == 0 && let Some(n) = self.gen_for(i, op) { return Some(n); }
        if ins.opcode == OpCode::BeginArgs && regs && let Some(n) = self.spread(i) { return Some(n); }
        if ins.opcode == OpCode::Call && regs && (6..=MAX_ARGS as u16).contains(&op) && let Some(n) = self.many_call(i, op as usize) { return Some(n); }
        if ins.opcode == OpCode::Call && regs && op >> 8 != 0 && let Some(n) = self.kw_call(i, op) { return Some(n); }
        if ins.opcode == OpCode::Call && regs && let Some(n) = self.spread_call(i, op) { return Some(n); }
        None
    }

    /* lowers instruction `i` and gives the count of compiler instructions it took */
    fn one(&mut self, i: usize) -> usize {
        let ins = self.src[i];
        let op = ins.operand;
        let regs = self.facts.regs;
        // only calls and their spreads take the checks before the match
        if matches!(ins.opcode, OpCode::Call | OpCode::CallMethod | OpCode::BeginArgs) && let Some(n) = self.call_first(i, ins, regs) { return n; }
        match ins.opcode {
            OpCode::LoadName if regs && (op as usize) < self.names => self.load_name(op),
            OpCode::LoadConst if regs && (op as usize) < self.consts => self.push(Entry { r: (self.cbase + op as usize) as u16, name: false }),
            OpCode::LoadNone if regs => self.push(Entry::reg((self.cbase + self.consts) as u16)),
            OpCode::LoadTrue if regs => self.push(Entry::reg((self.cbase + self.consts + 1) as u16)),
            OpCode::LoadFalse if regs => self.push(Entry::reg((self.cbase + self.consts + 2) as u16)),
            OpCode::StoreName if regs && (op as usize) < self.names && !self.pending.is_empty() => self.store_name(op),
            // a `global` name reads and writes its module binding by index
            OpCode::LoadGlobal if regs && let Some(Kind::Global(g)) = self.kind(op) => {
                let t = self.dest();
                self.produce(Ins { op: OpCode::LoadGlobalR, x: 0, a: t, b: g as u16, c: 0 });
            }
            OpCode::StoreGlobal if regs && !self.pending.is_empty() && let Some(Kind::Global(g)) = self.kind(op) => {
                let v = self.pop();
                // a builtin's store redirects its fused calls
                self.emit(Ins { op: OpCode::StoreGlobalR, x: self.builtin_flag(op), a: g as u16, b: v.r, c: 0 });
            }
            OpCode::PopTop if !self.pending.is_empty() => { self.pending.pop(); }
            // a declaration does nothing at run time
            OpCode::Global | OpCode::Outer => {}
            OpCode::Dup if !self.pending.is_empty() => { let e = self.pending[self.pending.len() - 1]; self.push(e); }
            OpCode::Swap if self.pending.len() >= 2 => { let k = self.pending.len(); self.pending.swap(k - 1, k - 2); }
            OpCode::Rot3 if self.pending.len() >= 3 => {
                let k = self.pending.len();
                self.pending[k - 3..].rotate_left(1);
            }
            OpCode::Dup2 if self.pending.len() >= 2 => {
                let k = self.pending.len();
                let (a, b) = (self.pending[k - 2], self.pending[k - 1]);
                self.push(a);
                self.push(b);
            }
            // `s += t` on a local its loop only appends to grows its owned string
            OpCode::InPlaceAdd if self.appends.get(i).copied().unwrap_or(false) && self.pending.len() >= 2
                && self.pending[self.pending.len() - 2] == (Entry { r: self.src[i + 1].operand, name: true }) => {
                let (_, t) = self.pop2();
                let mut t = [t];
                self.settle_with(&mut t);
                self.emit(Ins { op: OpCode::AppendR, x: 0, a: self.src[i + 1].operand, b: t[0].r, c: 0 });
                return 2;
            }
            code if binary_form(code).is_some() && self.pending.len() >= 2 && op < AUGMENTED as u16 => return self.binary(i, code, op as u8),
            OpCode::Minus | OpCode::Not if !self.pending.is_empty() => {
                let v = self.pop();
                let t = self.dest();
                let form = if ins.opcode == OpCode::Minus { OpCode::MinusR } else { OpCode::NotR };
                self.produce(Ins { op: form, x: 0, a: t, b: v.r, c: 0 });
            }
            OpCode::JumpIfFalse if self.chunk.ternaries() && self.pending.len() > 1 && self.tern.is_none() && let Some((end_a, join)) = self.ternary(i) => {
                let c = self.pop();
                self.jump(OpCode::JumpIfFalseR, op, c.r, 0);
                self.tern = Some(alloc::boxed::Box::new(Tern { start_a: i + 1, else_at: op as usize, end_a, join, saved: self.pending.clone(), t: None, bail: false }));
            }
            // a ternary's first arm leaves its value where the second will keeping what was pending
            OpCode::Jump if self.chunk.ternaries() && self.tern.as_ref().is_some_and(|t| t.end_a == i && !t.bail) => {
                let Some(mut tern) = self.tern.take() else { return 1 };
                match arm_value(&self.pending, &tern.saved) {
                    Some(v) => {
                        // the value its own row holds is where the second arm leaves its own
                        let own = tern.saved.len() < self.rows && v.r as usize == self.s0 + tern.saved.len();
                        let t = if !v.name && (self.is_temp(v.r as usize) || own) { v.r } else {
                            let t = self.temp();
                            self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: v.r, c: 0 });
                            t
                        };
                        tern.t = Some(t);
                        self.pending.clone_from(&tern.saved);
                        self.emit_orig(ins);
                    }
                    None => tern.bail = true,
                }
                self.tern = Some(tern);
            }
            OpCode::JumpIfFalse if !self.pending.is_empty() => {
                let mut c = [self.pop()];
                self.settle_rows_with(self.pending.len(), &mut c, false);
                self.jump(OpCode::JumpIfFalseR, op, c[0].r, 0);
            }
            // a handler's test of two pending values writes its truth to a temporary
            OpCode::MatchExc if regs && self.pending.len() >= 2 => {
                let (e, c) = self.pop2();
                let t = self.dest();
                self.produce(Ins { op: OpCode::MatchExcR, x: 0, a: t, b: e.r, c: c.r });
            }
            OpCode::ListToTuple if regs && !self.pending.is_empty() => {
                let l = self.pop();
                let t = self.dest();
                self.produce(Ins { op: OpCode::ListToTupleR, x: 0, a: t, b: l.r, c: 0 });
            }
            OpCode::CallNative if regs && let Some(site) = native_site(op, &self.chunk.native_sites) && !site.spread && site.kw == 0
                && site.pos as usize <= NATIVE_ARGS && self.pending.len() >= site.pos as usize => return self.native(i, site),
            // the accumulator stays on the stack or over rows in its register
            OpCode::ListAppend | OpCode::SetAdd if !self.pending.is_empty() => {
                let v = self.pop();
                let form = if ins.opcode == OpCode::ListAppend { OpCode::ListAppendR } else { OpCode::SetAddR };
                match self.pending.last() {
                    Some(&acc) => self.emit(Ins { op: form, x: 1, a: acc.r, b: v.r, c: v.r }),
                    None => { self.flush(); self.emit(Ins { op: form, x: 0, a: 0, b: v.r, c: v.r }); }
                }
            }
            OpCode::MapAdd if self.pending.len() >= 2 => {
                let (k, v) = self.pop2();
                match self.pending.last() {
                    Some(&acc) => self.emit(Ins { op: OpCode::MapAddR, x: 1, a: acc.r, b: k.r, c: v.r }),
                    None => { self.flush(); self.emit(Ins { op: OpCode::MapAddR, x: 0, a: 0, b: k.r, c: v.r }); }
                }
            }
            // a short-circuit feeding a test jumps where the test would and never stacks its value
            OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop if !self.pending.is_empty()
                && let Some(test) = self.threaded_test(ins) => {
                let mut c = [self.pop()];
                self.settle_rows_with(self.pending.len(), &mut c, false);
                let c = c[0];
                if ins.opcode == OpCode::JumpIfFalseOrPop {
                    self.jump(OpCode::JumpIfFalseR, self.src[test].operand, c.r, 0);
                } else {
                    self.jump(OpCode::JumpIfFalseR, test as u16 + 1, c.r, 0);
                    if let Some(j) = self.out.last_mut() { j.x = 1; }
                }
            }
            // over rows the value stays in its row where the jump lands
            OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop if !self.pending.is_empty() => {
                self.flush();
                let c = self.pending[self.pending.len() - 1];
                self.jump(OpCode::JumpIfFalseR, op, c.r, 0);
                if ins.opcode == OpCode::JumpIfTrueOrPop && let Some(j) = self.out.last_mut() { j.x = 1; }
                self.pending.pop();
            }
            OpCode::ReturnValue if !self.pending.is_empty() => {
                let v = self.pop();
                // what stays below the value only leaves with the frame
                self.pending.clear();
                self.emit(Ins { op: OpCode::ReturnR, x: 0, a: 0, b: v.r, c: 0 });
            }
            // a pattern's length which no binding redirects
            OpCode::Len if !self.pending.is_empty() => {
                let o = self.pop();
                let t = self.dest();
                self.produce(Ins { op: OpCode::LenR, x: 0, a: t, b: o.r, c: ENGINE_LEN });
            }
            OpCode::GetItem if self.pending.len() >= 2 => {
                let (o, k) = self.pop2();
                let t = self.dest();
                self.produce(Ins { op: OpCode::GetItemR, x: 0, a: t, b: o.r, c: k.r });
            }
            OpCode::StoreItem if self.pending.len() >= 3 => {
                let (k, v) = self.pop2();
                let o = self.pop();
                self.emit(Ins { op: OpCode::StoreItemR, x: 0, a: o.r, b: k.r, c: v.r });
            }
            OpCode::LoadAttr if !self.pending.is_empty() => {
                let o = self.pop();
                let t = self.dest();
                self.produce(Ins { op: OpCode::GetAttrR, x: 0, a: t, b: o.r, c: op });
            }
            OpCode::StoreAttr if self.pending.len() >= 2 => {
                let (o, v) = self.pop2();
                self.emit(Ins { op: OpCode::SetAttrR, x: 0, a: o.r, b: v.r, c: op });
            }
            // a `yield` whose result goes unused hands its value from a register over nothing
            OpCode::Yield if regs && !self.pending.is_empty() && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::PopTop) && !self.target(i + 1) => return self.dropped_yield(),
            // a `yield` whose value lies in its row waits there for what it is sent
            OpCode::Yield if regs && !self.pending.is_empty() => {
                self.settle_rows(self.pending.len());
                self.quiet = self.cur;
                let slot = self.pending[self.pending.len() - 1].r;
                self.yields.push(self.out.len());
                self.emit(Ins { op: OpCode::Yield, x: 2, a: slot, b: 0, c: 0 });
            }
            // a laid generator's loop steps its section while unlowered or turned-down runs loop here plainly
            OpCode::ForIter if let Some(g) = self.gen_next.take() => {
                self.patches.push(self.out.len());
                self.emit(Ins { op: OpCode::ForIterR, x: 0, a: op, b: g.target, c: 0 });
                if g.global.is_some() { self.prep.push((self.out.len() - 1, 1, Prep::GenVal(g.k))); }
                self.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: 0, c: 0 });
                self.prep.extend([(self.out.len() - 1, 0, Prep::GenReg(g.k)), (self.out.len() - 1, 1, Prep::GenEntry(g.k))]);
                let into = self.out.len();
                self.emit(Ins { op: OpCode::Jump, x: 1, a: 0, b: 0, c: 0 });
                let step = self.out.len();
                self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: 0, b: 0, c: 0 });
                self.prep.extend([(step, 0, Prep::GenReg(g.k)), (step, 1, Prep::GenReg(g.k))]);
                self.out[g.skip].a = step as u16 + 1;
                let r = |l: &[u16], m: usize| l.get(m).copied().unwrap_or(0);
                let call = Ins { op: OpCode::CallR, x: g.list.len() as u8, a: g.regs[0], b: r(&g.list, 0), c: r(&g.list, 1) };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(&g.list, 2), b: r(&g.list, 3), c: r(&g.list, 4) };
                self.head(call, rest, g.target, g.k, true, &[]);
                if g.global.is_some() { self.prep.push((self.out.len() - HEADER as usize + 1, 0, Prep::GenVal(g.k))); }
                self.out[into].a = self.out.len() as u16;
                if let Some(b) = g.global {
                    self.emit(Ins { op: OpCode::StoreGlobalR, x: 0, a: b, b: 0, c: 0 });
                    self.prep.push((self.out.len() - 1, 1, Prep::GenVal(g.k)));
                }
                let a = &g.regs[1..];
                let call = Ins { op: OpCode::CallR, x: a.len() as u8, a: g.regs[0], b: r(a, 0), c: r(a, 1) };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(a, 2), b: r(a, 3), c: r(a, 4) };
                let head = self.out.len() - HEADER as usize - g.global.is_some() as usize;
                self.genfors.push(GenFor { head, k: g.k, step, at: i, exit: g.exit, target: g.target, global: g.global, call, rest });
                return 2;
            }
            // a comprehension made and called on its iterator at once runs its body's shared section
            OpCode::MakeFunction if regs && self.chunk.functions.get(op as usize).is_some_and(|f| f.2 == 0 && super::scope::is_comp(&f.0) && !f.1.is_generator) && i + 3 < self.src.len()
                && self.src[i + 1].opcode == OpCode::Swap && self.src[i + 2].opcode == OpCode::Call && self.src[i + 2].operand == 1 && !self.target(i + 1) && !self.target(i + 2) => {
                // the iterator stays in its register while what lies under it is spilled
                let held = self.pending.pop();
                let mut kept = [held.unwrap_or(Entry::reg(0))];
                if held.is_some() { self.settle_with(&mut kept); } else { self.settle(); }
                let under = self.pending.clone();
                // each is held pending while the next is taken so the two differ
                let it = if held.is_some() { kept[0].r } else { self.temp() };
                self.pending.push(Entry::reg(it));
                let f = self.temp();
                self.pending.clone_from(&under);
                if held.is_none() { self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: it, b: 0, c: 0 }); }
                self.emit(Ins { op: OpCode::OpR, x: ins.opcode as u8, a: f, b: NO_ROW, c: op });
                self.cur = i as u32 + 2;
                let join = self.target(i + 3);
                let call = Ins { op: OpCode::CallR, x: 1, a: f, b: it, c: 0 };
                // laid only in a loop or function body and elsewhere called through its register
                let again = (self.facts.bound != 0 || !self.facts.bound_far.is_empty()) || self.src.iter().enumerate().any(|(j, b)| b.opcode == OpCode::Jump && j > i && (b.operand as usize) <= i);
                let made = if again { (self.sections)(Want::Made(self.chunk.functions[op as usize].1.id.get())) } else { None };
                let Some(section) = made.and_then(|c| self.want_callee(u16::MAX, c, 1)) else {
                    self.settle();
                    self.emit(call);
                    self.take_at(join, 1);
                    return 3;
                };
                let t = self.temp();
                self.head(call, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, t, section, true, &[]);
                self.pending.push(Entry::reg(t));
                self.dest = Some(self.out.len() - 3);
                if !join { self.resume = Some(self.out.len() - 3); }
                return 3;
            }
            // a `yield from` whose value goes unused reads nothing
            OpCode::LoadYieldFrom if regs && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::PopTop) && !self.target(i + 1) => return 2,
            // a maker or a value of the vm reads its pending row into a temporary
            OpCode::MakeFunction | OpCode::MakeCoroutine | OpCode::MakeClass | OpCode::LoadModule | OpCode::LoadEllipsis | OpCode::LoadYieldFrom
                if regs && self.pending.len() >= op_row(ins.opcode, op, self.chunk) => {
                let b = self.row(op_row(ins.opcode, op, self.chunk));
                // a definition stored to a local goes straight there and takes no temporary
                if let Some(s) = self.stored_local(i) {
                    self.emit(Ins { op: OpCode::OpR, x: ins.opcode as u8, a: s, b, c: op });
                    return 2;
                }
                let t = self.temp();
                self.produce(Ins { op: OpCode::OpR, x: ins.opcode as u8, a: t, b, c: op });
            }
            // a loop takes a register's iterator and the value form leaves it in one
            OpCode::GetIter if regs && !self.pending.is_empty() => {
                let v = self.pop();
                if op == crate::syntax::parser::ITER_VALUE {
                    let t = self.dest();
                    self.produce(Ins { op: OpCode::GetIterR, x: op as u8, a: t, b: v.r, c: 0 });
                } else {
                    self.emit(Ins { op: OpCode::GetIterR, x: op as u8, a: 0, b: v.r, c: 0 });
                }
            }
            // the loop variable takes each item straight from the iterator
            OpCode::ForIter if regs
                && let Some(&store) = self.src.get(i + 1)
                && store.opcode == OpCode::StoreName
                && (store.operand as usize) < self.names
                && !self.target(i + 1)
                && let Some(target) = match self.kind(store.operand) {
                    Some(Kind::Local) => Some((0, store.operand)),
                    Some(Kind::Global(g)) if self.builtin_flag(store.operand) == 0 => Some((1, g as u16)),
                    _ => None,
                } => {
                self.settle();
                self.patches.push(self.out.len());
                self.emit(Ins { op: OpCode::ForIterR, x: target.0, a: op, b: target.1, c: 0 });
                return 2;
            }
            // a `yield from` loop steps into its own register which the following `yield` hands out
            OpCode::ForIter if regs && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::Yield) && self.src.get(i + 2).is_some_and(|n| n.opcode == OpCode::PopTop)
                && !self.target(i + 1) && !self.target(i + 2) => {
                self.settle();
                let t = self.temp();
                self.patches.push(self.out.len());
                self.emit(Ins { op: OpCode::ForIterR, x: 0, a: op, b: t, c: 0 });
                self.pending.push(Entry::reg(t));
                // the item goes straight out of its register so no row holds it
                self.cur += 1;
                return 1 + self.dropped_yield();
            }
            OpCode::ForIter if regs && self.src.get(i + 1).is_some_and(|n| matches!(n.opcode, OpCode::UnpackSequence | OpCode::UnpackEx)) && !self.target(i + 1) => return self.unpack_step(i),
            // a tuple built only to unpack hands its values straight to the stores
            OpCode::BuildTuple if (op as usize) <= self.pending.len()
                && op > 0
                && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::UnpackSequence && n.operand == op)
                && !self.target(i + 1) => {
                let k = self.pending.len() - op as usize;
                self.pending[k..].reverse();
                return 2;
            }
            // calls of up to five positionals read registers settling what lies under in rows
            OpCode::Call if regs && op <= 5 && self.pending.len() > op as usize => return self.reg_call(i, op),
            // appending to a local only ever holding its list pushes onto it dropping the `None`
            OpCode::CallMethod if regs && let Some(args) = self.src.get(i + 1) && args.opcode == OpCode::CallMethodArgs && args.operand == 1
                && self.pending.len() >= 2 && self.chunk.names.get(op as usize).is_some_and(|n| self.pushes(n))
                && let [.., recv, v] = self.pending[..] && recv.name
                && self.lists.get_or_init(|| list_locals(self.src, self.flags, self.facts, self.names)).get(recv.r as usize) == Some(&true)
                && self.src.get(i + 2).is_some_and(|n| n.opcode == OpCode::PopTop) && !self.target(i + 1) && !self.target(i + 2) => {
                self.pending.truncate(self.pending.len() - 2);
                self.emit(Ins { op: OpCode::ListPushR, x: 0, a: recv.r, b: v.r, c: 0 });
                return 3;
            }
            // method calls of up to four positionals read registers settling what lies under in rows
            OpCode::CallMethod if regs && let Some(args) = self.src.get(i + 1) && args.opcode == OpCode::CallMethodArgs
                && args.operand <= 4 && self.pending.len() > args.operand as usize => {
                let join = self.target(i + 2);
                let under = self.pending.len() - args.operand as usize - 1;
                let r = |k: usize| self.pending.get(under + k).map_or(0, |e| e.r);
                let call = Ins { op: OpCode::CallMethodR, x: args.operand as u8, a: op, b: r(0), c: r(1) };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) };
                // a method met before is laid as a section checked against what the receiver finds
                if self.facts.regs && let Some(callee) = (self.sections)(Want::Method(i as u32))
                    && let Some(section) = { let n = call.x as usize + !matches!(callee.on_class, 1 | 4) as usize; self.want_callee(u16::MAX, callee, n) } {
                    self.want_method_alts(None, i as u32, section, call.x as usize + 1);
                    self.pending.truncate(under);
                    let t = self.landing(None);
                    // the head reads a receiver just loaded from a binding and takes the load's place
                    let at = self.out.len() as u32;
                    let global = self.out.last().copied().filter(|l| l.op == OpCode::LoadGlobalR && l.a == call.b && l.b < u16::MAX && !wide(call) && (call.x == 0 || call.c != l.a)).map(|l| l.b);
                    if global.is_some() {
                        self.out.pop();
                        self.orig.pop();
                        self.back.pop();
                        for a in self.at[..=i].iter_mut().rev().take_while(|a| **a & 0xFFFF == at) { *a = *a & !0xFFFF | (at - 1); }
                    }
                    self.head_take(call, rest, t, section, join);
                    if let Some(g) = global { let h = self.out.len() - HEADER as usize; self.out[h + 3].c = g + 1; }
                    return 2;
                }
                self.outside_call(under, call, rest, join, 2);
                return 2;
            }
            OpCode::CallMethod if let Some(args) = self.src.get(i + 1) && args.opcode == OpCode::CallMethodArgs => {
                let n = 1 + (args.operand & 0xFF) as usize + 2 * (args.operand >> 8) as usize;
                self.island(Ins { op: OpCode::CallMethod, x: 0, a: op, b: args.operand, c: 0 }, n, 1, 2);
                return 2;
            }
            // a slice store hands its bounds to one op with no slice built
            OpCode::BuildSlice if regs && self.cut_store(i) => {
                let len = op + 2;
                let row = self.row(len as usize);
                self.emit(Ins { op: OpCode::OpR, x: OpCode::StoreCut as u8, a: 0, b: row, c: len });
                return 3;
            }
            // builds read their pending values as a row of registers
            OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildSet | OpCode::BuildString | OpCode::BuildDict | OpCode::BuildSlice
                if regs && build_len(ins) <= self.pending.len() => {
                let n = build_len(ins);
                // only a call taking it at once found the build in a register before
                let taken = n > 0 && n <= self.pending.len() && !matches!(ins.opcode, OpCode::BuildDict | OpCode::BuildSlice)
                    && self.src.get(i + 1).is_some_and(|s| matches!(s.opcode, OpCode::Call | OpCode::CallMethod)) && !self.target(i + 1);
                // a list only a spread reads builds as the tuple the spread packs
                let spread = ins.opcode == OpCode::BuildList && self.src.get(i + 1).is_some_and(|s| s.opcode == OpCode::UnpackArgs && s.operand & 3 == 1) && !self.target(i + 1);
                self.build(if spread { Instruction { opcode: OpCode::BuildTuple, ..ins } } else { ins }, n, taken);
            }
            // a runtime's own opcode reads its pending operands as a row of registers
            OpCode::RuntimeOp if regs && usize::from(op >> 8) <= self.pending.len() => self.own_op(op),
            // a field formats its pending value and spec into a register
            OpCode::FormatValue if regs && self.pending.len() > (op & 1) as usize => {
                let spec = if op & 1 != 0 { self.pop().r } else { 0 };
                let v = self.pop();
                let t = self.dest();
                self.produce(Ins { op: OpCode::FormatR, x: op as u8, a: t, b: v.r, c: spec });
            }
            // a return over rows with nothing pending hands back none
            OpCode::ReturnValue => {
                self.pending.clear();
                self.emit(Ins { op: OpCode::ReturnR, x: 0, a: 0, b: (self.cbase + self.consts) as u16, c: 0 });
            }
            OpCode::Pos | OpCode::BitNot | OpCode::MatchSeq | OpCode::MatchMap | OpCode::MatchClass | OpCode::DelItem | OpCode::DelAttr | OpCode::Assert | OpCode::Cut | OpCode::DelCut
                | OpCode::Raise | OpCode::UnpackSequence | OpCode::UnpackEx if let Some(n) = self.forms(i) => return n,
            _ => self.island_op(ins),
        }
        1
    }

    /* the call at `i` of `op` positionals over registers with its callee pending under them */
    fn reg_call(&mut self, i: usize, op: u16) -> usize {
        // a call result joined by another path at the next instruction stays on the stack
        let join = self.target(i + 1);
        let under = self.pending.len() - op as usize - 1;
        let r = |k: usize| self.pending.get(under + k).map_or(0, |e| e.r);
        let call = Ins { op: OpCode::CallR, x: op as u8, a: r(0), b: r(1), c: r(2) };
        let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(3), b: r(4), c: r(5) };
        let at = self.out.len() as u32;
        // the call reads a global loaded just for it and takes the load's place
        let load = self.out.last().copied().filter(|load| load.op == OpCode::LoadGlobalR && load.a == call.a
            && [call.b, call.c, rest.a, rest.b, rest.c].iter().take(call.x as usize).all(|&r| r != load.a));
        let known = load.filter(|_| self.facts.regs).and_then(|load| self.want(load.b, call.x as usize));
        // a call past two arguments reads its global itself only when laid as a section
        let load = load.filter(|_| !wide(call) || known.is_some())
            // a binding's closure is called from its register which its section checks
            .filter(|_| !known.is_some_and(|k| self.wanted[k].1.twin));
        let Some(load) = load else {
            // an earlier global or function met before is laid as a section the call checks
            let section = if !self.facts.regs { None }
                else { super::block::global_of(&self.out, self.out.len(), call).and_then(|g| self.want(g, call.x as usize))
                    .or_else(|| (self.sections)(Want::Value(i as u32)).and_then(|c| { let n = call.x as usize + (c.on_class & 3 == 3) as usize; self.want_callee(u16::MAX, c, n) }).inspect(|&k| self.want_alts(None, i as u32, k, call.x as usize))) };
            if let Some(section) = section {
                let (call, rest) = if self.wanted[section].1.on_class & 3 == 3 { called_instance(call, rest) } else { (call, rest) };
                self.pending.truncate(under);
                let t = self.landing(None);
                self.head_take(call, rest, t, section, join);
                return 1;
            }
            self.outside_call(under, call, rest, join, 1);
            return 1;
        };
        self.out.pop();
        self.orig.pop();
        self.back.pop();
        for a in self.at[..=i].iter_mut().rev().take_while(|a| **a & 0xFFFF == at) { *a = *a & !0xFFFF | (at - 1); }
        let call = Ins { op: OpCode::CallGlobalR, a: load.b, ..call };
        // known sections are jumped to with a fresh result temporary and pending values in registers
        if let Some(section) = known {
            self.pending.truncate(under);
            let t = self.landing(None);
            self.head_take(call, rest, t, section, join);
            return 1;
        }
        self.outside_call(under, call, rest, join, 1);
        1
    }

    /* emits `call` outside every section spilling what lies under it and taking its value */
    fn outside_call(&mut self, under: usize, call: Ins, rest: Ins, join: bool, next: u32) {
        self.settle_rows_with(under, &mut [], true);
        // a spill may move a value the call reads out of a row it fills
        let r = |k: usize| self.pending.get(under + k).map_or(0, |e| e.r);
        let (call, rest) = match call.op {
            OpCode::CallMethodR => (Ins { b: r(0), c: r(1), ..call }, Ins { a: r(2), b: r(3), c: r(4), ..rest }),
            OpCode::CallGlobalR => (Ins { b: r(1), c: r(2), ..call }, Ins { a: r(3), b: r(4), c: r(5), ..rest }),
            _ => (Ins { a: r(0), b: r(1), c: r(2), ..call }, Ins { a: r(3), b: r(4), c: r(5), ..rest }),
        };
        self.pending.truncate(under);
        self.quiet = self.cur;
        self.emit(call);
        if wide(call) { self.emit(rest); }
        // the value takes the callee's register the call gives up
        if call.op == OpCode::CallR { self.prefer = Some(call.a); }
        self.take_at(join, next);
    }

    /* lowers a `yield` whose result its PopTop drops taking both instructions */
    fn dropped_yield(&mut self) -> usize {
        let mut v = [self.pop()];
        self.settle_with(&mut v);
        self.yields.push(self.out.len());
        // a yield over values in rows parks with them
        let kept = if self.pending.is_empty() { 0 } else { KEPT };
        self.emit(Ins { op: OpCode::Yield, x: 1 | kept, a: v[0].r, b: 0, c: 0 });
        2
    }

    /* runs stack op `ins` over its operand rows as rows code runs it */
    fn island(&mut self, ins: Ins, pops: usize, pushes: usize, next: u32) {
        let under = self.pending.len().saturating_sub(pops);
        // the op reads its operands from their rows and what lies under waits there
        self.settle_rows(self.pending.len());
        if self.overflow { return; }
        let row = (self.s0 + under) as u16;
        self.pending.truncate(under);
        self.quiet = self.cur;
        let c = pops as u16 | if pushes == 0 { ROW_VOID } else { 0 };
        self.emit(Ins { op: OpCode::RowsR, x: ins.op as u8, a: row, b: ins.a, c });
        // the take after it keeps what the op's word held past its operand
        self.quiet = self.cur + next;
        // a resume past it enters the take unless a jump lands there too
        if pushes != 0 && !self.target((self.cur + next) as usize) { (self.resume, self.pops) = (Some(self.out.len()), pushes as u16); }
        self.emit(Ins { op: OpCode::RowsBackR, x: ins.x, a: row, b: ins.b, c: ins.c });
        for k in 0..pushes as u16 { self.pending.push(Entry::reg(row + k)); }
    }

    /* a stack op with no register form in code keeping its values in rows */
    fn island_op(&mut self, ins: Instruction) {
        use OpCode::*;
        match ins.opcode {
            // a jump to a return returns the value at once
            Jump if self.src.get(ins.operand as usize).is_some_and(|r| r.opcode == ReturnValue) && let Some(v) = self.pending.pop() => {
                self.pending.clear();
                self.emit(Ins { op: ReturnR, x: 0, a: 0, b: v.r, c: 0 });
            }
            // a try names its handler's exception slot as the handler's start picks it
            SetupExcept => {
                self.settle();
                let slot = self.dest();
                self.emit(Ins { b: slot + 1, ..Ins::of(ins) });
            }
            Jump | SetupFinally | PopExcept | BeginFinally | EndFinally | UnwindFinally | PopIter | Del | Phi => { self.settle(); self.emit_orig(ins); }
            // a loop step writes its item into its row
            ForIter => {
                self.settle();
                let t = self.dest();
                self.patches.push(self.out.len());
                self.emit(Ins { op: ForIterR, x: 0, a: ins.operand, b: t, c: 0 });
                self.pending.push(Entry::reg(t));
            }
            // a merge reads the container under the value and leaves it there
            ListExtend | SetUpdate | DictUpdate => self.island(Ins::of(ins), 2, 1, 1),
            op => match super::depth::effect(op, ins.operand, self.chunk) {
                Some((0, 0)) | None => { self.settle(); self.emit_orig(ins); }
                Some((pops, pushes)) if matches!(op, Call | CallNative | CallSpread | UnpackArgs | CallExtern) => {
                    let word = if super::rows::calls::spreads(op, ins.operand, self.chunk) { self.spread_word(ins) } else { Ins::of(ins) };
                    self.island(word, pops as usize, pushes as usize, 1)
                }
                // an op with no row form keeps the code as the compiler wrote it
                Some((pops, pushes)) => {
                    self.refused = true;
                    // what it leaves pending holds the depth the code past it lowers at
                    self.pending.truncate(self.pending.len().saturating_sub(pops as usize));
                    for _ in 0..pushes { self.pending.push(Entry::reg(0)); }
                }
            },
        }
    }

    /* the word of spread call `ins` naming its packed operands or a refusal */
    fn spread_word(&mut self, ins: Instruction) -> Ins {
        let at = self.cur as usize;
        let src = self.src;
        let op_at = |k: usize| { let s = src[k]; (s.opcode, if s.opcode == OpCode::CallMethod { src.get(k + 1).filter(|n| n.opcode == OpCode::CallMethodArgs).map_or(s.operand, |n| n.operand) } else { s.operand }) };
        let mut packs = super::depth::spread_back(op_at, at, self.chunk);
        if packs.is_none() {
            if self.spread_depths.is_none() { self.spread_depths = Some(super::depth::depths(self.src, self.chunk).unwrap_or_default()); }
            packs = self.spread_depths.as_deref().and_then(|d| super::depth::spread_packs(self.src, d, at, self.chunk));
        }
        let Some((maps, packs)) = packs else { self.refused = true; return Ins::of(ins) };
        Ins { op: ins.opcode, x: maps, a: ins.operand, b: packs as u16, c: (packs >> 16) as u16 }
    }

    /* native call `site` on its pending arguments as LenR or CallNativeR with its take */
    #[inline(never)]
    fn native(&mut self, i: usize, site: NativeSite) -> usize {
        let n = site.pos as usize;
        let native = self.facts.natives.get(crate::lang::NativeId(site.id));
        // a length keeps its form whose plain values never leave the register loop
        if n == 1 && !site.void && native.is_some_and(|f| f.intrinsic == crate::lang::Intrinsic::Len) {
            let o = self.pop();
            let t = self.dest();
            self.produce(Ins { op: OpCode::LenR, x: 0, a: t, b: o.r, c: if site.pinned { ENGINE_LEN } else { site.id } });
            return 1;
        }
        // a name the module binds to its own value is called as that binding is
        if !self.facts.rebound.is_empty() && let Some(n) = self.bound_native(i, site) { return n; }
        let k = self.pending.len() - n;
        let join = self.target(i + 1);
        // what lies under it waits in its rows since any call may park
        self.settle_rows_with(k, &mut [], true);
        let r = |q: usize| self.pending.get(k + q).filter(|_| q < n).map_or(0, |e| e.r);
        let flags = n as u8 | if site.pinned { PINNED } else { 0 } | if site.void { VOID } else { 0 };
        let call = Ins { op: OpCode::CallNativeR, x: flags, a: site.id, b: r(0), c: r(1) };
        let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) };
        self.pending.truncate(k);
        self.quiet = self.cur;
        self.emit(call);
        if n > 2 { self.emit(rest); }
        // a statement's call takes nothing back
        if !site.void { self.take_at(join, 1); }
        1
    }

    /* builds ins from its n pending items in a register or false on the stack */
    fn build(&mut self, ins: Instruction, n: usize, taken: bool) -> bool {
        let k = self.pending.len() - n;
        // a row past what the plan's masks count moves its items into fresh temporaries
        let wide = n > 63;
        let plan = (n >= 2 && !wide && !self.in_row(k, true)).then(|| self.row_plan(k, true));
        let b = match plan {
            _ if n == 0 => 0,
            _ if wide => {
                let t = self.free_row(n);
                for q in 0..n { let e = self.pending[k + q]; self.emit(Ins { op: OpCode::Move, x: e.name as u8, a: t + q as u16, b: e.r, c: 0 }); }
                t
            }
            Some(plan) => self.laid(k, plan),
            None => self.pending[k].r,
        };
        self.pending.truncate(k);
        let t = self.dest();
        self.produce(Ins { op: OpCode::BuildR, x: ins.opcode as u8 | if taken { TAKEN } else { 0 }, a: t, b, c: n as u16 });
        true
    }

    /* native call `site` at `i` as a call of the module binding its name holds */
    #[cold]
    #[inline(never)]
    fn bound_native(&mut self, i: usize, site: NativeSite) -> Option<usize> {
        if site.pinned || site.void { return None; }
        let g = self.facts.binding_of(site.id)?;
        // the binding loads under the arguments where a callee would lie
        let t = self.temp();
        self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: t, b: g, c: 0 });
        let k = self.pending.len() - usize::from(site.pos);
        self.pending.insert(k, Entry::reg(t));
        Some(self.reg_call(i, u16::from(site.pos)))
    }

    /* jumps from `call` into section `k` with its value in `t` over the pending values */
    fn head_take(&mut self, call: Ins, rest: Ins, t: u16, k: usize, join: bool) {
        let below = self.pending.clone();
        self.head(call, rest, t, k, true, &below);
        self.pending.push(Entry::reg(t));
        self.dest = Some(self.out.len() - 3);
        if !join { self.resume = Some(self.out.len() - 3); }
    }

    /* whether the slice built at `i` keys a store whose value a Rot3 lifts */
    fn cut_store(&self, i: usize) -> bool {
        let n = self.src[i].operand as usize;
        matches!(n, 2 | 3) && self.pending.len() >= n + 2 && matches!(self.src.get(i + 1..i + 3), Some([a, b]) if a.opcode == OpCode::Rot3 && b.opcode == OpCode::StoreItem)
            && !self.target(i + 1) && !self.target(i + 2)
    }

    /* runtime opcode `op` on its pending operands read as a row into a temporary */
    fn own_op(&mut self, op: u16) {
        let n = usize::from(op >> 8);
        let k = self.pending.len() - n;
        let plan = (2..64).contains(&n).then(|| !self.in_row(k, true)).filter(|&apart| apart).map(|_| self.row_plan(k, true));
        let b = match plan {
            _ if n == 0 => 0,
            // a row past what the plan's masks count moves its items into fresh temporaries
            _ if n >= 64 => {
                let t = self.free_row(n);
                for q in 0..n { let e = self.pending[k + q]; self.emit(Ins { op: OpCode::Move, x: e.name as u8, a: t + q as u16, b: e.r, c: 0 }); }
                t
            }
            Some(plan) => self.laid(k, plan),
            None => self.pending[k].r,
        };
        self.pending.truncate(k);
        let t = self.dest();
        self.produce(Ins { op: OpCode::RuntimeR, x: op as u8, a: t, b, c: n as u16 });
    }

    fn kind(&self, s: u16) -> Option<Kind> { self.facts.kinds.get(s as usize).copied() }

    /* whether a jump or handler enters instruction `i` */
    fn target(&self, i: usize) -> bool { self.flags.get(i).is_some_and(|f| f & TARGET != 0) }

    /* the local the StoreName after `i` binds when no pending value reads it */
    fn stored_local(&self, i: usize) -> Option<u16> {
        let s = self.src.get(i + 1).filter(|n| n.opcode == OpCode::StoreName && !self.target(i + 1))?.operand;
        ((s as usize) < self.names && self.kind(s) == Some(Kind::Local) && !self.pending.iter().any(|e| e.name && e.r == s)).then_some(s)
    }

    /* whether the `n` instructions after `i` store into plain locals no jump lands between */
    fn unpacks_to_locals(&self, i: usize, n: usize) -> bool {
        (1..=n).all(|k| self.src.get(i + k).is_some_and(|s| s.opcode == OpCode::StoreName && (s.operand as usize) < self.names
            && self.kind(s.operand) == Some(Kind::Local) && !self.target(i + k)))
    }

    /* the test a short-circuit reaches through a chain of its own kind */
    fn threaded_test(&self, ins: Instruction) -> Option<usize> {
        let mut t = ins.operand as usize;
        for _ in 0..16 {
            match self.src.get(t)?.opcode {
                OpCode::JumpIfFalse => return Some(t),
                op if op == ins.opcode => t = self.src[t].operand as usize,
                _ => return None,
            }
        }
        None
    }

    /* whether list method `n` is the runtime's push which lowers to the in place push */
    fn pushes(&self, n: &str) -> bool { self.facts.natives.method(crate::lang::KindId::List, n).is_some_and(|m| m.flags & crate::lang::PUSH != 0) }

    /* the store flag a native's name carries since rebinding one redirects its calls */
    fn builtin_flag(&self, s: u16) -> u8 {
        let bare = self.chunk.names.get(s as usize).map_or("", |n| ssa_strip(n));
        if self.facts.natives.function_id(bare).is_some() { STORE_NOTES_BUILTIN } else { 0 }
    }

    /* a local's read waits for its use since only the frame writes it */
    fn deferrable(&self, s: usize) -> bool {
        !self.deleted.get(s).is_some_and(|&d| d) && self.bound.get(s / 64).is_some_and(|w| w & (1 << (s % 64)) != 0)
    }

    fn load_name(&mut self, s: u16) {
        let read = match self.kind(s) {
            Some(Kind::Global(g)) => Ins { op: OpCode::LoadGlobalR, x: 0, a: 0, b: g as u16, c: 0 },
            Some(Kind::Cell) => Ins { op: OpCode::LoadCellR, x: 0, a: 0, b: s, c: 0 },
            _ if self.deferrable(s as usize) => return self.push(Entry { r: s, name: true }),
            // read now so its error or value is the one at this point
            _ => Ins { op: OpCode::Move, x: 1, a: 0, b: s, c: 0 },
        };
        let t = self.dest();
        self.produce(Ins { a: t, ..read });
    }

    fn store_name(&mut self, s: u16) {
        match self.kind(s) {
            Some(Kind::Global(g)) => {
                let v = self.pop();
                let x = self.builtin_flag(s);
                return self.emit(Ins { op: OpCode::StoreGlobalR, x, a: g as u16, b: v.r, c: 0 });
            }
            Some(Kind::Cell) => {
                let v = self.pop();
                return self.emit(Ins { op: OpCode::StoreCellR, x: 0, a: s, b: v.r, c: 0 });
            }
            _ => {}
        }
        let spilled = self.spill(s);
        let top = self.pop();
        // a value in its own row is fresh as a temporary is
        let own = self.pending.len() < self.rows && top.r as usize == self.s0 + self.pending.len();
        let fresh = !top.name && (self.is_temp(top.r as usize) || own) && !self.pending.contains(&top);
        match (self.last, self.dest) {
            (Some(k), _) if !spilled && fresh && k + 1 == self.out.len() && self.out[k].a == top.r => self.out[k].a = s,
            // a section call writes its result straight to the variable
            (_, Some(k)) if !spilled && fresh && self.out[k].a == top.r => self.out[k].a = s,
            _ => self.emit(Ins { op: OpCode::Move, x: top.name as u8, a: s, b: top.r, c: 0 }),
        }
        (self.last, self.dest) = (None, None);
    }

    /* pending reads of `s` copy its value out before a store rebinds it */
    fn spill(&mut self, s: u16) -> bool {
        if !self.pending.iter().any(|e| e.name && e.r == s) { return false; }
        let t = self.temp();
        self.emit(Ins { op: OpCode::Move, x: 1, a: t, b: s, c: 0 });
        for e in &mut self.pending { if e.name && e.r == s { *e = Entry::reg(t); } }
        true
    }

    /* the two topmost pending values with the deeper one first */
    fn pop2(&mut self) -> (Entry, Entry) {
        let b = self.pop();
        let a = self.pop();
        (a, b)
    }

    /* the topmost pending value */
    fn pop(&mut self) -> Entry { self.pending.pop().unwrap_or(Entry::reg(0)) }

    fn binary(&mut self, i: usize, code: OpCode, operand: u8) -> usize {
        let (a, b) = self.pop2();
        if let Some(branch) = branch_form(code)
            && let Some(next) = self.src.get(i + 1)
            && next.opcode == OpCode::JumpIfFalse
            && !self.target(i + 1)
        {
            // a comparison testing a ternary keeps what is pending under it like a plain test
            if self.chunk.ternaries() && !self.pending.is_empty() && self.tern.is_none() && let Some((end_a, join)) = self.ternary(i + 1) {
                self.jump(branch, next.operand, a.r, b.r);
                self.tern = Some(alloc::boxed::Box::new(Tern { start_a: i + 2, else_at: next.operand as usize, end_a, join, saved: self.pending.clone(), t: None, bail: false }));
                return 2;
            }
            let mut ab = [a, b];
            self.settle_rows_with(self.pending.len(), &mut ab, false);
            self.jump(branch, next.operand, ab[0].r, ab[1].r);
            return 2;
        }
        let t = self.dest();
        let augmented = if matches!(code, OpCode::InPlaceBitAnd | OpCode::InPlaceBitOr | OpCode::InPlaceBitXor) { AUGMENTED } else { 0 };
        self.produce(Ins { op: binary_form(code).unwrap_or(code), x: operand | augmented, a: t, b: a.r, c: b.r });
        1
    }

    /* emits `ins` writing the temporary in its `a` and leaves that temporary pending */
    fn produce(&mut self, ins: Ins) {
        self.emit(ins);
        self.last = Some(self.out.len() - 1);
        self.pending.push(Entry::reg(ins.a));
    }

    /* the first register of the top `n` pending values as an OpR reads them */
    fn row(&mut self, n: usize) -> u16 { self.row_in(n, false) }

    /* `row` that leaves items in place when the form's analyses read the whole row */
    fn row_in(&mut self, n: usize, whole: bool) -> u16 {
        if n == 0 { return NO_ROW; }
        if n == 1 { return self.pop().r; }
        let from = self.pending.len() - n;
        // values already in a row are read where they lie
        let at = self.pending[from].r;
        if self.in_row(from, whole) {
            self.pending.truncate(from);
            return at;
        }
        let t = self.lay_row(from, whole);
        self.pending.truncate(from);
        t
    }

    /* whether pending values from k on lie in a row as `row_in` may read it */
    fn in_row(&self, k: usize, whole: bool) -> bool {
        let at = self.pending[k].r;
        let (first, end) = (at as usize, at as usize + self.pending.len() - k);
        let temps = self.pending[k..].iter().all(|e| !e.name && self.owned(e.r as usize));
        // a temporary also held under the row moves so that holder keeps it
        (whole || temps) && self.pending[k..].iter().enumerate().all(|(q, e)| e.r as usize == first + q)
            && !(whole && self.pending[..k].iter().any(|e| !e.name && self.owned(e.r as usize) && (first..end).contains(&(e.r as usize))))
    }

    /* where the row for pending values from k starts with its lifted and kept items */
    fn row_plan(&self, k: usize, whole: bool) -> (usize, Option<usize>, u64) {
        let (n, tbase) = (self.pending.len() - k, self.tbase);
        // the temporaries from tbase on as bits with the first 64 told apart
        let bit = |e: Entry| if e.name { 0 } else { (e.r as usize).checked_sub(tbase).filter(|&d| d < 64).map_or(0, |d| 1u64 << d) };
        let (mut below, mut seen, mut twice) = (0u64, 0u64, 0u64);
        for &e in &self.pending[..k] { below |= bit(e); }
        for &e in &self.pending[k..] { twice |= seen & bit(e); seen |= bit(e); }
        // an item only its own entry holds may stay where it is
        let own = |e: Entry| bit(e) != 0 && bit(e) & (below | twice) == 0;
        let items = &self.pending[k..];
        let first = items[0];
        // the instruction just emitted may write its item straight into the row
        let lifted = self.last.filter(|&j| j + 1 == self.out.len()).and_then(|j| items.iter().position(|&e| own(e) && e.r == self.out[j].a));
        let held = (below | seen) & !lifted.map_or(0, |q| bit(items[q]));
        let placed = |s: usize, q: usize| own(items[q]) && items[q].r as usize == s + q;
        // a whole row may start at its first item when the slots past are free
        let from_first = whole && own(first) && (1..n).all(|q| placed(first.r as usize, q) || first.r as usize + q - tbase < 64 && held & 1 << (first.r as usize + q - tbase) == 0);
        let ones = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        let fresh = (0..=64usize.saturating_sub(n)).find(|&d| held >> d & ones == 0).map_or_else(|| tbase.max(self.pending.iter().filter(|e| !e.name).map(|e| e.r as usize + 1).max().unwrap_or(0)), |d| tbase + d);
        let moves = |s: usize| (0..n).filter(|&q| !placed(s, q) && Some(q) != lifted).count();
        let s = if from_first && moves(first.r as usize) <= moves(fresh) { first.r as usize } else { fresh };
        (s, lifted, (0..n).filter(|&q| placed(s, q)).fold(0, |m, q| m | 1 << q))
    }

    /* the row of temporaries for pending values from k on found over bit masks */
    fn lay_row(&mut self, k: usize, whole: bool) -> u16 { let plan = self.row_plan(k, whole); self.laid(k, plan) }

    /* lays the row of pending values from k on as its plan says */
    fn laid(&mut self, k: usize, (s, lifted, kept): (usize, Option<usize>, u64)) -> u16 {
        let n = self.pending.len() - k;
        if let Some(q) = lifted && kept & 1 << q == 0 && let Some(j) = self.last { self.out[j].a = (s + q) as u16; }
        // items not in place move just before the form that reads them
        for q in (0..n).filter(|&q| kept & 1 << q == 0 && Some(q) != lifted) {
            let e = self.pending[k + q];
            self.emit(Ins { op: OpCode::Move, x: e.name as u8, a: (s + q) as u16, b: e.r, c: 0 });
        }
        self.temps = self.temps.max(s + n - self.tbase);
        s as u16
    }

    /* the first temporaries a stub may move `regs` into from the last one down */
    fn free_home(&mut self, regs: &[u16]) -> u16 {
        let (n, mut b) = (regs.len(), self.tbase);
        while (b..b + n).any(|u| self.pending.iter().any(|e| !e.name && e.r as usize == u)) || !moves_fit(regs, b as u16) { b += 1; }
        self.temps = self.temps.max(b + n - self.tbase);
        b as u16
    }

    /* `n` temporaries in a row that no pending value and none of `avoid` holds */
    fn free_block(&mut self, n: usize, avoid: &[u16]) -> u16 {
        let mut b = self.tbase;
        while (b..b + n).any(|u| avoid.contains(&(u as u16)) || self.pending.iter().any(|e| !e.name && e.r as usize == u)) { b += 1; }
        self.temps = self.temps.max(b + n - self.tbase);
        b as u16
    }

    // the first of `n` temporaries in a row that no pending value holds
    fn free_row(&mut self, n: usize) -> u16 {
        let mut t = self.tbase;
        while (t..t + n).any(|u| self.pending.iter().any(|e| !e.name && e.r as usize == u)) { t += 1; }
        self.temps = self.temps.max(t + n - self.tbase);
        t as u16
    }

    // the register form of the stack op at `i` and the instructions it took
    #[inline(never)]
    fn forms(&mut self, i: usize) -> Option<usize> {
        let (ins, op, n) = (self.src[i], self.src[i].operand, self.pending.len());
        match ins.opcode {
            OpCode::Pos | OpCode::BitNot | OpCode::MatchSeq | OpCode::MatchMap if n > 0 => {
                let v = self.pop();
                let t = self.dest();
                let form = if matches!(ins.opcode, OpCode::Pos | OpCode::BitNot) { OpCode::UnaryR } else { OpCode::OpR };
                self.produce(Ins { op: form, x: ins.opcode as u8, a: t, b: v.r, c: 0 });
            }
            OpCode::DelAttr | OpCode::Assert if n > 0 => {
                let v = self.pop();
                self.emit(Ins { op: OpCode::OpR, x: ins.opcode as u8, a: 0, b: v.r, c: op });
            }
            // a delete takes its object and its key or a slice's bounds as one row
            OpCode::DelItem | OpCode::DelCut if n >= (op as usize).max(2) => {
                let row = self.row((op as usize).max(2));
                self.emit(Ins { op: OpCode::OpR, x: ins.opcode as u8, a: 0, b: row, c: op });
            }
            // a slice read takes its object and bounds as one row
            OpCode::Cut if n >= op as usize => {
                let row = self.row(op as usize);
                let t = self.dest();
                self.produce(Ins { op: OpCode::OpR, x: ins.opcode as u8, a: t, b: row, c: op });
            }
            OpCode::MatchClass if n >= 3 => {
                let row = self.row(3);
                let t = self.temp();
                self.produce(Ins { op: OpCode::OpR, x: ins.opcode as u8, a: t, b: row, c: op });
            }
            // nothing pending under a raise is read again
            OpCode::Raise if op == 1 || n > 0 => {
                let b = if op == 1 { 0 } else { self.pop().r };
                self.pending.clear();
                self.emit(Ins { op: OpCode::RaiseR, x: (op == 1) as u8, a: 0, b, c: 0 });
            }
            OpCode::UnpackSequence | OpCode::UnpackEx if n > 0 => return Some(self.unpack(i)),
            _ => return None,
        }
        Some(1)
    }

    // a loop whose target unpacks steps each item into the register its unpacking reads
    #[inline(never)]
    fn unpack_step(&mut self, i: usize) -> usize {
        self.settle();
        let t = self.temp();
        self.patches.push(self.out.len());
        self.emit(Ins { op: OpCode::ForIterR, x: 0, a: self.src[i].operand, b: t, c: 0 });
        self.pending.push(Entry::reg(t));
        self.cur = i as u32 + 1;
        1 + self.unpack(i + 1)
    }

    // the unpacking at `i` of the top pending value and the instructions it took
    fn unpack(&mut self, i: usize) -> usize {
        let ins = self.src[i];
        let (ex, op) = (ins.opcode == OpCode::UnpackEx, ins.operand);
        let src = self.pop();
        // a pair stored into two locals no pending value reads goes straight to them
        if !ex && op == 2 && self.facts.regs && self.unpacks_to_locals(i, 2) {
            let (a, b) = (self.src[i + 1].operand, self.src[i + 2].operand);
            if !self.pending.iter().any(|e| e.name && (e.r == a || e.r == b)) {
                self.emit(Ins { op: OpCode::UnpackR, x: 2, a, b, c: src.r });
                return 3;
            }
        }
        let n = if ex { ((op >> 8) + 1 + (op & 0xFF)) as usize } else { op as usize };
        // an empty row names no register
        let row = if n == 0 { 0 } else { self.free_row(n) };
        let unpack = if ex { Ins { op: OpCode::OpR, x: OpCode::UnpackEx as u8, a: row, b: src.r, c: op } } else { Ins { op: OpCode::UnpackR, x: 0, a: row, b: src.r, c: op } };
        if n == 1 { self.produce(unpack); return 1; }
        self.emit(unpack);
        for k in 0..n as u16 { self.push(Entry::reg(row.saturating_add(k))); }
        1
    }

    /* the register of row `k` noting one past the frame's so the code lowers again */
    fn row_reg(&mut self, k: usize) -> u16 {
        if k >= self.rows { self.overflow = true; return self.tbase as u16; }
        (self.s0 + k) as u16
    }

    /* whether pending value `k` lies in its row */
    #[inline]
    fn in_its_row(&self, k: usize) -> bool { let e = self.pending[k]; !e.name && k < self.rows && e.r as usize == self.s0 + k }

    /* whether a run stopped here may resume in any code with values in rows */
    fn settled(&self) -> bool {
        let n = self.pending.len();
        // the value pushed last is most often the one out of its row
        if n == 0 || self.rows == 0 || n > self.rows || !self.in_its_row(n - 1) { return n == 0; }
        (0..n - 1).all(|k| self.in_its_row(k))
    }

    /* spills the pending values below `upto` into their rows */
    fn settle_rows(&mut self, upto: usize) { self.settle_rows_with(upto, &mut [], false); }

    /* `settle_rows` keeping `held` values out of the rows and names pending when `read` */
    fn settle_rows_with(&mut self, upto: usize, held: &mut [Entry], read: bool) {
        for k in 0..upto.min(self.pending.len()) {
            if self.in_its_row(k) { continue; }
            let r = self.row_reg(k);
            if self.overflow { return; }
            if self.pending.iter().enumerate().any(|(j, e)| j != k && !e.name && e.r == r) || held.iter().any(|e| !e.name && e.r == r) {
                let t = self.temp_past(held);
                self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: r, c: 0 });
                for e in self.pending.iter_mut().chain(held.iter_mut()) { if !e.name && e.r == r { e.r = t; } }
            }
            // a pending name is a local bound on every path so its copy checks nothing
            let e = self.pending[k];
            self.emit(Ins { op: OpCode::Move, x: 0, a: r, b: e.r, c: 0 });
            // a name or a constant is read where it lies and its row serves parks
            if !(read && (e.name || (self.cbase..self.tbase).contains(&(e.r as usize)))) { self.pending[k] = Entry::reg(r); }
        }
    }

    /* starts block `i` with its values in rows or false when no path reaches it */
    fn start(&mut self, i: usize, live: bool) -> bool {
        // a store at a join takes over no instruction another path skips
        (self.last, self.dest) = (None, None);
        // only code a path reaches falls into the next block
        let mut falls = live && i > 0 && !ends_flow(self.src[i - 1].opcode);
        // a jump notes the depth it enters at and the flow falling in holds it
        let d = match self.at.get(i) { Some(&d) if d != 0 => (d - 1) as u16, _ if falls || i == 0 => self.pending.len() as u16, _ => u16::MAX };
        // a block no path reaches holds nothing and an empty one lays nothing
        if d == u16::MAX || d == 0 && self.pending.is_empty() && self.flags.get(i).is_none_or(|f| f & CAUGHT == 0) { self.pending.clear(); return d != u16::MAX; }
        // what falls into a return returns at once
        if falls && self.src[i].opcode == OpCode::ReturnValue && let Some(&v) = self.pending.last() {
            self.emit(Ins { op: OpCode::ReturnR, x: 0, a: 0, b: v.r, c: 0 });
            falls = false;
        }
        if falls { self.settle_rows(self.pending.len()); }
        debug_assert!(!falls || self.overflow || self.pending.len() == d as usize, "a block of {:?} at {i} entered at another depth", self.chunk.qualname);
        let caught = self.flags.get(i).is_some_and(|f| f & CAUGHT != 0);
        // a return every jump into returned at once is entered by none
        if !falls && !caught && d as usize > self.rows && self.src[i].opcode == OpCode::ReturnValue { self.pending.clear(); return false; }
        self.pending.clear();
        for k in 0..d as usize - caught as usize { let r = self.row_reg(k); self.pending.push(Entry::reg(r)); }
        if caught {
            // the catch hands the exception on the stack to the store the handler starts with
            (self.quiet, self.cur) = (i as u32, i as u32);
            self.resume = Some(self.out.len());
            let t = self.dest();
            self.produce(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
        }
        true
    }

    /* `settle` keeping the values of `held` where it reads them after */
    fn settle_with(&mut self, held: &mut [Entry]) {
        self.settle_rows_with(self.pending.len(), held, false);
        self.quiet = self.cur;
    }

    /* the lowest temporary neither a pending value nor one of `held` holds */
    fn temp_past(&mut self, held: &[Entry]) -> u16 {
        let mut t = self.tbase;
        while self.pending.iter().chain(held).any(|e| !e.name && e.r as usize == t) { t += 1; }
        self.temps = self.temps.max(t + 1 - self.tbase);
        t as u16
    }

    /* the register a call's value lands in its free row or `prefer` or a temporary */
    fn landing(&mut self, prefer: Option<u16>) -> u16 {
        let u = self.pending.len();
        let held = |l: &Self, r: usize| l.pending.iter().any(|e| !e.name && e.r as usize == r);
        if u < self.rows && !held(self, self.s0 + u) { return (self.s0 + u) as u16; }
        match prefer { Some(r) if self.is_temp(r as usize) && !held(self, r as usize) => r, _ => self.temp() }
    }

    /* the register the value pushed next lands in as its free row or a temporary */
    fn dest(&mut self) -> u16 { self.landing(None) }

    /* the lowest temporary no pending value holds */
    fn temp(&mut self) -> u16 {
        let mut t = self.tbase;
        while self.pending.iter().any(|e| !e.name && e.r as usize == t) { t += 1; }
        self.temps = self.temps.max(t + 1 - self.tbase);
        t as u16
    }

    /* whether register `r` holds a temporary */
    #[inline]
    fn is_temp(&self, r: usize) -> bool { r >= self.tbase }

    /* whether register `r` is a temporary or a row a pending value owns */
    #[inline]
    fn owned(&self, r: usize) -> bool { r >= self.tbase || (self.s0..self.s0 + self.rows).contains(&r) }

    fn push(&mut self, e: Entry) { self.pending.push(e); }

    /* spills before an opcode reading only its rows so it resumes from itself */
    fn settle(&mut self) {
        self.flush();
        self.quiet = self.cur;
    }

    /* puts every pending value into its row */
    fn flush(&mut self) { self.settle_rows(self.pending.len()); }

    fn jump(&mut self, op: OpCode, target: u16, b: u16, c: u16) {
        self.patches.push(self.out.len());
        self.emit(Ins { op, x: 0, a: target, b, c });
    }

    fn emit_orig(&mut self, ins: Instruction) {
        if matches!(ins.opcode, OpCode::Jump | OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop | OpCode::ForIter) {
            self.patches.push(self.out.len());
        }
        self.emit(Ins::of(ins));
    }

    /* `take` for a call `next` compiler instructions long */
    fn take_at(&mut self, join: bool, next: u32) {
        // the value lands in a temporary through the store a resume past the call enters
        self.quiet = self.cur + next;
        if !join { self.resume = Some(self.out.len()); }
        let prefer = self.prefer.take();
        let t = self.landing(prefer);
        self.produce(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
    }

    fn emit(&mut self, ins: Ins) {
        self.out.push(ins);
        self.orig.push(self.cur as u16);
        self.back.push(self.quiet as u16);
        self.last = None;
        self.dest = None;
    }

    /* emits each of `ins` in turn as `emit` does */
    fn emit_all(&mut self, ins: &[Ins]) {
        self.out.extend_from_slice(ins);
        self.orig.resize(self.orig.len() + ins.len(), self.cur as u16);
        self.back.resize(self.back.len() + ins.len(), self.quiet as u16);
        self.last = None;
        self.dest = None;
    }
}

/* `ins` ended by the sentinel the cursor reads and checked in debug builds */
pub(crate) fn seal(mut ins: Vec<Ins>, frame: usize) -> Vec<Ins> {
    #[cfg(debug_assertions)]
    check_sealed(&ins, frame);
    #[cfg(not(debug_assertions))]
    let _ = frame;
    ins.reserve_exact(1);
    ins.spare_capacity_mut()[0].write(Ins { op: OpCode::Raise, x: 0, a: 0, b: 0, c: 0 });
    ins
}

/* panics where `ins` breaks what the loop reads unchecked from code of `frame` registers */
#[cfg(debug_assertions)]
pub(crate) fn check_sealed(ins: &[Ins], frame: usize) {
    let n = ins.len();
    for (i, w) in ins.iter().enumerate() {
        assert!(reg_top(w) <= frame, "word {i} {w:?} names a register past frame {frame}");
        match w.op {
            OpCode::JumpR => assert!(w.a as usize <= n, "word {i} jumps past the code"),
            OpCode::TurnR => assert!((w.a as usize + w.b as usize).max(w.a as usize + 1) <= n, "word {i} turns words past the code"),
            op if section_call(op) || op == OpCode::CallSectionR => {
                let (Some(s), Some(x), Some(t)) = (ins.get(i + 1), ins.get(i + 2), ins.get(i + 3)) else { panic!("head {i} past the code") };
                if x.op != OpCode::SectionR { continue; }
                assert!(s.b as usize <= n && x.a as usize <= n && t.a as usize <= n, "head {i} points past the code");
                assert!((s.a as usize) < frame && x.b >= 1 && x.b as usize + 2 <= frame, "head {i} links registers past frame {frame}");
            }
            _ => {}
        }
    }
}

/* a number no code made before has with 0 left for none */
fn next_layout() -> u32 {
    static NEXT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
    NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/* the `b` of an OpR that takes no operands */
pub(crate) const NO_ROW: u16 = u16::MAX;

/* how many values stack opcode `op` with `operand` takes as an OpR row */
fn op_row(op: OpCode, operand: u16, chunk: &SSAChunk) -> usize {
    if op.form() != Some(OpCode::OpR) { return 0 }
    op.effect(operand, chunk).map_or(0, |e| usize::from(e.0))
}

// the counts an OpR of stack opcode `op` reads from `b` and writes from `a`
pub(crate) fn op_rows(op: OpCode, operand: u16) -> Option<(u16, u16)> {
    if op.form() != Some(OpCode::OpR) { return None }
    op.plain_effect(operand)
}

// the bit of a row op's count marking a native whose statement keeps no value
pub(crate) const ROW_VOID: u16 = 0x8000;

/* the values row op `i` reads from its row and leaves there */
#[inline]
pub(crate) fn row_op_counts(i: &Ins) -> (u16, u16) {
    let gives = match OpCode::ALL.get(i.x as usize) {
        Some(OpCode::GetAwaitable) => 2,
        _ if i.c & ROW_VOID != 0 => 0,
        _ => 1,
    };
    (i.c & !ROW_VOID, gives)
}

/* whether row op `i` calls what it runs as a call does */
#[inline]
pub(crate) fn row_calls(i: &Ins) -> bool {
    matches!(OpCode::ALL.get(i.x as usize), Some(OpCode::Call | OpCode::CallMethod | OpCode::CallSpread | OpCode::CallExtern | OpCode::CallNative))
}

/* whether a register call's operands overflow into the ArgsR after it */
#[inline(always)]
pub(crate) fn wide(call: Ins) -> bool {
    if call.op == OpCode::CallNativeR { return call.x & ARGC > 2; }
    call.x > if matches!(call.op, OpCode::CallR | OpCode::CallGlobalR | OpCode::CallSectionR | OpCode::CallGlobalSectionR) { 2 } else { 1 }
}

/* a register call's registers with the callee or receiver first and how many it passes */
#[inline(always)]
pub(crate) fn call_regs(call: Ins, rest: Ins) -> ([u16; 6], usize) {
    if call.op == OpCode::CallR { ([call.a, call.b, call.c, rest.a, rest.b, rest.c], call.x as usize + 1) }
    // `b` stands in for a global callee's missing register and readers put the binding there
    else if matches!(call.op, OpCode::CallGlobalR | OpCode::CallSectionR | OpCode::CallGlobalSectionR) { ([call.b, call.b, call.c, rest.a, rest.b, rest.c], call.x as usize + 1) }
    else { ([call.b, call.c, rest.a, rest.b, rest.c, 0], call.x as usize + 1) }
}

/* the LenR `c` of a length no native stands for which no binding redirects */
pub(crate) const ENGINE_LEN: u16 = u16::MAX;

/* the StoreGlobalR flag of a store into a native's name which redirects its calls */
pub(crate) const STORE_NOTES_BUILTIN: u8 = 1;

/* the register form of a binary opcode */
fn binary_form(op: OpCode) -> Option<OpCode> {
    if matches!(op.operator(), crate::code::Operator::Binary | crate::code::Operator::Augmented) { op.form() } else { None }
}

/* the `x` bit marking a bitwise register form from an augmented opcode */
pub(crate) const AUGMENTED: u8 = 0x80;

/* the compare-and-branch form of a comparison */
fn branch_form(op: OpCode) -> Option<OpCode> { op.branch() }

/* the stack opcode a register form falls back to */
pub(crate) fn stack_form(op: OpCode) -> OpCode { STACK_FORMS[op as usize] }

// each op's stack form read with one load as the slow arms ask it
static STACK_FORMS: [OpCode; 256] = {
    let mut t = [OpCode::LoadConst; 256];
    let mut k = 0;
    while k < OpCode::EVERY.len() {
        t[k] = match OpCode::INFO[k].stack { Some(stack) => stack, None => OpCode::EVERY[k] };
        k += 1;
    }
    t
};

/* fuses LoadAttr and pure argument code and Call into CallMethod pairs never across jump targets */
pub(crate) fn fuse_method_calls(chunk: &SSAChunk, shape: u8) -> Cow<'_, [Instruction]> {
    let src = &chunk.instructions;
    // a shape not read yet is looked at for attribute reads alone
    if shape & ATTRS == 0 && (shape != 0 || !src.iter().any(|i| i.opcode == OpCode::LoadAttr)) { return Cow::Borrowed(src); }
    let n = src.len();
    let mut out = src.clone();
    let targeted = entered(src);
    const MAX_WINDOW: usize = 16;
    let mut i = 0;
    while i + 1 < n {
        if src[i].opcode != OpCode::LoadAttr { i += 1; continue; }
        /* args are loads and arithmetic over their own pushes so the object stays under them */
        let (mut j, mut depth) = (i + 1, 0usize);
        while j < n && j - i - 1 < MAX_WINDOW && !targeted[j] {
            match src[j].opcode {
                OpCode::LoadConst | OpCode::LoadName | OpCode::LoadTrue | OpCode::LoadFalse | OpCode::LoadNone => depth += 1,
                OpCode::Add | OpCode::Sub | OpCode::Mul | OpCode::Mod | OpCode::FloorDiv | OpCode::BitAnd | OpCode::BitOr
                | OpCode::BitXor | OpCode::Eq | OpCode::NotEq | OpCode::Lt | OpCode::Gt | OpCode::LtEq | OpCode::GtEq
                    if depth >= 2 => depth -= 1,
                OpCode::Minus | OpCode::Not if depth >= 1 => {}
                _ => break,
            }
            j += 1;
        }
        if j >= n || src[j].opcode != OpCode::Call || targeted[j] { i += 1; continue; }
        // the args must leave exactly what the call takes or the stack layout breaks
        let raw = src[j].operand as usize;
        if (raw & 0xFF) + 2 * ((raw >> 8) & 0xFF) != depth { i += 1; continue; }
        out[i..(j - 1)].copy_from_slice(&src[(i + 1)..j]);
        out[j - 1] = Instruction { opcode: OpCode::CallMethod, operand: src[i].operand };
        out[j] = Instruction { opcode: OpCode::CallMethodArgs, operand: src[j].operand };
        i = j + 1;
    }
    Cow::Owned(out)
}
