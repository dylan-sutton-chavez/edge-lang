/* Builtin types, methods and functions, and the types `edge build` proves for a program: the rules of `edge.ept`
   (read.rs), one step of the machine on typed states (step.rs), the tree of the program's paths over it (tree.rs), the
   reviewer that accepts only a tree closed under that step (check.rs), and what the tree finds dead. Only `edge build`
   runs them, the VM never does. */

pub use crate::parser::{BUILTIN_TYPES, CLASS_DUNDERS, builtin_type, class_dunder};
pub use crate::vm::{ALL_METHODS, BuiltinMethodId, MethodDesc, MethodKind, lookup_method, exc_parents};
pub use crate::value::NativeFnId;

pub mod read;
pub mod step;
pub mod tree;
pub mod check;

use alloc::{string::String, vec, vec::Vec};

/* A set of leaf type codes, bit `c` for code `c` of the `tag` lines. */
pub type Tags = u128;
pub const ALL: Tags = !0;
// Not a runtime type: the code a slot holds while unbound.
pub const UNBOUND: Tags = 1 << 127;

/* The runs of codes, as the CI reads them. */
pub fn runs(t: Tags) -> Vec<(u8, u8)> {
    let mut out: Vec<(u8, u8)> = Vec::new();
    for c in (0..128u8).filter(|c| t >> c & 1 == 1) { match out.last_mut() { Some(r) if r.1 + 1 == c => r.1 = c, _ => out.push((c, c)) } }
    if t | UNBOUND == ALL { vec![(0, 255)] } else { out }
}

/* Which value of its tags a value is known to be, when one is. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Known {
    No,
    // A builtin type object, by its code.
    Type(u8),
    // A function or class of the program, by its body.
    Body(u32),
    // A container made at a site (body << 16 | ip), and an instance of class body `c` made at one (NO for any).
    Site(u32),
    Obj(u32, u32),
    // Method `f` bound to an instance of class `c` made at a site, and builtin method `name` of a value of code `c` made at one.
    Bound(u32, u32, u32),
    Meth(u8, u32, u32),
    // The generator or coroutine of the tree's context `x`.
    Gen(u32),
}

impl Known {
    /* Instances of one class made at two sites keep their class, their site then any (NO). */
    pub fn join(self, o: Known) -> Known {
        match (self, o) {
            _ if self == o => self,
            (Known::Obj(c, _), Known::Obj(d, _)) if c == d => Known::Obj(c, NO),
            (Known::Bound(f, c, _), Known::Bound(g, d, _)) if (f, c) == (g, d) => Known::Bound(f, c, NO),
            _ => Known::No,
        }
    }
}

/* The type of a value: its tags, the integers it may be, and which value it is when known. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ty { pub tags: Tags, pub lo: i64, pub hi: i64, pub known: Known }

impl Ty {
    pub const NONE: Ty = Ty::of(0);
    pub const ANY: Ty = Ty::of(ALL);
    pub const fn of(tags: Tags) -> Ty { Ty { tags, lo: i64::MIN, hi: i64::MAX, known: Known::No } }
    pub fn int(tags: Tags, lo: i64, hi: i64) -> Ty { Ty { tags, lo, hi, known: Known::No } }
    pub fn with(self, known: Known) -> Ty { Ty { known, ..self } }
    pub fn minus(self, t: Tags) -> Ty { Ty { tags: self.tags & !t, ..self } }
    /* Equal but for the integers: two paths the tree joins. */
    pub fn same(&self, o: &Ty) -> bool { self.tags == o.tags && self.known == o.known }
    pub fn join(&self, o: &Ty) -> Ty {
        // Unbound is no value: the other side keeps its integers and which value it is.
        if self.tags & !UNBOUND == 0 || o.tags & !UNBOUND == 0 { let (v, u) = if self.tags & !UNBOUND == 0 { (o, self) } else { (self, o) }; return Ty { tags: v.tags | u.tags, ..*v }; }
        Ty { tags: self.tags | o.tags, lo: self.lo.min(o.lo), hi: self.hi.max(o.hi), known: self.known.join(o.known) }
    }
    /* Every value of `self` is one of `o`. */
    pub fn within(&self, o: &Ty) -> bool {
        // Unbound alone holds no integer and is no value.
        self.tags & !o.tags == 0 && (self.tags & !UNBOUND == 0 || o.lo <= self.lo && self.hi <= o.hi && o.known.join(self.known) == o.known)
    }
}

/* A value on the stack, the slot it was read from, and when a row that learns made it, that row (ip << 8 | row) with the slots its operands came from. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Entry { pub ty: Ty, pub from: u32, pub cond: u32, pub of: [u32; 3] }

pub const NO: u32 = u32::MAX;

impl Entry { pub fn of(ty: Ty) -> Self { Self { ty, from: NO, cond: NO, of: [NO; 3] } } }

/* An open `try` as the VM's exception frame keeps it: its handler, whether it is a finally, the stack, loop and unwind depths. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Try { pub handler: u16, pub finally: bool, pub depth: [u16; 3] }

/* Why a finally body runs, as the VM's unwind stack says. */
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Why { Normal, Return(Ty), Goto(u16, u16), Reraise(Ty) }

