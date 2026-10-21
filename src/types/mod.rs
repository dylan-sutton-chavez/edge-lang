/* Builtin types, methods and functions, and the types `edge build` proves for a program: the tree of its paths over
   the rules of `edge.ept` (tree.rs), the reviewer that accepts only a sound tree (check.rs), and what the tree finds
   dead. Only `edge build` runs them, the VM never does. */

pub use crate::parser::{BUILTIN_TYPES, CLASS_DUNDERS, builtin_type, class_dunder};
pub use crate::vm::{ALL_METHODS, BuiltinMethodId, MethodDesc, MethodKind, lookup_method, exc_parents};
pub use crate::value::NativeFnId;

pub mod read;
pub mod tree;
pub mod check;

use alloc::{string::String, vec::Vec};
use core::ops::{BitAnd, BitOr};

/* A set of leaf type codes, the codes of `tag` lines. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Tags(pub [u64; 4]);

impl Tags {
    pub const NONE: Tags = Tags([0; 4]);
    pub const ALL: Tags = Tags([!0; 4]);
    pub fn range(lo: u8, hi: u8) -> Tags {
        let mut t = Tags::NONE;
        for c in lo..=hi { t.0[c as usize / 64] |= 1 << (c % 64); }
        t
    }
    pub fn is_empty(self) -> bool { self == Tags::NONE }
    pub fn within(self, o: Tags) -> bool { self & o == self }
    pub fn meets(self, o: Tags) -> bool { !(self & o).is_empty() }
    pub fn minus(self, o: Tags) -> Tags { Tags(core::array::from_fn(|i| self.0[i] & !o.0[i])) }
    /* The runs of codes, as the CI reads them. */
    pub fn runs(self) -> Vec<(u8, u8)> {
        let mut out: Vec<(u8, u8)> = Vec::new();
        for c in 0..=255u8 {
            if self.0[c as usize / 64] >> (c % 64) & 1 == 0 { continue; }
            match out.last_mut() { Some(r) if r.1 + 1 == c => r.1 = c, _ => out.push((c, c)) }
        }
        out
    }
}
impl BitOr for Tags { type Output = Tags; fn bitor(self, o: Tags) -> Tags { Tags(core::array::from_fn(|i| self.0[i] | o.0[i])) } }
impl BitAnd for Tags { type Output = Tags; fn bitand(self, o: Tags) -> Tags { Tags(core::array::from_fn(|i| self.0[i] & o.0[i])) } }

/* Which value of its tags a value is known to be, when one is. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Known {
    No,
    // A builtin type object, by its code.
    Type(u8),
    // A function or class of the program, by its body.
    Body(u32),
    // A container made at a site (body << 16 | ip), or an instance of class body `c` made there.
    Site(u32),
    Obj(u32, u32),
    // A method of body `f` bound to an instance of class `c` made at `s`.
    Bound(u32, u32, u32),
    // A generator of the tree's context `x`, and module body `m`.
    Gen(u32),
    Module(u32),
    // Builtin method `name` of a value of code `c` made at site `s`.
    Meth(u8, u32, u32),
}

/* The type of a value: its tags, the integers it may be, and which value it is when known. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ty { pub tags: Tags, pub lo: i64, pub hi: i64, pub known: Known }

impl Ty {
    pub const NONE: Ty = Ty { tags: Tags::NONE, lo: i64::MAX, hi: i64::MIN, known: Known::No };
    pub const ANY: Ty = Ty::of(Tags::ALL);
    pub const fn of(tags: Tags) -> Ty { Ty { tags, lo: i64::MIN, hi: i64::MAX, known: Known::No } }
    pub fn int(tags: Tags, lo: i64, hi: i64) -> Ty { Ty { tags, lo, hi, known: Known::No } }
    pub fn with(self, known: Known) -> Ty { Ty { known, ..self } }
    /* Equal but for the integers, the tree's test for two paths it may join. */
    pub fn same(&self, o: &Ty) -> bool { self.tags == o.tags && self.known == o.known }
    pub fn join(&self, o: &Ty) -> Ty {
        if self.tags.is_empty() { return *o; }
        if o.tags.is_empty() { return *self; }
        Ty { tags: self.tags | o.tags, lo: self.lo.min(o.lo), hi: self.hi.max(o.hi), known: if self.known == o.known { self.known } else { Known::No } }
    }
    /* Every value of `self` is a value of `o`. */
    pub fn within(&self, o: &Ty) -> bool {
        self.tags.is_empty() || self.tags.within(o.tags) && (o.lo <= self.lo && self.hi <= o.hi) && (o.known == Known::No || o.known == self.known)
    }
}

/* A value on the stack, with the slot it was read from, and when it is the result of a row that learns, that row (ip << 8 | row) and the slots its operands came from. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Entry { pub ty: Ty, pub from: u32, pub cond: u32, pub of: [u32; 3] }

pub const NO: u32 = u32::MAX;

impl Entry { pub fn of(ty: Ty) -> Self { Self { ty, from: NO, cond: NO, of: [NO; 3] } } }

/* An open `try`, as the VM's exception frame keeps it. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Try { pub handler: u16, pub finally: bool, pub stack: u16, pub iters: u16, pub withs: u16, pub unwind: u16 }

/* Why a finally body runs, as the VM's unwind stack says. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Why { Normal, Return(Ty), Goto(u16, u16), Reraise(Ty) }

/* The machine at one instruction of one path. */
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct State { pub stack: Vec<Entry>, pub slots: Vec<Ty>, pub iters: Vec<Ty>, pub withs: Vec<Ty>, pub tries: Vec<Try>, pub unwind: Vec<Why> }

