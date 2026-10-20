use crate::parser::{OpCode, Instruction, SSAChunk, ssa_strip};
use alloc::{borrow::Cow, boxed::Box, rc::Rc, vec, vec::Vec};
use core::cell::{Cell, OnceCell};

use super::scope::Kind;
use super::types::Val;


/* One instruction as the VM runs it, an opcode and up to three operands. */
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ins {
    pub op: OpCode,
    pub x: u8,
    pub a: u16,
    pub b: u16,
    pub c: u16,
}

impl Ins {
    fn of(i: Instruction) -> Self { Self { op: i.opcode, x: 0, a: i.operand, b: 0, c: 0 } }
}

/* What a chunk's lowering may assume about the slots of its frame. */
#[derive(Clone)]
pub(crate) struct Facts {
    /* Names lower to registers, which a class body's namespace never does. */
    pub regs: bool,
    /* Slots bound before the body runs, those below 128 as bits and any past them listed. */
    pub bound: u128,
    pub bound_far: Vec<usize>,
    /* What each slot is, a local, a closure cell or a module binding. */
    pub kinds: Rc<[Kind]>,
    /* The module whose bindings the globals index. */
    pub module: usize,
    /* What the code holds, `shape`. */
    pub shape: u8,
    /* Whether an op of the code met an operator dunder before, so lowered again it may lay one. */
    pub dunders: bool,
    /* The functions, sorted, whose ops met dunders noted under them. */
    pub noted: Vec<i64>,
    /* The code is a module's, which runs once. */
    pub once: bool,
}

impl Facts {
    /* Sets the bits of the slots bound before the body runs in the words `out`. */
    fn bind(&self, out: &mut [u64]) {
        for (w, o) in out.iter_mut().take(2).enumerate() { *o |= (self.bound >> (64 * w)) as u64; }
        for &k in &self.bound_far { if let Some(o) = out.get_mut(k / 64) { *o |= 1 << (k % 64); } }
    }
}

// What a chunk's code holds that its lowering looks for, read in one pass, a call, a loop, an attribute read, an append or a delete.
pub(crate) const CALLS: u8 = 1;
pub(crate) const LOOPS: u8 = 2;
pub(crate) const ATTRS: u8 = 4;
pub(crate) const APPENDS: u8 = 8;
pub(crate) const DELETES: u8 = 16;
// A jump, a handler, or flow that ends before the last instruction, so more than one block.
pub(crate) const JUMPS: u8 = 64;
// Set on every shape read, so a pool knows it read one.
pub(crate) const READ: u8 = 128;

/* What `src` holds, as bits of the constants above. */
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
            _ => 0,
        };
    }
    shape
}

/* A chunk's code as the VM runs it, mapped back to the compiler's instructions. */
pub(crate) struct Code {
    pub ins: Vec<Ins>,
    /* The compiler instruction each one came from, empty for unchanged code. */
    orig: Vec<u32>,
    /* Where each compiler instruction starts in `ins`, plus one past the end. */
    at: Vec<u32>,
    /* The compiler instruction a run stopped before `ins[i]` resumes from. */
    back: Vec<u32>,
    /* Frame length, the names then the constants then the temporaries. */
    pub frame: usize,
    pub kinds: Rc<[Kind]>,
    pub module: usize,
    /* Back-edges an unlowered frame took, its loop lowers once they reach `HOT_LOOP`. */
    pub hot: Cell<u32>,
    /* The sites its instructions learned, which every frame running it shares. */
    pub cache: super::cache::OpcodeCache,
    /* Whether a call may lay it as a section of its own code, `block::pure`, found when first asked (0 not yet, 1 it may, 2 not). */
    pure: Cell<u8>,
    /* The sections laid in it, none for most code. */
    pub block: Option<Box<Block>>,
    /* The same body lowered with no sections, which a caller lays when this one laid its own, lowered when one first asks. */
    pub plain: OnceCell<Option<Rc<Code>>>,
    /* Which code this is among all made, so a window knows the code it is laid out for. */
    pub layout: u32,
    /* Whether its frame's stacks end as they began, found by the first frame that runs it (0 not yet, 1 flat, 2 not). */
    flat: Cell<u8>,
    /* Calls from outside its sections that met another function than the section a head checks, after which it is laid again. */
    pub misses: Cell<u32>,
    /* How many entries of the calls' sites its lowering read, which it is remade from after a snapshot and which lay nothing new when met again. */
    pub read: Cell<u32>,
}

/* Calls after which a chunk's code is lowered again with its callees as sections, at once in a hot loop, as only code that runs often pays that off. */
pub(crate) const TIER: u32 = 64;
// The frames of one body running at once past which it is lowered again, a recursion's levels after running as its sections.
pub(crate) const LAY_DEPTH: u32 = 32;

/* Back-edges after which an unlowered loop carries on as lowered code. */
pub(crate) const HOT_LOOP: u32 = 16;

/* Calls a laid code's heads turn down for another function before it is laid again beside the first, and the functions a register call checks in turn. */
pub(crate) const MISSES: u32 = 64;
// Calls a head turns down for a function its table holds not, past which the code grows to lay it.
pub(crate) const TURNS: u32 = 24;
const ALTS: u32 = 16;

impl Code {
    /* Whether this is the compiler's code as written, each ip its own. */
    #[inline]
    pub fn unchanged(&self) -> bool { self.at.is_empty() }

    /* Where compiler instruction `ip` starts, the end past the last one. */
    #[inline]
    pub fn lowered(&self, ip: usize) -> usize {
        if self.at.is_empty() { ip.min(self.ins.len()) } else { self.at.get(ip).map_or(self.ins.len(), |&i| i as usize) }
    }

    /* The compiler instruction a run stopped before `ip` resumes from. */
    #[inline]
    pub fn resume(&self, ip: usize) -> usize {
        if self.back.is_empty() { ip.min(self.ins.len()) } else { self.back.get(ip).map_or(self.at.len() - 1, |&i| i as usize) }
    }

    /* Whether it pushes no handler, finally, spread or iterator, so a frame running it keeps no bases. */
    #[inline(always)]
    pub fn flat(&self) -> bool { self.pushes() == 1 }

    /* Whether it pushes only its loops' iterators, so a generator's frame running it finds its one base, the stepping loop's iterator, under them. */
    #[inline(always)]
    pub fn loops_only(&self) -> bool { self.pushes() <= 2 }

    /* 1 for code that pushes nothing on the handler, unwind, spread and iterator stacks, 2 for one pushing only iterators, 3 for any other. */
    #[inline(always)]
    fn pushes(&self) -> u8 {
        if self.flat.get() == 0 { self.flat.set(pushes_of(&self.ins)); }
        self.flat.get()
    }

    /* Whether a call may lay it as a section of its own code, `block::pure`. */
    pub fn pure(&self) -> bool {
        if self.pure.get() == 0 { self.pure.set(if super::block::pure(&self.ins) { 1 } else { 2 }); }
        self.pure.get() == 1
    }

    /* The compiler instruction `ins[i]` came from. */
    #[inline]
    pub fn orig(&self, i: usize) -> u32 { self.orig.get(i).copied().unwrap_or(i as u32) }
}

/* One past the highest register the register loop reads or writes, so a frame that long lets it skip each bounds check. */
pub(crate) fn reg_bound(ins: &[Ins]) -> usize {
    // A SectionR holds places in the code and frame, a lookup its head and table, a TurnR its words, none of them a register.
    ins.iter().filter(|i| i.op.is_register() && !matches!(i.op, OpCode::SectionR | OpCode::CallValueR | OpCode::JumpR | OpCode::TurnR)).map(|i| {
        use OpCode::*;
        // An operand that names a jump target, a module slot, a cell or a name instead of a register stays out.
        let a = !matches!(i.op, JumpIfFalseR | ForIterR | JumpUnlessEq | JumpUnlessNotEq | JumpUnlessLt | JumpUnlessLtEq | JumpUnlessGt | JumpUnlessGtEq | StoreGlobalR | StoreGlobalAtR | CallMethodR | CallGlobalR | CallSectionR | CallGlobalSectionR | CallMethodSectionR | CallValueSectionR | CallCtorSectionR | CallMethodGlobalSectionR | DunderSectionR | SectionR)
            && !(i.op == StoreTopR && i.x != 0);
        // A section call's StoreTopR keeps its return point in `b`.
        let b = !matches!(i.op, LoadGlobalR | LoadGlobalAtR | LoadCellR | StoreCellR | StoreTopR) && !(i.op == ForIterR && i.x != 0);
        let c = !matches!(i.op, LoadGlobalAtR | StoreGlobalAtR);
        let top = |on: bool, r: u16| if on { r as usize + 1 } else { 0 };
        top(a, i.a).max(top(b, i.b)).max(top(c, i.c))
    }).max().unwrap_or(0)
}

/* `Code::pushes` of `ins`, the first stack past the values it meets settling it. */
#[cold]
#[inline(never)]
fn pushes_of(ins: &[Ins]) -> u8 {
    let mut pushes = 1;
    for i in ins {
        match i.op {
            OpCode::GetIter => pushes = 2,
            OpCode::SetupExcept | OpCode::SetupFinally | OpCode::BeginArgs | OpCode::BeginFinally | OpCode::EndFinally | OpCode::UnwindFinally => return 3,
            _ => {}
        }
    }
    pushes
}

/* The compiler's own code, unchanged so each ip it marks is that instruction. */
pub(crate) fn identity(src: &[Instruction], names: usize, kinds: Rc<[Kind]>, module: usize) -> Code {
    // Room for the sentinel past the end.
    let mut ins: Vec<Ins> = Vec::with_capacity(src.len() + 1);
    ins.extend(src.iter().map(|&i| Ins::of(i)));
    // A fused call keeps both halves, the first carrying the counts past the second.
    for k in 1..ins.len() {
        if ins[k].op == OpCode::CallMethodArgs && ins[k - 1].op == OpCode::CallMethod { ins[k - 1].b = ins[k].a; ins[k - 1].x = 1; }
    }
    let cache = super::cache::OpcodeCache::new(ins.len());
    // Only the lowering writes register forms, so this code names no register past its names.
    debug_assert!(reg_bound(&ins) <= names, "a register form in unlowered code");
    let ins = sealed(ins);
    let layout = next_layout();
    Code { ins, orig: Vec::new(), at: Vec::new(), back: Vec::new(), frame: names, kinds, module, hot: Cell::new(0), cache, pure: Cell::new(2), block: None, plain: OnceCell::new(), layout, flat: Cell::new(0), misses: Cell::new(0), read: Cell::new(0)}
}

// The most parameters a section takes, those past five moved in from the registers a call passes them in.
const MAX_ARGS: usize = 12;

// Past this many pending values the lowering spills them, so temporaries stay few.
const MAX_PENDING: usize = 16;
// Values pending under a register call that stay in registers, which a section never writes and its stub takes to the stack and back.
const MAX_UNDER: usize = 6;

/* A stack value kept in a register until the stack needs it. */
#[derive(Clone, Copy, PartialEq)]
struct Entry {
    r: u16,
    // A local read put off until its use, bound by then.
    name: bool,
}

impl Entry {
    const fn reg(r: u16) -> Self { Self { r, name: false } }
}

/* Lowers fused stack code to register code, None when the frame cannot fit. */
/* What a block keeps for its growths. */
#[derive(Clone, Default)]
pub(crate) struct Grown {
    /* The instructions it was lowered from and what the lowering assumed, which a growth lowers from again. */
    pub src: Option<Option<Rc<Vec<Instruction>>>>,
    pub facts: Option<Rc<Facts>>,
    /* For each head laid past the end for a call of code that grew, where it returns (straight past the call) and where it lies, by return point. */
    pub displaced: Vec<(u32, u32)>,
    /* The heads, and the function each met, that a growth was asked for, so none is asked for twice. */
    pub tried: core::cell::RefCell<Vec<(u32, u64)>>,
    /* Calls its heads turned down for a function no table held, past `TURNS` of which each asks for a growth. */
    pub turned: Cell<u32>,
    /* The head and the function the last turned-down call met, noted once while the same come again. */
    pub last: Cell<(u32, u64)>,
    /* For each head and stub laid past the end for an op inside a section, where it starts and ends and the op it stands for in its section. */
    pub moved: Vec<(u32, u32, u32)>,
    /* The stub calls turned down before `TURNS`, and the function each met, which the first ask notes. */
    pub pending: core::cell::RefCell<Vec<(u32, Val)>>,
    /* The last calls whose asks were turned down for good, and the function each met, which turn down at once when met again. */
    pub refused: Cell<[(u32, u64); 4]>,
    pub refused_at: Cell<u8>,
    /* The last ops outside the sections that met a dunder after running, which ask for a head of their own as they run again. */
    pub ops: Cell<[u32; 8]>,
}

impl Grown {
    /* Notes the op at `i`, which met a dunder, so it asks for a head of its own as it runs again. */
    pub(crate) fn met(&self, i: u32) {
        let mut o = self.ops.get();
        if o.contains(&i) { return; }
        o.rotate_right(1);
        o[0] = i;
        self.ops.set(o);
    }
    /* Whether the op at `i` met a dunder. */
    pub(crate) fn meets(&self, i: u32) -> bool { self.ops.get().contains(&i) }
    /* Whether the call at `i` meeting `f` was turned down for good. */
    pub(crate) fn refuses(&self, i: u32, f: u64) -> bool { self.refused.get().contains(&(i, f)) }
    /* Notes the call at `i` meeting `f` as turned down for good. */
    pub(crate) fn refuse(&self, i: u32, f: u64) {
        let (mut r, at) = (self.refused.get(), self.refused_at.get());
        r[at as usize & 3] = (i, f);
        self.refused.set(r);
        self.refused_at.set(at.wrapping_add(1));
    }
}

impl Block {
    /* Where the instruction at `at` lies for the sections, itself or the op inside a section its head and stub past the end were laid for. */
    #[inline]
    pub(crate) fn origin(&self, at: usize) -> usize {
        match self.grown.get().filter(|_| self.moved) { Some(g) => g.moved.iter().find(|m| (m.0 as usize..m.1 as usize).contains(&at)).map_or(at, |m| m.2 as usize), _ => at }
    }
    /* The section of the op inside it a head or a stub laid past the end at `at` was laid for. */
    #[cold]
    #[inline(never)]
    pub(crate) fn moved_holding(&self, at: usize) -> Option<&Section> {
        let o = self.origin(at);
        (o != at).then(|| self.sections.iter().find(|s| (s.start as usize..s.end as usize).contains(&o))).flatten()
    }
    /* Whether the instruction at `at` is the code's own or a section's, no head or stub laid past either. */
    pub(crate) fn plain_at(&self, at: usize) -> bool { self.sections.first().is_none_or(|s| at < s.start as usize) || self.body_at(at).is_some() }
    /* The section whose body holds the instruction at `at`, none for a head or a stub. */
    pub(crate) fn body_at(&self, at: usize) -> Option<&Section> {
        let k = self.sections.partition_point(|s| s.start as usize <= at).checked_sub(1)?;
        self.sections.get(k).filter(|s| at < s.end as usize)
    }
    /* The section holding the instruction at `at`, and where in it the instruction lies for it. */
    pub(crate) fn holding(&self, at: usize) -> Option<(&Section, usize)> {
        // A head or a stub laid past the end lies outside every section, so one inside a section is never one.
        let find = |at: usize| { let k = self.sections.partition_point(|s| s.start as usize <= at).checked_sub(1)?; self.sections.get(k).filter(|s| at < s.end as usize).map(|s| (s, at)) };
        find(at).or_else(|| { let o = self.origin(at); (o != at).then(|| find(o)).flatten() })
    }
    /* What it keeps for its growths, made when first asked. */
    pub(crate) fn grown(&self) -> &Grown { self.grown.get_or_init(Default::default) }
    /* The heads laid past the end for calls of code that grew, none for most. */
    pub(crate) fn displaced(&self) -> &[(u32, u32)] { self.grown.get().map_or(&[], |g| &g.displaced[..]) }
}

#[derive(Clone)]
pub(crate) struct Block {
    pub sections: Box<[Section]>,
    /* The constants the sections read, held in the frame at the `ranges` places, one per growth giving its frame start and count of `extra`. */
    pub extra: Box<[Val]>,
    pub ranges: Box<[(usize, usize)]>,
    /* What each section was laid from, by the binding it is called through, so a growth lays more beside them. */
    pub callees: Vec<(u16, Callee<'static>)>,
    /* The code's layout by sections laid past its end, its own for code lowered whole, as each code of a lineage keeps the earlier ones' places. */
    pub lineage: u32,
    /* What its growths keep, made once a head first asks for one. */
    pub grown: OnceCell<Box<Grown>>,
    /* `scope::REBOUND` when every section's binding last held its function. */
    pub binds: Cell<u32>,
    /* The stub lookup tables, one per register call that met several functions, each function and its section at the slot its heap index picks. */
    pub alt_tab: Box<[(Val, u32)]>,
    /* Where each loop over a generator's section lies in the code's own instructions, with its section and head, so a stopped run resumes here. */
    pub loops: Box<[(u32, u32, u32, u32)]>,
    /* Whether a growth laid a head past the end for an op inside a section, which the section's walks take for the op. */
    pub moved: bool,
    /* For a method call's table, the section a receiver's class last found at the slot the class picks, and the class version then. */
    pub by_class: Box<[Cell<(Val, u32, u32)>]>,
}

#[derive(Clone)]
/* One function laid once as a section, where its instructions lie, its link register and the function, kept so a collection never lends its slot. */
pub(crate) struct Section {
    pub start: u32,
    pub end: u32,
    pub link: u16,
    // The function the section was laid from, or for one any function of its body runs the one it was last entered with.
    pub value: Cell<Val>,
    // Whether a call back into it keeps a level of a recursion, the register before its link noting where the stack began.
    pub levels: bool,
    // The module the section's function lies in.
    pub module: usize,
    // The module binding that holds the function, and the module it lies in, an importing one's for an imported function.
    pub global: u16,
    pub bound: usize,
    pub fi: usize,
    // For each of its instructions the compiler instruction of the body it was laid from, where an error inside it points.
    pub orig: Box<[u32]>,
    // For a class's `__init__`, the class version it was last laid for, the attribute room its instances start with, and the `__init__`.
    pub ctor: Option<(Cell<u32>, usize, Val)>,
    // For a section any function of its body may run, its parameter count and the register of each captured cell, filled by a call with the defaults.
    pub by_body: Option<(u8, Box<[u16]>)>,
    // For a method a `super()` proxy's call runs, the class past which the instance's class finds it, and its name.
    pub via_super: Option<(Val, alloc::rc::Rc<str>)>,
    // For a body that makes functions, the register each of its slots lies in, which their captures read, empty for any other.
    pub slots: alloc::rc::Rc<[u16]>,
    // For an op's dunder, the class version and the two classes an op's call last laid it for and the one it last did not, checked first.
    pub dunder: Cell<(u32, Val, Val, Val)>,
    // For an op's dunder, the class version and the class of a right operand last let it run first.
    pub other: Cell<(u32, Val)>,
    // For an op's dunder, the left instance it first held for and the class version then, which holds at once until a class changes or a slot is lent.
    pub seen: Cell<(Val, u32)>,
    // Whether the first class it holds for has no subclass, whose instance on the right needs no look, while no class changed.
    pub lone: Cell<bool>,
    // For a method, the last receiver checked, its class and the class version then, which hold again at once until a class changes or a slot is lent.
    pub guard: Cell<(Val, u32, Val)>,
    // For a generator's section a loop steps, the compiler instruction and head of the call whose return hands the loop its value via a `yield`.
    pub tail: Option<(u32, u32)>,
    // For a generator's section, the register its loop's step comes back by.
    pub step_reg: u16,
    // For a generator's section with a `try` or `with`, the first of two registers noting where the stacks began, `u16::MAX` for any other.
    pub held: u16,
    // For a generator's section, where a step resumes past each `yield`, the compiler instruction a suspended object carries on at and the section index.
    pub steps: Box<[(u32, u32, u32, u32)]>,
    // For a method head's section, where its table of other methods starts in `Block::alt_tab` and its size less one, `u32::MAX` for none.
    pub table: (u32, u32),
    // Its parameters.
    pub arity: u8,
    // For a method, the class of the receiver it was laid for, whose order a `super()` in it follows, undef when unknown.
    pub class: Val,
    // 1 for a function taking `*args` alone or 2 for `**kwargs` alone, whose section a spreading call enters with it packed into a tuple or dict.
    pub packs: u8,
    // For a method a call on its class itself runs, `Callee::on_class`.
    pub on_class: u8,
    // The defaults of its last parameters, which a call entering it from another head's table passes no argument for.
    pub defaults: Box<[Val]>,
}

/* A function a module binding holds that a call may lay as a section, with its lowered body and frame (names, constants, None, True, False, temporaries). */
#[derive(Clone)]
pub(crate) struct Callee<'p> {
    pub value: Val,
    pub fi: usize,
    // The cells a closure captured, by the slot each fills, the same for every call of this function.
    pub cells: Vec<(usize, Val)>,
    // The defaults of its trailing parameters, and how many parameters a call must pass.
    pub defaults: Vec<Val>,
    pub required: usize,
    // For an op's dunder, whether it is the reflected one, whose instance is the op's right operand.
    pub reflected: bool,
    // For a truth test's dunder, the check its result takes, 2 for a `__bool__` and 3 for a `__len__`.
    pub check: u8,
    pub body: Rc<Code>,
    pub arity: usize,
    pub names: usize,
    pub consts: Vec<Val>,
    pub module: usize,
    /* The module of the binding a call reads it through when not the function's own (an imported name), `u16::MAX` for its own. */
    pub bound: u16,
    /* What few callees have, kept apart so the many that have none of it stay small. */
    pub rare: Option<Box<Rare>>,
    // Its parameters' names, which a keyword call matches.
    pub params: &'p [alloc::string::String],
    // How many of them are positional-only, which no keyword call matches.
    pub posonly: usize,
    // Its body's constants, which name the parameters a keyword call in it passes.
    pub literals: &'p [crate::parser::Value],
    /* Whether any function of its body may run its section, its cells and defaults read from the function each call makes. */
    pub by_body: bool,
    /* For a closure, whether a section any function of its body runs lies right after its own, for sibling closures once the first is gone. */
    pub twin: bool,
    /* For a method that calls `super()`, the module binding of the builtin, its proxy made from the instance in place. */
    pub supers: Option<u16>,
    /* For a function taking `*args` or `**kwargs`, the parameter of each past its positionals, which a call packs the rest into. */
    pub star: Option<usize>,
    pub dstar: Option<usize>,
    /* The slots its body keeps in cells, which a call wraps as it enters. */
    pub cellvars: Vec<usize>,
    /* For a generator one `for` steps, each `yield` hands its value to the loop's variable and resumes the loop's body, and its end leaves the loop. */
    pub generator: bool,
    /* The parameters a call may pass by position, those before a lone `*`. */
    pub positional: usize,
    /* For a method, the class of the receiver it was found for, whose order a `super()` in it follows, undef when unknown. */
    pub class: Val,
    /* How a call on class `class` takes it, 1 static or plain, 2 class method, 3 `__call__`, 7 a bound method's function, 4 an instance's, else 0. */
    pub on_class: u8,
    /* For a class deriving from an exception, whose instance takes the arguments of its call as its `args`. */
    pub exc: bool,
}

/* What a callee has apart. */
#[derive(Clone, Default)]
pub(crate) struct Rare {
    // For a method, the site that found it, which the stub of a call of it starts with.
    pub site: Option<super::sites::Site>,
    // For an attribute's `__getattr__`, the name it is called with, its second parameter.
    pub arg: Option<Val>,
    /* For a class, the `__init__` laid in its place with the class version and instance attribute room, the call passing and returning the instance. */
    pub ctor: Option<(u32, usize, Val)>,
    /* For the method a `super()` proxy's call finds, the calling method's class and the method name, which the call checks still resolve the same. */
    pub via_super: Option<(Val, alloc::rc::Rc<str>)>,
}

impl Callee<'_> {
    pub(crate) fn site(&self) -> Option<super::sites::Site> { self.rare.as_ref().and_then(|r| r.site) }
    pub(crate) fn arg(&self) -> Option<Val> { self.rare.as_ref().and_then(|r| r.arg) }
    pub(crate) fn ctor(&self) -> Option<(u32, usize, Val)> { self.rare.as_ref().and_then(|r| r.ctor) }
    pub(crate) fn via_super(&self) -> Option<&(Val, alloc::rc::Rc<str>)> { self.rare.as_ref().and_then(|r| r.via_super.as_ref()) }
    /* What it has apart, made when first written. */
    pub(crate) fn rare_mut(&mut self) -> &mut Rare { self.rare.get_or_insert_with(Default::default) }
    pub(crate) fn set_site(&mut self, v: Option<super::sites::Site>) { if v.is_some() || self.rare.is_some() { self.rare_mut().site = v; } }
    pub(crate) fn set_arg(&mut self, v: Option<Val>) { if v.is_some() || self.rare.is_some() { self.rare_mut().arg = v; } }
    pub(crate) fn set_ctor(&mut self, v: Option<(u32, usize, Val)>) { if v.is_some() || self.rare.is_some() { self.rare_mut().ctor = v; } }
    pub(crate) fn set_via_super(&mut self, v: Option<(Val, alloc::rc::Rc<str>)>) { if v.is_some() || self.rare.is_some() { self.rare_mut().via_super = v; } }
}

/* Whether the method call `c` at `j` of `ins` is on the proxy the `super()` just before it made, `g` being the module binding of `super`. */
fn proxy(ins: &[Ins], j: usize, c: Ins, g: u16) -> bool {
    // Values pending under the call move between the store of the proxy and the call, never into its register.
    let Some(m) = (j.saturating_sub(5)..j).rev().find(|&m| ins[m].op == OpCode::StoreTopR && ins[m].x == 0 && ins[m].a == c.b) else { return false };
    m >= 1 && ins[m - 1].op == OpCode::CallGlobalR && ins[m - 1].x == 0 && ins[m - 1].a == g
        && ins[m + 1..j].iter().all(|i| matches!(i.op, OpCode::PushRegs | OpCode::StoreTopR) && !(i.op == OpCode::StoreTopR && i.a == c.b))
}

/* Whether the body of `c` has a `try`, whose handlers a recursion entering it through a head's table would share. */
fn guarded(c: &Callee) -> bool {
    c.body.ins.iter().any(|i| matches!(i.op, OpCode::SetupExcept | OpCode::SetupFinally))
}

/* Whether the section wanted for `w` serves call `c` of the same function, called the same way and laid for the same receiver class under `super()`. */
fn same_laid(w: &Callee, c: &Callee) -> bool {
    w.on_class == c.on_class && w.via_super().is_some() == c.via_super().is_some() && (c.supers.is_none() || w.class.0 == c.class.0) && w.ctor().map(|c| c.2.0) == c.ctor().map(|c| c.2.0)
}

/* Whether a spreading call (a mapping for `kind` 2) may jump to `c`'s section, a plain function or one taking `*args` or `**kwargs` alone. */
fn spreads(c: &Callee, kind: u8) -> bool {
    let fits = if kind == 2 { c.star.is_none() && c.dstar == Some(0) } else { c.dstar.is_none() && (c.star.is_none() && c.arity == c.positional && c.defaults.is_empty() || c.star == Some(0)) };
    c.ctor().is_none() && !c.by_body && c.via_super().is_none() && c.supers.is_none() && fits && (c.star.is_none() && c.dstar.is_none() || c.arity == 1)
}

/* The function the call through a register at `j` of `code` found. */
pub(crate) fn value_of(code: &Code, j: usize) -> Option<Val> {
    match code.cache.site(j) { super::sites::Site::Call { callee, .. } => Some(callee), _ => None }
}

/* The site of the method call at `j` of `code`, when it found a method. */
pub(crate) fn method_of(code: &Code, j: usize) -> Option<super::sites::Site> {
    let site = code.cache.site(j);
    matches!(site, super::sites::Site::Method { .. }).then_some(site)
}

/* An operand of a section call laid with the sections. */
#[derive(Clone, Copy)]
enum Prep { Param(usize, usize), Const(Val), GenReg(usize), GenVal(usize), GenResume(usize), GenEntry(usize) }

/* What a call lays as a section, a module binding's function (only with `*args` or `**kwargs`) or the method a call at a compiler instruction found. */
#[derive(Clone, Copy)]
pub(crate) enum Want { Global(usize, u16), Held(usize, u16), Spread(usize, u16), Method(u32), Value(u32), Nested(usize, u32, Option<super::sites::Site>, u16), NestedValue(usize, u32, Option<Val>), Super(usize, u16, Val, Val), Dunder(u32, OpCode), NestedDunder(usize, u32, Option<super::sites::Site>, OpCode), NestedDunderAlt(usize, u32, Val, u32, OpCode), Alt(Option<usize>, u32, u32), MethodAlt(Option<usize>, u32, u32), Func(Val, bool), Celled(Val), DunderFunc(Val, OpCode, u16), DunderAlt(u32, u32, OpCode), Gen(usize, u16), GenFunc(Val), GenAt(u32), Body(*const SSAChunk), BodyOf(Val), Made(*const SSAChunk), MadeIn(usize, u16), GenBody(*const SSAChunk) }

impl Callee<'_> {
    /* The module of the binding a call reads the function through. */
    pub(crate) fn bound_module(&self) -> usize { if self.bound == u16::MAX { self.module } else { self.bound as usize } }
}

/* What laying the sections carries along, the next free register, the constants and their readers, the sections, generator steps and their loops. */
struct LayState {
    next: usize,
    extra: Vec<Val>,
    uses: Vec<(usize, u8, usize)>,
    sections: Vec<Section>,
    gens: Vec<(u16, Option<Val>)>,
    slots: crate::util::hash::FxHashMap<u64, usize>,
    late: Vec<(usize, usize, usize, usize)>,
}

/* A `for` stepping a generator section laid for it, with its head, section, step, ForIter, exit, variable and the call as written for its stub. */
struct GenFor { head: usize, k: usize, step: usize, at: usize, exit: usize, target: u16, global: Option<u16>, call: Ins, rest: Ins }

/* A `for` over a generator laid for it whose step the next ForIter lays, with its section, step jump, call registers, variable or binding, and exit. */
struct GenNext { k: usize, skip: usize, list: Vec<u16>, regs: Vec<u16>, target: u16, global: Option<u16>, exit: usize }

/* The call of an op's dunder section with the op's operands, swapped for a reflected dunder (marked by `a`) whose instance is the right operand. */
fn call_of(op: Ins, mode: u16) -> Ins {
    match op.op {
        // A negation's dunder takes its operand alone, a store's the object, key and value, a test's the container and item, none checking the other operand.
        OpCode::MinusR | OpCode::GetAttrR => Ins { x: 1, a: 2, b: op.b, c: op.b, ..op },
        // A builtin's call checks the builtin is not rebound.
        OpCode::CallStr | OpCode::CallRepr => Ins { x: 1, a: 3, b: op.b, c: op.b, ..op },
        OpCode::StoreItemR => Ins { x: 3, a: 2, b: op.a, c: op.b, ..op },
        OpCode::SetAttrR => Ins { x: 2, a: 2, b: op.a, c: op.b, ..op },
        OpCode::InR => Ins { x: 2, a: 2, b: op.c, c: op.b, ..op },
        // A reflected dunder's instance is the right operand, for a left number (1), after the left one declined (6), or for a subclass on the right (7).
        _ if mode != 0 => Ins { x: 2, a: mode, b: op.c, c: op.b, ..op },
        _ => Ins { x: 2, a: 0, ..op },
    }
}

/* Whether `op` is an op whose operator dunder a section may run. */
pub(crate) fn dunder_op(op: OpCode) -> bool {
    use OpCode::*;
    matches!(op, AddR | SubR | MulR | DivR | ModR | FloorDivR | InPlaceAddR | InPlaceSubR | EqR | NotEqR | LtR | LtEqR | GtR | GtEqR | GetItemR)
}

/* The site of the op at `j` of `code`, when it found an operator dunder of two operands. */
pub(crate) fn dunder_of(code: &Code, j: usize) -> Option<super::sites::Site> {
    let site = code.cache.site(j);
    matches!(site, super::sites::Site::Dunder { arity: 2 | super::sites::REFLECTED | super::sites::INVERTS, .. }).then_some(site)
}

/* The site of the load of an attribute at `j` of `code`, when it found a property's getter. */
fn getter_of(code: &Code, j: usize) -> Option<super::sites::Site> {
    let site = code.cache.site(j);
    matches!(site, super::sites::Site::Dunder { arity: 1, .. }).then_some(site)
}

/* What a section's body holds that its laying looks for, read in one pass. */
struct Marks { makes: bool, deletes: bool, builds: bool, tried: bool, stacked: bool, yields: usize, yields_stack: bool }

fn body_marks(ins: &[Ins]) -> Marks {
    use OpCode::*;
    let mut m = Marks { makes: false, deletes: false, builds: false, tried: false, stacked: false, yields: 0, yields_stack: false };
    for i in ins {
        match i.op {
            MakeFunction => { m.makes = true; m.builds = true; }
            MakeCoroutine => m.makes = true,
            Del => m.deletes = true,
            BuildList | BuildDict | BuildSet | CallList | CallDict | CallSet | CallTuple => m.builds = true,
            SetupExcept | SetupFinally => m.tried = true,
            ReturnValue | StoreName => m.stacked = true,
            Yield => { m.yields += 1; m.yields_stack |= i.x == 0; }
            _ => {}
        }
    }
    m
}

/* Lowers fused stack code to register code, None when the frame cannot fit, `sections` giving the function a call may lay. */
pub(crate) fn lower<'p>(src: &[Instruction], chunk: &SSAChunk, facts: &Facts, sections: &dyn Fn(Want) -> Option<Callee<'p>>) -> Option<Code> {
    let (names, consts) = (chunk.names.len(), chunk.constants.len());
    let len = src.len();
    // Code of one block marks no instruction.
    let blocks = facts.shape & JUMPS != 0;
    let mut flags = if blocks { vec![0u32; len + 1] } else { Vec::new() };
    for i in (0..len).filter(|_| blocks) {
        let ins = src[i];
        if ins.opcode.is_jump() && (ins.operand as usize) <= len {
            let forward = ins.opcode == OpCode::Jump && ins.operand as usize > i + 1;
            flags[ins.operand as usize] |= TARGET | START | if forward { FORWARD } else { ENTERED };
        }
        // `Unwind::Goto` resumes at the instruction after UnwindFinally.
        if ins.opcode == OpCode::UnwindFinally { flags[i + 1] |= TARGET | START | ENTERED; }
        if ins.opcode.is_jump() || ends_flow(ins.opcode) { flags[i + 1] |= START; }
    }
    if let Some(f) = flags.first_mut() { *f |= START; }
    let mut deleted = Vec::new();
    for ins in src.iter().filter(|i| facts.shape & DELETES != 0 && i.opcode == OpCode::Del) {
        deleted.resize(names, false);
        if let Some(s) = deleted.get_mut(ins.operand as usize) { *s = true; }
    }
    // Code of one block binds at its start only what its call binds.
    let one = flags.is_empty() || !flags[1..len.max(1)].iter().any(|f| f & START != 0);
    let bound_at = (facts.regs && !one).then(|| bound_on_entry(src, chunk, names, facts, &mut flags));
    let (appends, resets) = if facts.regs && facts.shape & APPENDS != 0 { append_loops(src, facts) } else { (Vec::new(), Vec::new()) };
    let cbase = names;
    let tbase = names + consts + 3;
    // In a module's code, the instructions no loop runs again, whose calls are not laid as what they call runs once.
    let once = if facts.once && facts.regs && facts.shape & CALLS != 0 { runs_once(src) } else { Vec::new() };
    let mut l = Lower {
        src, chunk, facts, flags: &flags, deleted, names, cbase, consts, tbase,
        out: Vec::with_capacity(len), orig: Vec::with_capacity(len + 1), at: vec![0; len + 1], back: Vec::with_capacity(len + 1),
        pending: Vec::with_capacity(MAX_PENDING), patches: Vec::new(), temps: 0, last: None, cur: 0, takes: false, under: Vec::new(), quiet: 0, bound: vec![0; names.div_ceil(64)],
        appends, lists: core::cell::OnceCell::new(), sections, wanted: Vec::new(), heads: Vec::new(), prep: Vec::new(), seeds: Vec::new(), dest: None, resume: None,
        gaps: 0, kwcall: None, kwstubs: Vec::new(), alts: Vec::new(), lookups: Vec::new(), dunders: Vec::new(), declining: Vec::new(), dunder_alts: Vec::new(), dunder_heads: Vec::new(), head_arg: None, yields: Vec::new(),
        genfors: Vec::new(), gen_next: None, gen_args: Vec::new(), ret_to: None, nested: Vec::new(), gen_loops: Vec::new(), gen_steps: Vec::new(),
        unders: Vec::new(), stub_stores: Vec::new(), keyed: false, more_args: Vec::new(), wide: false, once: false, old: None, first: 0, alts_first: 0, only: None, calls: OnceCell::new(), displaced: Vec::new(), turn_bases: Vec::new(), tern: None,
    };
    let mut i = 0;
    let terns = chunk.ternaries();
    while i < len {
        // What falls into a block lands on the stack under the instruction before it.
        if flags.get(i).map_or(i == 0, |f| f & START != 0) {
            // The arms of `a if c else b` keep what was pending before it, the join taking the value either left.
            match l.tern.as_deref() {
                Some(t) if terns && (t.else_at == i || t.start_a == i) => {}
                Some(t) if terns && t.join == i && !t.bail => l.join_tern(),
                _ => l.flush(),
            }
            l.quiet = i as u32;
            match &bound_at {
                Some(b) => b.entry(&flags, i, &mut l.bound),
                None => {
                    l.bound.fill(0);
                    if facts.regs { facts.bind(&mut l.bound); }
                }
            }
            // Entering a loop that appends to a local gives up the string it owned, a back-edge skips this.
            for &(_, slot) in resets.iter().filter(|r| r.0 == i) { l.emit(Ins { op: OpCode::OwnResetR, x: 0, a: slot, b: 0, c: 0 }); }
        }
        l.cur = i as u32;
        l.once = once.get(i).copied().unwrap_or(false);
        l.at[i] = l.resume.take().unwrap_or(l.out.len()) as u32;
        // Values a call takes that joining paths left on the stack, most often from `a if c else b`, come back into registers so the call may be laid.
        if !l.takes && let Some(m) = l.joined_args(i) {
            let first = l.pending.len();
            for _ in 0..m { let t = l.temp(); l.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 }); l.pending.insert(first, Entry::reg(t)); }
        }
        if core::mem::take(&mut l.takes) {
            // What the call left under it comes back off the stack, into its temporaries since a resume in unlowered code never wrote them.
            let under = core::mem::take(&mut l.under);
            l.pending.extend_from_slice(&under);
            let t = l.temp();
            l.produce(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
            for e in under.iter().rev() {
                l.emit(if e.name { Ins { op: OpCode::PopTop, x: 0, a: 0, b: 0, c: 0 } } else { Ins { op: OpCode::StoreTopR, x: 0, a: e.r, b: 0, c: 0 } });
            }
        }
        let before = l.out.len();
        let step = l.one(i);
        // A fused group's later instructions start where its last instruction does.
        for k in 1..step { l.at[i + k] = before.max(l.out.len().saturating_sub(1)) as u32; }
        for (k, &ins) in src[i..i + step].iter().enumerate() { transfer(&mut l.bound, ins, i + k, chunk); }
        i += step;
        if l.pending.is_empty() { l.quiet = i as u32; }
    }
    l.flush();
    if terns && l.tern.is_some() { return None; }
    l.at[len] = l.out.len() as u32;
    for &p in &l.patches {
        let t = l.out[p].a as usize;
        // A jump back to a loop over a generator's section goes to its step.
        l.out[p].a = match l.genfors.iter().find(|g| g.at == t) { Some(g) => g.step as u16, None if t <= len => l.at[t] as u16, None => u16::MAX };
    }
    let frame = if facts.regs { tbase + l.temps } else { names };
    // The sections the calls want lie past the code, behind a jump to its end, their registers past the temporaries.
    let (block, frame) = l.lay(frame);
    // A resume after a call with several values pending under it enters its stub's store, which takes them back too.
    for &(at, store) in l.stub_stores.iter().filter(|s| l.unders.iter().any(|u| u.0 == s.0)) {
        for a in l.at.iter_mut().filter(|a| **a as usize == at + 1) { *a = store as u32; }
    }
    if l.out.len() >= u16::MAX as usize { return None; }
    // The register loop reads registers unchecked, so the frame covers every one it names.
    debug_assert!(reg_bound(&l.out) <= frame, "a register past the frame");
    let frame = frame.max(reg_bound(&l.out));
    // Registers are u16 operands.
    if frame >= u16::MAX as usize { return None; }
    l.orig.push(len as u32);
    l.back.push(len as u32);
    // A register `yield` keeps its next step's start in `c` and that compiler instruction in `b`, a `c` of 1 marking a loop stepped in place.
    for &k in &l.yields {
        let next = match l.out.get(k + 1) { Some(j) if j.op == OpCode::Jump && (j.a as usize) < l.out.len() => j.a as usize, _ => k + 1 };
        (l.out[k].b, l.out[k].c) = (l.back[next] as u16, next as u16);
        if next + 1 == k && l.out[next].op == OpCode::ForIterR && l.out[next].x == 0 && l.out[next].b == l.out[k].a { l.out[next].c = 1; }
    }
    let cache = super::cache::OpcodeCache::new(l.out.len());
    for &(at, site) in l.seeds.iter().filter(|s| s.0 & SEEDED != 0) { cache.set_site(at & !SEEDED, site); }
    // Section calls and returns take the instruction of their kind, past the lowering, so each runs no check of flags.
    if block.is_some() { specialize(&mut l.out, 0); }
    // Code that laid sections is never laid itself, any other is looked at once a call would lay it.
    let pure = Cell::new(if block.is_none() { 0 } else { 2 });
    let layout = next_layout();
    let mut block = block;
    if let Some(b) = block.as_mut() { b.lineage = layout; }
    Some(Code { ins: sealed(l.out), orig: l.orig, at: l.at, back: l.back, frame, kinds: facts.kinds.clone(), module: facts.module, hot: Cell::new(0), cache, pure, block, plain: OnceCell::new(), layout, flat: Cell::new(0), misses: Cell::new(0), read: Cell::new(0)})
}


/* What a turned-down head asks a growth for, the function a register call met, the method a call met, or the function its binding now holds. */
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Grow { Value, Method, Global, Dunder, Site, Op, All, Chain }

impl Grow {
    /* Every kind, by its index. */
    pub(crate) const ALL: [Grow; 8] = [Grow::Value, Grow::Method, Grow::Global, Grow::Dunder, Grow::Site, Grow::Op, Grow::All, Grow::Chain];
}

/* The compiler instruction the head at `at` of `code` was laid for, and the function whose body it lies in when inside a section. */
pub(crate) fn head_origin(code: &Code, at: usize) -> (Option<usize>, u32) {
    match code.block.as_deref().and_then(|b| b.holding(at)) {
        Some((s, at)) => (Some(s.fi), s.orig[at - s.start as usize]),
        None => (None, code.orig(at)),
    }
}

/* Whether the head at `at` of `ins` is a keyword call's, whose stub makes the call off the stack with its keywords. */
pub(crate) fn kw_head(ins: &[Ins], at: usize) -> bool {
    let stub = ins[at + 3].a as usize;
    ins.get(stub..).into_iter().flatten().take(32).take_while(|j| !(j.op == OpCode::Jump && j.x == 1 && j.a as usize == at + HEADER as usize)).any(|j| j.op == OpCode::Call && j.a >> 8 != 0)
}

/* The head of the op whose dunder heads include the one at `at`, the head itself or the one each head its stub checks returns past. */
fn dunder_main(ins: &[Ins], at: usize) -> Option<usize> {
    let head = |h: usize| ins.get(h).is_some_and(|i| i.op == OpCode::DunderSectionR) && ins.get(h + 3).is_some_and(|t| t.op == OpCode::SectionR && t.x == STUB);
    if !head(at) { return None; }
    // Every other head of the op is laid past its own, which is the first that runs the op.
    let run = ins[at + 3].b;
    (0..=at).find(|&h| head(h) && ins[h + 3].b == run)
}

/* Whether the instruction at `h` of `ins` is a head calling a binding's section or a class's constructor, which a growth may move. */
fn global_head(ins: &[Ins], h: usize) -> bool {
    matches!(ins[h].op, OpCode::CallGlobalSectionR | OpCode::CallSectionR | OpCode::CallCtorSectionR)
        && ins.get(h + 2).is_some_and(|s| s.op == OpCode::SectionR && s.x & (8 | 32 | 64) == 0 && s.x & (4 | 16) != 4)
        && ins.get(h + 3).is_some_and(|s| s.op == OpCode::SectionR && s.x == STUB)
}

/* `old` grown by the sections the head at `at` asks for, laid past its end so every instruction keeps its place, None when nothing new is laid. */
#[allow(clippy::too_many_arguments)]
pub(crate) fn extend<'p>(old: &Code, src: &[Instruction], chunk: &SSAChunk, facts: &Facts, sections: &dyn Fn(Want) -> Option<Callee<'p>>, at: usize, kind: Grow, func: Val) -> Option<Code> {
    if kind == Grow::All { return extend_all(old, src, chunk, facts, sections, at != 0); }
    let block = old.block.as_deref()?;
    if block.callees.len() != block.sections.len() || old.unchanged() { return None; }
    let (names, consts) = (chunk.names.len(), chunk.constants.len());
    let len = old.ins.len();
    let (mut orig, mut back) = (old.orig.clone(), old.back.clone());
    orig.pop();
    back.pop();
    let wanted: Vec<(u16, Callee<'p>)> = block.callees.clone();
    // The pairs of the tables it has, each head's section and one its table holds.
    let mut alts = Vec::new();
    for (k, s) in block.sections.iter().enumerate().filter(|s| s.1.table.1 != u32::MAX) {
        let (start, mask) = (s.table.0 as usize, s.table.1 as usize);
        alts.extend(block.alt_tab[start..=start + mask].iter().filter(|e| e.1 != u32::MAX).map(|e| (k, (e.1 & !DEFAULTS) as usize)));
    }
    let mut l = Lower {
        src, chunk, facts, flags: &[], deleted: Vec::new(), names, cbase: names, consts, tbase: names + consts + 3,
        out: old.ins.clone(), orig, at: old.at.clone(), back,
        pending: Vec::new(), patches: Vec::new(), temps: 0, last: None, cur: 0, takes: false, under: Vec::new(), quiet: 0, bound: vec![0; names.div_ceil(64)],
        appends: Vec::new(), lists: core::cell::OnceCell::new(), sections, wanted, heads: Vec::new(), prep: Vec::new(), seeds: Vec::new(), dest: None, resume: None,
        gaps: 0, kwcall: None, kwstubs: Vec::new(), alts_first: alts.len(), alts, lookups: Vec::new(), dunders: Vec::new(), declining: Vec::new(), dunder_alts: Vec::new(), dunder_heads: Vec::new(), head_arg: None, yields: Vec::new(),
        genfors: Vec::new(), gen_next: None, gen_args: Vec::new(), ret_to: None, nested: Vec::new(), gen_loops: Vec::new(), gen_steps: Vec::new(),
        unders: Vec::new(), stub_stores: Vec::new(), keyed: false, more_args: Vec::new(), wide: false, once: false, old: Some(block), first: block.callees.len(), only: Some(func), calls: OnceCell::new(), displaced: Vec::new(), turn_bases: Vec::new(), tern: None,
    };
    let (scope, cat) = head_origin(old, at);
    l.cur = cat;
    let k = l.out[at + 2].c as usize;
    let call = l.out[at];
    // The heads moved to a binding's new section, by where each is laid now, and the section.
    let mut moved: Vec<(usize, usize, usize)> = Vec::new();
    let mut detach: Option<usize> = None;
    let mut swapped: Option<usize> = None;
    // The section an op or a call inside one that grows a head lies in, and where the head laid for a call starts.
    let mut enclosing: Option<usize> = None;
    let mut laid_from: Option<usize> = None;
    // Where the head that asked lies for the sections, itself or the op inside a section a head past the end was laid for.
    let lies = block.origin(at);
    match kind {
        // A keyword call's head passes its arguments as its laid function names its parameters, which no other function's section takes.
        Grow::Method if kw_head(&l.out, at) => return None,
        // One through a register is laid again for the function met.
        Grow::Value if kw_head(&l.out, at) => {
            // A class met lays its constructor, the head checking the class and making the instance.
            let mut c = (l.sections)(Want::Func(func, false))?;
            (c.twin, c.by_body) = (false, false);
            if !l.rekeyed(at, c, u16::MAX) { return None; }
        }
        // A generator loop's head steps the section of another generator it met by the same register, and a spreading head lays the other function it met.
        Grow::Value if l.out[at + 2].x & 72 == 72 => {
            let c = (l.sections)(Want::Func(func, false)).filter(|c| spreads(c, call.x) && c.ctor().is_none() && !c.by_body)?;
            let arity = c.arity;
            let to = l.want_callee(u16::MAX, c, arity)?;
            if to == k || l.alts.contains(&(k, to)) { return None; }
            l.alts.push((k, to));
        }
        Grow::Value if let Some(&(_, exit, _, _)) = block.loops.iter().find(|g| g.3 as usize == at) => {
            let n = call.x as usize;
            let c = (l.sections)(Want::GenFunc(func)).filter(|c| c.star.is_none() && c.dstar.is_none() && n >= c.required && n <= c.arity && super::block::gen_pure(&c.body.ins))?;
            let step = block.sections[k].step_reg;
            // The loop binds its variable through the head's store, or a module binding past it.
            let target = (l.out.get(at + HEADER as usize).map(|i| i.op) != Some(OpCode::StoreGlobalR)).then_some(l.out[at + 1].a);
            let k2 = l.wanted.len();
            l.gen_loops.push((k2, exit as u16, target, at as u16 + HEADER, true));
            l.gen_steps.push((k2, step));
            let to = l.want_callee(u16::MAX, c, n)?;
            if to != k2 { return None; }
            l.alts.push((k, to));
        }
        Grow::Value => {
            // It lays every function its head met, which its asks noted.
            l.only = None;
            l.want_alts(scope, cat, k, call.x as usize);
            if l.alts.len() == l.alts_first { return None; }
            // A head whose own function no longer comes takes the one met as its own, its old one going to its table, when both take the same arguments.
            let met = l.alts[l.alts_first..].iter().position(|a| a.0 == k && l.wanted[a.1].1.value.0 == func.0).map(|p| l.alts[l.alts_first + p].1);
            let own = &l.wanted[k].1;
            let fresh = |c: &Callee| c.arity == call.x as usize && !c.by_body;
            if let Some(to) = met && call.op == OpCode::CallValueSectionR && call.x <= 2 && fresh(own) && fresh(&l.wanted[to].1) {
                let others: Vec<usize> = l.alts.iter().filter(|a| a.0 == k && a.1 != to).map(|a| a.1).collect();
                for a in others.into_iter().chain(core::iter::once(k)) { if !l.alts.contains(&(to, a)) { l.alts.push((to, a)); } }
                l.out[at + 2].c = to as u16;
                swapped = Some(to);
            }
            // A head whose stub looks up no table checks the table first.
            let stub = l.out[at + 3].a as usize;
            let looks = l.out.get(stub).is_some_and(|i| i.op == OpCode::CallValueR && i.a as usize == at);
            if call.op != OpCode::CallValueSectionR && !looks {
                let from = l.out.len();
                l.lookups.push((l.out.len(), k));
                l.emit(Ins { op: OpCode::CallValueR, x: 0, a: at as u16, b: 0, c: 0 });
                l.emit(Ins { op: OpCode::Jump, x: 1, a: stub as u16, b: 0, c: 0 });
                l.out[at + 3].a = from as u16;
            }
        }
        // A head with no table (a static, class or register call) checks a head past the end for the function met, held by the receiver or its class.
        Grow::Chain => {
            let (sect, stub) = (l.out[at + 2], l.out[at + 3]);
            let (s, method) = (&block.sections[k], sect.x & 8 != 0);
            let n = call.x as usize;
            // A method a `super()` proxy finds past the class its method lies in, laid as the head's own is.
            let via = sect.x & 64 != 0 && s.via_super.is_some();
            if kw_head(&l.out, at) || sect.x & (16 | 32) != 0 || n > 4 || !(via || sect.x & 64 == 0 && if method { matches!(s.on_class, 1 | 2) } else { s.on_class == 0 && sect.x & 4 != 0 }) { return None; }
            // One inside a section lays its head past the end, which its section's walks take for the call.
            enclosing = block.holding(at).map(|(s, _)| block.sections.iter().position(|t| core::ptr::eq(t, s)).unwrap_or(usize::MAX));
            // The arguments as the call passes them, the ones its head reads and those the moves before it wrote to its section.
            let param = |l: &Lower, p: usize| (at.saturating_sub(16)..at).rev().find(|&j| l.out[j].op == OpCode::Move && l.out[j].a == s.link + 1 + p as u16).map(|j| l.out[j].b);
            (l.cur, l.quiet) = (l.orig[at], l.back[at]);
            let from = l.out.len();
            laid_from = Some(from);
            if via {
                let e = block.sections.get(enclosing?)?;
                let c = (l.sections)(Want::Super(e.fi, call.a, e.ctor.as_ref().map_or(e.value.get(), |c| c.2), s.class)).filter(|c| c.value.0 == func.0)?;
                let to = l.want_callee(u16::MAX, c, n + 1).filter(|&to| to != k)?;
                let (arity, defaults) = (l.wanted[to].1.arity, l.wanted[to].1.defaults.clone());
                if n + 1 < arity.min(2) || l.reaches(to, enclosing?) { return None; }
                // The arguments past the second go where its parameters lie, as the moves before the head wrote them to its own.
                for p in 2..arity {
                    let j = l.out.len();
                    l.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: if p <= n { param(&l, p)? } else { 0 }, c: 0 });
                    l.prep.push((j, 0, Prep::Param(to, p)));
                    if p > n { l.prep.push((j, 1, Prep::Const(defaults[p + defaults.len() - arity]))); }
                }
                let nh = l.out.len();
                for f in 0..4 { let i = l.out[at + f]; l.emit(i); }
                (l.out[nh + 1].b, l.out[nh + 2].c) = ((nh + HEADER as usize) as u16, to as u16);
                l.dunder_heads.push((nh, to));
            } else {
                let first = if method { (s.on_class == 2) as usize } else { 0 };
                let mut args = [0u16; 4];
                for (i, a) in args.iter_mut().enumerate().take(n) {
                    *a = match (method, i) { (true, 0) => call.c, (false, 0) => call.b, (false, 1) => call.c, _ => param(&l, i + first)? };
                }
                let marks: &[u32] = if method { &[FIELD, STATIC] } else { &[CALLED] };
                let c = marks.iter().flat_map(|&m| (0..ALTS).map(move |j| (m, j))).find_map(|(m, j)| (l.sections)(Want::MethodAlt(scope, cat | m, j)).filter(|c| c.value.0 == func.0))?;
                if c.star.is_some() || c.dstar.is_some() || c.by_body || c.via_super().is_some() || c.supers.is_some() { return None; }
                let stat = matches!(c.on_class, 1 | 4);
                let to = l.want_callee(u16::MAX, c, n + !stat as usize).filter(|&to| to != k)?;
                // Its head reads its first parameters from the call, which a default would write to the stub's call too.
                if !stat && n + 1 < l.wanted[to].1.arity.min(2) || enclosing.is_some_and(|e| l.reaches(to, e)) { return None; }
                let (recv, name) = if method { (call.b, call.a) } else { (call.a, 0) };
                l.head(Ins { op: OpCode::CallMethodR, x: n as u8, a: name, b: recv, c: args[0] }, Ins { op: OpCode::ArgsR, x: 0, a: args[1], b: args[2], c: args[3] }, l.out[at + 1].a, to, true, &[]);
                // Laid for its section as an op's other heads are, it goes to the turned-down head's stub in turn.
                let nh = l.out.len() - HEADER as usize;
                l.heads.pop();
                l.dunder_heads.push((nh, to));
                l.out[nh + 3] = stub;
            }
            l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + HEADER, b: 0, c: 0 });
            l.out[at + 3].a = from as u16;
        }
        Grow::Method => {
            l.only = None;
            // A head calling an instance looks for the `__call__` of the class of each instance it met.
            let cat = if l.wanted[k].1.on_class == 3 { cat | CALLED } else { cat };
            l.want_method_alts(scope, cat, k, call.x as usize + 1);
            if l.alts.len() == l.alts_first { return None; }
            // A method head whose receiver now holds the function met calls that one's section first, then its own and its table's, as the lowering does.
            let held = l.alts[l.alts_first..].iter().find(|a| a.0 == k && l.wanted[a.1].1.on_class == 4 && l.wanted[a.1].1.value.0 == func.0).map(|a| a.1);
            if let Some(to) = held && l.wanted[k].1.on_class == 0 && matches!(call.op, OpCode::CallMethodSectionR | OpCode::CallMethodGlobalSectionR) && call.x <= 1 {
                let others: Vec<usize> = l.alts.iter().filter(|a| a.0 == k && a.1 != to).map(|a| a.1).collect();
                for a in others.into_iter().chain(core::iter::once(k)) { if !l.alts.contains(&(to, a)) { l.alts.push((to, a)); } }
                // Its arguments go where that function's parameters lie, a new head past the end the old one jumps to.
                (l.cur, l.quiet) = (l.orig[at], l.back[at]);
                let from = l.out.len();
                // A receiver the head read from a module binding is read into its register first.
                if call.op == OpCode::CallMethodGlobalSectionR { let g = l.out[at + 3].c - 1; l.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: call.b, b: g, c: 0 }); }
                l.head(Ins { op: OpCode::CallMethodR, ..call }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, l.out[at + 1].a, to, true, &[]);
                l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + HEADER, b: 0, c: 0 });
                l.displaced_to(at, from);
            }
        }
        Grow::Global => {
            if !global_head(&l.out, at) { return None; }
            let s = &block.sections[k];
            let (g, module) = (s.global, s.bound);
            if g == u16::MAX || l.wanted[k].1.star.is_some() { return None; }
            let n = call.x as usize;
            l.wanted[k].0 = u16::MAX;
            // A constructor's head lays the binding's class with the `__init__` it finds now, its instance the first parameter.
            let ctor = s.ctor.is_some() as usize;
            let fresh = (l.sections)(Want::Global(module, g)).filter(|c| c.star.is_none() && c.dstar.is_none() && c.ctor().is_some() == (ctor == 1) && !c.by_body && !c.twin);
            let to = fresh.and_then(|c| l.want_callee(g, c, n + ctor));
            match to {
                // A closure made again or what a bound method or an instance holds is laid by lowering the code again.
                None if (l.sections)(Want::Global(module, g)).is_some_and(|c| c.twin) || (l.sections)(Want::Held(module, g)).is_some() => return None,
                None => detach = Some(k),
                Some(to) => {
                    let (arity, defaults) = (l.wanted[to].1.arity, l.wanted[to].1.defaults.clone());
                    let link = s.link as usize;
                    let default = |p: usize| defaults[p + defaults.len() - arity];
                    let heads: Vec<usize> = (0..len).filter(|&h| global_head(&l.out, h) && l.out[h + 2].c as usize == k).collect();
                    for h in heads {
                        // A keyword call's head is laid again for the new function's parameters, or left to its stub.
                        if kw_head(&l.out, h) { let c = l.wanted[to].1.clone(); l.rekeyed(h, c, g); continue; }
                        let head = l.out[h];
                        let given = head.x as usize + ctor;
                        // The arguments past the second, which the moves before the head wrote to the old section's parameters.
                        let mut args = Vec::new();
                        for p in 2..arity.min(given) {
                            let m = (h.saturating_sub(16)..h).rev().find(|&j| l.out[j].op == OpCode::Move && l.out[j].a as usize == link + 1 + p);
                            args.push(l.out[m?].b);
                        }
                        let from = l.out.len();
                        l.cur = old.orig(h);
                        for p in 2..arity {
                            let at = l.out.len();
                            l.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: args.get(p - 2).copied().unwrap_or(0), c: 0 });
                            l.prep.push((at, 0, Prep::Param(to, p)));
                            if p >= given { l.prep.push((at, 1, Prep::Const(default(p)))); }
                        }
                        let at = l.out.len();
                        for f in 0..4 { let i = l.out[h + f]; l.emit(i); }
                        // A constructor's head reads its first argument, the second parameter, from `b`.
                        for p in given..arity.min(2) { l.prep.push((at, (1 + p - ctor) as u8, Prep::Const(default(p)))); }
                        l.out[at + 2].c = to as u16;
                        // The section comes back past the head, as every head's does, and on past the old head from there.
                        l.out[at + 1].b = (at + HEADER as usize) as u16;
                        l.emit(Ins { op: OpCode::JumpR, x: 0, a: (h + HEADER as usize) as u16, b: 0, c: 0 });
                        moved.push((h, from, at));
                    }
                }
            }
        }
        // A length, truth test, `bool`, `str`, `repr` or loop whose head turned down another class checks past it a head laid for that class's dunder.
        Grow::Dunder if dunder_main(&l.out, at).is_some_and(|m| matches!(l.out[l.out[m + 3].b as usize].op, OpCode::LenR | OpCode::NotR | OpCode::CallBool | OpCode::CallStr | OpCode::CallRepr | OpCode::GetIter)) => {
            let main = dunder_main(&l.out, at)?;
            let (head, store, sect, stub) = (l.out[main], l.out[main + 1], l.out[main + 2], l.out[main + 3]);
            let run = stub.b as usize;
            let form = match l.out[run].op { OpCode::NotR => OpCode::JumpIfFalseR, op => op };
            let c = (l.sections)(Want::DunderFunc(func, form, u16::MAX)).filter(|c| c.arg().is_none() && !c.reflected)?;
            let kind = match form { OpCode::LenR => 1, OpCode::GetIter => 0, OpCode::CallBool if c.check == 3 => 3, _ => c.check };
            if kind != l.out.get(main + HEADER as usize).filter(|i| i.op == OpCode::DunderCheckR).map_or(0, |i| i.x) { return None; }
            let s = l.want_callee(u16::MAX, c, 1)?;
            // The sections its heads check, a dunder laid already needing no head more.
            let mut h = main;
            for _ in 0..4 * ALTS {
                if !l.out.get(h).is_some_and(|i| i.op == OpCode::DunderSectionR) || !l.out.get(h + 3).is_some_and(|t| t.x == STUB) { break; }
                if l.out[h + 2].c as usize == s { return None; }
                h = l.out[h + 3].a as usize;
            }
            // One inside a section lays its head past the end, which its section's walks take for the op.
            enclosing = block.holding(main).map(|(e, _)| block.sections.iter().position(|t| core::ptr::eq(t, e)).unwrap_or(usize::MAX));
            if enclosing.is_some_and(|e| l.reaches(s, e)) { return None; }
            l.cur = l.orig[main];
            let h = l.out.len();
            laid_from = Some(h);
            l.dunder_heads.push((h, s));
            l.emit(head);
            l.emit(Ins { b: h as u16 + HEADER, ..store });
            l.emit(Ins { c: s as u16, ..sect });
            l.emit(stub);
            l.emit(Ins { op: OpCode::JumpR, x: 0, a: main as u16 + HEADER, b: 0, c: 0 });
            l.out[main + 3].a = h as u16;
        }
        Grow::Dunder => {
            // The op's own head is the first its stub's heads go back past, and `at` may be any of them.
            let main = dunder_main(&l.out, at)?;
            let (store, sect, stub) = (l.out[main + 1], l.out[main + 2], l.out[main + 3]);
            let run = stub.b as usize;
            let op = l.out[run];
            let attr = matches!(op.op, OpCode::GetAttrR | OpCode::SetAttrR);
            if !dunder_op(op.op) && !attr { return None; }
            let k = sect.c as usize;
            // The sections its heads check, which a dunder laid already needs no head more.
            let mut laid = alloc::vec![k];
            for mut h in [stub.a as usize, stub.c as usize].into_iter().filter(|&h| h != 0) {
                for _ in 0..4 * ALTS {
                    if h == run || !l.out.get(h).is_some_and(|i| i.op == OpCode::DunderSectionR) || !l.out.get(h + 3).is_some_and(|t| t.x == STUB) { break; }
                    laid.push(l.out[h + 2].c as usize);
                    h = l.out[h + 3].a as usize;
                }
            }
            let c = (l.sections)(Want::DunderFunc(func, op.op, u16::MAX)).filter(|c| c.arg().is_none())?;
            let refl = c.reflected;
            // A `!=` laid on an `__eq__` inverts what it gives, past the op's own inversion when that inverts too, or through one of its own laid once.
            let inverts = op.op == OpCode::NotEqR && c.check == 1;
            let s = l.want_callee(u16::MAX, c, if op.op == OpCode::GetAttrR { 1 } else { 2 })?;
            if laid.contains(&s) { return None; }
            l.cur = l.orig[main];
            let main_inverts = op.x & INVERTED != 0;
            let back = if op.op != OpCode::NotEqR || inverts == main_inverts { main + HEADER as usize } else if main_inverts { main + HEADER as usize + 1 } else {
                let at = l.out.len();
                l.emit(Ins { op: OpCode::NotR, x: 0, a: store.a, b: store.a, c: 0 });
                l.emit(Ins { op: OpCode::Jump, x: 1, a: main as u16 + HEADER, b: 0, c: 0 });
                at
            };
            let binary = op.op != OpCode::GetItemR && !attr;
            let same = binary && super::opcodes::dunder::op_names(op.op).is_some_and(|(a, b)| a == b);
            // Laid as the lowering lays a dunder beside the first, the left operand's, or a reflected one for a left number, right subclass or declined left.
            let modes: &[u16] = if !refl && same { &[0, 1, 6] } else if !refl || attr { &[0] } else if binary { &[1, 7, 6] } else { &[1] };
            if !attr && !l.declining.contains(&s) { l.declining.push(s); }
            // Each head goes first among its kind, the turned-down ones' before the op, the declined ones' before the others a declining reaches.
            let (mut turns, mut declines) = (stub.a, if stub.c != 0 { stub.c } else { run as u16 });
            for &mode in modes.iter().rev() {
                let h = l.out.len();
                let next = if mode == 6 { declines } else { turns };
                l.dunder_heads.push((h, s));
                l.emit(Ins { op: OpCode::DunderSectionR, ..call_of(op, mode) });
                l.emit(Ins { op: OpCode::StoreTopR, x: 0, a: store.a, b: h as u16 + HEADER, c: 0 });
                l.emit(Ins { c: s as u16, ..sect });
                l.emit(Ins { op: OpCode::SectionR, x: STUB, a: next, b: run as u16, c: if mode == 6 { k as u16 } else { 0 } });
                l.emit(Ins { op: OpCode::Jump, x: 1, a: back as u16, b: 0, c: 0 });
                if mode == 6 { declines = h as u16; } else { turns = h as u16; }
            }
            l.out[main + 3] = Ins { a: turns, c: if declines as usize == run { stub.c } else { declines }, ..stub };
        }
        // A call outside the sections no head lays gets one past the end.
        Grow::Site => {
            if !block.plain_at(at) { return None; }
            enclosing = block.holding(at).map(|(s, _)| block.sections.iter().position(|t| core::ptr::eq(t, s)).unwrap_or(usize::MAX));
            // One inside a section lays a function its binding holds, of the module the code binds in.
            if let Some(e) = enclosing {
                let c = l.out[at];
                if block.sections.get(e).is_none_or(|s| s.module != l.facts.module) || !matches!(c.op, OpCode::CallGlobalR | OpCode::CallR) { return None; }
            }
            // A call that turned down a few functions runs again and again, so it lays what it meets.
            laid_from = Some(l.out.len());
            if !l.displace(at, old) { return None; }
            // One inside a section that may come back into it would need what that keeps, a recursion laid apart.
            let start = laid_from.unwrap_or(0);
            let k = (start..l.out.len()).find(|&h| l.out.get(h + 3).is_some_and(|t| t.op == OpCode::SectionR && t.x == STUB)).map(|h| l.out[h + 2].c as usize)?;
            if enclosing.is_some_and(|e| l.reaches(k, e)) { return None; }
        }
        // An op outside the sections laid as no head gets one past the end for the dunder it met, its stub the op as written.
        Grow::Op => {
            if !block.plain_at(at) { return None; }
            // An op inside a section lays its head past the end, which its section's walks take for the op.
            enclosing = block.holding(at).map(|(s, _)| block.sections.iter().position(|t| core::ptr::eq(t, s)).unwrap_or(usize::MAX));
            let op = l.out[at];
            if enclosing.is_some() && op.op == OpCode::ForIterR { return None; }
            // A truth test calls the `__bool__` or `__len__` it met into its first parameter, checks and tests the result, its stub testing as written.
            if matches!(op.op, OpCode::NotR | OpCode::JumpIfFalseR) {
                let form = OpCode::JumpIfFalseR;
                let c = (l.sections)(Want::DunderFunc(func, form, u16::MAX)).filter(|c| c.arg().is_none() && c.check != 0)?;
                let kind = c.check;
                let k = l.want_callee(u16::MAX, c, 1)?;
                // One that may come back into the section it lies in would need what that keeps, a recursion laid apart.
                if enclosing.is_some_and(|e| l.reaches(k, e)) { return None; }
                (l.cur, l.quiet) = (old.orig(at), old.back.get(at).copied().unwrap_or(0));
                let (from, v) = (l.out.len(), op.b);
                l.head(Ins { op: form, x: 1, a: 2, b: v, c: v }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, 0, k, true, &[]);
                let h = l.out.len() - HEADER as usize;
                l.prep.push((h + 1, 0, Prep::Param(k, 0)));
                l.dunders.push((h, Ins { op: form, x: 0, a: 0, b: v, c: v }));
                l.prep.push((l.out.len(), 0, Prep::Param(k, 0)));
                l.emit(Ins { op: OpCode::DunderCheckR, x: kind, a: 0, b: v, c: 0 });
                l.prep.push((l.out.len(), 1, Prep::Param(k, 0)));
                l.emit(Ins { b: 0, ..op });
                l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + 1, b: 0, c: 0 });
                l.displaced_to(at, from);
            } else if op.op == OpCode::GetIter {
                // A loop over an instance calls the `__iter__` it met on the pushed value and steps what it gave, its stub making the iterator as written.
                let push = at.checked_sub(1).map(|p| l.out[p]).filter(|p| p.op == OpCode::PushRegs && p.x == 1 && op.a == 0 && op.x == 0)?;
                if l.out.iter().any(|i| i.a as usize == at && matches!(i.op, OpCode::Jump | OpCode::JumpR | OpCode::JumpIfFalseR)) { return None; }
                let v = push.a;
                let c = (l.sections)(Want::DunderFunc(func, OpCode::GetIter, u16::MAX)).filter(|c| c.arg().is_none())?;
                let k = l.want_callee(u16::MAX, c, 1)?;
                if enclosing.is_some_and(|e| l.reaches(k, e)) { return None; }
                (l.cur, l.quiet) = (old.orig(at), old.back.get(at).copied().unwrap_or(0));
                let from = l.out.len();
                l.head(Ins { op: OpCode::GetIter, x: 1, a: 2, b: v, c: v }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, 0, k, true, &[]);
                let h = l.out.len() - HEADER as usize;
                l.prep.push((h + 1, 0, Prep::Param(k, 0)));
                l.dunders.push((h, Ins { op: OpCode::GetIter, x: 0, a: 0, b: v, c: v }));
                l.prep.push((l.out.len(), 0, Prep::Param(k, 0)));
                l.emit(Ins { op: OpCode::PushRegs, x: 1, a: 0, b: 0, c: 0 });
                l.emit(Ins { op: OpCode::GetIter, x: 0, a: crate::parser::ITER_AS_IS, b: 0, c: 0 });
                l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + 1, b: 0, c: 0 });
                l.displaced_to(at - 1, from);
            } else if matches!(op.op, OpCode::LenR | OpCode::CallBool | OpCode::CallStr | OpCode::CallRepr) {
                // `len`, `bool`, `str` or `repr` of an instance calls its dunder into the first parameter, checks the result and gives it where the op does.
                let stacked = op.op != OpCode::LenR;
                let push = at.checked_sub(1).map(|p| l.out[p]).filter(|p| p.op == OpCode::PushRegs && (1..=3).contains(&p.x) && op.a == 1);
                if stacked && (push.is_none() || l.out.iter().any(|i| i.a as usize == at && matches!(i.op, OpCode::Jump | OpCode::JumpR | OpCode::JumpIfFalseR))) { return None; }
                let v = match push { Some(p) if stacked => [p.a, p.b, p.c][p.x as usize - 1], _ => op.b };
                let c = (l.sections)(Want::DunderFunc(func, op.op, u16::MAX)).filter(|c| c.arg().is_none() && !c.reflected)?;
                let kind = match op.op { OpCode::LenR => 1, OpCode::CallBool if c.check == 3 => 3, _ => c.check };
                if kind == 0 { return None; }
                let k = l.want_callee(u16::MAX, c, 1)?;
                if enclosing.is_some_and(|e| l.reaches(k, e)) { return None; }
                (l.cur, l.quiet) = (old.orig(at), old.back.get(at).copied().unwrap_or(0));
                let from = l.out.len();
                // What the push took to the stack under the operand goes there first.
                if let Some(p) = push.filter(|p| stacked && p.x > 1) { l.emit(Ins { x: p.x - 1, ..p }); }
                let call = if matches!(op.op, OpCode::CallStr | OpCode::CallRepr) { call_of(Ins { op: op.op, x: 0, a: 0, b: v, c: v }, 0) } else { Ins { op: op.op, x: 1, a: 3, b: v, c: v } };
                l.head(call, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, 0, k, true, &[]);
                let h = l.out.len() - HEADER as usize;
                l.prep.push((h + 1, 0, Prep::Param(k, 0)));
                l.dunders.push((h, Ins { op: op.op, x: 0, a: 0, b: v, c: v }));
                l.prep.push((l.out.len(), 0, Prep::Param(k, 0)));
                l.emit(Ins { op: OpCode::DunderCheckR, x: kind, a: 0, b: v, c: 0 });
                // A `__len__`'s length is made a bool.
                if kind == 3 { for _ in 0..2 { l.prep.extend([(l.out.len(), 0, Prep::Param(k, 0)), (l.out.len(), 1, Prep::Param(k, 0))]); l.emit(Ins { op: OpCode::NotR, x: 0, a: 0, b: 0, c: 0 }); } }
                // A `len` of what `str` or `repr` gave, which no jump lands on, takes it from the register, as the lowering lays it.
                let len = stacked && op.op != OpCode::CallBool && l.out.get(at + 1).is_some_and(|n| n.op == OpCode::CallLen && n.a == 1)
                    && !l.out.iter().any(|i| i.a as usize == at + 1 && (i.op.is_jump() || matches!(i.op, OpCode::JumpR | OpCode::JumpIfFalseR | OpCode::ForIterR | OpCode::JumpUnlessEq | OpCode::JumpUnlessNotEq | OpCode::JumpUnlessLt | OpCode::JumpUnlessLtEq | OpCode::JumpUnlessGt | OpCode::JumpUnlessGtEq)));
                if len {
                    l.cur = old.orig(at + 1);
                    l.prep.extend([(l.out.len(), 0, Prep::Param(k, 0)), (l.out.len(), 1, Prep::Param(k, 0))]);
                    l.emit(Ins { op: OpCode::LenR, x: 0, a: 0, b: 0, c: 0 });
                }
                l.prep.push((l.out.len(), if stacked { 0 } else { 1 }, Prep::Param(k, 0)));
                l.emit(if stacked { Ins { op: OpCode::PushRegs, x: 1, a: 0, b: 0, c: 0 } } else { Ins { op: OpCode::Move, x: 0, a: op.a, b: 0, c: 0 } });
                l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + 1 + len as u16, b: 0, c: 0 });
                l.displaced_to(if stacked { at - 1 } else { at }, from);
            } else if matches!(op.op, OpCode::MinusR | OpCode::InR | OpCode::NotInR | OpCode::StoreItemR | OpCode::ForIterR) {
                // A `not in` calls `__contains__` as an `in` does, its truth negated past the head.
                let negated = op.op == OpCode::NotInR;
                let op = if negated { Ins { op: OpCode::InR, ..op } } else { op };
                // A negation, membership test, item store or user iterator step calls the dunder it met as the lowering lays it, its stub the op as written.
                let n = match op.op { OpCode::MinusR | OpCode::ForIterR => 1, OpCode::InR => 2, _ => 3 };
                let c = (l.sections)(Want::DunderFunc(func, op.op, u16::MAX)).filter(|c| c.arg().is_none() && !c.reflected)?;
                let k = l.want_callee(u16::MAX, c, n)?;
                // One that may come back into the section it lies in would need what that keeps, a recursion laid apart.
                if enclosing.is_some_and(|e| l.reaches(k, e)) { return None; }
                if op.op != OpCode::ForIterR && !l.declining.contains(&k) { l.declining.push(k); }
                (l.cur, l.quiet) = (old.orig(at), old.back.get(at).copied().unwrap_or(0));
                let from = l.out.len();
                let (call, run, store) = match op.op {
                    // A step's stub names the loop's end by its compiler instruction, a module binding target taking the item from the section's first parameter.
                    OpCode::ForIterR => {
                        let exit = l.at.iter().position(|&t| t == op.a as u32)?;
                        let t = if op.x == 0 { op.b } else if k < block.sections.len() { block.sections[k].link + 1 } else if k == l.first { old.frame as u16 + 1 } else { return None };
                        (Ins { op: OpCode::ForIterR, x: 1, a: 4, b: 0, c: 0 }, Ins { x: 0, a: exit as u16, b: t, c: 0, ..op }, t)
                    }
                    OpCode::StoreItemR => (call_of(op, 0), op, 0),
                    _ => (call_of(Ins { a: 0, ..op }, 0), Ins { a: 0, ..op }, op.a),
                };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: if op.op == OpCode::StoreItemR { op.c } else { 0 }, b: 0, c: 0 };
                l.head(call, rest, store, k, true, &[]);
                let h = l.out.len() - HEADER as usize;
                if op.op == OpCode::StoreItemR { l.prep.push((h + 1, 0, Prep::Param(k, 0))); }
                l.dunders.push((h, run));
                // A test takes the truth of what `__contains__` gave.
                if op.op == OpCode::InR { for _ in 0..2 - negated as usize { l.emit(Ins { op: OpCode::NotR, x: 0, a: op.a, b: op.a, c: 0 }); } }
                if op.op == OpCode::ForIterR && op.x != 0 { l.emit(Ins { op: OpCode::StoreGlobalR, x: 0, a: op.b, b: store, c: 0 }); }
                l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + 1, b: 0, c: 0 });
                l.displaced_to(at, from);
            } else {
            // A load or a store of an attribute lays the property's function or the class's `__getattr__` it met.
            let attr = matches!(op.op, OpCode::GetAttrR | OpCode::SetAttrR);
            if !dunder_op(op.op) && !attr || op.x & INVERTED != 0 { return None; }
            let c = (l.sections)(Want::DunderFunc(func, op.op, if op.op == OpCode::GetAttrR { op.c } else { u16::MAX })).filter(|c| attr || c.arg().is_none())?;
            let (first, arg) = (if c.reflected { 1 } else { 0 }, c.arg());
            // A `!=` laid on an `__eq__` inverts what it gives past its head, which its stub, the op, skips.
            let inverted = op.op == OpCode::NotEqR && c.check == 1;
            let n = match op.op { OpCode::GetAttrR => 1, _ => 2 } + arg.is_some() as usize;
            let k = l.want_callee(u16::MAX, c, n)?;
                // One that may come back into the section it lies in would need what that keeps, a recursion laid apart.
                if enclosing.is_some_and(|e| l.reaches(k, e)) { return None; }
            if !attr && !l.declining.contains(&k) { l.declining.push(k); }
            let run = if op.op == OpCode::SetAttrR { op } else { Ins { a: 0, x: op.x | if inverted { INVERTED } else { 0 }, ..op } };
            (l.cur, l.quiet) = (old.orig(at), old.back.get(at).copied().unwrap_or(0));
            let from = l.out.len();
            l.head_arg = arg;
            let main = call_of(run, first);
            // A store gives nothing, its section's return going to its first parameter, which nothing reads past it.
            l.head(if arg.is_some() { Ins { a: 5, ..main } } else { main }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, if attr && op.op == OpCode::SetAttrR { 0 } else { op.a }, k, true, &[]);
            let h = l.out.len() - HEADER as usize;
            if op.op == OpCode::SetAttrR { l.prep.push((h + 1, 0, Prep::Param(k, 0))); }
            l.dunders.push((h, run));
            if inverted { l.emit(Ins { op: OpCode::NotR, x: 0, a: op.a, b: op.a, c: 0 }); }
            // Its section and its stub come back past the head, and on past the op from there.
            l.emit(Ins { op: OpCode::JumpR, x: 0, a: at as u16 + 1, b: 0, c: 0 });
            l.displaced_to(at, from);
            }
        }
        Grow::All => return None,
    }
    let (block, frame) = l.lay(old.frame);
    let mut block = block?;
    // An op's head inside a section, from its moves to its way back, and its stub, lie where the op does for the section's walks.
    let mut laid_past = false;
    if enclosing.is_some() && let Some(g) = block.grown.get_mut() && let Some(from) = laid_from.or_else(|| l.out.get(at).filter(|i| i.op == OpCode::JumpR).map(|i| i.a as usize))
        && let Some(h) = (from..l.out.len()).find(|&h| l.out.get(h + 3).is_some_and(|t| t.op == OpCode::SectionR && t.x == STUB)) {
        let end = (h + HEADER as usize..l.out.len()).find(|&j| l.out[j].op == OpCode::JumpR).map_or(h + HEADER as usize, |j| j + 1);
        g.moved.push((from as u32, end as u32, lies as u32));
        laid_past = true;
        let st = l.out[h + 3].a as usize;
        if let Some(e) = (st..l.out.len()).find(|&j| l.out[j].op == OpCode::Jump && l.out[j].x == 1 && l.out[j].a as usize == h + HEADER as usize) { g.moved.push((st as u32, e as u32 + 1, lies as u32)); }
    }
    block.moved |= laid_past;
    if let Some(to) = swapped { (l.out[at + 2].a, l.out[at + 2].b) = (block.sections[to].start as u16, block.sections[to].link + 1); }
    for &(h, from, at) in &moved {
        let to = l.out[at + 2].c as usize;
        (l.out[at + 2].a, l.out[at + 2].b) = (block.sections[to].start as u16, block.sections[to].link + 1);
        l.out[h] = Ins { op: OpCode::JumpR, x: 0, a: from as u16, b: 0, c: 0 };
    }
    if kind == Grow::Global {
        // The old section is no binding's any more, its heads moved or sent to their stubs, and the new one is the binding's, which the block checks.
        let (g, m) = (block.sections[k].global, block.sections[k].bound);
        block.sections[k].global = u16::MAX;
        if let Some(&(_, _, at)) = moved.first() {
            let to = &mut block.sections[l.out[at + 2].c as usize];
            if to.bound != m || to.global != u16::MAX && to.global != g { return None; }
            to.global = g;
        }
        // A keyword call's head, laid for the old function's parameters, goes to its stub with the rest when none is laid.
        let heads: Vec<usize> = (0..len).filter(|&h| global_head(&l.out, h) && l.out[h + 2].c as usize == k && (detach.is_some() || kw_head(&l.out, h))).collect();
        for h in heads { l.out[h] = Ins { op: OpCode::Jump, x: 1, a: l.out[h + 3].a, b: 0, c: 0 }; }
    }
    specialize(&mut l.out, len);
    // The jump past the sections goes past the new ones too.
    let over = *old.at.last()? as usize;
    if l.out.get(over).is_some_and(|i| i.op == OpCode::Jump && i.x == 0) { l.out[over].a = l.out.len() as u16; }
    for &k in &l.yields {
        let next = match l.out.get(k + 1) { Some(j) if j.op == OpCode::Jump && (j.a as usize) < l.out.len() => j.a as usize, _ => k + 1 };
        (l.out[k].b, l.out[k].c) = (l.back[next] as u16, next as u16);
    }
    if l.out.len() >= u16::MAX as usize { return None; }
    let frame = frame.max(reg_bound(&l.out[len..])).max(old.frame);
    if frame >= u16::MAX as usize { return None; }
    l.orig.push(src.len() as u32);
    l.back.push(src.len() as u32);
    let cache = old.cache.extended(l.out.len());
    for &(at, site) in l.seeds.iter().filter(|s| s.0 & SEEDED != 0) { cache.set_site(at & !SEEDED, site); }
    let plain = OnceCell::new();
    if let Some(p) = old.plain.get() { let _ = plain.set(p.clone()); }
    Some(Code { ins: sealed(l.out), orig: l.orig, at: l.at, back: l.back, frame, kinds: old.kinds.clone(), module: old.module, hot: Cell::new(0), cache, pure: Cell::new(2), block: Some(block), plain, layout: next_layout(), flat: Cell::new(0), misses: Cell::new(old.misses.get()), read: Cell::new(old.read.get())})
}

/* `old`, lowered without sections, grown past its end by a section for each call whose site learned its callee, every instruction kept in place. */
fn extend_all<'p>(old: &Code, src: &[Instruction], chunk: &SSAChunk, facts: &Facts, sections: &dyn Fn(Want) -> Option<Callee<'p>>, keep: bool) -> Option<Code> {
    if old.block.is_some() || old.unchanged() || !facts.regs { return None; }
    let (names, consts) = (chunk.names.len(), chunk.constants.len());
    let len = old.ins.len();
    let (mut orig, mut back) = (old.orig.clone(), old.back.clone());
    orig.pop();
    back.pop();
    let once = if facts.once && facts.shape & CALLS != 0 { runs_once(src) } else { Vec::new() };
    let mut l = Lower {
        src, chunk, facts, flags: &[], deleted: Vec::new(), names, cbase: names, consts, tbase: names + consts + 3,
        out: old.ins.clone(), orig, at: old.at.clone(), back,
        pending: Vec::new(), patches: Vec::new(), temps: 0, last: None, cur: 0, takes: false, under: Vec::new(), quiet: 0, bound: vec![0; names.div_ceil(64)],
        appends: Vec::new(), lists: core::cell::OnceCell::new(), sections, wanted: Vec::new(), heads: Vec::new(), prep: Vec::new(), seeds: Vec::new(), dest: None, resume: None,
        gaps: 0, kwcall: None, kwstubs: Vec::new(), alts: Vec::new(), lookups: Vec::new(), dunders: Vec::new(), declining: Vec::new(), dunder_alts: Vec::new(), dunder_heads: Vec::new(), head_arg: None, yields: Vec::new(),
        genfors: Vec::new(), gen_next: None, gen_args: Vec::new(), ret_to: None, nested: Vec::new(), gen_loops: Vec::new(), gen_steps: Vec::new(),
        unders: Vec::new(), stub_stores: Vec::new(), keyed: false, more_args: Vec::new(), wide: false, once: false, old: None, first: 0, alts_first: 0, only: None, calls: OnceCell::new(), displaced: Vec::new(), turn_bases: Vec::new(), tern: None,
    };
    // The jump past the heads and sections, where the code ended.
    l.cur = old.orig(len.saturating_sub(1));
    l.emit(Ins { op: OpCode::Jump, x: 0, a: 0, b: 0, c: 0 });
    for j in 0..len {
        l.once = once.get(old.orig(j) as usize).copied().unwrap_or(false);
        l.displace(j, old);
    }
    // One its misses grew lays a block with no section past the jump, so its calls and ops grow heads of their own.
    if l.wanted.is_empty() && !keep { return None; }
    let (block, frame) = l.lay(old.frame);
    let mut block = block.or_else(|| keep.then(|| Box::new(Block { sections: Box::default(), extra: Box::default(), ranges: Box::new([(old.frame, 0)]), callees: Vec::new(), lineage: 0, grown: OnceCell::new(),
        binds: Cell::new(super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed)), alt_tab: Box::default(), loops: Box::default(), moved: false, by_class: Box::default() })))?;
    block.lineage = old.layout;
    specialize(&mut l.out, len);
    l.out[len].a = l.out.len() as u16;
    for &k in &l.yields {
        let next = match l.out.get(k + 1) { Some(j) if j.op == OpCode::Jump && (j.a as usize) < l.out.len() => j.a as usize, _ => k + 1 };
        (l.out[k].b, l.out[k].c) = (l.back[next] as u16, next as u16);
    }
    if l.out.len() >= u16::MAX as usize { return None; }
    let frame = frame.max(reg_bound(&l.out[len..])).max(old.frame);
    if frame >= u16::MAX as usize { return None; }
    l.orig.push(src.len() as u32);
    l.back.push(src.len() as u32);
    let cache = old.cache.extended(l.out.len());
    for &(at, site) in l.seeds.iter().filter(|s| s.0 & SEEDED != 0) { cache.set_site(at & !SEEDED, site); }
    Some(Code { ins: sealed(l.out), orig: l.orig, at: l.at, back: l.back, frame, kinds: old.kinds.clone(), module: old.module, hot: Cell::new(0), cache, pure: Cell::new(2), block: Some(block), plain: OnceCell::new(), layout: next_layout(), flat: Cell::new(0), misses: Cell::new(0), read: Cell::new(old.read.get())})
}

/* Gives each section call and return of `out` from `from` on the instruction of its kind, so each runs no check of flags. */
fn specialize(out: &mut [Ins], from: usize) {
    for k in from..out.len() {
        let ins = out[k];
        match ins.op {
            // A call of a binding's section by position that keeps nothing takes the call that checks only the binding.
            OpCode::CallSectionR => {
                let (x, stub) = (out[k + 2].x, out[k + 3]);
                out[k].op = match x & (4 | 8 | 16 | 32 | 64) {
                    0 => OpCode::CallGlobalSectionR,
                    4 => OpCode::CallValueSectionR,
                    8 if stub.c != 0 => OpCode::CallMethodGlobalSectionR,
                    8 => OpCode::CallMethodSectionR,
                    16 => OpCode::CallCtorSectionR,
                    _ => continue,
                };
            }
            // A return from inside a `try` leaves through its handlers, and one of a value always bound checks none.
            OpCode::ReturnSectionR if ins.x != 0 => out[k].op = OpCode::ReturnTriedR,
            OpCode::ReturnSectionR if ins.c == 1 => out[k].op = OpCode::ReturnBoundR,
            _ => {}
        }
    }
}

/* Instructions a jump, handler or finished `finally` enters, which no fusion may swallow. */
fn entered(src: &[Instruction]) -> Vec<bool> {
    let mut entered = vec![false; src.len() + 1];
    for (k, ins) in src.iter().enumerate() {
        if ins.opcode.is_jump() && (ins.operand as usize) <= src.len() { entered[ins.operand as usize] = true; }
        // Unwind::Goto resumes at the instruction after UnwindFinally.
        if ins.opcode == OpCode::UnwindFinally { entered[k + 1] = true; }
    }
    entered
}

/* The InPlaceAdd instructions that append to a local a loop uses for nothing else, and the loop heads with the slots they reset. */
fn append_loops(src: &[Instruction], facts: &Facts) -> (Vec<bool>, Vec<(usize, u16)>) {
    let mut appends = vec![false; src.len()];
    let mut resets = Vec::new();
    let refers = |ins: &Instruction| matches!(ins.opcode, OpCode::LoadName | OpCode::StoreName | OpCode::Del | OpCode::Phi);
    for (end, back) in src.iter().enumerate().filter(|(j, b)| b.opcode == OpCode::Jump && (b.operand as usize) <= *j) {
        let head = back.operand as usize;
        // A jump from outside into the loop would skip the reset at its head.
        let outside = |k: usize| k < head || k > end;
        if src.iter().enumerate().any(|(k, ins)| outside(k) && ins.opcode.is_jump() && (head..=end).contains(&(ins.operand as usize))) { continue; }
        // Each local an append in the loop stores, looked at once.
        let mut slots: Vec<u16> = (head..end).filter(|&k| src[k].opcode == OpCode::InPlaceAdd && src[k + 1].opcode == OpCode::StoreName)
            .map(|k| src[k + 1].operand).filter(|&s| facts.kinds.get(s as usize) == Some(&Kind::Local)).collect();
        slots.sort_unstable();
        slots.dedup();
        for slot in slots {
            // Every use of the slot in the loop is a load an append adds to and that append's store.
            let uses: Vec<usize> = (head..=end).filter(|&u| refers(&src[u]) && src[u].operand == slot).collect();
            let paired = uses.chunks(2).all(|p| matches!(p, [load, st] if src[*load].opcode == OpCode::LoadName && src[*st].opcode == OpCode::StoreName
                && *st >= 1 && src[*st - 1].opcode == OpCode::InPlaceAdd));
            if !uses.len().is_multiple_of(2) || !paired { continue; }
            for p in uses.chunks(2) { appends[p[1] - 1] = true; }
            resets.push((head, slot));
        }
    }
    (appends, resets)
}

/* The locals stored once, with the list made just before, and never deleted nor merged, which hold that list wherever read. */
fn list_locals(src: &[Instruction], flags: &[u32], facts: &Facts, names: usize) -> Vec<bool> {
    let mut stores = vec![0u8; names];
    let mut list = vec![false; names];
    for (k, ins) in src.iter().enumerate() {
        let s = ins.operand as usize;
        if s >= names { continue; }
        match ins.opcode {
            OpCode::StoreName => {
                stores[s] = stores[s].saturating_add(1);
                list[s] = k > 0 && src[k - 1].opcode == OpCode::BuildList && !flags.get(k).is_some_and(|f| f & TARGET != 0);
            }
            OpCode::Del | OpCode::Phi => stores[s] = 2,
            _ => {}
        }
    }
    (0..names).map(|s| stores[s] == 1 && list[s] && facts.kinds.get(s) == Some(&Kind::Local)).collect()
}

/* No instruction after it runs from it. */
fn ends_flow(op: OpCode) -> bool { matches!(op, OpCode::Jump | OpCode::ReturnValue | OpCode::Raise | OpCode::RaiseFrom) }

#[inline]
fn transfer(bound: &mut [u64], ins: Instruction, k: usize, chunk: &SSAChunk) {
    if !matches!(ins.opcode, OpCode::StoreName | OpCode::Phi | OpCode::Del) { return; }
    let s = ins.operand as usize;
    let has = |b: &[u64], r: u16| b.get(r as usize / 64).is_some_and(|w| w & 1 << (r % 64) != 0);
    // A Phi binds its slot only when one of its sources is bound.
    if ins.opcode == OpCode::Phi && !chunk.phi_map.get(k).and_then(|&p| chunk.phi_sources.get(p)).is_some_and(|&(a, b)| has(bound, a) || has(bound, b)) { return; }
    let Some(w) = bound.get_mut(s / 64) else { return };
    if ins.opcode == OpCode::Del { *w &= !(1 << (s % 64)); } else { *w |= 1 << (s % 64); }
}

/* The slots bound on every path into each block, a bit row per block. */
struct Bound {
    words: usize,
    // Each block's row, its slots' words and then a word set once any path reaches the block.
    bits: Vec<u64>,
}

// An instruction's flags, a jump or handler enters, a block starts, a forward `Jump` enters, another jump or handler enters, the block's row above.
const TARGET: u32 = 1;
const START: u32 = 2;
const FORWARD: u32 = 4;
const ENTERED: u32 = 8;
const ROW: u32 = 4;

/* The slots bound entering the block at `i` of `rows`, none when no path reaches it. */
#[inline]
fn entry(rows: &[u64], words: usize, flags: &[u32], i: usize, out: &mut [u64]) {
    let f = flags.get(i).copied().unwrap_or(0);
    let at = (f >> ROW) as usize * (words + 1);
    if f & START == 0 || rows[at + words] == 0 { out.fill(0); return; }
    for (o, &b) in out.iter_mut().zip(&rows[at..at + words]) { *o = b; }
}

/* Narrows the block at `t` of `rows` to what `s` binds, true when that changed it. */
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

impl Bound {
    /* The slots bound entering the block at `i`, none when no path reaches it. */
    #[inline]
    fn entry(&self, flags: &[u32], i: usize, out: &mut [u64]) { entry(&self.bits, self.words, flags, i, out) }
}

/* Whether a body calls the function module binding `g` holds, itself when that is the binding it was called through. */
fn recurs(ins: &[Ins], g: u16) -> bool {
    g != u16::MAX && ins.iter().enumerate().any(|(j, &i)| matches!(i.op, OpCode::CallGlobalR | OpCode::CallR) && super::block::global_of(ins, j, i) == Some(g))
}

/* For each instruction of `src`, whether no loop runs it again, as no jump back from past it lands at or before it. */
fn runs_once(src: &[Instruction]) -> Vec<bool> {
    let mut depth = vec![0i32; src.len() + 1];
    for (j, ins) in src.iter().enumerate().filter(|(j, i)| i.opcode.is_jump() && (i.operand as usize) <= *j) {
        depth[ins.operand as usize] += 1;
        depth[j + 1] -= 1;
    }
    let mut d = 0;
    depth.iter().take(src.len()).map(|&k| { d += k; d == 0 }).collect()
}

/* The slots bound entering each block of `src`, each block start's row noted in its `flags`. */
#[inline(never)]
fn bound_on_entry(src: &[Instruction], chunk: &SSAChunk, names: usize, facts: &Facts, flags: &mut [u32]) -> Bound {
    let len = src.len();
    let words = names.div_ceil(64);
    let mut blocks = 0;
    for f in flags.iter_mut() { if *f & START != 0 { *f |= blocks << ROW; blocks += 1; } }
    // The rows, then the slots of the block being walked.
    let at = blocks as usize * (words + 1);
    let mut bits = vec![0; at + words];
    let (rows, s) = bits.split_at_mut(at);
    facts.bind(s);
    merge(rows, words, flags, 0, s);
    // Passes in order until one narrows no block passed already (a loop head by its back-edge), a block no path reached yet waiting for one that does.
    loop {
        let (mut again, mut live) = (false, false);
        for i in 0..len {
            if flags[i] & START != 0 {
                let at = (flags[i] >> ROW) as usize * (words + 1);
                live = rows[at + words] != 0;
                if live { entry(rows, words, flags, i, s); }
            }
            if !live { continue; }
            let ins = src[i];
            transfer(s, ins, i, chunk);
            if ins.opcode.is_jump() && (ins.operand as usize) <= len && merge(rows, words, flags, ins.operand as usize, s) && ins.operand as usize <= i { again = true; }
            if ends_flow(ins.opcode) { live = false; continue; }
            if flags[i + 1] & START != 0 { merge(rows, words, flags, i + 1, s); }
        }
        if !again { break; }
    }
    Bound { words, bits }
}

/* An `a if c else b` lowered in registers, as `Lower::tern` holds it, its arm starts, first arm's jump, join, pending values and arm registers. */
struct Tern { start_a: usize, else_at: usize, end_a: usize, join: usize, saved: Vec<Entry>, t: Option<u16>, bail: bool }

/* The value an arm of `a if c else b` left on top of what was pending before it, `saved`, None when it left anything else. */
fn arm_value(pending: &[Entry], saved: &[Entry]) -> Option<Entry> {
    let n = saved.len();
    (pending.len() == n + 1 && pending[..n] == saved[..]).then(|| pending[n])
}

struct Lower<'c, 'p> {
    src: &'c [Instruction],
    chunk: &'c SSAChunk,
    facts: &'c Facts,
    // Each instruction's flags, whether a jump or handler enters, a block starts, how jumps enter it, and past them the block's row.
    flags: &'c [u32],
    deleted: Vec<bool>,
    names: usize,
    cbase: usize,
    consts: usize,
    tbase: usize,
    out: Vec<Ins>,
    orig: Vec<u32>,
    at: Vec<u32>,
    back: Vec<u32>,
    pending: Vec<Entry>,
    // Lowered jumps whose target is still a compiler index.
    patches: Vec<usize>,
    temps: usize,
    // The last instruction, when it wrote a fresh temporary a store may take over.
    last: Option<usize>,
    cur: u32,
    // A register call left its result on the stack, which the next instruction takes into a register first.
    takes: bool,
    // The values pending under that call, copied to the stack in case it suspends.
    under: Vec<Entry>,
    // The InPlaceAdd instructions that append to a local their loop only ever appends to.
    appends: Vec<bool>,
    // The locals that only ever hold the list they were made with, `list_locals`.
    lists: core::cell::OnceCell<Vec<bool>>,
    // The first compiler instruction whose work no emitted instruction has finished.
    quiet: u32,
    bound: Vec<u64>,
    sections: &'c dyn Fn(Want) -> Option<Callee<'p>>,
    // The functions to lay as sections, by the binding each is called through, and each call's place and section.
    wanted: Vec<(u16, Callee<'p>)>,
    heads: Vec<(usize, usize, Ins)>,
    // Operands of section calls laid only once the sections are, a section's parameter register or a constant's extra slot.
    prep: Vec<(usize, u8, Prep)>,
    // The sites stubs of method calls start with, by the head and once laid by the stub's call, marked.
    seeds: Vec<(usize, super::sites::Site)>,
    // A call's StoreTopR, whose register a store after it may take over.
    dest: Option<usize>,
    // Where a resume after the last call, which left its value on the stack, enters, the StoreTopR of a section call.
    resume: Option<usize>,
    // For the next head, the parameters a keyword call left to their defaults, and the registers and operand its stub calls with.
    gaps: u8,
    kwcall: Option<(Vec<u16>, u16)>,
    // The keyword calls among the heads, by the head's place.
    kwstubs: Vec<(usize, Vec<u16>, u16)>,
    // Plain sections of at most two parameters a register call checks after the one its head names, by that one.
    alts: Vec<(usize, usize)>,
    // The lookups stubs lay, by place, and the section their head names, given their table once it is made.
    lookups: Vec<(usize, usize)>,
    // Where each `yield` of a register lies.
    yields: Vec<usize>,
    // The loops that step a generator's section, and the one whose step comes next.
    genfors: Vec<GenFor>,
    gen_next: Option<GenNext>,
    // For a generator's section, a call through a parameter given a laid function, with the section, the call's place and the function's section.
    gen_args: Vec<(usize, usize, usize)>,
    // Where the next head's call returns to, past it unless a generator's `yield` of its result returns it to the loop's body.
    ret_to: Option<u16>,
    // For a section's loop over a parameter given a generator call's result, the section, the loop's PushRegs and the generator's section it steps.
    nested: Vec<(usize, usize, usize)>,
    // For each generator's section, the loop it feeds, with its exit, its variable (None for a module binding), body start and whether it is own code.
    gen_loops: Vec<(usize, u16, Option<u16>, u16, bool)>,
    // A generator's section a growth lays for a loop another generator's section steps, and that one's step register, which the loop reads either way.
    gen_steps: Vec<(usize, u16)>,
    // The values pending under each head with more than one, by the head's place, and where its stub's store lies once laid.
    unders: Vec<(usize, Vec<Entry>)>,
    stub_stores: Vec<(usize, usize)>,
    // The next section wanted is for a call with keywords, which may fill the parameters past a lone `*`.
    keyed: bool,
    // The instruction lowering runs once, a module's outside its loops.
    once: bool,
    // For the next head, the registers of the arguments past the fifth.
    more_args: Vec<u16>,
    // The next section wanted is for a call that names its arguments past the fifth, the only one that may pass more than five.
    wide: bool,
    // The ops among the heads that call their operator dunder's section, by the head's place, as their stubs run them.
    dunders: Vec<(usize, Ins)>,
    // The sections an op calls, whose return of NotImplemented hands the op back to its stub.
    declining: Vec<usize>,
    // The other dunders an op's stub checks before running the op, by the head's place, with the section, its check and whether a `!=` inverts it.
    dunder_alts: Vec<(usize, usize, u16, bool)>,
    // The heads stubs lay for those, by place, and the section each checks.
    dunder_heads: Vec<(usize, usize)>,
    // For the next head, the value its second parameter takes past the arguments, an attribute's name for its `__getattr__`.
    head_arg: Option<Val>,
    // For a growth, the block it grows, whose first `first` sections are laid already, as are the first `alts_first` table pairs of `alts`.
    old: Option<&'c Block>,
    first: usize,
    alts_first: usize,
    // For a growth, the one function the head met, which alone it lays beside the ones it has.
    only: Option<Val>,
    // The TurnRs of calls into another section, which name the register before its link once it is laid.
    turn_bases: Vec<(usize, usize)>,
    // The heads laid past the end for calls of code that grew, where each returns and where it lies.
    displaced: Vec<(u32, u32)>,
    // While the sections are laid, the calls each section makes of the others, found when first asked.
    calls: OnceCell<Vec<OnceCell<Calls>>>,
    /* The `a if c else b` being lowered with its pending values kept in registers, made only when one is, and one left at the end gives up the code. */
    tern: Option<alloc::boxed::Box<Tern>>,
}


/* The calls in a section's body that jump to sections, each with the section it jumps to. */
type Calls = Rc<[(usize, usize)]>;

impl<'p> Lower<'_, 'p> {
    /* Lowers instruction `i`, the count of compiler instructions it took. */
    fn one(&mut self, i: usize) -> usize {
        let ins = self.src[i];
        let op = ins.operand;
        let regs = self.facts.regs;
        // A call deep in an expression leaves all but the value right under it on the stack, so it still reads its operands from registers.
        if regs {
            let args = match ins.opcode {
                OpCode::Call if op <= 5 => Some(op as usize),
                OpCode::Call if op >> 8 != 0 => Some((op & 0xFF) as usize + 2 * (op >> 8) as usize),
                OpCode::CallMethod => self.src.get(i + 1).filter(|a| a.opcode == OpCode::CallMethodArgs && a.operand <= 4).map(|a| a.operand as usize),
                _ => None,
            };
            if let Some(n) = args && self.pending.len() > n + 1 + MAX_UNDER {
                let extra = self.pending.len() - (n + 1 + MAX_UNDER);
                let deep: Vec<Entry> = self.pending.drain(..extra).collect();
                self.push_regs(&deep);
            }
        }
        if ins.opcode == OpCode::Call && regs && op >> 8 == 0 && let Some(n) = self.gen_for(i, op) { return n; }
        if ins.opcode == OpCode::BeginArgs && regs && let Some(n) = self.spread(i) { return n; }
        if ins.opcode == OpCode::Call && regs && (6..=MAX_ARGS as u16).contains(&op) && let Some(n) = self.many_call(i, op as usize) { return n; }
        if ins.opcode == OpCode::Call && regs && op >> 8 != 0 && let Some(n) = self.kw_call(i, op) { return n; }
        if ins.opcode == OpCode::Call && regs && let Some(n) = self.spread_call(i, op) { return n; }
        match ins.opcode {
            OpCode::LoadName if regs && (op as usize) < self.names => self.load_name(op),
            OpCode::LoadConst if regs && (op as usize) < self.consts => self.push(Entry::reg((self.cbase + op as usize) as u16)),
            OpCode::LoadNone if regs => self.push(Entry::reg((self.cbase + self.consts) as u16)),
            OpCode::LoadTrue if regs => self.push(Entry::reg((self.cbase + self.consts + 1) as u16)),
            OpCode::LoadFalse if regs => self.push(Entry::reg((self.cbase + self.consts + 2) as u16)),
            OpCode::StoreName if regs && (op as usize) < self.names && !self.pending.is_empty() => self.store_name(op),
            // A value left on the stack reaches its variable in the register loop.
            OpCode::StoreName if regs && (op as usize) < self.names => match self.kind(op) {
                Some(Kind::Local) => self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: op, b: 0, c: 0 }),
                Some(Kind::Global(g)) if self.builtin_flag(op) == 0 => self.emit(Ins { op: OpCode::StoreTopR, x: 1, a: g as u16, b: 0, c: 0 }),
                _ => self.emit_orig(ins),
            },
            // A `global` name reads and writes its module binding by index.
            OpCode::LoadGlobal if regs && let Some(Kind::Global(g)) = self.kind(op) => {
                let t = self.temp();
                self.produce(Ins { op: OpCode::LoadGlobalR, x: 0, a: t, b: g as u16, c: 0 });
            }
            OpCode::StoreGlobal if regs && !self.pending.is_empty() && let Some(Kind::Global(g)) = self.kind(op) => {
                let v = self.pop();
                // A builtin's store redirects its fused calls.
                self.emit(Ins { op: OpCode::StoreGlobalR, x: self.builtin_flag(op), a: g as u16, b: v.r, c: 0 });
            }
            OpCode::PopTop if !self.pending.is_empty() => { self.pending.pop(); }
            // A declaration does nothing at run time.
            OpCode::Global | OpCode::Nonlocal => {}
            OpCode::Dup if !self.pending.is_empty() => { let e = self.pending[self.pending.len() - 1]; self.push(e); }
            OpCode::Swap if self.pending.len() >= 2 => { let k = self.pending.len(); self.pending.swap(k - 1, k - 2); }
            OpCode::Rot3 if self.pending.len() >= 3 => {
                let k = self.pending.len();
                self.pending[k - 3..].rotate_left(1);
            }
            // A value a call left on the stack moves into a temporary, so the store after takes registers.
            OpCode::Swap | OpCode::Rot3 if regs && self.pending.len() + 1 == if ins.opcode == OpCode::Swap { 2 } else { 3 } => {
                let t = self.temp();
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                self.pending.push(Entry::reg(t));
            }
            OpCode::Dup2 if self.pending.len() >= 2 => {
                let k = self.pending.len();
                let (a, b) = (self.pending[k - 2], self.pending[k - 1]);
                self.push(a);
                self.push(b);
            }
            // `s += t` on a local its loop only appends to grows the string it owns.
            OpCode::InPlaceAdd if self.appends.get(i).copied().unwrap_or(false) && self.pending.len() >= 2
                && self.pending[self.pending.len() - 2] == (Entry { r: self.src[i + 1].operand, name: true }) => {
                // One whose operands found a dunder calls its section, the store after it taking the result.
                if let Some(n) = self.dunder_call(i, OpCode::InPlaceAdd, op) { return n; }
                let (_, t) = self.pop2();
                self.settle();
                self.emit(Ins { op: OpCode::AppendR, x: 0, a: self.src[i + 1].operand, b: t.r, c: 0 });
                return 2;
            }
            code if binary_form(code).is_some() && self.pending.len() >= 2 && op < AUGMENTED as u16 => return self.binary(i, code, op as u8),
            OpCode::Minus | OpCode::Not if !self.pending.is_empty() => {
                if ins.opcode == OpCode::Minus && let Some(n) = self.dunder_call(i, OpCode::Minus, 0) { return n; }
                // `not` of an instance takes the truth its `__bool__` or `__len__` gives, called as a section.
                if ins.opcode == OpCode::Not && let Some(&top) = self.pending.last() && let Some(t) = self.checked_call(i, OpCode::JumpIfFalseR, top) {
                    self.pending.pop();
                    self.produce(Ins { op: OpCode::NotR, x: 0, a: t, b: t, c: 0 });
                    return 1;
                }
                let v = self.pop();
                let t = self.temp();
                let form = if ins.opcode == OpCode::Minus { OpCode::MinusR } else { OpCode::NotR };
                self.produce(Ins { op: form, x: 0, a: t, b: v.r, c: 0 });
            }
            OpCode::JumpIfFalse if self.chunk.ternaries() && self.pending.len() > 1 && self.tern.is_none() && let Some((end_a, join)) = self.ternary(i) => {
                let c = self.pop();
                let t = self.checked_call(i, OpCode::JumpIfFalseR, c).unwrap_or(c.r);
                self.jump(OpCode::JumpIfFalseR, op, t, 0);
                self.tern = Some(alloc::boxed::Box::new(Tern { start_a: i + 1, else_at: op as usize, end_a, join, saved: self.pending.clone(), t: None, bail: false }));
            }
            // The first arm of `a if c else b` leaves its value where the second will, what was pending before it kept.
            OpCode::Jump if self.chunk.ternaries() && self.tern.as_ref().is_some_and(|t| t.end_a == i && !t.bail) => {
                let Some(mut tern) = self.tern.take() else { return 1 };
                match arm_value(&self.pending, &tern.saved) {
                    Some(v) => {
                        let t = if !v.name && v.r as usize >= self.tbase { v.r } else {
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
                let c = self.pop();
                self.flush();
                // An instance tested branches on the truth its `__bool__` or `__len__` gives, called as a section.
                let t = self.checked_call(i, OpCode::JumpIfFalseR, c).unwrap_or(c.r);
                self.jump(OpCode::JumpIfFalseR, op, t, 0);
            }
            // `isinstance` reads its two operands off the stack, what is pending under them kept in registers.
            OpCode::CallIsInstance if regs && op == 2 && self.pending.len() > 2 => {
                let (o, c) = self.pop2();
                self.push_regs(&[o, c]);
                self.emit_orig(ins);
                let t = self.temp();
                self.produce(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
            }
            // The accumulator stays on the stack, what it takes comes from registers.
            OpCode::ListAppend | OpCode::SetAdd if !self.pending.is_empty() => {
                let v = self.pop();
                self.flush();
                let form = if ins.opcode == OpCode::ListAppend { OpCode::ListAppendR } else { OpCode::SetAddR };
                self.emit(Ins { op: form, x: 0, a: 0, b: v.r, c: v.r });
            }
            OpCode::MapAdd if self.pending.len() >= 2 => {
                let (k, v) = self.pop2();
                self.flush();
                self.emit(Ins { op: OpCode::MapAddR, x: 0, a: 0, b: k.r, c: v.r });
            }
            // A short-circuit feeding a test jumps where the test would, its value never stacked.
            OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop if !self.pending.is_empty()
                && let Some(test) = self.threaded_test(ins) => {
                let c = self.pop();
                self.flush();
                if ins.opcode == OpCode::JumpIfFalseOrPop {
                    self.jump(OpCode::JumpIfFalseR, self.src[test].operand, c.r, 0);
                } else {
                    self.jump(OpCode::JumpIfFalseR, test as u16 + 1, c.r, 0);
                    if let Some(j) = self.out.last_mut() { j.x = 1; }
                }
            }
            OpCode::ReturnValue if !self.pending.is_empty() => {
                let v = self.pop();
                // What stays below the value only leaves with the frame.
                self.pending.clear();
                self.emit(Ins { op: OpCode::ReturnR, x: 0, a: 0, b: v.r, c: 0 });
            }
            // `bool` of an instance takes the truth its `__bool__` or `__len__` gives, called as a section.
            OpCode::CallBool if op == 1 && let Some(&top) = self.pending.last() && let Some(t) = self.checked_call(i, OpCode::CallBool, top) => {
                self.pending.pop();
                // A `__len__`'s length is made a bool.
                if self.out.last().is_some_and(|c| c.x == 3) { let u = self.temp(); self.emit(Ins { op: OpCode::NotR, x: 0, a: u, b: t, c: 0 }); self.produce(Ins { op: OpCode::NotR, x: 0, a: t, b: u, c: 0 }); }
                else { self.pending.push(Entry::reg(t)); }
            }
            // A `for` over an instance steps the iterator its `__iter__` gives, called as a section, taken as it is.
            OpCode::GetIter if op == 0 && self.pending.len() == 1 && let Some(t) = self.checked_call(i, OpCode::GetIter, self.pending[0]) => {
                self.pending[0] = Entry::reg(t);
                self.settle();
                self.emit(Ins { op: OpCode::GetIter, x: 0, a: crate::parser::ITER_AS_IS, b: 0, c: 0 });
            }
            // One positional and no spread, which fused `len` carries as its bare count.
            OpCode::CallLen if op == 1 && !self.pending.is_empty() => {
                // An instance's `__len__` is called as a section, its result checked to be a length.
                let top = *self.pending.last().unwrap_or(&Entry::reg(0));
                if let Some(t) = self.checked_call(i, OpCode::LenR, top) { self.pending.pop(); self.pending.push(Entry::reg(t)); return 1; }
                let o = self.pop();
                let t = self.temp();
                self.produce(Ins { op: OpCode::LenR, x: 0, a: t, b: o.r, c: 0 });
            }
            OpCode::GetItem if self.pending.len() >= 2 => {
                if let Some(n) = self.dunder_call(i, OpCode::GetItem, 0) { return n; }
                let (o, k) = self.pop2();
                let t = self.temp();
                self.produce(Ins { op: OpCode::GetItemR, x: 0, a: t, b: o.r, c: k.r });
            }
            OpCode::StoreItem if self.pending.len() >= 3 => {
                if self.dunder_call(i, OpCode::StoreItem, 0).is_some() { return 1; }
                let (k, v) = self.pop2();
                let o = self.pop();
                self.emit(Ins { op: OpCode::StoreItemR, x: 0, a: o.r, b: k.r, c: v.r });
            }
            // `str` or `repr` of an instance calls its `__str__` or `__repr__` as a section.
            OpCode::CallStr | OpCode::CallRepr if regs && op == 1 && !self.pending.is_empty() && let Some(n) = self.dunder_call(i, ins.opcode, op) => return n,
            OpCode::LoadAttr if !self.pending.is_empty() => {
                // A property's getter the load found is called as a section.
                if let Some(n) = self.dunder_call(i, OpCode::LoadAttr, op) { return n; }
                let o = self.pop();
                let t = self.temp();
                self.produce(Ins { op: OpCode::GetAttrR, x: 0, a: t, b: o.r, c: op });
            }
            OpCode::StoreAttr if self.pending.len() >= 2 => {
                if self.dunder_call(i, OpCode::StoreAttr, op).is_some() { return 1; }
                let (o, v) = self.pop2();
                self.emit(Ins { op: OpCode::SetAttrR, x: 0, a: o.r, b: v.r, c: op });
            }
            // A `yield` whose result goes unused hands its value from a register and takes nothing back.
            OpCode::Yield if regs && !self.pending.is_empty() && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::PopTop) && !self.target(i + 1) => {
                let v = self.pop();
                self.settle();
                self.yields.push(self.out.len());
                self.emit(Ins { op: OpCode::Yield, x: 1, a: v.r, b: 0, c: 0 });
                return 2;
            }
            // A loop over a generator laid for it steps its section, entered here only by an unlowered run or a turned-down call, as an ordinary loop.
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
            // A comprehension made and called at once on the iterator under it runs the section any function of its body runs.
            OpCode::MakeFunction if regs && self.chunk.functions.get(op as usize).is_some_and(|f| f.2 == 0 && super::scope::is_comp(&f.0) && !f.1.is_generator) && i + 3 < self.src.len()
                && self.src[i + 1].opcode == OpCode::Swap && self.src[i + 2].opcode == OpCode::Call && self.src[i + 2].operand == 1 && !self.target(i + 1) && !self.target(i + 2) => {
                self.settle();
                // Each held pending while the next is taken, so the two are apart.
                let it = self.temp();
                self.pending.push(Entry::reg(it));
                let f = self.temp();
                self.pending.clear();
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: it, b: 0, c: 0 });
                self.emit_orig(ins);
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: f, b: 0, c: 0 });
                self.cur = i as u32 + 2;
                let join = self.target(i + 3);
                let call = Ins { op: OpCode::CallR, x: 1, a: f, b: it, c: 0 };
                // Code with no sections calls it through its register, which a section laying that code jumps from, only in a loop or a function body.
                let again = (self.facts.bound != 0 || !self.facts.bound_far.is_empty()) || self.src.iter().enumerate().any(|(j, b)| b.opcode == OpCode::Jump && j > i && (b.operand as usize) <= i);
                let made = if again { (self.sections)(Want::Made(&self.chunk.functions[op as usize].1)) } else { None };
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
            // A lambda or generator made with no defaults pops nothing and goes straight to a register, so a call taking it reads its operands from registers.
            OpCode::MakeFunction if regs && self.chunk.functions.get(op as usize).is_some_and(|f| f.2 == 0 && (f.3 == u16::MAX || f.1.is_generator)) && i + 1 < self.src.len() && !self.target(i + 1) => {
                self.emit_orig(ins);
                let t = self.temp();
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                self.pending.push(Entry::reg(t));
            }
            // A generator expression's iterator and its range come back in registers, so its call reads its operands there and what is pending stays put.
            OpCode::GetIter if regs && op == crate::parser::ITER_VALUE && self.pending.last().is_some_and(|e| !e.name) && !self.target(i + 1)
                && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::MakeFunction && self.chunk.functions.get(n.operand as usize).is_some_and(|f| f.1.is_generator)) => {
                let e = self.pop();
                self.push_regs(&[e]);
                self.emit_orig(ins);
                let t = self.temp();
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                self.pending.push(Entry::reg(t));
            }
            OpCode::CallRange if regs && (1..=3).contains(&op) && self.pending.len() >= op as usize && !self.target(i + 1)
                && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::GetIter && n.operand == crate::parser::ITER_VALUE)
                && self.src.get(i + 2).is_some_and(|n| n.opcode == OpCode::MakeFunction && self.chunk.functions.get(n.operand as usize).is_some_and(|f| f.1.is_generator)) => {
                let args: Vec<Entry> = self.pending.drain(self.pending.len() - op as usize..).collect();
                self.push_regs(&args);
                self.emit_orig(ins);
                let t = self.temp();
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                self.pending.push(Entry::reg(t));
            }
            // The loop variable takes each item straight from the iterator.
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
                // A user iterator whose `__next__` the step found calls it as a section into the loop's variable.
                if self.next_call(i, op, target) { return 2; }
                self.patches.push(self.out.len());
                self.emit(Ins { op: OpCode::ForIterR, x: target.0, a: op, b: target.1, c: 0 });
                return 2;
            }
            // A `yield from`'s loop steps its iterator into a register of its own, which the `yield` after hands out.
            OpCode::ForIter if regs && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::Yield) && self.src.get(i + 2).is_some_and(|n| n.opcode == OpCode::PopTop)
                && !self.target(i + 1) && !self.target(i + 2) => {
                self.settle();
                let t = self.temp();
                self.patches.push(self.out.len());
                self.emit(Ins { op: OpCode::ForIterR, x: 0, a: op, b: t, c: 0 });
                self.pending.push(Entry::reg(t));
                return 1;
            }
            // A tuple built only to unpack hands its values straight to the stores.
            OpCode::BuildTuple if (op as usize) <= self.pending.len()
                && op > 0
                && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::UnpackSequence && n.operand == op)
                && !self.target(i + 1) => {
                let k = self.pending.len() - op as usize;
                self.pending[k..].reverse();
                return 2;
            }
            // Unpacking straight into locals writes each one, nothing left on the stack between.
            OpCode::UnpackSequence if (1..=3).contains(&op) && regs && self.unpacks_to_locals(i, op as usize) => {
                self.settle();
                // Only the first `op` fields name targets, the rest stay 0 rather than read what follows the stores.
                let t = |k: usize| if k < op as usize { self.src.get(i + 1 + k).map_or(0, |s| s.operand) } else { 0 };
                self.emit(Ins { op: OpCode::UnpackR, x: op as u8, a: t(0), b: t(1), c: t(2) });
                return 1 + op as usize;
            }
            // A call of up to five positionals reads its operands from registers, what is pending under it goes to the stack first.
            OpCode::Call if regs && op <= 5 && self.pending.len() > op as usize && self.pending.len() <= op as usize + 1 + MAX_UNDER && i + 1 < self.src.len() => {
                // A call whose result meets another path's value at the next instruction leaves it on the stack, as it lies there.
                let join = self.target(i + 1);
                let under = self.pending.len() - op as usize - 1;
                let r = |k: usize| self.pending.get(under + k).map_or(0, |e| e.r);
                let call = Ins { op: OpCode::CallR, x: op as u8, a: r(0), b: r(1), c: r(2) };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(3), b: r(4), c: r(5) };
                let at = self.out.len() as u32;
                // A global loaded just before only to be called is read by the call itself, which starts where the load did.
                let load = self.out.last().copied().filter(|load| load.op == OpCode::LoadGlobalR && load.a == call.a
                    && [call.b, call.c, rest.a, rest.b, rest.c].iter().take(call.x as usize).all(|&r| r != load.a));
                let known = load.filter(|_| self.facts.regs).and_then(|load| self.want(load.b, call.x as usize));
                // A call past two arguments reads its global itself only when it is laid as a section.
                let load = load.filter(|_| !wide(call) || known.is_some())
                    // A closure the binding holds is called from the register it was read into, which its section checks.
                    .filter(|_| !known.is_some_and(|k| self.wanted[k].1.twin));
                let Some(load) = load else {
                    // A global loaded earlier in the call, or a function the call found before, is laid as a section, the call checking its register.
                    let section = if !self.facts.regs { None }
                        else { super::block::global_of(&self.out, self.out.len(), call).and_then(|g| self.want(g, call.x as usize))
                            .or_else(|| (self.sections)(Want::Value(i as u32)).and_then(|c| { let n = call.x as usize + (c.on_class & 3 == 3) as usize; self.want_callee(u16::MAX, c, n) }).inspect(|&k| self.want_alts(None, i as u32, k, call.x as usize))) };
                    if let Some(section) = section {
                        // The other classes whose instances the call met are looked up by their `__call__`.
                        if self.wanted[section].1.on_class == 3 { self.want_method_alts(None, i as u32 | CALLED, section, call.x as usize + 1); }
                        let (call, rest) = if self.wanted[section].1.on_class & 3 == 3 { called_instance(call, rest) } else { (call, rest) };
                        self.pending.truncate(under);
                        let t = self.temp();
                        let below = self.pending.clone();
                        self.head(call, rest, t, section, true, &below);
                        self.pending.push(Entry::reg(t));
                        self.dest = Some(self.out.len() - 3);
                        if !join { self.resume = Some(self.out.len() - 3); }
                        return 1;
                    }
                    self.flush_under(op as usize + 1);
                    self.pending.clear();
                    self.settle();
                    self.emit(call);
                    if wide(call) { self.emit(rest); }
                    self.take(join);
                    return 1;
                };
                self.out.pop();
                self.orig.pop();
                self.back.pop();
                for a in self.at[..=i].iter_mut().rev().take_while(|a| **a == at) { *a = at - 1; }
                let call = Ins { op: OpCode::CallGlobalR, a: load.b, ..call };
                // A function that may be laid as a section is jumped to, its result in a fresh temporary, what is pending staying in its registers.
                if let Some(section) = known {
                    self.pending.truncate(under);
                    let t = self.temp();
                    let below = self.pending.clone();
                    self.head(call, rest, t, section, true, &below);
                    self.pending.push(Entry::reg(t));
                    self.dest = Some(self.out.len() - 3);
                    if !join { self.resume = Some(self.out.len() - 3); }
                    return 1;
                }
                self.flush_under(op as usize + 1);
                self.pending.clear();
                self.settle();
                self.emit(call);
                self.take(join);
            }
            // An append to a local only ever holding its list, its None dropped, pushes onto the list.
            OpCode::CallMethod if regs && let Some(args) = self.src.get(i + 1) && args.opcode == OpCode::CallMethodArgs && args.operand == 1
                && self.pending.len() >= 2 && self.chunk.names.get(op as usize).is_some_and(|n| n == "append")
                && let [.., recv, v] = self.pending[..] && recv.name
                && self.lists.get_or_init(|| list_locals(self.src, self.flags, self.facts, self.names)).get(recv.r as usize) == Some(&true)
                && self.src.get(i + 2).is_some_and(|n| n.opcode == OpCode::PopTop) && !self.target(i + 1) && !self.target(i + 2) => {
                self.pending.truncate(self.pending.len() - 2);
                self.emit(Ins { op: OpCode::ListPushR, x: 0, a: recv.r, b: v.r, c: 0 });
                return 3;
            }
            // A method call of up to four positionals reads its receiver and arguments from registers, what is pending under it goes to the stack first.
            OpCode::CallMethod if regs && let Some(args) = self.src.get(i + 1) && args.opcode == OpCode::CallMethodArgs
                && args.operand <= 4 && self.pending.len() > args.operand as usize && self.pending.len() <= args.operand as usize + 1 + MAX_UNDER && i + 2 < self.src.len() => {
                let join = self.target(i + 2);
                let under = self.pending.len() - args.operand as usize - 1;
                let r = |k: usize| self.pending.get(under + k).map_or(0, |e| e.r);
                let call = Ins { op: OpCode::CallMethodR, x: args.operand as u8, a: op, b: r(0), c: r(1) };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) };
                // A method the call found before is laid as a section, the call checking its receiver still finds it.
                if self.facts.regs && let Some(callee) = (self.sections)(Want::Method(i as u32))
                    && let Some(section) = { let n = call.x as usize + !matches!(callee.on_class, 1 | 4) as usize; self.want_callee(u16::MAX, callee, n) } {
                    self.want_method_alts(None, i as u32, section, call.x as usize + 1);
                    self.pending.truncate(under);
                    let t = self.temp();
                    let below = self.pending.clone();
                    // A receiver loaded from a module binding just before is read by the head itself, which starts where the load did.
                    let at = self.out.len() as u32;
                    let global = self.out.last().copied().filter(|l| l.op == OpCode::LoadGlobalR && l.a == call.b && l.b < u16::MAX && !wide(call) && (call.x == 0 || call.c != l.a)).map(|l| l.b);
                    if global.is_some() {
                        self.out.pop();
                        self.orig.pop();
                        self.back.pop();
                        for a in self.at[..=i].iter_mut().rev().take_while(|a| **a == at) { *a = at - 1; }
                    }
                    self.head(call, rest, t, section, true, &below);
                    if let Some(g) = global { let h = self.out.len() - HEADER as usize; self.out[h + 3].c = g + 1; }
                    self.pending.push(Entry::reg(t));
                    self.dest = Some(self.out.len() - 3);
                    if !join { self.resume = Some(self.out.len() - 3); }
                    return 2;
                }
                self.flush_under(args.operand as usize + 1);
                let r = |k: usize| self.pending.get(k).map_or(0, |e| e.r);
                let call = Ins { op: OpCode::CallMethodR, x: args.operand as u8, a: op, b: r(0), c: r(1) };
                let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) };
                self.pending.clear();
                self.settle();
                self.emit(call);
                if wide(call) { self.emit(rest); }
                self.take_at(join, 2);
                return 2;
            }
            OpCode::CallMethod if self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::CallMethodArgs) => {
                self.settle();
                self.emit(Ins { op: OpCode::CallMethod, x: 0, a: op, b: self.src[i + 1].operand, c: 0 });
                return 2;
            }
            // The `__enter__` and `__exit__` a `with` stages go to registers from `b` on, so the call after may jump to its method's section.
            OpCode::WithEnter | OpCode::WithExit if regs && i + 1 < self.src.len() && self.src[i + 1].opcode == OpCode::Call && !self.target(i + 1) => {
                let enter = ins.opcode == OpCode::WithEnter;
                let cm = if enter && self.pending.last().is_some_and(|e| !e.name) { self.pending.pop() } else { None };
                self.settle();
                let n = if enter { 2 } else { 5 };
                // Nothing is pending once settled, so the temps follow one another.
                let first = self.temp();
                for k in 0..n { let t = self.temp(); debug_assert_eq!(t, first + k); self.pending.push(Entry::reg(t)); }
                self.emit(Ins { op: ins.opcode, x: 1 + cm.is_some() as u8, a: op, b: first, c: cm.map_or(0, |e| e.r) });
            }
            // A `with` left by falling through pops its block, stages `__exit__` and skips the WithExit after it, which the exits that carry something reach.
            OpCode::PopExcept if regs && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::BeginFinally) && !self.target(i + 1)
                && self.src.get(i + 2).is_some_and(|n| n.opcode == OpCode::WithExit) && self.src.get(i + 3).is_some_and(|n| n.opcode == OpCode::Call) && !self.target(i + 3) => {
                self.settle();
                self.emit(Ins { op: OpCode::WithExit, x: 2, a: self.src[i + 2].operand, b: 0, c: 0 });
                return 2;
            }
            // `__exit__`'s result is judged from its register, and a fall-through ends its block here too.
            OpCode::WithJudge if regs && self.pending.last().is_some_and(|e| !e.name) && self.src.get(i + 1).is_some_and(|n| n.opcode == OpCode::EndFinally) && !self.target(i + 1) => {
                let r = self.pop();
                self.settle();
                self.emit(Ins { op: OpCode::WithJudge, x: 1, a: r.r, b: 0, c: 0 });
            }
            // A list, tuple, set or string built from pending values stacks only its items and comes back in a register, so a call taking it reads registers.
            OpCode::BuildList | OpCode::BuildTuple | OpCode::BuildSet | OpCode::BuildString if regs && (op as usize) <= self.pending.len() && op > 0
                && i + 1 < self.src.len() && !self.target(i + 1) && matches!(self.src[i + 1].opcode, OpCode::Call | OpCode::CallMethod) => {
                let items: Vec<Entry> = self.pending.drain(self.pending.len() - op as usize..).collect();
                self.push_regs(&items);
                self.emit_orig(ins);
                let t = self.temp();
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                self.pending.push(Entry::reg(t));
            }
            OpCode::Call if regs && op <= 5 && let Some(n) = self.stack_call(op as usize) => return n,
            _ => {
                self.settle();
                self.emit_orig(ins);
            }
        }
        1
    }

    /* A stack call of `n` positionals of a module binding's function laid as a section, its function and arguments popped into temporaries. */
    fn stack_call(&mut self, n: usize) -> Option<usize> {
        if !self.facts.regs { return None; }
        self.settle();
        let (p, r) = super::block::stack_callee(&self.out, self.out.len(), n)?;
        let k = super::block::global_of(&self.out, p, Ins { op: OpCode::CallR, x: 0, a: r, b: 0, c: 0 }).and_then(|g| self.want(g, n))?;
        if self.wanted[k].1.on_class & 3 == 3 || self.wanted[k].1.twin { return None; }
        let t = self.tbase as u16;
        self.temps = self.temps.max(n + 1);
        for q in (0..=n as u16).rev() { self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t + q, b: 0, c: 0 }); }
        let r = |q: u16| if q as usize <= n { t + q } else { 0 };
        self.head(Ins { op: OpCode::CallR, x: n as u8, a: t, b: r(1), c: r(2) }, Ins { op: OpCode::ArgsR, x: 0, a: r(3), b: r(4), c: r(5) }, t, k, true, &[]);
        // Its result goes back on the stack, where the code as written takes it, so a run suspended inside resumes in either tier.
        self.emit(Ins { op: OpCode::PushRegs, x: 1, a: t, b: 0, c: 0 });
        Some(1)
    }

    /* How many values the plain or method call after the join at `i` takes off the stack as callee or receiver and first arguments, None for no such call. */
    fn joined_args(&self, i: usize) -> Option<usize> {
        if self.flags.get(i).is_none_or(|f| f & (FORWARD | ENTERED) != FORWARD) || !self.pending.is_empty() { return None; }
        let loads = self.src[i..].iter().take_while(|n| matches!(n.opcode, OpCode::LoadName | OpCode::LoadConst | OpCode::LoadGlobal)).count();
        let call = self.src.get(i + loads)?;
        // A method call takes its receiver and then its arguments, a fused one counting them on the instruction after it.
        let n = match call.opcode {
            OpCode::Call if call.operand <= 5 => call.operand as usize,
            OpCode::CallMethod if let Some(args) = self.src.get(i + loads + 1) && args.opcode == OpCode::CallMethodArgs && args.operand <= 4 => args.operand as usize,
            _ => return None,
        };
        if (i + 1..=i + loads).any(|j| self.target(j)) { return None; }
        let m = (n + 1).checked_sub(loads)?;
        (m >= 1).then_some(m)
    }

    /* For a JumpIfFalse at `i` testing `a if c else b` over pending values, the first arm's Jump and the join, when both arms are simple one-value code. */
    fn ternary(&self, i: usize) -> Option<(usize, usize)> {
        let else_at = self.src[i].operand as usize;
        let end_a = else_at.checked_sub(1)?;
        let jump = self.src.get(end_a)?;
        if end_a <= i || jump.opcode != OpCode::Jump || (jump.operand as usize) <= else_at { return None; }
        let join = jump.operand as usize;
        // Each arm pushes one value of its own, read from nothing under it.
        let arm = |from: usize, to: usize| -> Option<usize> {
            let (mut depth, mut most) = (0i32, 0i32);
            for k in from..to {
                if k != else_at && self.target(k) { return None; }
                let ins = self.src[k];
                depth += match ins.opcode {
                    OpCode::LoadName if (ins.operand as usize) < self.names && matches!(self.kind(ins.operand), Some(Kind::Local | Kind::Global(_))) => 1,
                    OpCode::LoadConst if (ins.operand as usize) < self.consts => 1,
                    OpCode::LoadNone | OpCode::LoadTrue | OpCode::LoadFalse => 1,
                    OpCode::LoadAttr => if depth < 1 { return None } else { 0 },
                    OpCode::Minus | OpCode::Not => if depth < 1 { return None } else { 0 },
                    code if binary_form(code).is_some() && ins.operand < AUGMENTED as u16 => if depth < 2 { return None } else { -1 },
                    _ => return None,
                };
                most = most.max(depth);
            }
            (depth == 1).then_some(most as usize)
        };
        let (a, b) = (arm(i + 1, end_a)?, arm(else_at, join)?);
        // The join is entered by the first arm's jump and the second's falling in, no other.
        let others = self.src.iter().enumerate().any(|(k, n)| k != end_a && k != i && matches!(n.opcode, OpCode::Jump | OpCode::JumpIfFalse | OpCode::JumpIfFalseOrPop | OpCode::JumpIfTrueOrPop | OpCode::ForIter | OpCode::SetupExcept | OpCode::SetupFinally) && matches!(n.operand as usize, t if t == else_at || t == join));
        (!others && self.pending.len() + a.max(b) + 1 < MAX_PENDING).then_some((end_a, join))
    }

    /* Joins the arms of `a if c else b`, the second's value going where the first left its own, pending over what was before. */
    fn join_tern(&mut self) {
        let Some(mut tern) = self.tern.take() else { return };
        let (Some(v), Some(t)) = (arm_value(&self.pending, &tern.saved), tern.t) else { tern.bail = true; self.tern = Some(tern); return };
        if v.name || v.r != t { self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: v.r, c: 0 }); }
        self.pending.clone_from(&tern.saved);
        self.pending.push(Entry::reg(t));
        self.last = None;
    }

    fn kind(&self, s: u16) -> Option<Kind> { self.facts.kinds.get(s as usize).copied() }

    /* Whether a jump or handler enters instruction `i`. */
    fn target(&self, i: usize) -> bool { self.flags.get(i).is_some_and(|f| f & TARGET != 0) }

    /* Whether the `n` instructions after `i` store into plain locals no jump lands between. */
    fn unpacks_to_locals(&self, i: usize, n: usize) -> bool {
        (1..=n).all(|k| self.src.get(i + k).is_some_and(|s| s.opcode == OpCode::StoreName && (s.operand as usize) < self.names
            && self.kind(s.operand) == Some(Kind::Local) && !self.target(i + k)))
    }

    /* The test a short-circuit reaches through a chain of its own kind. */
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

    /* The store flag a builtin's name carries, since rebinding one redirects its fused calls. */
    fn builtin_flag(&self, s: u16) -> u8 {
        let bare = self.chunk.names.get(s as usize).map_or("", |n| ssa_strip(n));
        if crate::value::NativeFnId::from_name(bare).is_some() { STORE_NOTES_BUILTIN } else { 0 }
    }

    /* A local's read waits for its use, since only the frame writes it. */
    fn deferrable(&self, s: usize) -> bool {
        !self.deleted.get(s).is_some_and(|&d| d) && self.bound.get(s / 64).is_some_and(|w| w & (1 << (s % 64)) != 0)
    }

    fn load_name(&mut self, s: u16) {
        let read = match self.kind(s) {
            Some(Kind::Global(g)) => Ins { op: OpCode::LoadGlobalR, x: 0, a: 0, b: g as u16, c: 0 },
            Some(Kind::Cell) => Ins { op: OpCode::LoadCellR, x: 0, a: 0, b: s, c: 0 },
            _ if self.deferrable(s as usize) => return self.push(Entry { r: s, name: true }),
            // Read now, so its error or its value is the one of this point.
            _ => Ins { op: OpCode::Move, x: 1, a: 0, b: s, c: 0 },
        };
        let t = self.temp();
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
        let fresh = !top.name && top.r as usize >= self.tbase && !self.pending.contains(&top);
        match (self.last, self.dest) {
            (Some(k), _) if !spilled && fresh && k + 1 == self.out.len() && self.out[k].a == top.r => self.out[k].a = s,
            // A section call writes its result straight to the variable.
            (_, Some(k)) if !spilled && fresh && self.out[k].a == top.r => self.out[k].a = s,
            _ => self.emit(Ins { op: OpCode::Move, x: top.name as u8, a: s, b: top.r, c: 0 }),
        }
        (self.last, self.dest) = (None, None);
    }

    /* Pending reads of `s` copy its value out before a store rebinds it. */
    fn spill(&mut self, s: u16) -> bool {
        if !self.pending.iter().any(|e| e.name && e.r == s) { return false; }
        let t = self.temp();
        self.emit(Ins { op: OpCode::Move, x: 1, a: t, b: s, c: 0 });
        for e in &mut self.pending { if e.name && e.r == s { *e = Entry::reg(t); } }
        true
    }

    /* The two topmost pending values, the deeper one first. */
    fn pop2(&mut self) -> (Entry, Entry) {
        let b = self.pop();
        let a = self.pop();
        (a, b)
    }

    /* The topmost pending value. */
    fn pop(&mut self) -> Entry { self.pending.pop().unwrap_or(Entry::reg(0)) }

    fn binary(&mut self, i: usize, code: OpCode, operand: u8) -> usize {
        if let Some(n) = self.dunder_call(i, code, operand as u16) { return n; }
        let (a, b) = self.pop2();
        if let Some(branch) = branch_form(code)
            && let Some(next) = self.src.get(i + 1)
            && next.opcode == OpCode::JumpIfFalse
            && !self.target(i + 1)
        {
            // A comparison testing `a if c else b` keeps what is pending under it, as a plain test does.
            if self.chunk.ternaries() && !self.pending.is_empty() && self.tern.is_none() && let Some((end_a, join)) = self.ternary(i + 1) {
                self.jump(branch, next.operand, a.r, b.r);
                self.tern = Some(alloc::boxed::Box::new(Tern { start_a: i + 2, else_at: next.operand as usize, end_a, join, saved: self.pending.clone(), t: None, bail: false }));
                return 2;
            }
            self.flush();
            self.jump(branch, next.operand, a.r, b.r);
            return 2;
        }
        let t = self.temp();
        let augmented = if matches!(code, OpCode::InPlaceBitAnd | OpCode::InPlaceBitOr | OpCode::InPlaceBitXor) { AUGMENTED } else { 0 };
        self.produce(Ins { op: binary_form(code).unwrap_or(code), x: operand | augmented, a: t, b: a.r, c: b.r });
        1
    }

    /* An op whose dunder the earlier run found, laid as a call of its section checking the left operand's class, its stub the op itself, else None. */
    fn dunder_call(&mut self, i: usize, code: OpCode, operand: u16) -> Option<usize> {
        use OpCode::*;
        if !self.facts.regs { return None; }
        let form = match code {
            GetItem => GetItemR, In | NotIn => InR, Minus => MinusR, StoreItem => StoreItemR, LoadAttr => GetAttrR, StoreAttr => SetAttrR,
            CallStr | CallRepr => code,
            Add | Sub | Mul | Div | Mod | FloorDiv | InPlaceAdd | InPlaceSub | Eq | NotEq | Lt | LtEq | Gt | GtEq => binary_form(code)?,
            _ => return None,
        };
        let n = match form { MinusR | GetAttrR | CallStr | CallRepr => 1, StoreItemR => 3, _ => 2 };
        if !self.facts.dunders { return None; }
        let callee = (self.sections)(Want::Dunder(i as u32, form))?;
        let (reflected, arg, check) = (callee.reflected, callee.arg(), callee.check);
        // A `!=` laid on its class's `__eq__` inverts the result, which its stub, the op, gives as it is.
        let inverted = form == NotEqR && callee.check == 1;
        // A `__getattr__` takes the name past the instance.
        let k = self.want_callee(u16::MAX, callee, n + arg.is_some() as usize)?;
        // A property's result is its own, NotImplemented as any other value, and a string's never NotImplemented.
        if !matches!(form, GetAttrR | SetAttrR | CallStr | CallRepr) && !self.declining.contains(&k) { self.declining.push(k); }
        // The op as its stub runs it, a negation's operand, a store's object, key and value, a test's item and container.
        let op = match form {
            MinusR | CallStr | CallRepr => { let v = self.pop(); Ins { op: form, x: 0, a: 0, b: v.r, c: v.r } }
            StoreItemR => { let (k, v) = self.pop2(); let o = self.pop(); Ins { op: form, x: 0, a: o.r, b: k.r, c: v.r } }
            GetAttrR => { let o = self.pop(); Ins { op: form, x: 0, a: 0, b: o.r, c: operand } }
            SetAttrR => { let (o, v) = self.pop2(); Ins { op: form, x: 0, a: o.r, b: v.r, c: operand } }
            _ => { let (a, b) = self.pop2(); Ins { op: form, x: operand as u8 | if inverted { INVERTED } else { 0 }, a: 0, b: a.r, c: b.r } }
        };
        // A reflected first dunder is laid for a number on the left, whose check a right subclass fails, as a dunder only noted may have met either.
        let first = if reflected { 1 } else { 0 };
        let t = self.temp();
        // The other dunders the op met are each laid beside the first and checked in turn in its stub, a reflected one three ways.
        let binary = binary_form(code).is_some() && !matches!(form, InR);
        let same = binary && super::opcodes::dunder::op_names(form).is_some_and(|(l, r)| l == r);
        let mut alts: Vec<(usize, u16)> = Vec::new();
        // A `!=`'s dunders invert what they give each its own way, an `__eq__`'s inverted and a `__ne__`'s as it is.
        let mut inverts: Vec<usize> = if inverted { alloc::vec![k] } else { Vec::new() };
        // A comparison's own dunder reflects to itself, laid again for a number on the left and after the other's declined.
        if same { if first != 1 { alts.push((k, 1)); } alts.push((k, 6)); }
        for j in (0..ALTS).take_while(|_| form != StoreItemR && arg.is_none()) {
            let Some(c) = (self.sections)(Want::DunderAlt(i as u32, j, form)).filter(|c| c.arg().is_none()) else { break };
            let refl = c.reflected;
            let inverts_it = form == NotEqR && c.check == 1;
            let Some(s) = self.want_callee(u16::MAX, c, n) else { continue };
            // The first, laid for a number on the left, lays its other ways too.
            if alts.iter().any(|d| d.0 == s) || s == k && !(same || refl && binary && reflected) { continue; }
            if inverts_it && !inverts.contains(&s) { inverts.push(s); }
            if s == k { if !same { alts.extend([(s, 8 - first), (s, 6)]); } }
            // A comparison's own dunder of another class is the left operand's, or the right one's past a number or once the left one's declined.
            else if !refl && same { alts.extend([(s, 0), (s, 1), (s, 6)]); }
            else if !refl { alts.push((s, 0)); } else if binary { alts.extend([(s, 1), (s, 7), (s, 6)]); } else { alts.push((s, 1)); }
            if !matches!(form, GetAttrR | SetAttrR | CallStr | CallRepr) && !self.declining.contains(&s) { self.declining.push(s); }
        }
        let rest = Ins { op: ArgsR, x: 0, a: if form == StoreItemR { op.c } else { 0 }, b: 0, c: 0 };
        self.head_arg = arg;
        let main = call_of(op, first);
        self.head(if arg.is_some() { Ins { a: 5, ..main } } else { main }, rest, t, k, true, &[]);
        // The head lies past the moves of its parameters beyond the second.
        let at = self.out.len() - HEADER as usize;
        self.dunders.push((at, op));
        self.dunder_alts.extend(alts.into_iter().map(|(s, mode)| (at, s, mode, inverts.contains(&s))));
        match code {
            // A store keeps nothing, a test takes the truth of what `__contains__` gave, negated for `not in`.
            StoreItem | StoreAttr => {}
            In => { let u = self.temp(); self.emit(Ins { op: NotR, x: 0, a: u, b: t, c: 0 }); self.produce(Ins { op: NotR, x: 0, a: t, b: u, c: 0 }); }
            NotIn => self.produce(Ins { op: NotR, x: 0, a: t, b: t, c: 0 }),
            _ if inverted => self.produce(Ins { op: NotR, x: 0, a: t, b: t, c: 0 }),
            // `str` or `repr` checks the dunder gave a string.
            CallStr | CallRepr => { self.emit(Ins { op: DunderCheckR, x: check, a: t, b: op.b, c: 0 }); self.pending.push(Entry::reg(t)); }
            _ => { self.pending.push(Entry::reg(t)); self.dest = Some(self.out.len() - 3); }
        }
        Some(1)
    }

    /* A `for` step at `i` over a user iterator whose `__next__` was found, laid as a call of its section into `var`, its stub stepping to `exit`. */
    fn next_call(&mut self, i: usize, exit: u16, (global, var): (u8, u16)) -> bool {
        if !self.facts.dunders { return false; }
        let Some(callee) = (self.sections)(Want::Dunder(i as u32, OpCode::ForIterR)) else { return false };
        let Some(k) = self.want_callee(u16::MAX, callee, 1) else { return false };
        // A module binding takes the item from a register the step writes.
        let t = if global == 0 { var } else { self.temp() };
        let op = Ins { op: OpCode::ForIterR, x: 0, a: exit, b: t, c: 0 };
        self.head(Ins { op: OpCode::ForIterR, x: 1, a: 4, b: 0, c: 0 }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, t, k, true, &[]);
        self.dunders.push((self.out.len() - HEADER as usize, op));
        if global != 0 { self.emit(Ins { op: OpCode::StoreGlobalR, x: 0, a: var, b: t, c: 0 }); }
        true
    }

    /* A length, truth test or `for` at `i` of the instance in `v` whose found dunder is laid as a section call then checked, its stub the op, else None. */
    fn checked_call(&mut self, i: usize, form: OpCode, v: Entry) -> Option<u16> {
        if !self.facts.regs || !self.facts.dunders { return None; }
        let callee = (self.sections)(Want::Dunder(i as u32, form))?;
        let kind = if form == OpCode::LenR { 1 } else { callee.check };
        // `bool` takes a `__len__`'s length as a truth test does.
        let kind = if form == OpCode::CallBool && kind == 3 { 3 } else { kind };
        if kind == 0 && form != OpCode::GetIter { return None; }
        let k = self.want_callee(u16::MAX, callee, 1)?;
        // The temporary is not the operand's, which the check reads.
        let held = self.pending.contains(&v);
        if !held { self.pending.push(v); }
        let t = self.temp();
        if !held { self.pending.pop(); }
        // A length checks `len` is its own, `bool` it is, a truth test nothing.
        let call = Ins { op: form, x: 1, a: if matches!(form, OpCode::LenR | OpCode::CallBool) { 3 } else { 2 }, b: v.r, c: v.r };
        self.head(call, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, t, k, true, &[]);
        self.dunders.push((self.out.len() - HEADER as usize, Ins { op: form, x: 0, a: 0, b: v.r, c: v.r }));
        if kind != 0 { self.emit(Ins { op: OpCode::DunderCheckR, x: kind, a: t, b: v.r, c: 0 }); }
        Some(t)
    }

    /* Emits an instruction writing temporary `ins.a` and leaves that temporary pending. */
    fn produce(&mut self, ins: Ins) {
        if self.pending.len() >= MAX_PENDING { self.flush(); }
        self.emit(ins);
        self.last = Some(self.out.len() - 1);
        self.pending.push(Entry::reg(ins.a));
    }

    /* The lowest temporary no pending value holds. */
    fn temp(&mut self) -> u16 {
        let mut t = self.tbase;
        while self.pending.iter().any(|e| !e.name && e.r as usize == t) { t += 1; }
        self.temps = self.temps.max(t + 1 - self.tbase);
        t as u16
    }

    fn push(&mut self, e: Entry) {
        if self.pending.len() >= MAX_PENDING { self.flush(); }
        self.pending.push(e);
    }

    /* Flushes before an opcode that reads only the stack, so it resumes from itself. */
    fn settle(&mut self) {
        self.flush();
        self.quiet = self.cur;
    }

    /* Copies the pending values under the top `keep` to the stack, deepest first, so a call that suspends resumes at itself. */
    fn flush_under(&mut self, keep: usize) {
        let under = self.pending.len() - keep;
        if under == 0 { return; }
        self.under = self.pending.drain(..under).collect();
        let below = core::mem::take(&mut self.under);
        self.push_regs(&below);
        self.under = below;
    }

    /* Puts every pending value on the stack, deepest first. */
    fn flush(&mut self) {
        if self.pending.is_empty() { return; }
        let mut pending = core::mem::take(&mut self.pending);
        self.push_regs(&pending);
        // The buffer comes back, so lowering allocates it once.
        pending.clear();
        self.pending = pending;
    }

    fn push_regs(&mut self, regs: &[Entry]) {
        for group in regs.chunks(3) {
            let r = |k: usize| group.get(k).map_or(0, |e| e.r);
            self.emit(Ins { op: OpCode::PushRegs, x: group.len() as u8, a: r(0), b: r(1), c: r(2) });
        }
        if !regs.is_empty() { self.last = None; }
    }

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

    /* Takes the just emitted call's result off the stack at the next instruction, or now into a temporary when that one is a jump target. */
    fn take(&mut self, join: bool) { self.take_at(join, 1) }

    /* `take` for a call `next` compiler instructions long. */
    fn take_at(&mut self, join: bool, next: u32) {
        if !join { self.takes = true; return; }
        self.under.clear();
        let t = self.temp();
        self.produce(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
        // A run stopped at the store carries on at the join with the result on the stack, as it lies there.
        if let Some(b) = self.back.last_mut() { *b = self.cur + next; }
    }

    /* `for v in g(...)` of a module binding's generator laid as the section the loop steps, the next ForIter's head entering it, else None. */
    fn gen_for(&mut self, i: usize, op: u16) -> Option<usize> {
        let n = op as usize;
        if n > 5 || self.chunk.is_generator || self.pending.len() != n + 1 { return None; }
        let at = |k: usize| self.src.get(i + k).copied();
        let (get, step, store) = (at(1)?, at(2)?, at(3)?);
        if get.opcode != OpCode::GetIter || get.operand == crate::parser::ITER_VALUE || step.opcode != OpCode::ForIter || store.opcode != OpCode::StoreName
            || self.target(i + 1) || self.target(i + 3) || store.operand as usize >= self.names { return None; }
        // The loop binds a local, or a module binding no builtin's name redirects.
        let global = match self.kind(store.operand) {
            Some(Kind::Local) => None,
            Some(Kind::Global(b)) if self.builtin_flag(store.operand) == 0 => Some(b as u16),
            _ => return None,
        };
        let regs: Vec<u16> = self.pending.iter().map(|e| e.r).collect();
        // A generator a module binding holds, or the one the call through a register met.
        let g = Self::global_in(&self.out, regs[0], self.out.len());
        let callee = match g { Some(g) => (self.sections)(Want::Gen(self.facts.module, g))?, None => (self.sections)(Want::GenAt(i as u32))? };
        let body = callee.body.clone();
        // The positionals past `*args` go to its tuple, made here.
        let p = callee.star.unwrap_or(callee.arity);
        if !super::block::gen_pure(&body.ins) || n < callee.required || callee.star.is_some() && n < p || callee.star.is_none() && n > p || callee.dstar.is_some() { return None; }
        let tuple = callee.star.map(|_| self.temp());
        let mut list: Vec<u16> = regs[1..].to_vec();
        // The binding is checked as the sections' are only for a generator of this module.
        let bind = g.filter(|_| callee.module == self.facts.module).unwrap_or(u16::MAX);
        let k = self.want_callee(bind, callee, list.len().min(p) + tuple.is_some() as usize)?;
        self.bind_args(k, &body, &list[..p.min(n)], None, 0);
        if let Some(t) = tuple {
            let rest: Vec<Entry> = list[p..].iter().map(|&r| Entry::reg(r)).collect();
            self.push_regs(&rest);
            self.emit(Ins { op: OpCode::BuildTuple, x: 0, a: (n - p) as u16, b: 0, c: 0 });
            self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
            list.truncate(p);
            list.push(t);
        }
        self.pending.clear();
        self.settle();
        // A body that yields at one place only comes back past it each step, which its register holds from the start.
        if body.ins.iter().filter(|i| i.op == OpCode::Yield).count() == 1 {
            let at = self.out.len();
            self.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: 0, c: 0 });
            self.prep.extend([(at, 0, Prep::GenReg(k)), (at, 1, Prep::GenResume(k))]);
        }
        // The loop is entered at its head, past its step.
        let skip = self.out.len();
        self.emit(Ins { op: OpCode::Jump, x: 1, a: 0, b: 0, c: 0 });
        self.gen_next = Some(GenNext { k, skip, list, regs, target: if global.is_some() { 0 } else { store.operand }, global, exit: step.operand as usize });
        Some(2)
    }

    /* What section `k` laid from `body` may assume of the parameters `args` fill, a function passed runs its section, a generator its own, `depth` deep. */
    fn bind_args(&mut self, k: usize, body: &Code, args: &[u16], ctx: Option<&Code>, depth: usize) {
        let out = core::mem::take(&mut self.out);
        let code: &[Ins] = ctx.map_or(&out, |c| &c.ins);
        self.bind_in(k, body, args, code, ctx.is_none(), depth);
        self.out = out;
    }

    /* `bind_args` with the registers of `args` in `code`, this code's own when `own`. */
    fn bind_in(&mut self, k: usize, body: &Code, args: &[u16], code: &[Ins], own: bool, depth: usize) {
        let writes = |w: &Ins, r: u16| matches!(super::block::role(w), Some((true, _, _))) && w.a == r || w.op == OpCode::ForIterR && w.b == r
            || w.op == OpCode::UnpackR || w.op == OpCode::StoreTopR && w.x == 0 && w.a == r;
        let param = |r: u16| (r as usize) < args.len() && !body.ins.iter().any(|w| writes(w, r));
        for (j, c) in body.ins.iter().enumerate() {
            // A function the call passes for a parameter the body calls and never writes is laid as a section of the loop's code.
            if c.op == OpCode::CallR && !wide(*c) && param(c.a) {
                let arg = Ins { op: OpCode::CallR, x: 0, a: args[c.a as usize], b: 0, c: 0 };
                let s = match Self::global_in(code, arg.a, code.len()) {
                    Some(f) => self.want(f, c.x as usize),
                    // A function the code made just before, a lambda most often, runs the section of its body.
                    None if own => self.made_at(code, arg.a).and_then(|body| (self.sections)(Want::Body(body))).and_then(|f| self.want_callee(u16::MAX, f, c.x as usize)),
                    None => None,
                };
                if let Some(s) = s { self.gen_args.push((k, j, s)); }
            }
            // A loop over a parameter the call passes a generator call's result steps that generator's section.
            if depth < 4 && param(c.a) && Self::loop_step(&body.ins, j).is_some()
                && let Some((callee, inner)) = self.gen_call_at(code, code.len(), args[c.a as usize], own)
                && let body2 = callee.body.clone()
                && callee.module == self.facts.module && super::block::gen_pure(&body2.ins) && callee.star.is_none() && inner.len() <= callee.arity && inner.len() >= callee.required
                && let Some(k2) = self.want_callee(u16::MAX, callee, inner.len()) {
                self.nested.push((k, j, k2));
                self.bind_in(k2, &body2, &inner, code, own, depth + 1);
            }
        }
    }

    /* The module binding register `r` holds before `at`, read by a LoadGlobalR or popped back from where a deep call left it, unwritten since. */
    fn global_in(code: &[Ins], r: u16, at: usize) -> Option<u16> {
        let writes = |i: &Ins| super::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r) || i.op == OpCode::StoreTopR && i.x == 0 && i.a == r;
        let mut end = at;
        for _ in 0..4 {
            let w = code[..end].iter().rposition(writes)?;
            match code[w] {
                Ins { op: OpCode::LoadGlobalR, b, .. } => return Some(b),
                Ins { op: OpCode::StoreTopR, .. } => {
                    let pushes = |i: &Ins| i.op == OpCode::PushRegs && [i.a, i.b, i.c][..i.x.min(3) as usize].contains(&r);
                    let p = code[..w].iter().rposition(pushes)?;
                    if code[p + 1..w].iter().any(writes) { return None; }
                    end = p;
                }
                _ => return None,
            }
        }
        None
    }

    /* For a loop at `j` of `ins` pushing a register and taking its iterator, the ForIterR that steps it, past a string append's OwnResetR. */
    fn loop_step(ins: &[Ins], j: usize) -> Option<usize> {
        let (push, get) = (ins.get(j)?, ins.get(j + 1)?);
        if push.op != OpCode::PushRegs || push.x != 1 || get.op != OpCode::GetIter || get.a != 0 || get.x != 0 { return None; }
        let f = if ins.get(j + 2)?.op == OpCode::OwnResetR { j + 3 } else { j + 2 };
        // A loop whose target unpacks steps its item onto the stack, the UnpackR after taking it off.
        let unpacks = |i: &Ins| i.op == OpCode::ForIter && i.x == 0 && ins.get(f + 1).is_some_and(|u| u.op == OpCode::UnpackR);
        ins.get(f).filter(|i| i.op == OpCode::ForIterR && i.x == 0 || unpacks(i)).map(|_| f)
    }

    /* The generator a call of this code made into register `r`, which nothing writes since, and the registers it was passed. */
    fn gen_call_at(&self, code: &[Ins], at: usize, r: u16, own: bool) -> Option<(Callee<'p>, Vec<u16>)> {
        let k = code[..at].iter().rposition(|i| super::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r) || i.op == OpCode::StoreTopR && i.x == 0 && i.a == r)?;
        if code[k].op != OpCode::StoreTopR { return None; }
        let call = *code.get(k.checked_sub(1)?)?;
        if !matches!(call.op, OpCode::CallR | OpCode::CallGlobalR) || wide(call) { return None; }
        let callee = match if call.op == OpCode::CallGlobalR { Some(call.a) } else { Self::global_in(code, call.a, k - 1) } {
            Some(g) => (self.sections)(Want::Gen(self.facts.module, g))?,
            // A generator the code made just before, a generator expression most often.
            None if own => (self.sections)(Want::GenBody(self.made_at(code, call.a)?))?,
            None => return None,
        };
        let regs = [call.b, call.c];
        Some((callee, regs[..call.x as usize].to_vec()))
    }

    /* The body of the function register `r` holds when the code last wrote it from a MakeFunction, which nothing writes since. */
    fn made_at(&self, code: &[Ins], r: u16) -> Option<*const SSAChunk> {
        let k = code.iter().rposition(|i| super::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r) || i.op == OpCode::StoreTopR && i.x == 0 && i.a == r)?;
        let make = code.get(k.checked_sub(1)?).filter(|m| code[k].op == OpCode::StoreTopR && m.op == OpCode::MakeFunction)?;
        self.chunk.functions.get(make.a as usize).map(|f| &f.1 as *const SSAChunk)
    }

    /* The section a call of binding `g` with `n` arguments jumps to, wanted with all its body calls, None when the function cannot be laid. */
    /* A call with keywords of a layable function, its keywords matched to parameters as a section call with defaults filled, else None. */
    fn kw_call(&mut self, i: usize, op: u16) -> Option<usize> {
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let total = np + 2 * nk;
        if np + nk > 5 || self.pending.len() <= total || self.pending.len() > total + 1 + MAX_UNDER || i + 1 >= self.src.len() { return None; }
        let under = self.pending.len() - total - 1;
        let f = self.pending[under].r;
        let g = super::block::global_of(&self.out, self.out.len(), Ins { op: OpCode::CallR, x: 0, a: f, b: 0, c: 0 })?;
        let callee = (self.sections)(Want::Global(self.facts.module, g))?;
        let regs: Vec<u16> = self.pending[under..].iter().map(|e| e.r).collect();
        let (m, slots, first) = Self::kw_slots(&regs, op, &callee, (&self.chunk.constants, self.cbase))?;
        self.keyed = true;
        let section = self.want_callee(g, callee, m)?;
        let join = self.target(i + 1);
        self.pending.truncate(under);
        let t = self.temp();
        let below = self.pending.clone();
        self.kw_lay(regs, op, m, &slots, first, t, section, &below);
        self.pending.push(Entry::reg(t));
        self.dest = Some(self.out.len() - 3);
        if !join { self.resume = Some(self.out.len() - 3); }
        Some(1)
    }

    /* A call of a layable function taking `*args` or `**kwargs`, the rest packed into its tuple and dict as a section call, else None. */
    fn spread_call(&mut self, i: usize, op: u16) -> Option<usize> {
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let total = np + 2 * nk;
        if self.pending.len() <= total || self.pending.len() > total + 1 + MAX_UNDER || i + 1 >= self.src.len() { return None; }
        let under = self.pending.len() - total - 1;
        let f = self.pending[under].r;
        // The function a module binding holds, or one the call found before, wherever it reads it from.
        let g = super::block::global_of(&self.out, self.out.len(), Ins { op: OpCode::CallR, x: 0, a: f, b: 0, c: 0 });
        let callee = match g { Some(g) => (self.sections)(Want::Spread(self.facts.module, g))?, None => (self.sections)(Want::Value(i as u32))? };
        let g = g.unwrap_or(u16::MAX);
        let regs: Vec<u16> = self.pending[under..].iter().map(|e| e.r).collect();
        let packed = Self::packed(&regs, op, &callee, (&self.chunk.constants, self.cbase))?;
        let section = self.want_callee(g, callee.clone(), callee.arity)?;
        let join = self.target(i + 1);
        // The tuple's and the dict's registers are none the call reads.
        let tuple = callee.star.map(|_| self.temp());
        let held = self.pending.len();
        self.pending.push(Entry::reg(tuple.unwrap_or(0)));
        let dict = callee.dstar.map(|_| self.temp());
        self.pending.truncate(held);
        self.pending.truncate(under);
        let (list, gaps) = self.pack(&regs, &packed, tuple.zip(callee.star), dict.zip(callee.dstar));
        self.gaps = gaps;
        let r = |k: usize| list.get(k).copied().unwrap_or(0);
        let call = Ins { op: OpCode::CallR, x: list.len() as u8, a: f, b: r(0), c: r(1) };
        let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) };
        let t = self.temp();
        let below = self.pending.clone();
        self.kwcall = Some((regs, op));
        self.head(call, rest, t, section, true, &below);
        self.pending.push(Entry::reg(t));
        self.dest = Some(self.out.len() - 3);
        if !join { self.resume = Some(self.out.len() - 3); }
        Some(1)
    }

    /* `f(*a)` after the BeginArgs at `i`, its callee pending and a sequence or list or tuple of them loaded, as a spreading register call, else None. */
    fn spread(&mut self, i: usize) -> Option<usize> {
        use OpCode::*;
        let src = self.src;
        let simple = |k: usize| src.get(k).is_some_and(|s| matches!(s.opcode, LoadName | LoadConst | LoadGlobal | LoadNone | LoadTrue | LoadFalse | BuildList | BuildTuple | BuildDict)) && !self.target(k);
        let mut end = i + 1;
        while simple(end) { end += 1; }
        let at = |k: usize| src.get(k).filter(|_| !self.target(k)).copied();
        let (unpack, call) = (at(end)?, at(end + 1)?);
        let held = self.pending.len();
        // 0 for the items of a sequence, 2 for the pairs of a mapping.
        let kind = if unpack.operand == 2 { 2 } else { 0 };
        if end == i + 1 || unpack.opcode != UnpackArgs || !matches!(unpack.operand, 1 | 2) || call.opcode != CallSpread || call.operand != 1 || held == 0 || held > 1 + MAX_UNDER
            || end + 2 >= src.len() || held + end - i > MAX_PENDING { return None; }
        // The sequence must come out as one value over the callee, each load one more and each build taking what it holds.
        let mut depth = 0usize;
        for s in &src[i + 1..end] {
            depth = match s.opcode { BuildList | BuildTuple => (depth + 1).checked_sub(s.operand as usize)?, BuildDict => (depth + 1).checked_sub(2 * s.operand as usize)?, _ => depth + 1 };
        }
        if depth != 1 { return None; }
        // The binding the callee was read from, found before the sequence's loads and builds.
        let f = self.pending[held - 1];
        let g = super::block::global_of(&self.out, self.out.len(), Ins { op: OpCode::CallR, x: 0, a: f.r, b: 0, c: 0 });
        for (k, &s) in src.iter().enumerate().take(end).skip(i + 1) {
            self.cur = k as u32;
            match s.opcode {
                BuildList | BuildTuple | BuildDict => {
                    let n = s.operand as usize * if s.opcode == BuildDict { 2 } else { 1 };
                    let items: Vec<Entry> = self.pending.drain(self.pending.len() - n..).collect();
                    self.push_regs(&items);
                    self.emit_orig(s);
                    let t = self.temp();
                    self.emit(Ins { op: StoreTopR, x: 0, a: t, b: 0, c: 0 });
                    self.pending.push(Entry::reg(t));
                }
                _ => { self.one(k); }
            }
        }
        if self.pending.len() != held + 1 { return None; }
        let under = self.pending.len() - 2;
        let seq = self.pending[under + 1];
        // The CallSpread, where an error in the call points.
        let (c, steps) = (end + 1, end + 2 - i);
        let join = self.target(c + 1);
        // A function a module binding holds, or one the call found before, is laid as a section the items go to.
        let callee = match g { Some(g) => (self.sections)(Want::Global(self.facts.module, g)), None => (self.sections)(Want::Value(c as u32)) };
        self.cur = c as u32;
        if let Some(callee) = callee.filter(|c| spreads(c, kind)) && let Some(section) = self.want_callee(g.unwrap_or(u16::MAX), callee.clone(), callee.arity) {
            self.pending.truncate(under);
            let t = self.temp();
            let below = self.pending.clone();
            self.head(Ins { op: OpCode::CallSpreadR, x: kind, a: f.r, b: seq.r, c: 0 }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, t, section, true, &below);
            self.pending.push(Entry::reg(t));
            self.dest = Some(self.out.len() - 3);
            if !join { self.resume = Some(self.out.len() - 3); }
            return Some(steps);
        }
        self.flush_under(2);
        self.pending.clear();
        self.settle();
        self.emit(Ins { op: OpCode::CallSpreadR, x: kind, a: f.r, b: seq.r, c: 0 });
        self.take_at(join, 1);
        Some(steps)
    }

    /* A call of more than five pending positionals as a section call moving the arguments past the second into parameters, else None. */
    fn many_call(&mut self, i: usize, n: usize) -> Option<usize> {
        if self.pending.len() <= n || self.pending.len() > n + 1 + MAX_UNDER || i + 1 >= self.src.len() { return None; }
        let under = self.pending.len() - n - 1;
        let f = self.pending[under];
        let g = super::block::global_of(&self.out, self.out.len(), Ins { op: OpCode::CallR, x: 0, a: f.r, b: 0, c: 0 });
        let callee = match g { Some(g) => (self.sections)(Want::Global(self.facts.module, g))?, None => (self.sections)(Want::Value(i as u32))? };
        if callee.star.is_some() || callee.dstar.is_some() || callee.ctor().is_some() || callee.on_class != 0 { return None; }
        self.wide = true;
        let section = self.want_callee(g.unwrap_or(u16::MAX), callee, n)?;
        let regs: Vec<u16> = self.pending[under..].iter().map(|e| e.r).collect();
        let a = |k: usize| regs.get(1 + k).copied().unwrap_or(0);
        let call = Ins { op: OpCode::CallR, x: n as u8, a: f.r, b: a(0), c: a(1) };
        let rest = Ins { op: OpCode::ArgsR, x: 0, a: a(2), b: a(3), c: a(4) };
        let join = self.target(i + 1);
        self.pending.truncate(under);
        let t = self.temp();
        let below = self.pending.clone();
        self.more_args = regs[6..].to_vec();
        // The stub calls with the stack as the call was written.
        self.kwcall = Some((regs, n as u16));
        self.head(call, rest, t, section, true, &below);
        self.pending.push(Entry::reg(t));
        self.dest = Some(self.out.len() - 3);
        if !join { self.resume = Some(self.out.len() - 3); }
        Some(1)
    }

    fn want(&mut self, g: u16, n: usize) -> Option<usize> { self.want_in(self.facts.module, g, n) }

    /* Wants the other functions the register call at `at` of function `fi` or this code met, checked after section `k`, when each takes `n` plainly. */
    fn want_alts(&mut self, fi: Option<usize>, at: u32, k: usize, n: usize) {
        let fits = |c: &Callee| !guarded(c) && c.required <= n && n <= c.arity && c.positional == c.arity && c.ctor().is_none() && !c.twin && c.via_super().is_none() && c.star.is_none() && c.dstar.is_none();
        let plain = |c: &Callee| fits(c) && !c.by_body;
        // The head's own section may be one any function of its body runs, whose check gives way to the lookup, and a growth adds the function met.
        let (grows, only) = (self.old.is_some(), self.only.take());
        // A growth's head may be one laid for any function of a closure's body, whose stub looks the others up.
        let head_fits = |c: &Callee| fits(c) || grows && fits(&Callee { twin: false, ..c.clone() });
        if !head_fits(&self.wanted[k].1) || !grows && self.alts.iter().any(|a| a.0 == k || a.1 == k) { return; }
        for j in 0..ALTS {
            let Some(mut c) = (match only { Some(f) if j == 0 => (self.sections)(Want::Func(f, false)), Some(_) => None, None => (self.sections)(Want::Alt(fi, at, j)) }) else { break };
            // A growth meeting another function of a body laid already lays one section any function of the body runs.
            if grows && (self.wanted.iter().any(|w| w.1.fi == c.fi && (w.1.value.0 != c.value.0 || w.1.by_body))) { (c.by_body, c.twin) = (true, false); }
            if !(plain(&c) || grows && c.by_body && fits(&c)) { continue; }
            // A call of more than five arguments names those past the fifth apart, as its head does.
            self.wide = n > 5;
            let Some(a) = self.want_callee(u16::MAX, c, n) else { continue };
            let free = if grows { !self.alts.iter().any(|x| x.0 == k && x.1 == a) } else { !self.alts.iter().any(|x| x.0 == a || x.1 == a) };
            if a != k && free && (plain(&self.wanted[a].1) || grows && self.wanted[a].1.by_body) { self.alts.push((k, a)); }
        }
    }

    /* Wants the other methods the method call at `at` of function `fi` or this code met, looked up by section `k`'s stub, when each takes `n` plainly. */
    fn want_method_alts(&mut self, fi: Option<usize>, at: u32, k: usize, n: usize) {
        let on = self.wanted[k].1.on_class;
        // A method takes its receiver past the arguments, a receiver's function the arguments alone, and either may lie in the other's head table.
        let takes = |c: &Callee| n - (c.on_class == 4) as usize;
        let kinds = |c: &Callee| c.on_class == on && matches!(on, 0 | 3) || matches!((on, c.on_class), (0, 4) | (4, 0) | (4, 4)) && c.arity == takes(c);
        let plain = |c: &Callee| !guarded(c) && c.required <= takes(c) && takes(c) <= c.arity && c.positional == c.arity && kinds(c) && c.ctor().is_none() && !c.by_body && !c.twin && c.via_super().is_none() && c.star.is_none() && c.dstar.is_none();
        let (grows, only) = (self.old.is_some(), self.only.take());
        if !plain(&self.wanted[k].1) || !grows && self.alts.iter().any(|a| a.0 == k || a.1 == k) { return; }
        // The methods the call met and, past a method head or a held one, the functions receivers held.
        let held = matches!(on, 0 | 4) && at & CALLED == 0;
        for at in core::iter::once(at).chain(held.then_some(at | FIELD)) {
            for j in 0..ALTS {
                let Some(c) = (match only { Some(f) if j == 0 && at & FIELD == 0 => (self.sections)(Want::Func(f, true)), Some(_) => None, None => (self.sections)(Want::MethodAlt(fi, at, j)) }) else { break };
                if !plain(&c) { continue; }
                let m = takes(&c);
                let Some(a) = self.want_callee(u16::MAX, c, m) else { continue };
                let free = if grows { !self.alts.iter().any(|x| x.0 == k && x.1 == a) } else { !self.alts.iter().any(|x| x.0 == a || x.1 == a || x.0 == k && x.1 == a) };
                if a != k && free && plain(&self.wanted[a].1) { self.alts.push((k, a)); }
            }
        }
    }

    /* `want` of binding `g` of module `m`, which a body laid from that module calls. */
    fn want_in(&mut self, m: usize, g: u16, n: usize) -> Option<usize> {
        // A class's call passes its instance before the arguments, and a head reads five arguments only from a call that names the rest apart.
        if let Some(k) = self.wanted_at(m, g) { let c = &self.wanted[k].1; let n = n + c.ctor().is_some() as usize; return ((n <= 5 || self.wide) && n <= c.positional && n >= c.required && c.star.is_none() || packs(c, n).is_some()).then_some(k); }
        let callee = (self.sections)(Want::Global(m, g))?;
        // A call of a function taking `*args` packs into its tuple what its positionals leave, one taking `**kwargs` laid only by `spread_call`.
        if packs(&callee, n).is_some() { let arity = callee.arity; return self.want_callee(g, callee, arity); }
        if callee.star.is_some() || callee.dstar.is_some() { return None; }
        let n = n + callee.ctor().is_some() as usize;
        self.want_callee(g, callee, n)
    }

    /* The section wanted for binding `g` of module `m`. */
    fn wanted_at(&self, m: usize, g: u16) -> Option<usize> { self.wanted.iter().position(|w| w.0 == g && w.1.bound_module() == m && !w.1.generator) }

    /* `want` of the function `callee`, `g` the binding it is called through or `u16::MAX` for a method. */
    fn want_callee(&mut self, g: u16, callee: Callee<'p>, n: usize) -> Option<usize> {
        // A generator's section is laid for its one loop, and a keyword call may fill parameters past a lone `*`, a positional one only those before.
        let keyed = core::mem::take(&mut self.keyed);
        // A head reads five arguments from the call, a class's instance or a method's receiver among them, more only from a call naming them apart.
        let named = core::mem::take(&mut self.wide);
        if n > 5 && !named { return None; }
        let most = |c: &Callee| if keyed || c.star.is_some() || c.dstar.is_some() { c.arity } else { c.positional };
        if let Some(k) = self.wanted.iter().position(|w| !w.1.generator && !callee.generator && (w.1.value.0 == callee.value.0 && !callee.value.is_undef() || w.1.by_body && callee.by_body && w.1.fi == callee.fi) && same_laid(&w.1, &callee)) { let c = &self.wanted[k].1; return (n <= most(c) && n >= c.required).then_some(k); }
        // A call that runs once lays none, but for a generator's loop or a recursion, whose body runs again and again.
        if self.once && !callee.generator && !recurs(&callee.body.ins, g) { return None; }
        if n > most(&callee) || n < callee.required || callee.arity > MAX_ARGS || !callee.value.is_undef() && Val::heap((callee.value.0 >> 4) as u32).0 != callee.value.0 { return None; }
        let body = callee.body.clone();
        // What it calls runs as often as it does, a generator's body at each step.
        self.once = false;
        // Wanted before what it calls, so a call back into it, a recursion, finds it.
        let k = self.wanted.len();
        let twin = callee.twin.then(|| Callee { by_body: true, twin: false, ..callee.clone() });
        // The few a lowering most often wants take the room they need, a callee being large.
        if self.wanted.len() == self.wanted.capacity() { self.wanted.reserve_exact(self.wanted.len().max(1)); }
        self.wanted.push((g, callee));
        if let Some(twin) = twin { self.wanted.push((g, twin)); }
        {
            let ins = &body.ins;
            // The instructions of the body each look below takes, found in one pass.
            let marked: Vec<(OpCode, usize)> = ins.iter().enumerate().filter(|(_, i)| dunder_op(i.op) || matches!(i.op, OpCode::CallGlobalR | OpCode::CallR | OpCode::PushRegs
                | OpCode::CallMethodR | OpCode::JumpIfFalseR | OpCode::CallSpreadR | OpCode::MakeFunction | OpCode::MakeCoroutine | OpCode::StoreName | OpCode::Call)).map(|(j, i)| (i.op, j)).collect();
            let of = |op: OpCode| marked.iter().filter(move |m| m.0 == op).map(|m| m.1);
            // A call of a function that may be laid jumps to its section, any other calls as it would outside them.
            let mut calls: Vec<(u16, usize, bool)> = marked.iter().filter(|m| matches!(m.0, OpCode::CallGlobalR | OpCode::CallR))
                .filter_map(|&(_, j)| Some((super::block::global_of(ins, j, ins[j])?, ins[j].x as usize, false))).collect();
            // So is the function a stack call of positionals calls through the binding it was read from.
            calls.extend(of(OpCode::Call).filter(|&j| ins[j].a as usize <= MAX_ARGS).filter_map(|j| {
                let (p, r) = super::block::stack_callee(ins, j, ins[j].a as usize)?;
                Some((super::block::global_of(ins, p, Ins { op: OpCode::CallR, x: 0, a: r, b: 0, c: 0 })?, ins[j].a as usize, ins[j].a > 5))
            }));
            // So is the function a stack call of more than five arguments calls through its binding, which names them apart.
            calls.extend(of(OpCode::PushRegs).filter_map(|j| {
                let (_, regs) = many_at(ins, j)?;
                Some((super::block::global_of(ins, j, Ins { op: OpCode::CallR, x: 0, a: regs[0], b: 0, c: 0 })?, regs.len() - 1, true))
            }));
            let module = self.wanted[k].1.module;
            for &(h, m, wide) in &calls { self.wide = wide; self.want_in(module, h, m); self.wide = false; }
            // And one with keywords, the parameters its names match.
            for j in (0..ins.len()).filter(|&j| ins[j].op == OpCode::Call && ins[j].a >> 8 != 0) {
                let Some((g, kw)) = self.stack_kw(k, j) else { continue };
                let Some(c) = (self.sections)(Want::Global(module, g)).filter(|c| c.ctor().is_none()) else { continue };
                let lits = (self.wanted[k].1.literals, self.wanted[k].1.names);
                // One of a function taking `*args` or `**kwargs` passes them all, packed.
                let m = if c.star.is_some() || c.dstar.is_some() { Self::packed(&kw, ins[j].a, &c, lits).map(|_| c.arity) } else { Self::kw_slots(&kw, ins[j].a, &c, lits).map(|s| s.0) };
                let Some(m) = m else { continue };
                self.keyed = true;
                self.want_callee(g, c, m);
            }
            // A method a call inside it found, or found when it was laid before, is laid as well.
            let fi = self.wanted[k].1.fi;
            for j in of(OpCode::CallMethodR) {
                let i = ins[j];
                if let Some(callee) = (self.sections)(Want::Nested(fi, body.orig(j), method_of(&body, j), i.a)) && let Some(m) = { let n = i.x as usize + !matches!(callee.on_class, 1 | 4) as usize; self.want_callee(u16::MAX, callee, n) } {
                    self.want_method_alts(Some(fi), body.orig(j), m, i.x as usize + 1);
                }
            }
            // So is the method a call on a `super()` proxy finds.
            if let Some(g) = self.wanted[k].1.supers {
                // A class's `__init__` names the function it was laid from in its constructor, which the proxy is made for.
                let value = self.wanted[k].1.ctor().map_or(self.wanted[k].1.value, |c| c.2);
                for i in of(OpCode::CallMethodR).filter(|&j| proxy(ins, j, ins[j], g)).map(|j| ins[j]) {
                    if let Some(callee) = (self.sections)(Want::Super(fi, i.a, value, self.wanted[k].1.class)) { self.want_callee(u16::MAX, callee, i.x as usize + 1); }
                }
            }
            // So is the operator dunder an op found.
            for j in marked.iter().filter(|m| dunder_op(m.0)).map(|m| m.1) {
                let Some(seen) = self.nested_dunder(fi, &body, j) else { continue };
                let Some(callee) = (self.sections)(Want::NestedDunder(fi, body.orig(j), seen, ins[j].op)).filter(|c| !c.reflected) else { continue };
                let first = callee.value;
                if let Some(t) = self.want_callee(u16::MAX, callee, 2) && !self.declining.contains(&t) { self.declining.push(t); }
                // And the other dunders its stub met, laid beside it.
                for n in 0..ALTS {
                    let Some(c) = (self.sections)(Want::NestedDunderAlt(fi, body.orig(j), first, n, ins[j].op)) else { break };
                    if let Some(t) = self.want_callee(u16::MAX, c, 2) && !self.declining.contains(&t) { self.declining.push(t); }
                }
            }
            // So is the getter of a property a load of an attribute found.
            for (j, _) in ins.iter().enumerate().filter(|(_, i)| i.op == OpCode::GetAttrR) {
                let Some(seen) = self.nested_getter(fi, &body, j) else { continue };
                if let Some(callee) = (self.sections)(Want::NestedDunder(fi, body.orig(j), seen, OpCode::GetAttrR)) { self.want_callee(u16::MAX, callee, 1); }
            }
            // So is the `__bool__` or `__len__` a truth test of an instance found.
            for j in of(OpCode::JumpIfFalseR) {
                let Some(seen) = self.nested_dunder(fi, &body, j) else { continue };
                let Some(callee) = (self.sections)(Want::NestedDunder(fi, body.orig(j), seen, OpCode::JumpIfFalseR)) else { continue };
                self.want_callee(u16::MAX, callee, 1);
            }
            // So is what a call through a register or of a binding holding nothing laid found, a closure, a bound method or an instance.
            let valued: Vec<(usize, Ins)> = of(OpCode::CallR).chain(of(OpCode::CallGlobalR)).map(|j| (j, ins[j])).filter(|&(j, i)| self.valued(ins, j, i, module)).collect();
            for (j, i) in valued {
                // A comprehension the body makes and calls at once runs the section of its body.
                if i.op == OpCode::CallR && let Some(callee) = made_in(ins, j, i.a).and_then(|m| (self.sections)(Want::MadeIn(fi, m))) { self.want_callee(u16::MAX, callee, i.x as usize); continue; }
                let Some(callee) = self.met(k, j, i) else { continue };
                // A function taking `*args` past the positionals the call passes takes the rest packed into its tuple.
                if packs(&callee, i.x as usize).is_some() { self.want_callee(u16::MAX, callee.clone(), callee.arity); continue; }
                if let Some(v) = { let n = i.x as usize + (callee.on_class & 3 == 3) as usize; self.want_callee(u16::MAX, callee, n) } {
                    self.want_alts(Some(fi), body.orig(j), v, i.x as usize);
                }
            }
            // So is a function a call that spreads a sequence calls, which the items fill.
            for (j, i) in of(OpCode::CallSpreadR).map(|j| (j, ins[j])) {
                let g = super::block::global_of(ins, j, Ins { op: OpCode::CallR, ..i });
                let callee = match g { Some(h) => (self.sections)(Want::Global(module, h)), None => self.celled(k, j, i.a).or_else(|| (self.sections)(Want::NestedValue(fi, body.orig(j), value_of(&body, j)))) };
                if let Some(c) = callee.filter(|c| spreads(c, i.x)) { self.want_callee(g.unwrap_or(u16::MAX), c.clone(), c.arity); }
            }
            // A function made inside reads its captures from registers, which a closure's cells laid as constants are not.
            let makes = marked.iter().any(|m| matches!(m.0, OpCode::MakeFunction | OpCode::MakeCoroutine));
            // A store by name a section holds only into a cell the body keeps.
            if of(OpCode::StoreName).any(|j| !self.wanted[k].1.cellvars.contains(&(ins[j].a as usize))) { self.wanted.truncate(k); return None; }

            if makes && !self.wanted[k].1.cells.is_empty() && !self.wanted[k].1.by_body { self.wanted.truncate(k); return None; }
            // A body with `try` lays no recursion, as a handler would meet registers another level wrote, and a loop over a called generator steps its section.
            for j in 2..ins.len() {
                let (call, store, push) = (ins[j - 2], ins[j - 1], ins[j]);
                if store.op != OpCode::StoreTopR || store.x != 0 || store.a != push.a || Self::loop_step(ins, j).is_none() { continue; }
                if module != self.facts.module { break; }
                let Some((callee, args)) = self.gen_call_at(ins, j, push.a, false) else { continue };
                let body2 = callee.body.clone();
                if callee.module != module || !super::block::gen_pure(&body2.ins) || callee.star.is_some() || args.len() > callee.arity || args.len() < callee.required { continue; }
                // A generator stepping itself, or one a section of it already steps, would lay sections without end, so its loop runs it as a frame.
                if wide(call) || self.wanted.iter().any(|w| w.1.generator && w.1.fi == callee.fi) { continue; }
                if let Some(k2) = self.want_callee(u16::MAX, callee, args.len()) {
                    self.nested.push((k, j, k2));
                    self.bind_args(k2, &body2, &args, Some(&body), 1);
                }
            }
        }
        Some(k)
    }

    /* The section a stack call of more than five arguments pushed from `j` of section `k`'s body jumps to, its Call and registers. */
    fn laid_many(&self, k: usize, j: usize) -> Option<(usize, usize, Vec<u16>)> {
        let w = &self.wanted[k].1;
        let ins = &w.body.ins;
        let (e, regs) = many_at(ins, j)?;
        let g = super::block::global_of(ins, j, Ins { op: OpCode::CallR, x: 0, a: regs[0], b: 0, c: 0 })?;
        let to = self.wanted_at(w.module, g)?;
        let c = &self.wanted[to].1;
        (c.positional >= regs.len() - 1 && c.required < regs.len() && c.star.is_none() && c.dstar.is_none() && c.ctor().is_none()).then_some((to, e, regs))
    }

    /* The section of the binding a stack call in section `k`'s body reads its function from, with how a keyword call passes it. */
    fn laid_stack(&self, k: usize, j: usize, c: Ins) -> Option<(usize, Option<Result<Passed, Packed>>)> {
        if c.op != OpCode::Call { return None; }
        let w = &self.wanted[k].1;
        let ins = &w.body.ins;
        let (g, kw) = if c.a >> 8 != 0 { self.stack_kw(k, j)? } else {
            let (p, r) = super::block::stack_callee(ins, j, c.a as usize).filter(|_| c.a as usize <= MAX_ARGS)?;
            (super::block::global_of(ins, p, Ins { op: OpCode::CallR, x: 0, a: r, b: 0, c: 0 })?, Vec::new())
        };
        let to = self.wanted_at(w.module, g)?;
        let t = &self.wanted[to].1;
        if t.ctor().is_some() || t.twin || t.by_body { return None; }
        let spreads = t.star.is_some() || t.dstar.is_some();
        if kw.is_empty() { return (!spreads && (t.required..=t.positional).contains(&(c.a as usize))).then_some((to, None)); }
        if spreads { return Some((to, Some(Err(Self::packed(&kw, c.a, t, (w.literals, w.names))?)))); }
        let (m, slots, _) = Self::kw_slots(&kw, c.a, t, (w.literals, w.names))?;
        Some((to, Some(Ok((m, slots)))))
    }

    /* The binding a keyword stack call's function was read from, and its operands as `kw_slots` takes them, a name by its register. */
    fn stack_kw(&self, k: usize, j: usize) -> Option<(u16, Vec<u16>)> {
        let ins = &self.wanted[k].1.body.ins;
        let (np, nk) = ((ins[j].a & 0xFF) as usize, (ins[j].a >> 8) as usize);
        let n = np + 2 * nk;
        let (p, r) = super::block::stack_callee(ins, j, n)?;
        let mut kw: Vec<u16> = (0..=n as u16).collect();
        for q in 0..nk { kw[1 + np + 2 * q] = super::block::stack_callee(ins, j, n - 1 - np - 2 * q)?.1; }
        Some((super::block::global_of(ins, p, Ins { op: OpCode::CallR, x: 0, a: r, b: 0, c: 0 })?, kw))
    }

    fn laid_call(&self, ins: &[Ins], j: usize, c: Ins, m: usize) -> bool {
        matches!(c.op, OpCode::CallGlobalR | OpCode::CallR)
            && super::block::global_of(ins, j, c).and_then(|g| self.wanted_at(m, g)).is_some_and(|k| (self.wanted[k].1.star.is_none() && self.wanted[k].1.dstar.is_none() || packs(&self.wanted[k].1, c.x as usize).is_some()) && (c.x < 5 || self.wanted[k].1.ctor().is_none()))
    }

    /* The section the method call `c` at `j` of the body of section `k` jumps to, the one laid for the method it found. */
    fn laid_method(&self, k: usize, j: usize, c: Ins) -> Option<usize> {
        let body = &self.wanted[k].1.body;
        if c.op != OpCode::CallMethodR { return None; }
        let callee = (self.sections)(Want::Nested(self.wanted[k].1.fi, body.orig(j), method_of(body, j), c.a))?;
        let n = c.x as usize + !matches!(callee.on_class, 1 | 4) as usize;
        self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && same_laid(&w.1, &callee) && w.1.positional >= n && w.1.required <= n)
    }

    /* The section the register call `c` at `j` of the body of section `k` jumps to, the one laid for the function it found. */
    fn laid_value(&self, k: usize, j: usize, c: Ins) -> Option<usize> {
        if let Some(g) = self.gen_args.iter().find(|g| g.0 == k && g.1 == j) { return Some(g.2); }
        let body = &self.wanted[k].1.body;
        if !self.valued(&body.ins, j, c, self.wanted[k].1.module) { return None; }
        if c.op == OpCode::CallR && let Some(callee) = made_in(&body.ins, j, c.a).and_then(|m| (self.sections)(Want::MadeIn(self.wanted[k].1.fi, m))) {
            return self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.by_body && w.1.fi == callee.fi);
        }
        let callee = self.met(k, j, c)?;
        if packs(&callee, c.x as usize).is_some() { return self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && packs(&w.1, c.x as usize).is_some()); }
        self.wanted.iter().position(|w| w.0 == u16::MAX && (w.1.value.0 == callee.value.0 || w.1.by_body && callee.by_body && w.1.fi == callee.fi) && w.1.on_class == callee.on_class && w.1.positional >= c.x as usize + (callee.on_class & 3 == 3) as usize && w.1.required <= c.x as usize + (callee.on_class & 3 == 3) as usize)
    }

    /* What the call `c` at `j` of section `k`'s body calls, a closure's cell, what its binding holds now, or what it met. */
    fn met(&self, k: usize, j: usize, c: Ins) -> Option<Callee<'p>> {
        let (w, body) = (&self.wanted[k].1, &self.wanted[k].1.body);
        if c.op == OpCode::CallR && let Some(c) = self.celled(k, j, c.a) { return Some(c); }
        super::block::global_of(&body.ins, j, c).and_then(|g| (self.sections)(Want::Held(w.module, g))).or_else(|| (self.sections)(Want::NestedValue(w.fi, body.orig(j), value_of(body, j))))
    }

    /* Whether a call in a body calls what it met, through a register or a binding holding nothing a section is laid for. */
    fn valued(&self, ins: &[Ins], j: usize, c: Ins, m: usize) -> bool {
        matches!(c.op, OpCode::CallR | OpCode::CallGlobalR) && super::block::global_of(ins, j, c).map_or(c.op == OpCode::CallR, |g| self.wanted_at(m, g).is_none())
    }

    /* Whether the body of section `k` never returns NotImplemented, each return giving a constant, its instance, an op's result or a laid class's instance. */
    /* Whether the body of section `k` holds only ops a section lays as they are, no call, dunder op, iterator loop, handler or stack work, and few locals. */
    fn plain_lay(&self, k: usize, body: &Code) -> bool {
        use OpCode::*;
        let c = &self.wanted[k].1;
        if c.generator || c.exc || !c.cellvars.is_empty() || c.names - c.arity > 64 || self.genfors.iter().any(|g| g.k == k) { return false; }
        body.ins.iter().enumerate().all(|(j, i)| match i.op {
            Move | MinusR | NotR | LenR | SetAttrR | StoreItemR | LoadGlobalR | StoreGlobalR | IsR | IsNotR | InR | NotInR | PowR
            | BitAndR | BitOrR | BitXorR | ShlR | ShrR | JumpUnlessLt | JumpUnlessLtEq | JumpUnlessGt | JumpUnlessGtEq
            | JumpUnlessEq | JumpUnlessNotEq | ReturnR => true,
            // A truth test whose `__bool__` or `__len__` is laid calls it, which this lays not.
            JumpIfFalseR => self.laid_truth(k, j, *i).is_none(),
            // So does a load of an attribute whose getter is laid.
            GetAttrR => self.laid_dunder(k, j, *i).is_none(),
            Jump => i.x == 0,
            // A call of a module binding's section, by position, its result stored, which comes back into none of this one.
            CallGlobalR | CallR => self.plain_call(k, &body.ins, j).is_some(),
            StoreTopR => j > 0 && i.x == 0 && self.plain_call(k, &body.ins, j - 1).is_some(),
            op => dunder_op(op) && self.laid_dunder(k, j, *i).is_none(),
        })
    }

    /* The section the call at `j` of section `k`'s body jumps to under `plain_lay`, a module binding's call of at most two arguments stored at once. */
    fn plain_call(&self, k: usize, ins: &[Ins], j: usize) -> Option<usize> {
        let i = ins[j];
        if !matches!(i.op, OpCode::CallGlobalR | OpCode::CallR) || wide(i) || j > 0 && ins[j - 1].op == OpCode::PushRegs
            || !ins.get(j + 1).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0) { return None; }
        let to = self.wanted_at(self.wanted[k].1.module, super::block::global_of(ins, j, i)?)?;
        let w = &self.wanted[to].1;
        (!w.twin && w.on_class & 3 != 3 && !self.reaches(to, k)).then_some(to)
    }

    fn never_declines(&self, k: usize) -> bool {
        use OpCode::*;
        let w = &self.wanted[k].1;
        let body = &w.body;
        let (ins, consts) = (&body.ins, w.names..w.names + w.consts.len() + 3);
        let landed = |j: usize| ins.iter().any(|i| super::block::role(i).is_none_or(|(_, _, jump)| jump && i.a as usize == j));
        let writes = |r: u16| ins.iter().any(|i| super::block::role(i).is_none_or(|(dest, _, _)| dest && i.a == r));
        ins.iter().enumerate().all(|(j, i)| match i.op {
            ReturnR if consts.contains(&(i.b as usize)) || i.b == 0 && !writes(0) => true,
            ReturnR if j > 0 && !landed(j) && ins[j - 1].a == i.b => match ins[j - 1].op {
                AddR | InPlaceAddR | SubR | InPlaceSubR | MulR | DivR | ModR | FloorDivR | BitAndR | BitOrR | BitXorR | ShlR | ShrR | PowR
                    | EqR | NotEqR | LtR | LtEqR | GtR | GtEqR | NotR | MinusR | IsR | IsNotR | InR | NotInR => true,
                StoreTopR => j > 1 && self.laid_call(ins, j - 2, ins[j - 2], w.module)
                    && super::block::global_of(ins, j - 2, ins[j - 2]).and_then(|g| self.wanted_at(w.module, g)).is_some_and(|c| self.wanted[c].1.ctor().is_some()),
                _ => false,
            },
            ReturnR | ReturnValue => false,
            _ => true,
        })
    }

    /* `nested_dunder` for a load of an attribute that may have met a property's getter. */
    fn nested_getter(&self, fi: usize, body: &Code, j: usize) -> Option<Option<super::sites::Site>> {
        let seen = getter_of(body, j);
        (seen.is_some() || self.facts.noted.binary_search(&(fi as i64)).is_ok()).then_some(seen)
    }

    /* The site of the op at `j` of function `fi`'s body that may have met a dunder, one it found or none when one was noted, else None. */
    fn nested_dunder(&self, fi: usize, body: &Code, j: usize) -> Option<Option<super::sites::Site>> {
        let seen = dunder_of(body, j);
        (seen.is_some() || self.facts.noted.binary_search(&(fi as i64)).is_ok()).then_some(seen)
    }

    /* The heads the op `c` at `j` of section `k`, laid as a call of `to`, checks in turn once its own fails, each another dunder its stub met. */
    fn nested_alts(&self, k: usize, j: usize, c: Ins, to: usize, inverted: bool) -> Vec<(usize, u16, bool)> {
        let body = &self.wanted[k].1.body;
        let fi = self.wanted[k].1.fi;
        let same = super::opcodes::dunder::op_names(c.op).is_some_and(|(l, r)| l == r);
        let mut alts: Vec<(usize, u16, bool)> = if same { alloc::vec![(to, 1, inverted), (to, 6, inverted)] } else { Vec::new() };
        for n in 0..ALTS {
            let Some(callee) = (self.sections)(Want::NestedDunderAlt(fi, body.orig(j), self.wanted[to].1.value, n, c.op)) else { break };
            let Some(s) = self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && w.1.arity >= 2 && w.1.required <= 2).filter(|t| self.declining.contains(t)) else { continue };
            if alts.iter().any(|a| a.0 == s) { continue; }
            let inv = c.op == OpCode::NotEqR && callee.check == 1;
            let modes: &[u16] = if same { &[0, 1, 6] } else if !callee.reflected { &[0] } else { &[1, 7, 6] };
            alts.extend(modes.iter().map(|&m| (s, m, inv)));
        }
        alts
    }

    /* The section the op `c` at `j` of section `k`'s body calls for the dunder it found, and whether it inverts the result (a `!=` on an `__eq__`). */
    fn laid_dunder(&self, k: usize, j: usize, c: Ins) -> Option<(usize, bool)> {
        let body = &self.wanted[k].1.body;
        let fi = self.wanted[k].1.fi;
        // A property's getter, whose result is its own.
        if c.op == OpCode::GetAttrR {
            let callee = (self.sections)(Want::NestedDunder(fi, body.orig(j), self.nested_getter(fi, body, j)?, c.op))?;
            return self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && w.1.arity >= 1 && w.1.required <= 1).map(|t| (t, false));
        }
        if !dunder_op(c.op) { return None; }
        let callee = (self.sections)(Want::NestedDunder(fi, body.orig(j), self.nested_dunder(fi, body, j)?, c.op))?;
        if callee.reflected { return None; }
        let inverted = c.op == OpCode::NotEqR && callee.check == 1;
        self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && w.1.arity >= 2 && w.1.required <= 2).filter(|t| self.declining.contains(t)).map(|t| (t, inverted))
    }

    /* The section the truth test `c` at `j` of section `k`'s body calls for its `__bool__` or `__len__`, and how its result is checked. */
    fn laid_truth(&self, k: usize, j: usize, c: Ins) -> Option<(usize, u8)> {
        if c.op != OpCode::JumpIfFalseR { return None; }
        let body = &self.wanted[k].1.body;
        let fi = self.wanted[k].1.fi;
        let callee = (self.sections)(Want::NestedDunder(fi, body.orig(j), self.nested_dunder(fi, body, j)?, c.op))?;
        let to = self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && w.1.arity >= 1 && w.1.required <= 1)?;
        Some((to, callee.check))
    }

    /* The section the call `c` at `j` of section `k`'s body on a `super()` proxy jumps to, the one laid for the method it finds. */
    fn laid_super(&self, k: usize, j: usize, c: Ins) -> Option<usize> {
        let w = &self.wanted[k].1;
        let (g, body) = (w.supers?, &w.body);
        if c.op != OpCode::CallMethodR || !proxy(&body.ins, j, c, g) { return None; }
        let callee = (self.sections)(Want::Super(w.fi, c.a, w.ctor().map_or(w.value, |c| c.2), w.class))?;
        self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && same_laid(&w.1, &callee) && w.1.positional > c.x as usize && w.1.required <= c.x as usize + 1)
    }

    /* The section the call `c` at `j` of the body of section `k` that spreads a sequence into its arguments jumps to. */
    fn laid_spread(&self, k: usize, j: usize, c: Ins) -> Option<usize> {
        if c.op != OpCode::CallSpreadR { return None; }
        let w = &self.wanted[k].1;
        let body = &w.body;
        let callee = match super::block::global_of(&body.ins, j, Ins { op: OpCode::CallR, ..c }) {
            Some(h) => (self.sections)(Want::Global(w.module, h))?,
            None => self.celled(k, j, c.a).or_else(|| (self.sections)(Want::NestedValue(w.fi, body.orig(j), value_of(body, j))))?,
        };
        if !spreads(&callee, c.x) { return None; }
        self.wanted.iter().position(|w| w.1.value.0 == callee.value.0 && spreads(&w.1, c.x))
    }

    /* The function a call at `j` of section `k`'s body calls through `r`, read just before from a closure cell that holds the same function at each call. */
    fn celled(&self, k: usize, j: usize, r: u16) -> Option<Callee<'p>> {
        let w = &self.wanted[k].1;
        if w.by_body || w.cells.is_empty() { return None; }
        let ins = &w.body.ins;
        let at = ins[..j].iter().rposition(|i| super::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r) || i.op == OpCode::LoadCellR && i.a == r)?;
        if ins[at].op != OpCode::LoadCellR { return None; }
        let cell = w.cells.iter().find(|c| c.0 == ins[at].b as usize).map(|c| c.1).filter(|c| !c.is_undef())?;
        (self.sections)(Want::Celled(cell))
    }

    /* Whether section `from` may be called again while section `to` runs, a call from `to` to `from` coming back into it. */
    fn reaches(&self, from: usize, to: usize) -> bool {
        if from == to || self.twin(from) == Some(to) { return true; }
        // A section that calls none and has no others in its table reaches no other, the walk below never begun.
        if self.called(from).is_empty() && !self.alts.iter().any(|a| a.0 == from) { return false; }
        let mut seen = vec![false; self.wanted.len()];
        // A head may enter any section its table names for the other functions its call met.
        let alts = |t: usize| self.alts.iter().filter(move |a| a.0 == t).map(|a| a.1);
        let mut todo: Vec<usize> = core::iter::once(from).chain(alts(from)).collect();
        while let Some(k) = todo.pop() {
            if k == to || self.twin(k) == Some(to) { return true; }
            if core::mem::replace(&mut seen[k], true) { continue; }
            // A call of a closure's section may run its twin instead.
            for &(_, t) in self.called(k).iter() { todo.push(t); todo.extend(self.twin(t)); todo.extend(alts(t)); }
        }
        false
    }

    /* The twin of section `k` or the section `k` is the twin of, laid for any closure of the same body. */
    fn twin(&self, k: usize) -> Option<usize> {
        let w = &self.wanted;
        if w[k].1.twin { return Some(k + 1).filter(|&t| t < w.len()); }
        (w[k].1.by_body && k > 0 && w[k - 1].1.twin && w[k - 1].1.fi == w[k].1.fi).then(|| k - 1)
    }

    /* The calls in section `k`'s body that jump to sections, each with its section, found once while the sections are laid. */
    fn called(&self, k: usize) -> Calls {
        self.calls.get_or_init(|| (0..self.wanted.len()).map(|_| OnceCell::new()).collect())[k].get_or_init(|| self.calls_of(k).into()).clone()
    }

    fn calls_of(&self, k: usize) -> Vec<(usize, usize)> {
        let body = &self.wanted[k].1.body;
        let m = self.wanted[k].1.module;
        body.ins.iter().enumerate().filter_map(|(j, &i)| match i.op {
            OpCode::CallGlobalR | OpCode::CallR if self.laid_call(&body.ins, j, i, m) => {
                let g = super::block::global_of(&body.ins, j, i)?;
                Some((j, self.wanted_at(m, g)?))
            }
            OpCode::CallMethodR => Some((j, self.laid_method(k, j, i).or_else(|| self.laid_super(k, j, i))?)),
            op if dunder_op(op) || op == OpCode::GetAttrR => Some((j, self.laid_dunder(k, j, i)?.0)),
            OpCode::JumpIfFalseR => Some((j, self.laid_truth(k, j, i)?.0)),
            OpCode::CallR | OpCode::CallGlobalR => Some((j, self.laid_value(k, j, i)?)),
            OpCode::CallSpreadR => Some((j, self.laid_spread(k, j, i)?)),
            OpCode::Call => Some((j, self.laid_stack(k, j, i)?.0)),
            OpCode::PushRegs => Some((j, self.laid_many(k, j)?.0)),
            _ => None,
        }).collect()
    }

    /* Emits a call of section `k` with `call`'s arguments into `dest`, `outer` for one outside every section and `below` the value pending under it. */
    fn head(&mut self, call: Ins, rest: Ins, dest: u16, k: usize, outer: bool, below: &[Entry]) {
        // Each parameter takes an argument or its default, those past the second moved in before the call.
        let callee = &self.wanted[k].1;
        let (arity, ctor, by_body, twin, via) = (callee.arity, callee.ctor().is_some(), callee.by_body, callee.twin, callee.via_super().is_some());
        // A call that spreads a sequence fills every parameter as it enters, apart in the handler as a call on a `super()` proxy is.
        let spread = call.op == OpCode::CallSpreadR;
        // A static method called on its class takes the arguments alone, each moved in as those past the second are, as does an instance's function.
        let stat = matches!(callee.on_class, 1 | 4);
        let n = if spread { arity } else { call.x as usize + (call.op == OpCode::CallMethodR && !stat || ctor) as usize };
        // A class's call makes its instance the first parameter, its arguments the ones after, the second read from `b`.
        let args = if ctor { [0, call.b, call.c, rest.a, rest.b] } else if stat { [call.c, rest.a, rest.b, rest.c, 0] } else { [call.b, call.c, rest.a, rest.b, rest.c] };
        // A call of more than five arguments names those past the fifth apart.
        let more = core::mem::take(&mut self.more_args);
        let arg_at = |p: usize| if p < 5 { args[p] } else { more.get(p - 5).copied().unwrap_or(0) };
        // A `__getattr__` takes the attribute's name for its second parameter.
        let arg = self.head_arg.take();
        // Defaults are the `=`-marked parameters', in their order.
        let default = |l: &Self, p: usize| match arg { Some(v) if p == 1 => v, _ => { let c = &l.wanted[k].1; c.defaults[c.params.iter().take(p).filter(|m| m.ends_with('=')).count()] } };
        // A parameter past the arguments, or one a keyword call skipped, takes its default.
        let gaps = core::mem::take(&mut self.gaps);
        let given = |p: usize| p < n && gaps & 1 << p == 0;
        // The `__exit__` a `with` stages just before writes its last two arguments straight to their parameters, from its `c`.
        let staged = arity == 4 && n == 4 && call.op == OpCode::CallR && self.out.last().is_some_and(|w| w.op == OpCode::WithExit && w.x == 1
            && w.b == call.a && call.b == w.b + 1 && call.c == w.b + 2 && rest.a == w.b + 3 && rest.b == w.b + 4);
        if staged { self.prep.push((self.out.len() - 1, 2, Prep::Param(k, 2))); }
        for p in (if stat { 0 } else { 2 })..arity {
            let arg = arg_at(p);
            if staged || spread { break; }
            let at = self.out.len();
            // A section any function of its body runs takes the defaults of the function the call makes.
            if by_body && !given(p) { continue; }
            self.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: if given(p) { arg } else { 0 }, c: 0 });
            self.prep.push((at, 0, Prep::Param(k, p)));
            if !given(p) { let v = default(self, p); self.prep.push((at, 1, Prep::Const(v))); }
        }
        let at = self.out.len();
        // A parameter the call leaves to the function's default copies itself, the default written first.
        for p in (0..if stat { 0 } else { arity.min(2) }).filter(|&p| !given(p)) { let v = if by_body { Prep::Param(k, p) } else { Prep::Const(default(self, p)) }; self.prep.push((at, if ctor { p as u8 } else { 1 + p as u8 }, v)); }
        if let Some((regs, op)) = self.kwcall.take() { self.kwstubs.push((at, regs, op)); }
        let ret = self.ret_to.take().unwrap_or(self.out.len() as u16 + HEADER);
        if let Some(site) = self.wanted[k].1.site() { self.seeds.push((self.out.len(), site)); }
        self.heads.push((self.out.len(), k, rest));
        let dunder = !matches!(call.op, OpCode::CallR | OpCode::CallMethodR | OpCode::CallGlobalR | OpCode::CallSpreadR);
        self.emit(Ins { op: if dunder { OpCode::DunderSectionR } else { OpCode::CallSectionR }, ..call });
        // One value pending under it its store names, several the stub's store takes back, where a resume after it enters.
        let (name, under) = match below { [e] => (e.name, e.r + 1), _ => (false, 0) };
        if below.len() > 1 { self.unders.push((self.out.len() - 1, below.to_vec())); }
        let (held, method) = (call.op == OpCode::CallR, call.op == OpCode::CallMethodR || spread);
        // Where a call its section's check turns down goes instead, laid once the sections are, none for most.
        self.emit_all(&[
            Ins { op: OpCode::StoreTopR, x: 0, a: dest, b: ret, c: under },
            Ins { op: OpCode::SectionR, x: outer as u8 | (name as u8) << 1 | (held as u8) << 2 | (method as u8) << 3 | (ctor as u8) << 4 | ((by_body || twin) as u8) << 5 | ((via || spread) as u8) << 6, a: 0, b: 0, c: k as u16 },
            Ins { op: OpCode::SectionR, x: STUB, a: 0, b: 0, c: 0 },
        ]);
    }

    /* Lays the keyword call head at `h` through `g` again past the end for section `to`, its keywords naming `to`'s parameters, false when none match. */
    fn rekeyed(&mut self, h: usize, callee: Callee<'p>, g: u16) -> bool {
        let (store, sect, stub) = (self.out[h + 1], self.out[h + 2], self.out[h + 3]);
        if sect.x & 1 == 0 { return false; }
        let mut j = stub.a as usize;
        let mut regs = Vec::new();
        while let Some(&p) = self.out.get(j).filter(|p| p.op == OpCode::PushRegs) { regs.extend([p.a, p.b, p.c].into_iter().take(p.x as usize)); j += 1; }
        let Some(&Ins { op: OpCode::Call, a: op, .. }) = self.out.get(j) else { return false };
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let Some(under) = regs.len().checked_sub(1 + np + 2 * nk) else { return false };
        // What is pending under the call, taken back past its store, a name's dropped.
        let below: Option<Vec<Entry>> = (0..under).map(|e| match self.out.get(j + 2 + under - 1 - e) {
            Some(t) if t.op == OpCode::PopTop => Some(Entry { r: regs[e], name: true }),
            Some(t) if t.op == OpCode::StoreTopR && t.a == regs[e] => Some(Entry { r: regs[e], name: false }),
            _ => None,
        }).collect();
        let (Some(below), Some(&Ins { op: OpCode::StoreTopR, .. })) = (below, self.out.get(j + 1)) else { return false };
        let kw = regs[under..].to_vec();
        let Some((m, slots, first)) = Self::kw_slots(&kw, op, &callee, (&self.chunk.constants, self.cbase)) else { return false };
        self.keyed = true;
        let Some(to) = self.want_callee(g, callee, m) else { return false };
        (self.cur, self.quiet) = (self.orig[h], self.back[h]);
        let from = self.out.len();
        self.kw_lay(kw, op, m, &slots, first, store.a, to, &below);
        // Its section and its stub come back past the head, and on past the old one from there.
        self.emit(Ins { op: OpCode::JumpR, x: 0, a: (h + HEADER as usize) as u16, b: 0, c: 0 });
        self.displaced_to(h, from);
        true
    }

    /* How a keyword call passes `callee`'s parameters, its names among the constants from `base`, None when a name or default misses. */
    fn kw_slots(kw: &[u16], op: u16, callee: &Callee, (lits, base): (&[crate::parser::Value], usize)) -> Option<(usize, [Option<u16>; 5], usize)> {
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let first = callee.ctor().is_some() as usize;
        if kw.len() != 1 + np + 2 * nk || callee.star.is_some() || callee.dstar.is_some() || first + np > callee.positional { return None; }
        let mut slots: [Option<u16>; 5] = [None; 5];
        for p in 0..np { *slots.get_mut(first + p)? = Some(kw[1 + p]); }
        for q in 0..nk {
            let (name, value) = (kw[1 + np + 2 * q], kw[2 + np + 2 * q]);
            let crate::parser::Value::Str(key) = lits.get((name as usize).checked_sub(base)?)? else { return None };
            let p = callee.params.iter().position(|q| crate::parser::types::param_base_name(q) == key).filter(|&p| p >= first + np && p >= callee.posonly && p < callee.arity && p < 5)?;
            if slots[p].is_some() { return None; }
            slots[p] = Some(value);
        }
        let m = slots.iter().rposition(|s| s.is_some())? + 1;
        if (first..m).any(|p| p >= first && slots[p].is_none() && p < callee.arity - callee.defaults.len()) { return None; }
        Some((m, slots, first))
    }

    /* Where each operand of a call of a function taking `*args` or `**kwargs` goes, a parameter none passes taking its default. */
    fn packed(kw: &[u16], op: u16, callee: &Callee, (lits, base): (&[crate::parser::Value], usize)) -> Option<Packed> {
        let (np, nk, n) = ((op & 0xFF) as usize, (op >> 8) as usize, callee.positional);
        let apart = |p: usize| callee.star == Some(p) || callee.dstar == Some(p);
        if callee.star.is_none() && callee.dstar.is_none() || np > n && callee.star.is_none() || callee.arity > 5 || kw.len() != 1 + np + 2 * nk { return None; }
        let mut slots: Vec<Option<usize>> = alloc::vec![None; callee.arity];
        for (k, s) in slots.iter_mut().enumerate().take(np.min(n)) { *s = Some(1 + k); }
        let mut pairs = Vec::new();
        for q in 0..nk {
            let name = 1 + np + 2 * q;
            let crate::parser::Value::Str(key) = lits.get((kw[name] as usize).checked_sub(base)?)? else { return None };
            match (callee.posonly..callee.arity).find(|&p| !apart(p) && crate::parser::types::param_base_name(&callee.params[p]) == key) {
                Some(p) if slots[p].is_none() => slots[p] = Some(name + 1),
                Some(_) => return None,
                None if callee.dstar.is_some() => pairs.extend([name, name + 1]),
                None => return None,
            }
        }
        // A parameter no operand passes takes its default.
        if (0..callee.arity).any(|p| slots[p].is_none() && !apart(p) && !callee.params[p].ends_with('=')) { return None; }
        Some((slots, 1 + n.min(np)..1 + np, pairs))
    }

    /* Builds a packed call's tuple and dict, giving the registers it passes and a bit for each parameter left to its default. */
    fn pack(&mut self, regs: &[u16], (slots, rest, pairs): &Packed, tuple: Option<(u16, usize)>, dict: Option<(u16, usize)>) -> (Vec<u16>, u8) {
        let mut list: Vec<u16> = slots.iter().map(|s| s.map_or(0, |s| regs[s])).collect();
        let gaps = slots.iter().enumerate().filter(|s| s.1.is_none()).fold(0u8, |g, (p, _)| g | 1 << p);
        let packed = [tuple, dict].iter().flatten().fold(0u8, |g, t| g | 1 << t.1);
        for (into, from, build) in [(tuple, rest.clone().collect::<Vec<_>>(), OpCode::BuildTuple), (dict, pairs.clone(), OpCode::BuildDict)] {
            let Some((t, p)) = into else { continue };
            let from: Vec<Entry> = from.iter().map(|&q| Entry::reg(regs[q])).collect();
            self.push_regs(&from);
            self.emit(Ins { op: build, x: 0, a: (from.len() / if build == OpCode::BuildDict { 2 } else { 1 }) as u16, b: 0, c: 0 });
            self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
            list[p] = t;
        }
        (list, gaps & !packed)
    }

    /* Lays the head of a call packed as `packed`, its tuple and dict built straight into the parameters of section `to`. */
    fn pack_lay(&mut self, kw: &[u16], op: u16, packed: &Packed, store: u16, to: usize, below: &[Entry]) {
        let w = &self.wanted[to].1;
        let (arity, apart) = (w.arity, [w.star, w.dstar]);
        let (slots, rest, pairs) = packed;
        for (p, from, build) in [(apart[0], rest.clone().collect::<Vec<_>>(), OpCode::BuildTuple), (apart[1], pairs.clone(), OpCode::BuildDict)] {
            let Some(p) = p else { continue };
            let from: Vec<Entry> = from.iter().map(|&q| Entry::reg(kw[q])).collect();
            self.push_regs(&from);
            self.emit(Ins { op: build, x: 0, a: (from.len() / if build == OpCode::BuildDict { 2 } else { 1 }) as u16, b: 0, c: 0 });
            self.prep.push((self.out.len(), 0, Prep::Param(to, p)));
            self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: 0, b: 0, c: 0 });
        }
        let r = |p: usize| slots.get(p).copied().flatten().map_or(0, |q| kw[q]);
        self.gaps = (0..arity).filter(|&p| slots[p].is_none() && !apart.contains(&Some(p))).fold(0, |g, p| g | 1 << p);
        self.kwcall = Some((kw.to_vec(), op));
        let moves = self.out.len();
        self.head(Ins { op: OpCode::CallR, x: arity as u8, a: kw[0], b: r(0), c: r(1) }, Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) }, store, to, true, below);
        // The tuple and the dict already lie in their parameters, which the head's moves and call copy onto themselves.
        let call = self.out.len() - HEADER as usize;
        for p in apart.into_iter().flatten() { self.prep.push(if p >= 2 { (moves + p - 2, 1, Prep::Param(to, p)) } else { (call, 1 + p as u8, Prep::Param(to, p)) }); }
    }

    /* Lays the head of a keyword call `op` to section `to`, its arguments as `kw_slots` matched them, its stub the call off the stack as written. */
    #[allow(clippy::too_many_arguments)]
    fn kw_lay(&mut self, kw: Vec<u16>, op: u16, m: usize, slots: &[Option<u16>; 5], first: usize, store: u16, to: usize, below: &[Entry]) {
        let r = |p: usize| slots.get(p).copied().flatten().unwrap_or(0);
        let f = kw[0];
        let (call, rest) = if first == 1 {
            (Ins { op: OpCode::CallR, x: (m - 1) as u8, a: f, b: r(1), c: r(2) }, Ins { op: OpCode::ArgsR, x: 0, a: r(3), b: r(4), c: 0 })
        } else {
            (Ins { op: OpCode::CallR, x: m as u8, a: f, b: r(0), c: r(1) }, Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) })
        };
        self.gaps = (first..m).filter(|&p| p >= first && slots[p].is_none()).fold(0, |g, p| g | 1 << p);
        self.kwcall = Some((kw, op));
        self.head(call, rest, store, to, true, below);
    }

    /* Lays a head past the end for the call at `j` of `old`, which jumps there and comes back past its store, false when no section was learned. */
    fn displace(&mut self, j: usize, old: &Code) -> bool {
        let call = self.out[j];
        if call.op == OpCode::Call && call.a >> 8 != 0 { return self.displace_kw(j, old); }
        if !matches!(call.op, OpCode::CallR | OpCode::CallGlobalR | OpCode::CallMethodR) { return false; }
        let w = wide(call) as usize;
        // Only a call whose store takes its result at once is laid, its head keeping the values pending under it in their registers.
        let Some(&store) = self.out.get(j + 1 + w) else { return false };
        if store.op != OpCode::StoreTopR || store.c != 0 || store.b != 0 { return false; }
        let rest = if w == 1 { self.out[j + 1] } else { Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 } };
        if w == 1 && rest.op != OpCode::ArgsR { return false; }
        let mut p = j;
        while p > 0 && self.out[p - 1].op == OpCode::PushRegs && (1..=3).contains(&self.out[p - 1].x) { p -= 1; }
        let pushed: Vec<u16> = self.out[p..j].iter().flat_map(|g| [g.a, g.b, g.c].into_iter().take(g.x as usize)).collect();
        let back_at = j + 2 + w;
        let below: Option<Vec<Entry>> = pushed.iter().rev().enumerate().map(|(e, &r)| match self.out.get(back_at + e) {
            Some(t) if t.op == OpCode::PopTop => Some(Entry { r, name: true }),
            Some(t) if t.op == OpCode::StoreTopR && t.a == r && t.c == 0 => Some(Entry { r, name: false }),
            _ => None,
        }).collect();
        let Some(mut below) = below.filter(|b| b.len() <= MAX_UNDER) else { return false };
        below.reverse();
        let i = old.orig(j);
        (self.cur, self.quiet) = (i, old.back.get(j).copied().unwrap_or(i));
        let n = call.x as usize;
        let k = match call.op {
            OpCode::CallGlobalR => self.want(call.a, n),
            // One through a register inside a section lays the function it met, which no site of the code's own notes.
            OpCode::CallR if self.old.is_some_and(|b| b.body_at(j).is_some()) => super::block::global_of(&self.out, j, call).and_then(|g| self.want(g, n))
                .or_else(|| self.only.and_then(|f| (self.sections)(Want::Func(f, false))).filter(|c| c.on_class != 3).and_then(|c| { let m = n + (c.on_class == 7) as usize; self.want_callee(u16::MAX, c, m) })),
            OpCode::CallR => super::block::global_of(&self.out, j, call).and_then(|g| self.want(g, n))
                .or_else(|| (self.sections)(Want::Value(i)).filter(|c| c.on_class & 3 != 3).and_then(|c| self.want_callee(u16::MAX, c, n)).inspect(|&k| self.want_alts(None, i, k, n)))
                // One meeting a function its `def` makes at each pass runs the section any function of that body runs.
                .or_else(|| self.only.and_then(|f| (self.sections)(Want::BodyOf(f))).and_then(|c| self.want_callee(u16::MAX, c, n))),
            _ => (self.sections)(Want::Method(i)).and_then(|c| { let m = n + !matches!(c.on_class, 1 | 4) as usize; self.want_callee(u16::MAX, c, m) }).inspect(|&k| self.want_method_alts(None, i, k, n + 1)),
        };
        let Some(k) = k else { return false };
        // A head laid for a called instance takes its arguments past the instance, and one packing leftover positionals packs them its own way.
        let w = &self.wanted[k].1;
        if w.on_class == 3 || w.star.is_some() || w.dstar.is_some() { return false; }
        // A bound method's head passes its receiver first, as a method call does.
        let (call, rest) = if w.on_class == 7 { called_instance(call, rest) } else { (call, rest) };
        let by_body = call.op == OpCode::CallGlobalR && (w.twin || w.by_body);
        let (from, ret) = (self.out.len(), (back_at + below.len()) as u16);
        // A binding holding one function of a body after another is read into the section's first parameter, which the head checks and its stub calls.
        let call = if by_body {
            self.prep.push((self.out.len(), 0, Prep::Param(k, 0)));
            self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: 0, b: call.a, c: 0 });
            Ins { op: OpCode::CallR, ..call }
        } else { call };
        // Its section comes back straight past the call, its stub past the head and on from there.
        self.ret_to = Some(ret);
        self.head(call, rest, store.a, k, true, &below);
        if by_body { self.prep.push((self.out.len() - HEADER as usize, 0, Prep::Param(k, 0))); }
        self.displaced.push((ret as u32, (self.out.len() - HEADER as usize) as u32));
        self.emit(Ins { op: OpCode::JumpR, x: 0, a: ret, b: 0, c: 0 });
        self.displaced_to(p, from);
        true
    }

    /* Sends what ran at `at` to `from` past the end, so it and every jump to it become a jump there. */
    fn displaced_to(&mut self, at: usize, from: usize) {
        use OpCode::*;
        self.out[at] = Ins { op: JumpR, x: 0, a: from as u16, b: 0, c: 0 };
        // A stub's way back names the head it lies past, which it keeps.
        for i in self.out.iter_mut().filter(|i| i.a as usize == at && !(i.op == Jump && i.x == 1) && matches!(i.op, Jump | JumpR | JumpIfFalse | JumpIfFalseR | JumpIfFalseOrPop | JumpIfTrueOrPop
            | JumpUnlessEq | JumpUnlessNotEq | JumpUnlessLt | JumpUnlessLtEq | JumpUnlessGt | JumpUnlessGtEq | ForIterR | ForIter)) { i.a = from as u16; }
    }

    /* `displace` for the keyword call at `j`, its head passing arguments as the binding's function, or the one it met, names its parameters. */
    fn displace_kw(&mut self, j: usize, old: &Code) -> bool {
        let op = self.out[j].a;
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let mut p = j;
        while p > 0 && self.out[p - 1].op == OpCode::PushRegs && (1..=3).contains(&self.out[p - 1].x) { p -= 1; }
        let regs: Vec<u16> = self.out[p..j].iter().flat_map(|g| [g.a, g.b, g.c].into_iter().take(g.x as usize)).collect();
        let Some(mut under) = regs.len().checked_sub(1 + np + 2 * nk).filter(|&u| u <= MAX_UNDER) else { return false };
        let Some(&store) = self.out.get(j + 1) else { return false };
        // One whose result stays on the stack pushes it past its head, the pushes under it left as they are.
        let left = !(store.op == OpCode::StoreTopR && store.c == 0 && store.b == 0);
        let mut pushed = Vec::new();
        if left {
            if (regs[under] as usize) < self.tbase || regs.iter().filter(|&&r| r == regs[under]).count() != 1 { return false; }
            pushed = regs[..under].to_vec();
            under = 0;
        }
        let regs = regs[pushed.len()..].to_vec();
        // What is pending under the call, taken back past its store, a name's dropped.
        let below: Option<Vec<Entry>> = (0..under).map(|e| match self.out.get(j + 2 + under - 1 - e) {
            Some(t) if t.op == OpCode::PopTop => Some(Entry { r: regs[e], name: true }),
            Some(t) if t.op == OpCode::StoreTopR && t.a == regs[e] && t.c == 0 => Some(Entry { r: regs[e], name: false }),
            _ => None,
        }).collect();
        let Some(below) = below else { return false };
        let kw = regs[under..].to_vec();
        let i = old.orig(j);
        (self.cur, self.quiet) = (i, old.back.get(j).copied().unwrap_or(i));
        let g = super::block::global_of(&self.out, p, Ins { op: OpCode::CallR, x: 0, a: kw[0], b: 0, c: 0 });
        let callee = match g {
            Some(g) => (self.sections)(Want::Global(self.facts.module, g)),
            None => self.only.and_then(|f| (self.sections)(Want::Func(f, false))),
        };
        // A function its `def` makes again and again is laid for the one met, which the head checks.
        let Some(mut callee) = callee.filter(|c| !c.by_body) else { return false };
        callee.twin = false;
        let lits = (&self.chunk.constants[..], self.cbase);
        // One taking `*args` or `**kwargs` packs them, its other parameters taking the operands as a keyword call's do.
        let packed = if callee.star.is_some() || callee.dstar.is_some() { let Some(p) = Self::packed(&kw, op, &callee, lits) else { return false }; Some(p) } else { None };
        let passed = if packed.is_some() { None } else { let Some(s) = Self::kw_slots(&kw, op, &callee, lits) else { return false }; Some(s) };
        self.keyed = true;
        let m = passed.map_or(callee.arity, |s| s.0);
        let Some(to) = self.want_callee(g.unwrap_or(u16::MAX), callee, m) else { return false };
        let lay = |l: &mut Self, store: u16, below: &[Entry]| match (&packed, passed) {
            (Some(p), _) => l.pack_lay(&kw, op, p, store, to, below),
            (None, Some((m, slots, first))) => l.kw_lay(kw.clone(), op, m, &slots, first, store, to, below),
            _ => {}
        };
        let from = self.out.len();
        if left {
            let f = kw[0];
            for g in pushed.chunks(3) { self.emit(Ins { op: OpCode::PushRegs, x: g.len() as u8, a: g[0], b: g.get(1).copied().unwrap_or(0), c: g.get(2).copied().unwrap_or(0) }); }
            lay(self, f, &[]);
            self.emit(Ins { op: OpCode::PushRegs, x: 1, a: f, b: 0, c: 0 });
            self.emit(Ins { op: OpCode::JumpR, x: 0, a: j as u16 + 1, b: 0, c: 0 });
        } else {
            let ret = (j + 2 + under) as u16;
            // Its section comes back straight past the call, its stub past the head and on from there.
            self.ret_to = Some(ret);
            lay(self, store.a, &below);
            self.displaced.push((ret as u32, (self.out.len() - HEADER as usize) as u32));
            self.emit(Ins { op: OpCode::JumpR, x: 0, a: ret, b: 0, c: 0 });
        }
        self.displaced_to(p, from);
        true
    }

    /* Lays each wanted section past the code behind a jump to its end, registers from `frame` on and constants past them, giving the block and frame. */
    fn lay(&mut self, frame: usize) -> (Option<Box<Block>>, usize) {
        if self.wanted.is_empty() { return (None, frame); }
        let (over, outside) = (self.out.len(), self.heads.len());
        // Room for each section's body and a few instructions past it, so the code grows once.
        let room = self.wanted.iter().map(|w| w.1.body.ins.len() + 4).sum::<usize>() + 12 * self.heads.len();
        self.out.reserve_exact(room);
        self.orig.reserve_exact(room);
        self.back.reserve_exact(room);
        // A growth lays past the code it grows, whose own jump past its sections is moved to the new end.
        if self.old.is_none() { self.emit(Ins { op: OpCode::Jump, x: 0, a: 0, b: 0, c: 0 }); }
        let (next, extra, uses, mut sections) = (frame, Vec::<Val>::new(), Vec::new(), Vec::with_capacity(self.wanted.len()));
        // For each section, a generator's step register and the value it holds throughout for a body that yields at one place.
        let mut gens: Vec<(u16, Option<Val>)> = Vec::new();
        if let Some(old) = self.old {
            sections.extend(old.sections.iter().cloned());
            gens.extend(old.sections.iter().map(|s| (s.step_reg, None)));
        }
        let slots = crate::util::hash::FxHashMap::<u64, usize>::default();
        let main: Vec<(usize, u16, Option<u16>, u16, bool)> = self.genfors.iter().map(|g| (g.k, self.at[g.exit] as u16, g.global.is_none().then_some(g.target), g.head as u16 + HEADER, true)).collect();
        self.gen_loops.extend(main);
        // Each loop over a generator's section a section laid, with the generator's section, its intake, its entry for other iterators and the step.
        let late: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut st = LayState { next, extra, uses, sections, gens, slots, late };
        self.calls = OnceCell::new();
        for k in self.first..self.wanted.len() { let plain = self.lays_plain(k); self.place(k, &mut st, plain); }
        self.calls = OnceCell::new();
        let LayState { next, mut extra, mut uses, mut sections, gens, mut slots, late, .. } = st;
        for &(at, k, _) in &self.heads { (self.out[at + 2].a, self.out[at + 2].b) = (sections[k].start as u16, sections[k].link + 1); }
        // A level that notes where the section it calls began is kept apart from the loop.
        for &(at, to) in &self.turn_bases { if sections[to].levels { self.out[at].c = sections[to].link - 1; self.out[at].x &= !LONE_TURN; } }
        // A section's loop over a generator's section steps it by that section's register, its entry sending that register back to the entry itself.
        for &(k2, entry, mv, step) in &late {
            let r = gens[k2].0;
            (self.out[step].a, self.out[step].b, self.out[mv].a) = (r, r, r);
            let v = Val::int((entry as i64) << 16 | r as i64);
            let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 });
            uses.push((mv, 1, slot));
        }
        for (at, f, prep) in core::mem::take(&mut self.prep) {
            *super::block::field_mut(&mut self.out[at], f) = match prep {
                Prep::Param(k, p) => sections[k].link + 1 + p as u16,
                Prep::Const(v) => { let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 }); uses.push((at, f, slot)); 0 }
                Prep::GenReg(k) => gens[k].0,
                Prep::GenVal(k) => gens[k].0 + 1,
                Prep::GenResume(k) | Prep::GenEntry(k) => {
                    let entry = self.genfors.iter().find(|g| g.k == k).map_or(0, |g| g.step as i64 - 3);
                    let v = if matches!(prep, Prep::GenEntry(_)) { Val::int(entry << 16 | gens[k].0 as i64) } else { gens[k].1.unwrap_or_else(Val::none) };
                    let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 });
                    uses.push((at, f, slot));
                    0
                }
            };
        }
        // The stubs of the calls from outside every section.
        for h in 0..outside {
            let (at, k, rest) = self.heads[h];
            match self.genfors.iter().position(|g| g.head == at) {
                Some(g) => self.gen_stub(g, gens[k].0 + 1),
                None => self.stub(at, k, rest),
            }
        }
        // A stub's call reads the constants its head reads.
        let copies: Vec<(usize, u8, usize)> = uses.iter().filter(|u| self.out[u.0].op == OpCode::CallSectionR && self.out.get(u.0 + 3).is_some_and(|t| t.op == OpCode::SectionR && t.x == STUB))
            .map(|&(at, f, slot)| (self.out[at + 3].b as usize, f, slot)).collect();
        uses.extend(copies);
        // The heads a turned-down op checks next jump to the sections laid for the other dunders it met.
        for &(at, k) in &self.dunder_heads { (self.out[at + 2].a, self.out[at + 2].b) = (sections[k].start as u16, sections[k].link + 1); }
        if self.old.is_none() { self.out[over].a = self.out.len() as u16; }
        for &(at, f, slot) in &uses { *super::block::field_mut(&mut self.out[at], f) = (next + slot) as u16; }
        // The stub of an op reads its operands where the op's call does, a constant from its slot.
        for &(at, _) in self.dunders.iter().filter(|d| dunder_op(d.1.op)) {
            let (h, s) = (self.out[at], self.out[at + 3].b as usize);
            (self.out[s].b, self.out[s].c) = if h.a == 1 { (h.c, h.b) } else { (h.b, h.c) };
        }
        let frame = next + extra.len();
        let callees = core::mem::take(&mut self.wanted);
        // SAFETY a callee borrows only the parameter names of its function, which live as long as the VM that keeps the code.
        let callees: Vec<(u16, Callee<'static>)> = unsafe { core::mem::transmute::<Vec<(u16, Callee<'p>)>, Vec<(u16, Callee<'static>)>>(callees) };
        let Some(old) = self.old else {
            let binds = Cell::new(super::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed));
            let (alts, alt_tab) = alts_of(&self.alts, &sections);
            // A method's head looks its table up off its section.
            for &(k, start, mask) in &alts { sections[k as usize].table = (start, mask); }
            // Each lookup names where its table starts and its size less one.
            for &(at, k) in &self.lookups {
                if let Some(&(_, start, mask)) = alts.iter().find(|a| a.0 == k as u32) { (self.out[at].b, self.out[at].c) = (start as u16, mask as u16); }
            }
            let loops = self.genfors.iter().map(|g| (g.step as u32 - 3, self.at[g.exit], g.k as u32, g.head as u32)).collect();
            let ranges = alloc::vec![(next, extra.len())].into();
            return (Some(Box::new(Block { sections: sections.into(), extra: extra.into(), ranges, callees, lineage: 0, grown: { let d = core::mem::take(&mut self.displaced); if d.is_empty() { OnceCell::new() } else { OnceCell::from(Box::new(Grown { displaced: d, ..Default::default() })) } }, binds, by_class: alt_tab.iter().map(|_| Cell::new((Val::undef(), u32::MAX, 0))).collect(), alt_tab, loops, moved: false })), frame);
        };
        // A growth keeps every table it had where it was, laying the table of each head that met more functions anew past them.
        let (mut tab, mut by_class) = (old.alt_tab.to_vec(), old.by_class.to_vec());
        let mut grown: Vec<usize> = self.alts[self.alts_first..].iter().map(|a| a.0).collect();
        grown.sort_unstable();
        grown.dedup();
        let mut index = Vec::new();
        for k in grown {
            let pairs: Vec<(usize, usize)> = self.alts.iter().filter(|a| a.0 == k).copied().collect();
            let (mut i, t) = alts_of(&pairs, &sections);
            let Some(&(_, start, mask)) = i.first() else { continue };
            let at = tab.len() as u32;
            tab.extend_from_slice(&t);
            by_class.extend(t.iter().map(|_| Cell::new((Val::undef(), u32::MAX, 0))));
            i[0] = (k as u32, start + at, mask);
            sections[k].table = (start + at, mask);
            index.push(i[0]);
        }
        // Each lookup names its head's table, a grown head's new one or the one its section kept.
        let len = self.out.len();
        for at in 0..len {
            let ins = self.out[at];
            if ins.op != OpCode::CallValueR { continue; }
            let k = if self.lookups.iter().any(|l| l.0 == at) { self.lookups.iter().find(|l| l.0 == at).map(|l| l.1 as u32) } else { self.out.get(ins.a as usize + 2).map(|s| s.c as u32) };
            let table = k.and_then(|k| index.iter().find(|a| a.0 == k).map(|a| (a.1, a.2)).or_else(|| sections.get(k as usize).map(|s| s.table).filter(|t| t.1 != u32::MAX)));
            if let Some((start, mask)) = table { (self.out[at].b, self.out[at].c) = (start as u16, mask as u16); }
        }
        let mut ranges = old.ranges.to_vec();
        if !extra.is_empty() { ranges.push((next, extra.len())); }
        let all: Vec<Val> = old.extra.iter().copied().chain(extra).collect();
        (Some(Box::new(Block { sections: sections.into(), extra: all.into(), ranges: ranges.into(), callees, lineage: old.lineage, grown: OnceCell::from(Box::new({
            let g = old.grown();
            let mut displaced = g.displaced.clone();
            displaced.append(&mut self.displaced);
            displaced.sort_unstable();
            Grown { src: g.src.clone(), facts: g.facts.clone(), displaced, moved: g.moved.clone(), tried: core::cell::RefCell::new(g.tried.borrow().clone()), turned: Cell::new(g.turned.get()), last: Cell::new(g.last.get()), pending: Default::default(), refused: Cell::new(g.refused.get()), refused_at: Cell::new(g.refused_at.get()), ops: Cell::new(g.ops.get()) }
        })), binds: Cell::new(old.binds.get()), by_class: by_class.into(), alt_tab: tab.into(), loops: old.loops.clone(), moved: old.moved })), frame)
    }

    /* Whether section `k` is laid as its body is, one instruction for each of its own, as `plain_lay` takes. */
    fn lays_plain(&self, k: usize) -> bool {
        let c = &self.wanted[k].1;
        !c.generator && c.cells.is_empty() && !c.by_body && c.supers.is_none() && c.module == self.facts.module && self.plain_lay(k, &c.body)
    }

    /* Lays section `k` past the code as it lies, registers from `st.next` on and its constants as extras of `st`, `plain` for one `lays_plain` takes. */
    fn place(&mut self, k: usize, st: &mut LayState, plain: bool) {
        let mut next = st.next;
        let LayState { extra, uses, sections, gens, slots, late, .. } = st;
        let (global, callee) = (self.wanted[k].0, &self.wanted[k].1);
        let (value, body, arity, names, consts, module, fi) = (callee.value, callee.body.clone(), callee.arity, callee.names, callee.consts.clone(), callee.module, callee.fi);
        let (cells, ctor, by_body, supers) = (callee.cells.clone(), callee.ctor(), callee.by_body, callee.supers);
        let mut slot_map: Vec<u16> = Vec::new();
        let mut cell_regs = Vec::new();
        let mut orig = Vec::new();
        // A section a call back into it may enter keeps where the stack stood as each recursion level entered, in the register before its link.
        let recurs = !plain && self.called(k).iter().any(|&(_, t)| self.reaches(t, k));
        next += recurs as usize;
        let (link, start, inner) = (next as u16, self.out.len(), self.heads.len());
        // Two parameters at least, so a call copies both its argument registers whatever it passes.
        next += 1 + arity.max(2);
        // A generator's section keeps its step's return point in a register, a module binding loop's value in the next, and leaves to the loop's end.
        let shared = self.gen_steps.iter().find(|g| g.0 == k).map(|g| g.1);
        let stepped = self.wanted[k].1.generator.then(|| shared.unwrap_or_else(|| { next += 2; (next - 2) as u16 }));
        let lp = self.gen_loops.iter().find(|g| g.0 == k).copied();
        let exit = lp.map_or(0, |g| g.1);
        // Each `yield`'s step register move and body index past it, the step register for one `yield`, the call a `yield` takes, and each step's return.
        let mut yielded: Vec<(usize, usize)> = Vec::new();
        // The stubs of the yields that mark the iterators they hold, each with the yield's entry in `yielded` and where its stub lies.
        let mut unmarks: Vec<(usize, usize)> = Vec::new();
        let (mut resume, mut fused_at): (Option<Val>, Option<(u32, u32)>) = (None, None);
        let mut steps: Vec<(u32, u32, u32, u32)> = Vec::new();
        let mut held_at = u16::MAX;
        // The loop's variable and body start for a generator's section, and only the code's own loop takes a call's return as its value.
        let body_of_loop = lp.filter(|g| g.4).map(|g| (g.2.unwrap_or(stepped.unwrap_or(0) + 1), g.3));
        // The loops in this body over a generator's section, and the jumps back to them, which go to their steps.
        let mut loops_in: Vec<(usize, usize, usize, usize, usize, usize, u16)> = Vec::new();
        // The UnpackRs after those loops whose target unpacks, with the register the item lies in.
        let mut unpack_from: Vec<(usize, u16)> = Vec::new();
        if plain {
            // A body of plain ops is laid as it is, its registers moved past this frame's by kind and its jumps by where it starts.
            let (tail, locals) = (names + consts.len(), names - arity);
            let base = next as u16;
            next += locals + body.frame.saturating_sub(tail + 3);
            let specials = (self.cbase + self.consts) as u16;
            let reg = |r: u16| -> u16 {
                let r = r as usize;
                if r < arity { link + 1 + r as u16 }
                else if r < names { base + (r - arity) as u16 }
                else if r < tail { r as u16 }
                else if r < tail + 3 { specials + (r - tail) as u16 }
                else { base + (locals + r - tail - 3) as u16 }
            };
            let ins = &body.ins;
            // The body's instructions land in the code one or more each, made room for at once.
            orig.reserve(ins.len() + 1);
            for v in [&mut self.orig, &mut self.back] { v.reserve(ins.len() + 8); }
            self.out.reserve(ins.len() + 8);
            // A local the body reads through its check, or returns, may be unbound, so each call unbinds it first.
            let mut checked = 0u64;
            for i in ins.iter().filter(|i| i.op == OpCode::Move && i.x == 1 || i.op == OpCode::ReturnR) {
                if (arity..names).contains(&(i.b as usize)) { checked |= 1 << (i.b as usize - arity); }
            }
            for r in (0..locals).filter(|r| checked & 1 << r != 0) { self.emit(Ins { op: OpCode::UnbindR, x: 0, a: base + r as u16, b: 0, c: 0 }); }
            let at = self.out.len();
            orig.reserve(at - start + ins.len() + 1);
            orig.extend(core::iter::repeat_n(body.orig(0), at - start));
            // A body that calls lays more than one instruction for a call, its jumps landing where their targets were laid.
            let calls = ins.iter().any(|i| matches!(i.op, OpCode::CallGlobalR | OpCode::CallR));
            let (mut starts, mut jumps) = (Vec::new(), Vec::new());
            for (j, &i) in ins.iter().enumerate() {
                if calls { starts.push(self.out.len()); }
                let here = self.out.len();
                let mut out = i;
                if i.op == OpCode::StoreTopR { continue; }
                if let Some(to) = self.plain_call(k, ins, j).filter(|_| matches!(i.op, OpCode::CallGlobalR | OpCode::CallR)) {
                    let mut call = Ins { a: if i.op == OpCode::CallR { reg(i.a) } else { i.a }, b: reg(i.b), c: reg(i.c), ..i };
                    // A class's call moves its second argument into its parameter before the call, a constant's from a register of its own.
                    let moved = self.wanted[to].1.ctor().is_some() && i.x >= 2 && (names..tail).contains(&(i.c as usize));
                    if moved {
                        let v = consts[i.c as usize - names];
                        uses.push((self.out.len(), 1, *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 })));
                        call.c = next as u16;
                        next += 1;
                        self.emit(Ins { op: OpCode::Move, x: 0, a: call.c, b: 0, c: 0 });
                    }
                    self.head(call, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, reg(ins[j + 1].a), to, false, &[]);
                    // The constants it passes first and second are read by the call itself.
                    let h = self.out.len() - HEADER as usize;
                    for (f, r) in [(1u8, i.b), (2, i.c)].into_iter().take(i.x as usize).take(if moved { 1 } else { 2 }) {
                        if (names..tail).contains(&(r as usize)) { let v = consts[r as usize - names]; uses.push((h, f, *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 }))); }
                    }
                    orig.extend(core::iter::repeat_n(body.orig(j), self.out.len() - here));
                    continue;
                }
                if i.op == OpCode::ReturnR {
                    // A parameter nothing deletes or a constant is bound, which a `c` of 1 marks.
                    let bound = (names..tail + 3).contains(&(i.b as usize)) || (i.b as usize) < arity;
                    out = Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: reg(i.b), c: bound as u16 };
                    if (names..tail).contains(&(i.b as usize)) { let v = consts[i.b as usize - names]; uses.push((here, 1, *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 }))); }
                } else {
                    let (dest, reads, jump) = super::block::role(&i).unwrap_or((false, &[], false));
                    for &f in reads {
                        let r = super::block::field(&i, f);
                        *super::block::field_mut(&mut out, f) = reg(r);
                        if (names..tail).contains(&(r as usize)) { let v = consts[r as usize - names]; uses.push((here, f, *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 }))); }
                    }
                    if dest { out.a = reg(i.a); }
                    if jump && calls { jumps.push(here); } else if jump { out.a = (at + i.a as usize) as u16; }
                }
                self.emit(out);
                orig.push(body.orig(j));
            }
            // Running off the end returns None, as does a jump past the last instruction.
            starts.push(self.out.len());
            if !ins.last().is_some_and(|i| i.op == OpCode::ReturnR) {
                self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: specials, c: 1 });
                orig.push(body.orig(ins.len().saturating_sub(1)));
            }
            for p in jumps { self.out[p].a = starts[self.out[p].a as usize] as u16; }
            // The stubs of the calls inside it lie inside it too, naming what they name in its function's chunk.
            for h in inner..self.heads.len() {
                let (at, hk, rest) = self.heads[h];
                let from = self.out.len();
                self.stub(at, hk, rest);
                for _ in from..self.out.len() { orig.push(orig[at - start]); }
            }
            // An `__init__`, which returns None, gives back the instance it was called with.
            if ctor.is_some() { for r in &mut self.out[start..] { if r.op == OpCode::ReturnSectionR && r.a == link { (r.b, r.c) = (link + 1, 1); } } }
            // An op's dunder that returns NotImplemented hands the op back, to run the reflected dunder.
            if self.declining.contains(&k) && !self.never_declines(k) { for r in &mut self.out[start..] { if r.op == OpCode::ReturnSectionR && r.a == link { r.op = OpCode::ReturnDunderR; } } }
        } else {
            // Each callee register in this frame, a parameter after the link, a constant an extra slot, None, True and False its own, any other fresh.
            let tail = names + consts.len();
            let map: Vec<Option<u16>> = (0..body.frame).map(|r| match r {
                r if r < arity => Some(link + 1 + r as u16),
                // A cell of a section any function of its body runs is a register the call fills.
                r if by_body && cells.iter().any(|c| c.0 == r) => { next += 1; Some((next - 1) as u16) }
                r if (names..tail).contains(&r) || cells.iter().any(|c| c.0 == r) => None,
                r if (tail..tail + 3).contains(&r) => Some((self.cbase + self.consts + r - tail) as u16),
                _ => { next += 1; Some((next - 1) as u16) }
            }).collect();
            if by_body { cell_regs = cells.iter().map(|c| map.get(c.0).copied().flatten().unwrap_or(0)).collect(); }
            let ins = &body.ins;
            // The body's instructions land in the code one or more each, made room for at once.
            orig.reserve(ins.len() + 1);
            for v in [&mut self.orig, &mut self.back] { v.reserve(ins.len() + 8); }
            self.out.reserve(ins.len() + 8);
            // A body laid from another module reads that module's bindings, a call through one from a register of its own.
            let cross = (module != self.facts.module).then(|| { next += 1; (next - 1) as u16 });
            // A local the body reads through its check, or returns, may be unbound, so each call unbinds it first.
            let mut checked: Vec<u16> = ins.iter().filter(|i| i.op == OpCode::Move && i.x == 1 || i.op == OpCode::ReturnR).map(|i| i.b)
                .filter(|&r| (arity..names).contains(&(r as usize))).filter_map(|r| map.get(r as usize).copied().flatten()).collect();
            checked.sort_unstable();
            checked.dedup();
            for &r in &checked { self.emit(Ins { op: OpCode::UnbindR, x: 0, a: r, b: 0, c: 0 }); }
            // Each cell the body keeps is made as it enters, and an exception's instance keeps what it was made with as its `args`.
            if self.wanted[k].1.exc { self.emit(Ins { op: OpCode::ExcArgsR, x: 0, a: link + 1, b: link + 2, c: arity as u16 - 1 }); }
            let cellvars = self.wanted[k].1.cellvars.clone();
            for &c in &cellvars {
                let Some(Some(r)) = map.get(c).copied() else { continue };
                if c >= arity { self.emit(Ins { op: OpCode::UnbindR, x: 0, a: r, b: 0, c: 0 }); }
                self.emit(Ins { op: OpCode::WrapCellR, x: 0, a: r, b: 0, c: 0 });
            }
            // What the body holds, read in one pass.
            let marks = body_marks(ins);
            // So does a generator's, whose object a run made before its loop was lowered hands its state to the section.
            if stepped.is_some() || marks.makes { slot_map = map.iter().map(|m| m.unwrap_or(u16::MAX)).collect(); }
            // A return from inside its `for` loops drops their iterators, as a frame's end would.
            let loops = super::block::loops_around(ins);
            let deletes = by_body || marks.deletes;
            // A body that makes containers lets go of the one it last returned as it enters, so a collection does not find it still held.
            let builds = marks.builds;
            let tried = marks.tried;
            // A body that returns what it leaves on the stack returns it from a register of its own.
            let guard = marks.stacked.then(|| { next += 1; (next - 1) as u16 });
            if builds && let Some(t) = guard { self.emit(Ins { op: OpCode::UnbindR, x: 0, a: t, b: 0, c: 0 }); }
            // What a section keeps across a call back into its sections, found only when it makes one, with where each `try` and its handler start.
            let tries: Vec<(usize, usize)> = ins.iter().enumerate().filter(|(_, i)| matches!(i.op, OpCode::SetupExcept | OpCode::SetupFinally)).map(|(j, i)| (j, body.lowered(i.a as usize))).collect();
            let live = if recurs { super::block::live_across(ins, body.frame, &tries) } else { Vec::new() };
            // Each call of this very section, as the parameters it passes on as they are.
            let turning = recurs && !by_body && ctor.is_none() && self.wanted[k].1.on_class == 0;
            let targets = super::block::targets(ins, &tries);
            let selfs: Vec<(usize, u64)> = if !turning { Vec::new() } else {
                self.called(k).iter().filter(|&&(j, to)| to == k && matches!(ins[j].op, OpCode::CallGlobalR | OpCode::CallR | OpCode::CallMethodR) && packs(&self.wanted[k].1, ins[j].x as usize).is_none())
                    .map(|&(j, _)| (j, super::block::passed(ins, j, arity))).collect()
            };
            // The parameters every call back into this section returns as they were, passed on unchanged and read after, kept by a level for its caller.
            let bits = |r: &[u16]| r.iter().filter(|&&r| r < 64).fold(0u64, |m, &r| m | 1 << r);
            let fixed = super::block::params_written(ins, arity) | cellvars.iter().filter(|&&c| c < 64).fold(0u64, |m, &c| m | 1 << c);
            let steady = selfs.iter().fold(0u64, |m, &(j, same)| m | same & bits(&live[j])) & !fixed;
            // What a level keeps across a call back into this section, the registers read after and the steady parameters, less constants and `here`.
            let kept = |live: &[u16], here: u64| -> Vec<u16> {
                live.iter().copied().chain((0..arity.min(64) as u16).filter(|&q| steady >> q & 1 != 0 && !live.contains(&q)))
                    .filter(|&r| !(names..tail + 3).contains(&(r as usize)) && !(r < 64 && here >> r & 1 != 0)).filter_map(|r| map.get(r as usize).copied().flatten()).collect()
            };
            let (mut starts, mut jumps, mut skip) = (Vec::with_capacity(ins.len() + 1), Vec::new(), 0);
            // One sharing another's step register sets it at each yield too, the loop setting it first for the other's.
            let several = shared.is_some() || marks.yields > 1;
            // A generator's body with a `try` or a `with` notes where the stacks stand as it starts, two registers its yields read.
            let guard_at = (stepped.is_some() && super::block::guarded(ins)).then(|| { next += 2; (next - 2) as u16 });
            if let Some(g) = guard_at { self.emit(Ins { op: OpCode::PopIter, x: 3, a: g, b: 0, c: 0 }); held_at = g; }
            // A register for the value a `yield` takes off the stack.
            let held = if stepped.is_some() && marks.yields_stack { next += 1; (next - 1) as u16 } else { 0 };
            // The Jumps to a return left out, each landing where the return it jumps to lies, for a jump that lands on them.
            let mut gone = Vec::new();
            // A value an ordinary call leaves under it, pushed before it and taken back after its result.
            let under = |j: usize| j > 0 && ins[j - 1].op == OpCode::PushRegs && ins[j - 1].x == 1 && ins.get(j + 2 + wide(ins[j]) as usize).is_some_and(|t| t.op == OpCode::PopTop || t.op == OpCode::StoreTopR && t.x == 0 && t.a == ins[j - 1].a);
            // A constant the callee reads lies in an extra slot of this frame, its register patched in once they are all known.
            let konst = |at: usize, f: u8, r: u16, extra: &mut Vec<Val>, slots: &mut crate::util::hash::FxHashMap<u64, usize>, uses: &mut Vec<(usize, u8, usize)>| {
                if let Some(None) = map.get(r as usize) {
                    // A constant, or a cell the closure captured, read from an extra slot.
                    let v = cells.iter().find(|c| c.0 == r as usize).map_or_else(|| consts[r as usize - names], |c| c.1);
                    let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 });
                    uses.push((at, f, slot));
                }
            };
            // A call of a module binding's section read into a register just before for the call alone calls through the binding, the load left out.
            let folds: Vec<(usize, usize)> = if cross.is_some() { Vec::new() } else {
                (0..ins.len()).filter(|&j| ins[j].op == OpCode::CallR && self.laid_call(ins, j, ins[j], module)).filter_map(|j| {
                    let (c, w) = (ins[j], wide(ins[j]) as usize);
                    let d = (0..j).rev().find(|&d| super::block::role(&ins[d]).is_some_and(|r| r.0) && ins[d].a == c.a)?;
                    let alone = ins[d].op == OpCode::LoadGlobalR && !targets[d + 1..=j].iter().any(|&t| t)
                        && !ins[d + 1..j].iter().any(|i| super::block::role(i).is_none_or(|r| r.1.iter().any(|&f| super::block::field(i, f) == c.a)))
                        && ![c.b, c.c].iter().take(c.x as usize).any(|&r| r == c.a) && (w == 0 || ![ins[j + 1].a, ins[j + 1].b, ins[j + 1].c].contains(&c.a))
                        && ins.get(j + 1 + w).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0 && t.a == c.a)
                        && self.wanted_at(module, ins[d].b).is_some_and(|t| { let w = &self.wanted[t].1; !w.twin && !w.by_body && w.ctor().is_none() && w.star.is_none() && w.dstar.is_none() });
                    alone.then_some((d, j))
                }).collect()
            };
            for (j, &i) in ins.iter().enumerate() {
                starts.push(self.out.len());
                if skip > 0 { skip -= 1; continue; }
                if folds.iter().any(|f| f.0 == j) { continue; }
                let reg = |r: u16| map.get(r as usize).copied().flatten().unwrap_or(r);
                let leave = |l: &mut Self, loops: usize| {
                    // A return from a handler drops what the body left on the stacks.
                    if let Some(g) = guard_at { l.emit(Ins { op: OpCode::PopIter, x: 4, a: g, b: 0, c: 0 }); }
                    for _ in 0..loops { l.emit(Ins { op: OpCode::PopIter, x: 0, a: 0, b: 0, c: 0 }); }
                    l.emit(Ins { op: OpCode::Jump, x: 1, a: exit, b: 0, c: 0 });
                };
                match i.op {
                    // A generator's `yield` hands its value to the loop's variable and resumes its body, and a loop over a passed generator steps its section.
                    OpCode::PushRegs if let Some(&(_, _, k2)) = self.nested.iter().find(|n| n.0 == k && n.1 == j) && let Some(fj) = Self::loop_step(ins, j) => {
                        // A loop whose target unpacks takes its item into a register of the section's own, the UnpackR after reading it from there.
                        let f = if ins[fj].op == OpCode::ForIter {
                            let t = next as u16;
                            next += 1;
                            unpack_from.push((fj + 1, t));
                            Ins { op: OpCode::ForIterR, x: 0, a: ins[fj].a, b: t, c: 0 }
                        } else { Ins { b: reg(ins[fj].b), ..ins[fj] } };
                        self.emit(Ins { a: reg(i.a), ..i });
                        if fj == j + 3 { self.emit(Ins { a: reg(ins[j + 2].a), ..ins[j + 2] }); }
                        let adopt = self.out.len();
                        self.emit(Ins { op: OpCode::GetIter, x: ADOPT, a: 0, b: f.b, c: k2 as u16 });
                        let entry = self.out.len();
                        jumps.push(entry);
                        self.emit(f);
                        self.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: 0, c: 0 });
                        let into = self.out.len();
                        self.emit(Ins { op: OpCode::Jump, x: 1, a: 0, b: 0, c: 0 });
                        let step = self.out.len();
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: 0, b: 0, c: 0 });
                        loops_in.push((k2, fj, adopt, entry, into, step, f.b));
                        skip = fj - j;
                    }
                    // One whose value a stack call left yields it from a register of its own, past the PopTop after it.
                    OpCode::Yield if let Some(g) = stepped => {
                        let v = if i.x == 0 { self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: held, b: 0, c: 0 }); skip = 1; held } else { reg(i.a) };
                        // A yield inside other than its own one loop marks the iterators it holds, which the loop's body leaves, and unmarks them as it resumes.
                        let held_iters = loops.get(j).copied().unwrap_or(0);
                        let marks = held_iters != 1 || guard_at.is_some();
                        if marks { self.emit(Ins { op: OpCode::PopIter, x: 2, a: held_iters as u16, b: guard_at.unwrap_or(0), c: guard_at.is_some() as u16 }); }
                        yielded.push((if several { self.out.len() } else { usize::MAX }, j + 1 + skip));
                        if several { self.emit(Ins { op: OpCode::Move, x: 0, a: g, b: 0, c: 0 }); }
                        // A constant it yields is read from its slot by the return itself.
                        if i.x != 0 { konst(self.out.len(), 1, i.a, &mut *extra, &mut *slots, &mut *uses); }
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: v, c: 0 });
                        if marks {
                            unmarks.push((yielded.len() - 1, self.out.len()));
                            self.emit(Ins { op: OpCode::PopIter, x: 1, a: 0, b: 0, c: 0 });
                            self.emit(Ins { op: OpCode::Jump, x: 1, a: 0, b: 0, c: 0 });
                        }
                    }
                    // Its return ends the loop, dropping the iterators of the loops it lies in.
                    OpCode::PushRegs if stepped.is_some() && super::block::returns(ins, j) => {
                        leave(self, loops[j]);
                        skip = ins.get(j + 1).is_some_and(|n| n.op == OpCode::Jump) as usize;
                    }
                    OpCode::ReturnR if stepped.is_some() => leave(self, loops[j]),
                    // A join no instruction reads, as `pure` and `gen_pure` take, names its slot in the body's own frame and is left out.
                    OpCode::Phi => {}
                    OpCode::ReturnValue if stepped.is_some() => {
                        self.emit(Ins { op: OpCode::PopTop, x: 0, a: 0, b: 0, c: 0 });
                        leave(self, loops[j]);
                    }
                    // A pushed value a ReturnValue returns is returned from its register, and a wide stack call to a section jumps straight to it.
                    OpCode::PushRegs if let Some((to, e, regs)) = self.laid_many(k, j) => {
                        // A constant is read from its slot through a move into a register of the section's own.
                        let mut r: Vec<u16> = Vec::with_capacity(regs.len());
                        for &x in &regs {
                            if matches!(map.get(x as usize), Some(None)) {
                                let t = next as u16;
                                next += 1;
                                konst(self.out.len(), 1, x, &mut *extra, &mut *slots, &mut *uses);
                                self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: 0, c: 0 });
                                r.push(t);
                            } else { r.push(reg(x)); }
                        }
                        let a = |n: usize| r.get(1 + n).copied().unwrap_or(0);
                        let call = Ins { op: OpCode::CallR, x: (r.len() - 1) as u8, a: r[0], b: a(0), c: a(1) };
                        let rest = Ins { op: OpCode::ArgsR, x: 0, a: a(2), b: a(3), c: a(4) };
                        self.more_args = r[6..].to_vec();
                        self.kwcall = Some((r.clone(), (r.len() - 1) as u16));
                        // Its result goes where the store after it names, or back on the stack, where the code after reads it.
                        let stored = ins.get(e + 1).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0 && map.get(t.a as usize).is_some_and(|m| m.is_some()));
                        let dest = if stored { reg(ins[e + 1].a) } else { next += 1; (next - 1) as u16 };
                        let level = self.reaches(to, k).then(|| (self.turn(to), kept(&live[e], 0)));
                        let into = level.as_ref().filter(|l| l.1.contains(&dest)).map(|_| { next += 1; (next - 1) as u16 });
                        self.head(call, rest, into.unwrap_or(dest), to, false, &[]);
                        if let Some((t, keep)) = level { self.unturn(t, link, &keep); }
                        if let Some(t) = into { self.emit(Ins { op: OpCode::Move, x: 0, a: dest, b: t, c: 0 }); }
                        if !stored { self.emit(Ins { op: OpCode::PushRegs, x: 1, a: dest, b: 0, c: 0 }); }
                        skip = e - j + stored as usize;
                    }
                    OpCode::PushRegs if super::block::returns(ins, j) => {
                        konst(self.out.len(), 1, i.a, &mut *extra, &mut *slots, &mut *uses);
                        for _ in 0..loops.get(j).copied().unwrap_or(0) { self.emit(Ins { op: OpCode::PopIter, x: 0, a: 0, b: 0, c: 0 }); }
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: reg(i.a), c: 0 });
                        // The Jump after it is left out, but a ReturnValue other paths reach with their value on the stack stays.
                        skip = ins.get(j + 1).is_some_and(|n| n.op == OpCode::Jump) as usize;
                        if skip == 1 { gone.push((j + 1, ins[j + 1].a as usize)); }
                    }
                    // A value a handler leaves on the stack is returned from a register of its own.
                    OpCode::ReturnValue => if let Some(t) = guard {
                        self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                        for _ in 0..loops.get(j).copied().unwrap_or(0) { self.emit(Ins { op: OpCode::PopIter, x: 0, a: 0, b: 0, c: 0 }); }
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: tried as u8, a: link, b: t, c: 0 });
                    },
                    // The value pending under a call stays in its register, which the call's section does not write.
                    OpCode::PushRegs if ins.get(j + 1).is_some_and(|&c| self.laid_call(ins, j + 1, c, module) || self.laid_method(k, j + 1, c).is_some() || self.laid_value(k, j + 1, c).is_some() || self.laid_super(k, j + 1, c).is_some() || self.laid_spread(k, j + 1, c).is_some()) && under(j + 1) => {}
                    // A store into a constant's register, the value under a call taken back, has nowhere of its own to go.
                    OpCode::StoreTopR if i.x == 0 && matches!(map.get(i.a as usize), Some(None)) => self.emit(Ins { op: OpCode::PopTop, x: 0, a: 0, b: 0, c: 0 }),
                    OpCode::ReturnR => {
                        konst(self.out.len(), 1, i.b, &mut *extra, &mut *slots, &mut *uses);
                        for _ in 0..loops.get(j).copied().unwrap_or(0) { self.emit(Ins { op: OpCode::PopIter, x: 0, a: 0, b: 0, c: 0 }); }
                        // A parameter nothing deletes or a constant is bound, which a `c` of 1 marks.
                        let bound = (names..tail + 3).contains(&(i.b as usize)) || (i.b as usize) < arity && !deletes && !cellvars.contains(&(i.b as usize));
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: reg(i.b), c: bound as u16 });
                    }
                    // A call inside a section jumps to its own section, the StoreTopR after it naming where its result goes.
                    OpCode::CallGlobalR | OpCode::CallR | OpCode::CallMethodR | OpCode::CallSpreadR if self.laid_call(ins, j, i, module) || self.laid_method(k, j, i).is_some() || self.laid_value(k, j, i).is_some() || self.laid_super(k, j, i).is_some() || self.laid_spread(k, j, i).is_some() => {
                        let g = super::block::global_of(ins, j, i);
                        let to = self.laid_method(k, j, i).or_else(|| self.laid_value(k, j, i)).or_else(|| self.laid_super(k, j, i)).or_else(|| self.laid_spread(k, j, i)).unwrap_or_else(|| g.and_then(|g| self.wanted_at(module, g)).unwrap_or(0));
                        // One whose load was left out calls through the binding.
                        let i = match g { Some(g) if folds.iter().any(|f| f.1 == j) => Ins { op: OpCode::CallGlobalR, a: g, ..i }, _ => i };
                        let a = if matches!(i.op, OpCode::CallR | OpCode::CallSpreadR) { reg(i.a) } else { i.a };
                        // Another module's binding is read into a register the call checks.
                        let (i, a) = match cross {
                            Some(t) if i.op == OpCode::CallGlobalR => { self.emit(Ins { op: OpCode::LoadGlobalAtR, x: 0, a: t, b: i.a, c: module as u16 }); (Ins { op: OpCode::CallR, ..i }, t) }
                            // So is a binding that holds one function of a body after another, which the call checks by its body.
                            _ if i.op == OpCode::CallGlobalR && (self.wanted[to].1.twin || self.valued(ins, j, i, module)) => {
                                let t = next as u16;
                                next += 1;
                                self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: t, b: i.a, c: 0 });
                                (Ins { op: OpCode::CallR, ..i }, t)
                            }
                            _ => (i, a),
                        };
                        // A call that may come back into this section keeps a recursion level, what it reads after but the parameters passed on unchanged.
                        let back = self.reaches(to, k);
                        let here = selfs.iter().find(|s| s.0 == j && back && to == k).map_or(0, |s| s.1) & steady;
                        // A wide call's operands past the second are moved into the section's parameters, a constant's from a register of its own.
                        let w = wide(i) as usize;
                        let mut rest = Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 };
                        let more = if w == 0 { 0 } else { i.x as usize - 1 - (i.op != OpCode::CallMethodR) as usize };
                        for f in 0..more.min(3) {
                            let r = super::block::field(&ins[j + 1], f as u8);
                            *super::block::field_mut(&mut rest, f as u8) = match map.get(r as usize) {
                                Some(None) => {
                                    let t = next as u16;
                                    next += 1;
                                    konst(self.out.len(), 1, r, &mut *extra, &mut *slots, &mut *uses);
                                    self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: 0, c: 0 });
                                    t
                                }
                                _ => reg(r),
                            };
                        }
                        // A generator's `yield` of the call's result takes it from the call's return, which writes the loop's variable and resumes the body.
                        let fused = stepped.filter(|_| w == 0 && !back && !under(j) && fused_at.is_none() && ins.get(j + 1).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0)
                            && ins.get(j + 2).is_some_and(|y| y.op == OpCode::Yield && y.a == ins[j + 1].a)).and(body_of_loop);
                        if let Some(g) = stepped.filter(|_| fused.is_some()) {
                            yielded.push((if several { self.out.len() } else { usize::MAX }, j + 3));
                            if several { self.emit(Ins { op: OpCode::Move, x: 0, a: g, b: 0, c: 0 }); }
                        }
                        let (mut call, mut rest) = (Ins { a, b: reg(i.b), c: reg(i.c), ..i }, rest);
                        // The arguments past the positionals of a function taking `*args` go into its tuple, the stub calling with them all as written.
                        let order = if matches!(i.op, OpCode::CallR | OpCode::CallGlobalR) { packs(&self.wanted[to].1, i.x as usize) } else { None };
                        let packed = order.is_some();
                        if let Some(order) = order {
                            let n = i.x as usize;
                            // A constant passed first or second goes to a register of its own, as one past them did.
                            for (f, r) in [(1u8, i.b), (2, i.c)].into_iter().take(n) {
                                if !matches!(map.get(r as usize), Some(None)) { continue; }
                                let t = next as u16;
                                next += 1;
                                konst(self.out.len(), 1, r, &mut *extra, &mut *slots, &mut *uses);
                                self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: 0, c: 0 });
                                if f == 1 { call.b = t } else { call.c = t }
                            }
                            let all = [call.b, call.c, rest.a, rest.b, rest.c];
                            // A binding's function is read into a register, which the call checks and its stub calls.
                            if call.op == OpCode::CallGlobalR {
                                let f = next as u16;
                                next += 1;
                                self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: f, b: call.a, c: 0 });
                                call = Ins { op: OpCode::CallR, a: f, ..call };
                            }
                            let regs: Vec<u16> = core::iter::once(call.a).chain(all[..n].iter().copied()).collect();
                            let w = &self.wanted[to].1;
                            let (star, dstar) = (w.star.map(|p| { next += 1; ((next - 1) as u16, p) }), w.dstar.map(|p| { next += 1; ((next - 1) as u16, p) }));
                            let (list, gaps) = self.pack(&regs, &order, star, dstar);
                            self.gaps = gaps;
                            self.kwcall = Some((regs, n as u16));
                            let r = |q: usize| list.get(q).copied().unwrap_or(0);
                            (call, rest) = (Ins { x: list.len() as u8, b: r(0), c: r(1), ..call }, Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) });
                        }
                        // A recursive call moves arguments past the second into its parameters before reading the first two, as a parallel move would.
                        if to == k && self.wanted[k].1.ctor().is_none() {
                            let written = |r: u16, q: usize| (r as usize).checked_sub(link as usize + 3).is_some_and(|p| p + 2 < arity && p + 2 != q)
                                || q == 1 && r == link + 1;
                            let mut args = [call.b, call.c, rest.a, rest.b, rest.c];
                            let n = (call.x as usize + (call.op == OpCode::CallMethodR) as usize).min(5);
                            for (q, arg) in args.iter_mut().enumerate().take(n) {
                                if !written(*arg, q) { continue; }
                                let t = next as u16;
                                next += 1;
                                self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: *arg, c: 0 });
                                *arg = t;
                            }
                            (call.b, call.c, rest.a, rest.b, rest.c) = (args[0], args[1], args[2], args[3], args[4]);
                        }
                        // A class's call moves its second argument into its parameter before the call, a constant's from a register of its own.
                        let moved = !packed && self.wanted[to].1.ctor().is_some() && i.x >= 2 && matches!(map.get(i.c as usize), Some(None));
                        if moved {
                            let t = next as u16;
                            next += 1;
                            konst(self.out.len(), 1, i.c, &mut *extra, &mut *slots, &mut *uses);
                            self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: 0, c: 0 });
                            call.c = t;
                        }
                        let (call, rest) = if self.wanted[to].1.on_class & 3 == 3 { called_instance(call, rest) } else { (call, rest) };
                        self.ret_to = fused.map(|f| f.1);
                        // A receiver loaded from a module binding just before is read by the head itself, as outside every section.
                        let global = (i.op == OpCode::CallMethodR && cross.is_none() && w == 0 && j > 0 && ins[j - 1].op == OpCode::LoadGlobalR).then(|| self.out.last().copied()).flatten()
                            .filter(|l| l.op == OpCode::LoadGlobalR && l.a == call.b && l.b < u16::MAX && (call.x == 0 || call.c != l.a)).map(|l| l.b);
                        if global.is_some() {
                            self.out.pop();
                            self.orig.pop();
                            self.back.pop();
                            // The call starts where the load did, so its instructions point at the call.
                            if let Some(s) = starts.last_mut() { *s = self.out.len(); }
                        }
                        let level = back.then(|| (self.turn(to), kept(&live[j], here)));
                        // A result a handler reads, which the level keeps, comes back past the level, through a register of its own.
                        let dest = fused.map_or(reg(ins[j + 1 + w].a), |f| f.0);
                        let into = level.as_ref().filter(|l| l.1.contains(&dest)).map(|_| { next += 1; (next - 1) as u16 });
                        self.head(call, rest, into.unwrap_or(dest), to, false, &[]);
                        // The constants it passes first and second are read by the call itself.
                        let at = self.out.len() - HEADER as usize;
                        if let Some(g) = global { self.out[at + 3].c = g + 1; }
                        // A method call's receiver lies in `b` and its first argument in `c`.
                        let given = if packed { 0 } else { i.x as usize + matches!(i.op, OpCode::CallMethodR | OpCode::CallSpreadR) as usize };
                        if given > 0 { konst(at, 1, i.b, &mut *extra, &mut *slots, &mut *uses); }
                        if given > 1 && !moved { konst(at, 2, i.c, &mut *extra, &mut *slots, &mut *uses); }
                        if let Some((t, keep)) = level { self.unturn(t, link, &keep); }
                        if let Some(t) = into { self.emit(Ins { op: OpCode::Move, x: 0, a: dest, b: t, c: 0 }); }
                        skip = 1 + w + under(j) as usize;
                        // A call its check turns down hands its result to the loop's variable and the loop's body here.
                        if let Some((target, _)) = fused {
                            fused_at = Some((body.orig(j), (self.out.len() - HEADER as usize) as u32));
                            self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: target, c: 0 });
                            skip = 2;
                        }
                    }
                    // A truth test of an instance calls its `__bool__` or `__len__` section into a register and branches on it, its stub testing as written.
                    OpCode::JumpIfFalseR if !matches!(map.get(i.b as usize), Some(None)) && let Some((to, check)) = self.laid_truth(k, j, i) => {
                        let level = self.reaches(to, k).then(|| self.turn(to));
                        let (v, t) = (reg(i.b), next as u16);
                        next += 1;
                        self.dunders.push((self.out.len(), Ins { op: OpCode::JumpIfFalseR, x: 0, a: 0, b: v, c: v }));
                        self.head(Ins { op: OpCode::JumpIfFalseR, x: 1, a: 2, b: v, c: v }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, t, to, false, &[]);
                        if check != 0 { self.emit(Ins { op: OpCode::DunderCheckR, x: check, a: t, b: v, c: 0 }); }
                        if let Some(at) = level { self.unturn(at, link, &kept(&live[j], 0)); }
                        jumps.push(self.out.len());
                        self.emit(Ins { b: t, ..i });
                    }
                    // An op whose operator dunder is laid calls its section, its stub running the op.
                    op if (dunder_op(op) || op == OpCode::GetAttrR) && let Some((to, inverted)) = self.laid_dunder(k, j, i) => {
                        let getter = op == OpCode::GetAttrR;
                        let alts = if getter { Vec::new() } else { self.nested_alts(k, j, i, to, inverted) };
                        // A call that may come back into this section keeps a level, as deep as the dunder's nested calls went.
                        let level = (self.reaches(to, k) || alts.iter().any(|a| self.reaches(a.0, k))).then(|| self.turn(to));
                        let at = self.out.len();
                        konst(at, 1, i.b, &mut *extra, &mut *slots, &mut *uses);
                        // A getter's `c` names the attribute.
                        if !getter { konst(at, 2, i.c, &mut *extra, &mut *slots, &mut *uses); }
                        let op = Ins { op, x: i.x | if inverted { INVERTED } else { 0 }, b: reg(i.b), c: if getter { i.c } else { reg(i.c) }, ..i };
                        self.dunders.push((at, op));
                        self.dunder_alts.extend(alts.into_iter().map(|(s, mode, inv)| (at, s, mode, inv)));
                        self.head(call_of(op, 0), Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, reg(i.a), to, false, &[]);
                        // A `!=` on an `__eq__` inverts what it gave, which its stub skips.
                        if inverted { self.emit(Ins { op: OpCode::NotR, x: 0, a: reg(i.a), b: reg(i.a), c: 0 }); }
                        if let Some(at) = level { self.unturn(at, link, &kept(&live[j], 0)); }
                    }
                    // A stack call of a module binding's section pops its arguments and function and calls through a register, its result pushed back.
                    OpCode::Call if let Some((to, kw)) = self.laid_stack(k, j, i) => {
                        let n = (i.a & 0xFF) as usize + 2 * (i.a >> 8) as usize;
                        let regs: Vec<u16> = (0..=n).map(|_| { next += 1; (next - 1) as u16 }).collect();
                        for &t in regs.iter().rev() { self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 }); }
                        // Each parameter takes its operand or default, packing into `*args` and `**kwargs`, a stub calling as written.
                        let (m, r): (usize, [u16; 5]) = match &kw {
                            None => (n, core::array::from_fn(|p| regs.get(1 + p).copied().unwrap_or(0))),
                            Some(Ok((m, slots))) => {
                                self.gaps = (0..*m).filter(|&p| slots[p].is_none()).fold(0, |g, p| g | 1 << p);
                                (*m, slots.map(|s| s.map_or(0, |q| regs[q as usize])))
                            }
                            Some(Err(packed)) => {
                                let w = &self.wanted[to].1;
                                let (star, dstar) = (w.star.map(|p| { next += 1; ((next - 1) as u16, p) }), w.dstar.map(|p| { next += 1; ((next - 1) as u16, p) }));
                                let (list, gaps) = self.pack(&regs, packed, star, dstar);
                                self.gaps = gaps;
                                (list.len(), core::array::from_fn(|p| list.get(p).copied().unwrap_or(0)))
                            }
                        };
                        if n > 5 || kw.is_some() { (self.more_args, self.kwcall) = (regs.get(6..).filter(|_| kw.is_none()).unwrap_or(&[]).to_vec(), Some((regs.clone(), i.a))); }
                        let (call, rest) = (Ins { op: OpCode::CallR, x: m as u8, a: regs[0], b: r[0], c: r[1] }, Ins { op: OpCode::ArgsR, x: 0, a: r[2], b: r[3], c: r[4] });
                        let level = self.reaches(to, k).then(|| self.turn(to));
                        let t = next as u16;
                        next += 1;
                        self.head(call, rest, t, to, false, &[]);
                        if let Some(at) = level { self.unturn(at, link, &kept(&live[j], 0)); }
                        self.emit(Ins { op: OpCode::PushRegs, x: 1, a: t, b: 0, c: 0 });
                    }
                    // `super()` in a method makes its proxy from the instance, the method naming the class it was found in.
                    OpCode::CallGlobalR if i.x == 0 && supers == Some(i.a) && ins.get(j + 1).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0) => {
                        let at = self.out.len();
                        let method = ctor.map_or(value, |c| c.2);
                        let slot = *slots.entry(method.0).or_insert_with(|| { extra.push(method); extra.len() - 1 });
                        uses.push((at, 2, slot));
                        self.emit(Ins { op: OpCode::SuperR, x: 0, a: reg(ins[j + 1].a), b: link + 1, c: 0 });
                        skip = 1;
                    }
                    // A store into a cell the body keeps takes the value off the stack into the cell's register.
                    OpCode::StoreName => if let Some(t) = guard {
                        self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                        self.emit(Ins { op: OpCode::StoreCellR, x: 0, a: reg(i.a), b: t, c: 0 });
                    },
                    // A `with`'s dunder goes to the registers from `b` on, temps that map to registers one after another.
                    OpCode::WithEnter | OpCode::WithExit if i.x != 0 => {
                        if i.op == OpCode::WithEnter && i.x == 2 { konst(self.out.len(), 2, i.c, &mut *extra, &mut *slots, &mut *uses); }
                        self.emit(Ins { b: reg(i.b), c: if i.op == OpCode::WithEnter && i.x == 2 { reg(i.c) } else { 0 }, ..i });
                    }
                    // A delete of a module binding names the binding and the module, the name kept for its error.
                    OpCode::Del if let Some(&Kind::Global(g)) = body.kinds.get(i.a as usize) => self.emit(Ins { op: OpCode::Del, x: 1, a: i.a, b: g as u16, c: module as u16 }),
                    // An unpacking writes each of its targets.
                    OpCode::UnpackR => {
                        if let Some(&(_, t)) = unpack_from.iter().find(|u| u.0 == j) { self.emit(Ins { op: OpCode::PushRegs, x: 1, a: t, b: 0, c: 0 }); }
                        self.emit(Ins { a: reg(i.a), b: if i.x > 1 { reg(i.b) } else { 0 }, c: if i.x > 2 { reg(i.c) } else { 0 }, ..i });
                    }
                    // The result of `__exit__` judged from its register.
                    OpCode::WithJudge if i.x == 1 => self.emit(Ins { a: reg(i.a), ..i }),
                    // A `try`'s handler is its body's own index, laid as a jump is.
                    OpCode::SetupExcept | OpCode::SetupFinally => {
                        jumps.push(self.out.len());
                        self.emit(Ins { x: 1, a: body.lowered(i.a as usize) as u16, ..i });
                    }
                    _ => {
                        let (dest, reads, jump) = super::block::role(&i).unwrap_or((false, &[], false));
                        let mut out = i;
                        if let Some(t) = cross {
                            match i.op {
                                OpCode::LoadGlobalR => out = Ins { op: OpCode::LoadGlobalAtR, c: module as u16, ..i },
                                OpCode::StoreGlobalR => out = Ins { op: OpCode::StoreGlobalAtR, c: module as u16, ..i },
                                OpCode::CallGlobalR => {
                                    self.emit(Ins { op: OpCode::LoadGlobalAtR, x: 0, a: t, b: i.a, c: module as u16 });
                                    out = Ins { op: OpCode::CallR, a: t, ..i };
                                }
                                _ => {}
                            }
                        }
                        for &f in reads {
                            let r = super::block::field(&i, f);
                            if let Some(Some(m)) = map.get(r as usize) { *super::block::field_mut(&mut out, f) = *m; }
                            konst(self.out.len(), f, r, &mut *extra, &mut *slots, &mut *uses);
                        }
                        if dest { out.a = reg(i.a); }
                        if jump { jumps.push(self.out.len()); }
                        self.emit(out);
                    }
                }
            }
            // Running off the end returns None, as does a jump past the last instruction.
            starts.push(self.out.len());
            if stepped.is_some() { self.emit(Ins { op: OpCode::Jump, x: 1, a: exit, b: 0, c: 0 }); }
            else if !ins.last().is_some_and(|i| matches!(i.op, OpCode::ReturnR | OpCode::ReturnValue)) {
                self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: (self.cbase + self.consts) as u16, c: 1 });
            }
            // Each step comes back past the `yield` it last stopped at, or where the jump after it goes.
            for (y, &(at, after)) in yielded.iter().enumerate() {
                let back = match ins.get(after) { Some(n) if n.op == OpCode::Jump && n.x == 0 => n.a as usize, _ => after };
                // Coming back to a loop over a generator's section is coming back to its step, whose entry steps a generator object taken in there.
                let to = loops_in.iter().find(|n| n.1 == back).map_or(starts[back], |n| n.5);
                // A yield that marked its iterators comes back through its stub, which takes the mark back first.
                let to = match unmarks.iter().find(|u| u.0 == y) { Some(&(_, stub)) => { self.out[stub + 1].a = to as u16; stub } None => to };
                let taken = loops_in.iter().find(|n| n.1 == back).map_or(starts[back], |n| n.3);
                steps.push((body.resume(back) as u32, body.orig(after), taken as u32, to as u32));
                let v = Val::int((to as i64) << 16 | stepped.unwrap_or(0) as i64);
                if at == usize::MAX { resume = Some(v); continue; }
                let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 });
                uses.push((at, 1, slot));
            }
            // A return from a body with `try` statements leaves through their handlers, unlike a generator's yields.
            if tried && stepped.is_none() {
                for r in &mut self.out[start..] { if r.op == OpCode::ReturnSectionR && r.a == link { r.x = 1; } }
            }
            // An `__init__`, which returns None, gives back the instance it was called with.
            if ctor.is_some() { for r in &mut self.out[start..] { if r.op == OpCode::ReturnSectionR && r.a == link { (r.b, r.c) = (link + 1, 1); } } }
            // An op's dunder that returns NotImplemented hands the op back, to run the reflected dunder.
            if self.declining.contains(&k) && !self.never_declines(k) { for r in &mut self.out[start..] { if r.op == OpCode::ReturnSectionR && r.a == link { r.op = OpCode::ReturnDunderR; } } }
            // Each instruction points at the body instruction it was laid from, before a jump to a left-out return lands on the return it jumps to.
            let mut j = 0;
            for e in start..self.out.len() {
                while j + 1 < ins.len() && starts[j + 1] <= e { j += 1; }
                orig.push(body.orig(j));
            }
            for (k, to) in gone { starts[k] = starts[to]; }
            // A jump back to a loop over a generator's section goes to its step.
            for at in jumps {
                let t = self.out[at].a as usize;
                self.out[at].a = loops_in.iter().find(|n| n.1 == t && n.3 != at).map_or(starts[t], |n| n.5) as u16;
            }
            for &(k2, fj, adopt, entry, into, step, target) in &loops_in {
                let (body_at, leave) = (starts[fj + 1] as u16, starts[ins[fj].a as usize] as u16);
                (self.out[adopt].a, self.out[into].a) = (body_at, body_at);
                self.gen_loops.push((k2, leave, Some(target), body_at, false));
                late.push((k2, entry, entry + 1, step));
            }
            // The stubs of the calls inside it lie inside it too, naming what they name in its function's chunk.
            for h in inner..self.heads.len() {
                let (at, hk, rest) = self.heads[h];
                let from = self.out.len();
                self.stub(at, hk, rest);
                for _ in from..self.out.len() { orig.push(orig[at - start]); }
            }
        }
        let ctor = ctor.map(|(epoch, attrs, init)| (Cell::new(epoch), attrs, init));
        // A section any function of its body runs holds no function, each call filling its cells and defaults from the one it makes.
        let value = Cell::new(if by_body { Val::undef() } else { value });
        let by_body = by_body.then(|| (arity as u8, cell_regs.into_boxed_slice()));
        let via_super = self.wanted[k].1.via_super().cloned();
        gens.push((stepped.unwrap_or(0), resume));
        let step_reg = stepped.unwrap_or(0);
        // A section a call checks by its body holds no binding the block checks.
        let global = if by_body.is_some() || self.wanted[k].1.twin { u16::MAX } else { global };
        // Its levels note where the stack stood as each began, except where it leaves nothing on the stack, and are kept apart from the loop.
        let based = recurs && !super::block::flat(&self.out[start..]);
        if based {
            for i in &mut self.out[start..] {
                if i.op == OpCode::UnturnR && i.x & LINK != 0 { i.x = (i.x | BASED) & !LONE; }
                if i.op == OpCode::TurnR { i.x &= !LONE_TURN; }
            }
        }
        sections.push(Section { start: start as u32, end: self.out.len() as u32, link, value, levels: based, module, global, bound: self.wanted[k].1.bound_module(), fi, orig: orig.into(), ctor, by_body, via_super, slots: slot_map.into(), guard: Cell::new((Val::undef(), u32::MAX, Val::undef())), dunder: Cell::new((u32::MAX, Val::undef(), Val::undef(), Val::undef())), other: Cell::new((u32::MAX, Val::undef())), seen: Cell::new((Val::undef(), u32::MAX)), lone: Cell::new(false), table: (0, u32::MAX), arity: arity as u8, class: self.wanted[k].1.class, packs: if self.wanted[k].1.dstar == Some(0) { 2 } else { (self.wanted[k].1.star == Some(0)) as u8 }, on_class: self.wanted[k].1.on_class, defaults: if self.wanted[k].1.by_body { Box::default() } else { self.wanted[k].1.defaults.clone().into() }, tail: fused_at, steps: steps.into(), step_reg, held: held_at });
        st.next = next;
    }

    /* Lays the stub of the head of the loop `genfors[g]` steps, which makes the call as written and steps its result as an ordinary loop. */
    fn gen_stub(&mut self, g: usize, val: u16) {
        let GenFor { head, step, target, global, call, rest, .. } = self.genfors[g];
        let target = if global.is_some() { val } else { target };
        (self.out[head + 3].a, self.out[head + 3].b) = (self.out.len() as u16, self.out.len() as u16);
        self.cur = self.orig[head];
        self.emit(call);
        if wide(call) { self.emit(rest); }
        self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: target, b: 0, c: 0 });
        self.emit(Ins { op: OpCode::PushRegs, x: 1, a: target, b: 0, c: 0 });
        self.emit(Ins { op: OpCode::GetIter, x: 0, a: 0, b: 0, c: 0 });
        self.emit(Ins { op: OpCode::Jump, x: 1, a: step as u16 - 3, b: 0, c: 0 });
    }

    /* Lays the stub of the section call at `at`, which a turned-down check reaches to call as it would without the section and carry on. */
    fn stub(&mut self, at: usize, k: usize, rest: Ins) {
        let (call, store, sect) = (self.out[at], self.out[at + 1], self.out[at + 2]);
        self.out[at + 3].a = self.out.len() as u16;
        self.cur = self.orig[at];
        // A method's receiver its head reads from a module binding is read into its register first.
        if sect.x & 8 != 0 && self.out[at + 3].c != 0 { self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: call.b, b: self.out[at + 3].c - 1, c: 0 }); }
        // A turned-down op checks the other dunders it met, each a head, then runs as without the section, and a declining dunder hands it the op.
        if let Some(&(_, op)) = self.dunders.iter().find(|d| d.0 == at) {
            // Heads checked in turn once the first fails, then those its declining reaches, each passing to the next of its kind and last to the op.
            let mut alts: Vec<(usize, u16, bool)> = self.dunder_alts.iter().filter(|d| d.0 == at && d.2 != 6).map(|d| (d.1, d.2, d.3)).collect();
            let turns = alts.len();
            alts.extend(self.dunder_alts.iter().filter(|d| d.0 == at && d.2 == 6).map(|d| (d.1, d.2, d.3)));
            // A `!=`'s result goes on past the inversion an `__eq__` laid first gets, or through one of its own for an `__eq__` past a `__ne__`.
            let main = op.x & INVERTED != 0;
            let (on, invert) = (at as u16 + HEADER + main as u16, if main { at as u16 + HEADER } else { self.out.len() as u16 });
            if !main && alts.iter().any(|a| a.2) {
                self.emit(Ins { op: OpCode::NotR, x: 0, a: store.a, b: store.a, c: 0 });
                self.emit(Ins { op: OpCode::Jump, x: 1, a: on, b: 0, c: 0 });
                self.out[at + 3].a = self.out.len() as u16;
            }
            let run = self.out.len() + 5 * alts.len();
            let k = self.heads.iter().find(|h| h.0 == at).map_or(0, |h| h.1);
            for (j, &(a, mode, inverts)) in alts.iter().enumerate() {
                let head = self.out.len();
                let next = if j + 1 == turns || j + 1 == alts.len() { run } else { head + 5 };
                self.dunder_heads.push((head, a));
                self.emit(Ins { op: OpCode::DunderSectionR, ..call_of(op, mode) });
                self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: store.a, b: head as u16 + HEADER, c: 0 });
                self.emit(Ins { c: a as u16, ..sect });
                self.emit(Ins { op: OpCode::SectionR, x: STUB, a: next as u16, b: run as u16, c: if mode == 6 { k as u16 } else { 0 } });
                let to = if op.op != OpCode::NotEqR || inverts == main { at as u16 + HEADER } else if main { on } else { invert };
                self.emit(Ins { op: OpCode::Jump, x: 1, a: to, b: 0, c: 0 });
            }
            let declines = (turns < alts.len()).then_some(self.out.len() - 5 * (alts.len() - turns));
            // A builtin's call takes its argument from the stack and leaves its result there.
            let builtin = matches!(op.op, OpCode::CallStr | OpCode::CallRepr | OpCode::CallBool | OpCode::GetIter);
            if builtin { self.emit(Ins { op: OpCode::PushRegs, x: 1, a: op.b, b: 0, c: 0 }); }
            self.out[at + 3].b = self.out.len() as u16;
            for s in self.seeds.iter_mut().filter(|s| s.0 == at) { s.0 = self.out.len() | SEEDED; }
            // The first's declining goes to the heads laid for it.
            if let Some(d) = declines { self.out[at + 3].c = d as u16; }
            // A truth test's stub takes the operand's truth as `not not` would.
            if op.op == OpCode::JumpIfFalseR { self.emit(Ins { op: OpCode::NotR, x: 0, a: store.a, b: op.b, c: 0 }); }
            // A store names no result.
            let run = match op.op {
                OpCode::StoreItemR | OpCode::SetAttrR => op,
                // A `for`'s step leaves for the loop's end, its compiler index now the lowered one.
                OpCode::ForIterR => Ins { a: self.at.get(op.a as usize).map_or(u16::MAX, |&t| t as u16), ..op },
                OpCode::CallStr | OpCode::CallRepr | OpCode::CallBool => Ins { op: op.op, x: 0, a: 1, b: 0, c: 0 },
                OpCode::GetIter => Ins { op: op.op, x: 0, a: 0, b: 0, c: 0 },
                OpCode::JumpIfFalseR => Ins { op: OpCode::NotR, x: 0, a: store.a, b: store.a, c: 0 },
                _ => Ins { a: store.a, ..op },
            };
            self.emit(run);
            // An inverted `!=` carries on past its inversion.
            if op.x & INVERTED != 0 { self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER + 1, b: 0, c: 0 }); return; }
            // An iterator the op made carries on past the push and taking of the one `__iter__` gave.
            if op.op == OpCode::GetIter { self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER + 2, b: 0, c: 0 }); return; }
            if builtin { self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: store.a, b: 0, c: 0 }); }
            // `bool` rebound gives what it gives, past the check of a `__bool__`'s result.
            if op.op == OpCode::CallBool { self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER + 1, b: 0, c: 0 }); return; }
            self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER, b: 0, c: 0 });
            return;
        }
        // What is pending under the call goes to the stack around the ordinary call and back, its register or a name's read again.
        let under: Vec<Entry> = match self.unders.iter().find(|u| u.0 == at) {
            Some(u) => u.1.clone(),
            None => store.c.checked_sub(1).map(|r| Entry { r, name: sect.x & 2 != 0 }).into_iter().collect(),
        };
        // A call through a register that met other functions looks the one it makes up among the sections laid for them.
        if sect.x & 8 == 0 && !self.kwstubs.iter().any(|s| s.0 == at) && self.alts.iter().any(|a| a.0 == k) {
            self.lookups.push((self.out.len(), k));
            self.emit(Ins { op: OpCode::CallValueR, x: 0, a: at as u16, b: 0, c: 0 });
        }
        self.push_regs(&under);
        let back = |l: &mut Self| for e in under.iter().rev() { l.emit(if e.name { Ins { op: OpCode::PopTop, x: 0, a: 0, b: 0, c: 0 } } else { Ins { op: OpCode::StoreTopR, x: 0, a: e.r, b: 0, c: 0 } }); };
        // A keyword call turned down calls with its keywords as it was written.
        if let Some(k) = self.kwstubs.iter().position(|s| s.0 == at) {
            let (_, regs, op) = self.kwstubs[k].clone();
            for g in regs.chunks(3) {
                let r = |n: usize| g.get(n).copied().unwrap_or(0);
                self.emit(Ins { op: OpCode::PushRegs, x: g.len() as u8, a: r(0), b: r(1), c: r(2) });
            }
            self.out[at + 3].b = self.out.len() as u16;
            self.emit(Ins { op: OpCode::Call, x: 0, a: op, b: 0, c: 0 });
            self.stub_stores.push((at, self.out.len()));
            self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: store.a, b: STUB_STORE, c: 0 });
            back(self);
            self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER, b: 0, c: 0 });
            return;
        }
        // A called instance's stub calls it through its register again, its arguments where the call had them.
        if self.wanted[k].1.on_class & 3 == 3 {
            self.out[at + 3].b = self.out.len() as u16;
            let call = Ins { op: OpCode::CallR, x: call.x, a: call.b, b: call.c, c: rest.a };
            self.emit(call);
            if wide(call) { self.emit(Ins { op: OpCode::ArgsR, x: 0, a: rest.b, b: rest.c, c: 0 }); }
            self.stub_stores.push((at, self.out.len()));
            self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: store.a, b: STUB_STORE, c: 0 });
            back(self);
            self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER, b: 0, c: 0 });
            return;
        }
        let spread = sect.x & 72 == 72 && self.wanted[k].1.via_super().is_none();
        let op = if spread { OpCode::CallSpreadR } else if sect.x & 8 != 0 { OpCode::CallMethodR } else if sect.x & 4 != 0 { OpCode::CallR } else { OpCode::CallGlobalR };
        // The stub's call, whose site a method's check reads, starting with the site that found the method.
        self.out[at + 3].b = self.out.len() as u16;
        for s in self.seeds.iter_mut().filter(|s| s.0 == at) { s.0 = self.out.len() | SEEDED; }
        let store = Ins { op: OpCode::StoreTopR, x: 0, a: store.a, b: STUB_STORE, c: 0 };
        let jump = Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER, b: 0, c: 0 };
        let call = Ins { op, ..call };
        let base = self.out.len();
        if under.is_empty() {
            self.stub_stores.push((at, base + 1 + wide(call) as usize));
            if wide(call) { self.emit_all(&[call, rest, store, jump]); } else { self.emit_all(&[call, store, jump]); }
            return;
        }
        self.emit(call);
        if wide(call) { self.emit(rest); }
        self.stub_stores.push((at, self.out.len()));
        self.emit(store);
        back(self);
        self.emit(jump);
    }

    /* Emits the TurnR of a recursion level before calling section `to`, naming `to`'s stack-noting register or `u16::MAX` when it notes none. */
    fn turn(&mut self, to: usize) -> usize {
        let at = self.out.len();
        self.emit(Ins { op: OpCode::TurnR, x: 0, a: 0, b: 0, c: u16::MAX });
        self.turn_bases.push((at, to));
        at
    }

    /* Emits the UnturnR words where the TurnR at `at` resumes, naming the link of section `link` and the kept registers `keep`. */
    fn unturn(&mut self, at: usize, link: u16, keep: &[u16]) {
        let first = self.out.len();
        let r = |g: &[u16], n: usize| g.get(n).copied().unwrap_or(0);
        let (head, rest) = keep.split_at(keep.len().min(2));
        let lone = if rest.is_empty() { LONE } else { 0 };
        self.emit(Ins { op: OpCode::UnturnR, x: LINK | lone | head.len() as u8, a: link, b: r(head, 0), c: r(head, 1) });
        for g in rest.chunks(3) { self.emit(Ins { op: OpCode::UnturnR, x: g.len() as u8, a: r(g, 0), b: r(g, 1), c: r(g, 2) }); }
        let len = self.out.len() - first;
        let t = &mut self.out[at];
        (t.a, t.b, t.x) = (first as u16, len as u16, t.x | if lone != 0 { LONE_TURN } else { 0 } | (1 + keep.len()).min(COUNT as usize) as u8);
    }

    fn emit(&mut self, ins: Ins) {
        self.out.push(ins);
        self.orig.push(self.cur);
        self.back.push(self.quiet);
        self.last = None;
        self.dest = None;
    }

    /* Emits each of `ins` in turn, as `emit` does. */
    fn emit_all(&mut self, ins: &[Ins]) {
        self.out.extend_from_slice(ins);
        self.orig.resize(self.orig.len() + ins.len(), self.cur);
        self.back.resize(self.back.len() + ins.len(), self.quiet);
        self.last = None;
        self.dest = None;
    }
}

/* A head's table, the section the head names, where its table starts and its size less one. */
type Lookup = (u32, u32, u32);

// How many parameters a keyword call passes up to the last, and which of its operands passes each.
type Passed = (usize, [Option<u16>; 5]);

// Each parameter's operand, none for a default or the tuple and dict, the operands the tuple packs and the dict's names and values.
type Packed = (Vec<Option<usize>>, core::ops::Range<usize>, Vec<usize>);

/* The stub lookup tables by the head's section, each with its start in `Block::alt_tab` and its size less one (a power of two). */
fn alts_of(alts: &[(usize, usize)], sections: &[Section]) -> (Vec<Lookup>, Box<[(Val, u32)]>) {
    let mut heads: Vec<usize> = alts.iter().map(|a| a.0).collect();
    heads.sort_unstable();
    heads.dedup();
    let (mut index, mut tab) = (Vec::new(), Vec::new());
    for k in heads {
        // A section any function of its body runs is found by the body.
        let funcs: Vec<(Val, u32)> = alts.iter().filter(|a| a.0 == k).map(|&(_, a)| (if sections[a].by_body.is_some() { body_key(sections[a].fi) } else { sections[a].value.get() }, a as u32)).collect();
        let size = (2 * funcs.len()).next_power_of_two();
        let start = tab.len();
        tab.resize(start + size, (Val::undef(), u32::MAX));
        for (v, a) in funcs {
            // A section whose function has defaults is keyed and marked apart, so the head's lookup passes it to the stub's, which fills them in.
            let defaults = !sections[a as usize].defaults.is_empty() && sections[a as usize].by_body.is_none();
            let v = if defaults { with_defaults(v) } else { v };
            let mut at = alt_slot(v, size - 1);
            while tab[start + at].1 != u32::MAX { at = (at + 1) & (size - 1); }
            tab[start + at] = (v, a | if defaults { DEFAULTS } else { 0 });
        }
        index.push((k as u32, start as u32, (size - 1) as u32));
    }
    (index, tab.into())
}

/* The `b` of the StoreTopR after a stub's call, which marks the call as a stub's, though a StoreTopR run reads no `b`. */
pub(crate) const STUB_STORE: u16 = u16::MAX;

/* Marks the section a table names when its function has defaults, which a call passing fewer arguments moves in. */
pub(crate) const DEFAULTS: u32 = 1 << 30;

/* The key a table finds the section of function `v` by when the function has defaults, which no value is. */
#[inline]
pub(crate) fn with_defaults(v: Val) -> Val { Val(v.0 ^ 1 << 62) }

/* The key a table finds the section of any function of body `fi` by, which no function's value is. */
#[inline]
pub(crate) fn body_key(fi: usize) -> Val { Val::int(fi as i64) }

/* The first slot of a table of `mask + 1` the function `v` may lie at. */
#[inline(always)]
pub(crate) fn alt_slot(v: Val, mask: usize) -> usize { (v.0 >> 4) as usize & mask }

/* `ins` with a sentinel op past its end the register loop leaves on, so it needs no bounds check, as a code's instructions never change. */
pub(crate) fn sealed(mut ins: Vec<Ins>) -> Vec<Ins> {
    ins.reserve_exact(1);
    // SAFETY the slot past the end was just reserved, and writing it changes nothing the Vec holds.
    unsafe { ins.as_mut_ptr().add(ins.len()).write(Ins { op: OpCode::Raise, x: 0, a: 0, b: 0, c: 0 }); }
    ins
}

/* A number no code made before has, 0 left for none. */
fn next_layout() -> u32 {
    static NEXT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
    NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/* For the stack call of more than five positionals pushed from `j` of `ins`, its Call's place and the pushed registers, when the call takes just those. */
fn many_at(ins: &[Ins], j: usize) -> Option<(usize, Vec<u16>)> {
    if j > 0 && ins[j - 1].op == OpCode::PushRegs { return None; }
    let e = j + ins[j..].iter().take_while(|i| i.op == OpCode::PushRegs).count();
    let call = ins.get(e)?;
    if call.op != OpCode::Call || !(6..=MAX_ARGS as u16).contains(&call.a) { return None; }
    let regs: Vec<u16> = ins[j..e].iter().flat_map(|p| [p.a, p.b, p.c].into_iter().take(p.x as usize)).collect();
    (regs.len() == call.a as usize + 1 && (j == 0 || ins[j - 1].op != OpCode::PushRegs)).then_some((e, regs))
}

/* The function a comprehension's MakeFunction made into `r` just before the call at `j` of `ins`, by its index among the body's functions. */
fn made_in(ins: &[Ins], j: usize, r: u16) -> Option<u16> {
    let k = ins[..j].iter().rposition(|i| super::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r))?;
    (k > 0 && ins[k].op == OpCode::StoreTopR && ins[k - 1].op == OpCode::MakeFunction).then(|| ins[k - 1].a)
}

/* How a call of `n` positionals packs them for `c`, a function taking `*args` or `**kwargs`. */
fn packs(c: &Callee, n: usize) -> Option<Packed> {
    if c.on_class != 0 || c.ctor().is_some() || n > 5 { return None; }
    Lower::packed(&(0..=n as u16).collect::<Vec<_>>(), n as u16, c, (&[], 0))
}

fn called_instance(call: Ins, rest: Ins) -> (Ins, Ins) {
    (Ins { op: OpCode::CallMethodR, x: call.x, a: 0, b: call.a, c: call.b }, Ins { op: OpCode::ArgsR, x: 0, a: call.c, b: rest.a, c: rest.b })
}

/* Marks a compiler instruction noted with the class of an instance a call through a register called. */
pub(crate) const CALLED: u32 = 1 << 31;

/* The `x` of a GetIter that takes its generator into the section `c` names for the loop at `a` binding `b`, or else makes the loop's iterator. */
pub(crate) const ADOPT: u8 = 7;

/* Marks a compiler instruction noted with the function a method call found held by the instance it was called on. */
pub(crate) const FIELD: u32 = 1 << 30;

/* Marks a compiler instruction with the static, class or plain function a method call found on the receiver or its class. */
pub(crate) const STATIC: u32 = 1 << 26;

/* Marks a compiler instruction noted with the function of a bound method a call through a register called. */
pub(crate) const BOUND: u32 = 1 << 29;

/* Marks the call after a `with` noted with the class of the context manager whose dunder, naming `super`, it called. */
pub(crate) const WITH: u32 = 1 << 28;

/* Marks a call noted with the generator function it called, the one a loop over what it gives steps the section of. */
pub(crate) const GEN: u32 = 1 << 27;

/* Marks an op inside a section noted with the section's own function, which had its code lowered again for it. */
pub(crate) const RELAID: u32 = 1 << 25;

/* The instructions of a section call, the call, its result's store, the section and where a turned-down call goes. */
pub(crate) const HEADER: u16 = 4;
// Marks a seed laid at its stub's call.
const SEEDED: usize = 1 << 31;
// The `x` of the SectionR that holds where a turned-down call goes, in `a`.
pub(crate) const STUB: u8 = 0x80;
// Marked on the op a `!=` laid on an `__eq__` keeps for its stub, whose result the call inverts, which the op's run ignores.
pub(crate) const INVERTED: u8 = 0x40;
/* Whether `op` calls a section from its head. */
#[inline]
pub(crate) fn section_call(op: OpCode) -> bool { matches!(op, OpCode::CallSectionR | OpCode::CallGlobalSectionR | OpCode::CallMethodSectionR | OpCode::CallValueSectionR | OpCode::CallCtorSectionR | OpCode::CallMethodGlobalSectionR) }

// The `x` of an UnturnR word, the first naming the link in `a` and up to two kept registers, any other up to three, and a TurnR's its least kept count.
pub(crate) const LINK: u8 = 0x40;
pub(crate) const LONE: u8 = 0x20;
pub(crate) const BASED: u8 = 0x10;
pub(crate) const COUNT: u8 = 0x3F;
pub(crate) const LONE_TURN: u8 = 0x40;

/* Whether a register call holds more operands than fit it, the ArgsR after it holding the rest. */
#[inline(always)]
pub(crate) fn wide(call: Ins) -> bool { call.x > if matches!(call.op, OpCode::CallR | OpCode::CallGlobalR | OpCode::CallSectionR | OpCode::CallGlobalSectionR) { 2 } else { 1 } }

/* The registers of a register call, the callee or receiver first, and how many of them it passes. */
#[inline(always)]
pub(crate) fn call_regs(call: Ins, rest: Ins) -> ([u16; 6], usize) {
    if call.op == OpCode::CallR { ([call.a, call.b, call.c, rest.a, rest.b, rest.c], call.x as usize + 1) }
    // A global callee lies in no register, `b` stands in for it and every reader puts the binding there.
    else if matches!(call.op, OpCode::CallGlobalR | OpCode::CallSectionR | OpCode::CallGlobalSectionR) { ([call.b, call.b, call.c, rest.a, rest.b, rest.c], call.x as usize + 1) }
    else { ([call.b, call.c, rest.a, rest.b, rest.c, 0], call.x as usize + 1) }
}

/* The StoreGlobalR flag of a store into a builtin name, which redirects its fused calls. */
pub(crate) const STORE_NOTES_BUILTIN: u8 = 1;

/* The register form of a binary opcode. */
fn binary_form(op: OpCode) -> Option<OpCode> {
    Some(match op {
        OpCode::Add => OpCode::AddR, OpCode::Sub => OpCode::SubR, OpCode::Mul => OpCode::MulR,
        OpCode::Div => OpCode::DivR, OpCode::Mod => OpCode::ModR, OpCode::FloorDiv => OpCode::FloorDivR,
        OpCode::InPlaceAdd => OpCode::InPlaceAddR, OpCode::InPlaceSub => OpCode::InPlaceSubR,
        OpCode::Eq => OpCode::EqR, OpCode::NotEq => OpCode::NotEqR, OpCode::Lt => OpCode::LtR,
        OpCode::LtEq => OpCode::LtEqR, OpCode::Gt => OpCode::GtR, OpCode::GtEq => OpCode::GtEqR,
        OpCode::In => OpCode::InR, OpCode::NotIn => OpCode::NotInR, OpCode::Is => OpCode::IsR, OpCode::IsNot => OpCode::IsNotR,
        OpCode::BitAnd | OpCode::InPlaceBitAnd => OpCode::BitAndR, OpCode::BitOr | OpCode::InPlaceBitOr => OpCode::BitOrR,
        OpCode::BitXor | OpCode::InPlaceBitXor => OpCode::BitXorR, OpCode::Shl => OpCode::ShlR, OpCode::Shr => OpCode::ShrR,
        OpCode::Pow => OpCode::PowR,
        _ => return None,
    })
}

/* The `x` bit marking a bitwise register form from an augmented opcode. */
pub(crate) const AUGMENTED: u8 = 0x80;

/* The compare-and-branch form of a comparison. */
fn branch_form(op: OpCode) -> Option<OpCode> {
    Some(match op {
        OpCode::Eq => OpCode::JumpUnlessEq, OpCode::NotEq => OpCode::JumpUnlessNotEq, OpCode::Lt => OpCode::JumpUnlessLt,
        OpCode::LtEq => OpCode::JumpUnlessLtEq, OpCode::Gt => OpCode::JumpUnlessGt, OpCode::GtEq => OpCode::JumpUnlessGtEq,
        _ => return None,
    })
}

/* The stack opcode a register form falls back to. */
pub(crate) fn stack_form(op: OpCode) -> OpCode {
    match op {
        OpCode::AddR => OpCode::Add, OpCode::SubR => OpCode::Sub, OpCode::MulR => OpCode::Mul,
        OpCode::DivR => OpCode::Div, OpCode::ModR => OpCode::Mod, OpCode::FloorDivR => OpCode::FloorDiv,
        OpCode::InPlaceAddR => OpCode::InPlaceAdd, OpCode::InPlaceSubR => OpCode::InPlaceSub,
        OpCode::BitAndR => OpCode::BitAnd, OpCode::BitOrR => OpCode::BitOr, OpCode::BitXorR => OpCode::BitXor,
        OpCode::ShlR => OpCode::Shl, OpCode::ShrR => OpCode::Shr, OpCode::PowR => OpCode::Pow,
        OpCode::EqR | OpCode::JumpUnlessEq => OpCode::Eq, OpCode::NotEqR | OpCode::JumpUnlessNotEq => OpCode::NotEq,
        OpCode::LtR | OpCode::JumpUnlessLt => OpCode::Lt, OpCode::LtEqR | OpCode::JumpUnlessLtEq => OpCode::LtEq,
        OpCode::GtR | OpCode::JumpUnlessGt => OpCode::Gt, OpCode::GtEqR | OpCode::JumpUnlessGtEq => OpCode::GtEq,
        OpCode::MinusR => OpCode::Minus, OpCode::NotR => OpCode::Not, OpCode::GetItemR => OpCode::GetItem, OpCode::LenR => OpCode::CallLen,
        OpCode::InR => OpCode::In, OpCode::NotInR => OpCode::NotIn, OpCode::IsR => OpCode::Is, OpCode::IsNotR => OpCode::IsNot,
        other => other,
    }
}

/* Fuse LoadAttr + [single-push arg loads] + Call into CallMethod+CallMethodArgs. Arg loads shift left one slot so the pair sits adjacent at the Call. Only pure single-push opcodes relocate, and never across a jump target. */
pub(crate) fn fuse_method_calls(chunk: &SSAChunk, shape: u8) -> Cow<'_, [Instruction]> {
    let src = &chunk.instructions;
    // A shape not read yet is looked at for attribute reads alone.
    if shape & ATTRS == 0 && (shape != 0 || !src.iter().any(|i| i.opcode == OpCode::LoadAttr)) { return Cow::Borrowed(src); }
    let n = src.len();
    let mut out = src.clone();
    let targeted = entered(src);
    const MAX_WINDOW: usize = 16;
    let mut i = 0;
    while i + 1 < n {
        if src[i].opcode != OpCode::LoadAttr { i += 1; continue; }
        /* Scan the args after the LoadAttr, loads and arithmetic touching nothing below their pushes, so the object stays under them for CallMethod. */
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
        // The args must leave exactly what the call takes, else stack layout breaks.
        let raw = src[j].operand as usize;
        if (raw & 0xFF) + 2 * ((raw >> 8) & 0xFF) != depth { i += 1; continue; }
        out[i..(j - 1)].copy_from_slice(&src[(i + 1)..j]);
        out[j - 1] = Instruction { opcode: OpCode::CallMethod, operand: src[i].operand };
        out[j] = Instruction { opcode: OpCode::CallMethodArgs, operand: src[j].operand };
        i = j + 1;
    }
    Cow::Owned(out)
}