/* The machine at one instruction of one path: its stack, its slots, the values its loops iterate, its `try`s and finally reasons. */
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct State { pub stack: Vec<Entry>, pub slots: Vec<Ty>, pub iters: Vec<Ty>, pub tries: Vec<Try>, pub unwind: Vec<Why> }

impl State {
    fn all(&self, o: &State, f: impl Fn(&Ty, &Ty) -> bool) -> bool {
        (self.stack.len(), self.slots.len(), self.iters.len()) == (o.stack.len(), o.slots.len(), o.iters.len()) && self.tries == o.tries && self.unwind == o.unwind
            && self.stack.iter().zip(&o.stack).all(|(a, b)| (a.from, a.cond, a.of) == (b.from, b.cond, b.of) && f(&a.ty, &b.ty))
            && self.slots.iter().zip(&o.slots).chain(self.iters.iter().zip(&o.iters)).all(|(a, b)| f(a, b))
    }
    pub fn same(&self, o: &State) -> bool { self.all(o, Ty::same) }
    pub fn within(&self, o: &State) -> bool { self.all(o, Ty::within) }
}

/* The rules this build ships, an error with its reason when they are of other sources or not of format 1. */
pub fn rules() -> Result<read::Rules, String> { read::Rules::read(include_str!("edge.ept")) }

/* A program typed by the rules and accepted by the reviewer. */
pub struct Typed<'c> { pub prog: step::Prog<'c>, pub tree: tree::Tree }

pub fn typed<'c>(chunk: &'c crate::parser::SSAChunk, rules: &'c read::Rules) -> Result<Typed<'c>, String> {
    let prog = step::Prog::new(chunk, rules)?;
    let tree = tree::grow(&prog);
    check::types_check(&prog, &tree)?;
    Ok(Typed { prog, tree })
}

impl Typed<'_> {
    /* Each state of body `b` with its context and instruction. */
    fn states(&self, b: usize) -> impl Iterator<Item = (usize, usize, &State)> {
        self.tree.ctxs.iter().enumerate().filter(move |c| c.1.body as usize == b).flat_map(|(x, c)| c.seen.iter().enumerate().flat_map(move |(ip, ss)| ss.iter().map(move |s| (x, ip, s))))
    }

    /* The instructions of body `b` no path reaches, none when a context of it gave up to the VM. */
    pub fn dead(&self, b: usize) -> Vec<bool> {
        let mut dead = vec![!self.tree.ctxs.iter().any(|c| c.body as usize == b && c.gave_up); self.prog.bodies[b].chunk.instructions.len()];
        for (_, ip, _) in self.states(b) { if let Some(d) = dead.get_mut(ip) { *d = false; } }
        dead
    }

    /* Type coverage: how many of the values instructions take have one type over every path, of how many. */
    pub fn count(&self) -> (usize, usize) {
        let (mut proven, mut total) = (0, 0);
        for (b, body) in self.prog.bodies.iter().enumerate() {
            for (ip, ins) in body.chunk.instructions.iter().enumerate() {
                let Some(n) = crate::parser::effect(ins, body.chunk).map(|e| e.0 as usize) else { continue };
                let tops: Vec<&[Entry]> = self.states(b).filter(|x| x.1 == ip && x.2.stack.len() >= n).map(|x| &x.2.stack[x.2.stack.len() - n..]).collect();
                if tops.is_empty() { continue; }
                total += n;
                proven += (0..n).filter(|&j| tops.iter().fold(0, |t, s| t | s[j].ty.tags).count_ones() == 1).count();
            }
        }
        (proven, total)
    }
}

/* The tree's tags for each read of a variable, by source line and bare name, for the CI of the rules. */
#[allow(clippy::type_complexity)]
pub fn reads(src: &str, ept: &str) -> Result<Vec<(usize, String, Vec<(u8, u8)>)>, String> {
    let rules = read::Rules::read(ept)?;
    let (chunk, errs) = crate::code::compile(src);
    if let Some(e) = errs.first() { return Err(e.msg.clone()); }
    let typed = typed(&chunk, &rules)?;
    let mut out: Vec<(usize, String, Tags)> = Vec::new();
    for (b, body) in typed.prog.bodies.iter().enumerate() {
        for (x, ip, s) in typed.states(b) {
            let Some(&ins) = body.chunk.instructions.get(ip) else { continue };
            let Some(at) = body.chunk.resolve(ip as u32).filter(|_| matches!(ins.opcode, crate::parser::OpCode::LoadName | crate::parser::OpCode::LoadGlobal)) else { continue };
            let line = src.as_bytes()[..(at as usize).min(src.len())].iter().filter(|&&c| c == b'\n').count() + 1;
            let name = String::from(crate::parser::ssa_strip(&body.chunk.names[ins.operand as usize]));
            // The value read, as the step pushes it on each way it goes on.
            let mut st = step::Step::new(&typed.prog, &typed.tree, x);
            st.step(ip, s);
            let read = st.outs.iter().filter(|o| matches!(o.0, step::To::Ip(i) if i == ip + 1)).filter_map(|o| o.1.stack.last()).fold(0, |t, e| t | e.ty.tags);
            match out.iter_mut().find(|o| o.0 == line && o.1 == name) { Some(o) => o.2 |= read, None => out.push((line, name, read)) }
        }
    }
    Ok(out.into_iter().map(|(l, n, t)| (l, n, runs(t))).collect())
}