impl State {
    pub fn same(&self, o: &State) -> bool {
        let eq = |a: &[Ty], b: &[Ty]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same(y));
        self.stack.len() == o.stack.len() && self.stack.iter().zip(&o.stack).all(|(a, b)| a.ty.same(&b.ty) && (a.from, a.cond, a.of) == (b.from, b.cond, b.of))
            && eq(&self.slots, &o.slots) && eq(&self.iters, &o.iters) && eq(&self.withs, &o.withs) && self.tries == o.tries && self.unwind == o.unwind
    }
    pub fn within(&self, o: &State) -> bool {
        let w = |a: &[Ty], b: &[Ty]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.within(y));
        self.stack.len() == o.stack.len() && self.stack.iter().zip(&o.stack).all(|(a, b)| a.ty.within(&b.ty) && (a.from, a.cond, a.of) == (b.from, b.cond, b.of))
            && w(&self.slots, &o.slots) && w(&self.iters, &o.iters) && w(&self.withs, &o.withs) && self.tries == o.tries && self.unwind == o.unwind
    }
}

/* A program typed by the rules and accepted by the reviewer. */
pub struct Typed<'c> { pub prog: check::Prog<'c>, pub tree: tree::Tree }

pub fn typed<'c>(chunk: &'c crate::parser::SSAChunk, rules: &'c read::Rules) -> Result<Typed<'c>, String> {
    let prog = check::Prog::new(chunk, rules);
    let tree = tree::grow(&prog);
    check::types_check(&prog, &tree)?;
    Ok(Typed { prog, tree })
}

impl Typed<'_> {
    /* Each state of each context at each instruction of body `b`, with its context. */
    fn states(&self, b: usize) -> impl Iterator<Item = (usize, usize, &State)> {
        self.tree.ctxs.iter().enumerate().filter(move |c| c.1.body as usize == b).flat_map(|(x, c)| c.seen.iter().enumerate().flat_map(move |(ip, ss)| ss.iter().map(move |s| (x, ip, s))))
    }

    /* The instructions of body `b` no path reaches, none when a context of it gave up to the VM. */
    pub fn dead(&self, b: usize) -> Vec<bool> {
        let n = self.prog.bodies[b].chunk.instructions.len();
        if self.tree.ctxs.iter().any(|c| c.body as usize == b && c.gave_up) { return vec![false; n]; }
        let mut dead = vec![true; n];
        for (_, ip, _) in self.states(b) { if let Some(d) = dead.get_mut(ip) { *d = false; } }
        dead
    }

    /* The values instructions take, each over every path that reaches it, and how many of them have one type: `edge build --count`. */
    pub fn count(&self) -> (usize, usize) {
        let (mut proven, mut total) = (0, 0);
        for (b, body) in self.prog.bodies.iter().enumerate() {
            for (ip, ins) in body.chunk.instructions.iter().enumerate() {
                let Some((n, _)) = self.prog.rules.effect(ins.opcode, ins.operand, body.chunk) else { continue };
                let mut tags = vec![Tags::NONE; n];
                let mut reached = false;
                for (_, _, s) in self.states(b).filter(|x| x.1 == ip) {
                    reached = true;
                    for (t, e) in tags.iter_mut().zip(s.stack.iter().rev().take(n).collect::<Vec<_>>().into_iter().rev()) { *t = *t | e.ty.tags; }
                }
                if !reached { continue; }
                total += n;
                proven += tags.iter().filter(|t| t.runs().len() == 1 && t.runs()[0].0 == t.runs()[0].1).count();
            }
        }
        (proven, total)
    }
}

/* The tree's tag set for each read of a variable, by source line and bare name, for the CI of the rules. */
pub fn reads(src: &str, ept: &str) -> Result<Vec<(usize, String, Vec<(u8, u8)>)>, String> {
    let rules = read::Rules::read(ept)?;
    let (chunk, errs) = crate::code::compile(src);
    if let Some(e) = errs.first() { return Err(e.msg.clone()); }
    let typed = typed(&chunk, &rules)?;
    let mut out: Vec<(usize, String, Tags)> = Vec::new();
    for (b, body) in typed.prog.bodies.iter().enumerate() {
        for (x, ip, s) in typed.states(b) {
            let Some(&ins) = body.chunk.instructions.get(ip) else { continue };
            if !matches!(ins.opcode, crate::parser::OpCode::LoadName | crate::parser::OpCode::LoadGlobal) { continue; }
            let Some(at) = body.chunk.resolve(ip as u32) else { continue };
            let line = src[..(at as usize).min(src.len())].matches('\n').count() + 1;
            let name = String::from(crate::parser::ssa_strip(&body.chunk.names[ins.operand as usize]));
            // The value read, as the step pushes it on each way it goes on.
            let mut st = check::Step::new(&typed.prog, &typed.tree, x);
            st.step(ip, s);
            let read = st.outs.iter().filter(|o| matches!(o.0, check::To::Ip(i) if i == ip + 1)).filter_map(|o| o.1.stack.last()).fold(Tags::NONE, |t, e| t | e.ty.tags);
            match out.iter_mut().find(|o| o.0 == line && o.1 == name) { Some(o) => o.2 = o.2 | read, None => out.push((line, name, read)) }
        }
    }
    Ok(out.into_iter().map(|(l, n, t)| (l, n, t.runs())).collect())
}
