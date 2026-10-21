/* One step of the machine on a typed state: the VM's own control (jumps, loops, try, finally, calls, classes) and every
   other opcode by its rows in `edge.ept`. The tree searches with it and the reviewer checks with it. */

use alloc::{string::String, vec, vec::Vec};
use crate::code::{Binding, BodyKind as Kind, ParamKind, Program};
use crate::parser::{OpCode as O, SSAChunk, Value, ssa_strip};
use crate::util::hash::FxHashMap as Map;
use super::read::{Learn, Out, Pat, Rhs, Row, Rules};
use super::tree::Tree;
use super::*;

// The owners of table entries by kind in the top bits, over a site (body << 16 | ip) or a body; an owner's items are field ITEM.
pub const GLOBAL: u32 = 1 << 28;
pub const MEMBER: u32 = 2 << 28;
pub const ATTR: u32 = 3 << 28;
pub const YIELDS: u32 = 4 << 28;
pub const DEFAULTS: u32 = 5 << 28;
pub const BASES: u32 = 6 << 28;
pub const CELL: u32 = 7 << 28;
pub const ITEM: u32 = u32::MAX;
// The tags the machine's own control makes values of.
const NAMES: [&str; 18] = ["TypeError", "int", "bool", "float", "complex", "str", "bytes", "NoneType", "type", "function", "instance", "generator", "coroutine", "AttributeError", "NameError", "UnboundLocalError", "AssertionError", "GeneratorExit"];

/* One body of the program: its code, kind, where each name lives, its module, the bodies its MakeFunction and MakeClass
   make, the program-wide id of each name, each parameter's kind, slot and default, where back edges land, and whether an
   `async def` made it. */
pub struct Body<'c> { pub chunk: &'c SSAChunk, pub kind: Kind, pub binds: Vec<Binding>, pub module: u32, pub fns: Vec<u32>, pub classes: Vec<u32>, pub names: Vec<u32>, pub params: Vec<(ParamKind, usize, bool)>, pub heads: Vec<bool>, pub coro: bool }

// The rules, the bodies, each name of the program and whether the VM has a builtin of that name.
pub struct Prog<'c> { pub rules: &'c Rules, pub bodies: Vec<Body<'c>>, pub names: Vec<String>, builtins: Vec<bool> }

impl<'c> Prog<'c> {
    pub fn new(chunk: &'c SSAChunk, rules: &'c Rules) -> Result<Self, String> {
        if let Some(n) = NAMES.iter().find(|n| rules.tag(n).is_none()) { return Err(alloc::format!("edge.ept has no tag {n}")); }
        let mut p = Prog { rules, bodies: Vec::new(), names: Vec::new(), builtins: Vec::new() };
        p.walk(&Program::new(chunk), chunk, 0, &[], &mut Map::default());
        let mut vm = crate::vm::VM::new(chunk);
        p.builtins = p.names.iter().map(|n| { vm.register_builtin(n); vm.builtins.contains_key(n.as_str()) }).collect();
        Ok(p)
    }
    fn walk(&mut self, vm: &Program<'c>, c: &'c SSAChunk, module: u32, params: &[String], ids: &mut Map<String, u32>) -> u32 {
        let me = self.bodies.len() as u32;
        let (kind, binds) = vm.bindings_of(c).map_or((Kind::Module, Vec::new()), |(k, b)| (k, b.to_vec()));
        let module = if kind == Kind::Module { me } else { module };
        let names = c.names.iter().map(|n| { let (n, k) = (ssa_strip(n), ids.len() as u32); *ids.entry(n.into()).or_insert_with(|| { self.names.push(n.into()); k }) }).collect();
        let mut heads = vec![false; c.instructions.len() + 1];
        for (i, x) in c.instructions.iter().enumerate() { if x.opcode == O::Jump && x.operand as usize <= i { heads[x.operand as usize] = true; } }
        let params = match kind { Kind::Function(fi) => vm.params_of(fi).iter().zip(params).map(|(&(k, s), d)| (k, s, d.ends_with('='))).collect(), _ => Vec::new() };
        self.bodies.push(Body { chunk: c, kind, binds, module, fns: Vec::new(), classes: Vec::new(), names, params, heads, coro: false });
        let fns: Vec<u32> = c.functions.iter().map(|f| self.walk(vm, &f.1, module, &f.0, ids)).collect();
        for x in c.instructions.iter().filter(|x| x.opcode == O::MakeCoroutine) { if let Some(&f) = fns.get(x.operand as usize) { self.bodies[f as usize].coro = true; } }
        let classes = c.classes.iter().map(|k| self.walk(vm, k, module, &[], ids)).collect();
        (self.bodies[me as usize].fns, self.bodies[me as usize].classes) = (fns, classes);
        me
    }
    // A tag `new` made sure the file has.
    pub fn tag(&self, name: &str) -> Ty { Ty::of(self.rules.tag(name).unwrap_or(0)) }
    fn leaf(&self, c: u8) -> &str { self.rules.tags.iter().find(|t| t.1 == c).map_or("", |t| &t.0) }
    fn is(&self, b: u32, k: Kind) -> bool { core::mem::discriminant(&self.bodies[b as usize].kind) == core::mem::discriminant(&k) }
    /* What a name no code of its module binds is: a builtin type, by its code when it has a tag, another of the VM's builtins, or unbound. */
    pub fn builtin(&self, name: u32) -> Ty {
        let n = self.names.get(name as usize).map_or("", String::as_str);
        if crate::parser::builtin_type(n).is_some() { return self.rules.tags.iter().find(|t| t.0 == n).map_or(self.tag("type"), |t| self.tag("type").with(Known::Type(t.1))); }
        if NativeFnId::from_name(n).is_some() { self.tag("function") } else if self.builtins.get(name as usize) == Some(&true) { Ty::ANY.minus(UNBOUND) } else { Ty::of(UNBOUND) }
    }
    pub fn konst(&self, v: &Value) -> Ty {
        let (n, r) = match v { Value::Int(i) => ("int", Some(*i)), Value::BigInt(_) => ("int", None), Value::Bool(b) => ("bool", Some(*b as i64)), Value::Float(_) => ("float", None),
            Value::Imag(_) => ("complex", None), Value::Str(_) => ("str", None), Value::Bytes(_) => ("bytes", None), Value::None => ("NoneType", None) };
        r.map_or(self.tag(n), |i| Ty { lo: i, hi: i, ..self.tag(n) })
    }
}

/* Where a step goes. */
#[derive(Clone, Debug)]
pub enum To { Ip(usize), Ret(Ty), Raise(Ty) }

/* What a step does beside its states: entries it joins into the tables, calls it needs a context for, whether code nothing
   knows ran, whether no rule covers it (the VM then runs the body), and whether any code ran that may write the module. */
#[derive(Default, Debug)]
pub struct Eff { pub writes: Vec<(u32, u32, Ty)>, pub calls: Vec<(u32, Vec<Ty>)>, pub wild: bool, pub gave_up: bool, pub ran: bool }

pub struct Step<'a, 'c> { pub p: &'a Prog<'c>, pub t: &'a Tree, pub x: usize, pub outs: Vec<(To, State)>, pub eff: Eff }

impl<'a, 'c> Step<'a, 'c> {
    pub fn new(p: &'a Prog<'c>, t: &'a Tree, x: usize) -> Self { Self { p, t, x, outs: Vec::new(), eff: Eff::default() } }
    fn me(&self) -> u32 { self.t.ctxs[self.x].body }
    fn body(&self) -> &'a Body<'c> { &self.p.bodies[self.me() as usize] }
    fn site(&self, ip: usize) -> u32 { self.me() << 16 | ip as u32 }
    fn name(&self, i: u16) -> u32 { self.body().names.get(i as usize).copied().unwrap_or(NO) }
    fn table(&self, owner: u32, field: u32) -> Option<Ty> {
        if self.t.wild && owner >> 28 < 5 { return Some(Ty::ANY); }
        self.t.tables.get(&(owner, field)).copied()
    }
    fn write(&mut self, owner: u32, field: u32, ty: Ty) { self.eff.writes.push((owner, field, ty)); }
    fn go(&mut self, ip: usize, s: State) { self.outs.push((To::Ip(ip), s)); }
    fn push(&mut self, ip: usize, s: &State, tys: impl IntoIterator<Item = Ty>) {
        for t in tys { if t.tags != 0 { let mut s = s.clone(); s.stack.push(Entry::of(t)); self.go(ip, s); } }
    }

    /* The exception `e` rises here: to the innermost handler, or out of the body. */
    fn raise(&mut self, s: &State, e: Ty) {
        if e.tags == 0 { return; }
        let mut s = s.clone();
        let Some(f) = s.tries.pop() else { return self.outs.push((To::Raise(e), s)) };
        Self::cut(&mut s, &f);
        if f.finally { s.unwind.push(Why::Reraise(e)); } else { s.stack.push(Entry::of(e)); }
        self.go(f.handler as usize, s);
    }
    fn cut(s: &mut State, f: &Try) { s.stack.truncate(f.depth[0] as usize); s.iters.truncate(f.depth[1] as usize); s.unwind.truncate(f.depth[2] as usize); }
    /* The innermost finally handler, popping the frames above it as the VM does. */
    fn cleanup(s: &mut State) -> Option<usize> {
        while let Some(f) = s.tries.pop() { Self::cut(s, &f); if f.finally { return Some(f.handler as usize); } }
        None
    }
    fn ret(&mut self, mut s: State, v: Ty) {
        match Self::cleanup(&mut s) { Some(h) => { s.unwind.push(Why::Return(v)); self.go(h, s) } None => self.outs.push((To::Ret(v), s)) }
    }
    /* Code nothing knows ran: it may raise anything and write any table. */
    fn unknown(&mut self, s: &State) { (self.eff.wild, self.eff.ran) = (true, true); self.raise(s, Ty::ANY); }
    fn give_up(&mut self) { (self.eff.gave_up, self.eff.wild) = (true, true); }

    /* Body `f` called with these arguments: its result, raised where it is called, None while the tree holds no context for them. */
    fn call(&mut self, s: &State, f: u32, args: Vec<Ty>) -> Option<Ty> {
        self.eff.ran = true;
        let at = self.t.ctxs.iter().position(|c| c.body == f && c.args.len() == args.len() && c.args.iter().zip(&args).all(|(a, b)| a.same(b)) && args.iter().zip(&c.args).all(|(a, b)| a.within(b)));
        let Some(i) = at else { self.eff.calls.push((f, args)); return None };
        // A generator's or coroutine's body runs as it is stepped, the call giving the object that steps it.
        let b = &self.p.bodies[f as usize];
        if b.chunk.is_generator || b.coro { return Some(self.p.tag(if b.coro { "coroutine" } else { "generator" }).with(Known::Gen(i as u32))); }
        self.raise(s, self.t.ctxs[i].raises);
        Some(self.t.ctxs[i].ret)
    }

    /* Calls user function `f` with values given by position (or not, `plain` false), each parameter by its slot and default. */
    fn user(&mut self, s: &State, f: u32, given: &[Ty], plain: bool) -> Option<Ty> {
        let ps = &self.p.bodies[f as usize].params;
        let bound = (plain && given.len() <= ps.len() && ps.iter().all(|p| p.0 == ParamKind::Normal)).then(|| {
            (0..ps.len()).map(|k| given.get(k).copied().or_else(|| ps[k].2.then(|| self.table(DEFAULTS | f, k as u32)).flatten())).collect::<Option<Vec<_>>>()
        }).flatten();
        let any = vec![Ty::ANY; ps.len()];
        self.call(s, f, bound.unwrap_or(any))
    }

    /* Calls `callee` with these values: each value it may return, or none when its rows already went on. */
    fn invoke(&mut self, s: &State, ip: usize, callee: Ty, a: &[Entry], plain: bool, then: usize) -> Vec<Ty> {
        let args: Vec<Ty> = a.iter().map(|e| e.ty).collect();
        let inst = |c, at| self.p.tag("instance").with(Known::Obj(c, at));
        match callee.known {
            Known::Body(c) if self.p.is(c, Kind::Class) => {
                let me = inst(c, self.site(ip));
                for init in self.lookup(c, "__init__") {
                    let Known::Body(f) = init.known else { self.unknown(s); continue };
                    if self.user(s, f, &[&[me][..], &args].concat(), plain).is_none() { return Vec::new(); }
                }
                vec![me]
            }
            Known::Body(f) if self.p.is(f, Kind::Function(0)) => self.user(s, f, &args, plain).into_iter().collect(),
            Known::Bound(f, c, at) => self.user(s, f, &[&[inst(c, at)][..], &args].concat(), plain).into_iter().collect(),
            Known::Meth(code, name, at) if plain => {
                let ins = [&[Entry::of(Ty::of(1 << code).with(if at == NO { Known::No } else { Known::Site(at) }))][..], a].concat();
                let rows = self.p.rules.rows(&alloc::format!("{}.{}", self.p.leaf(code), self.p.names[name as usize]), 0).into_iter().filter(|r| r.ins.len() == ins.len()).collect();
                self.rows(s, ip, rows, &ins, 1, then);
                Vec::new()
            }
            Known::Type(code) => { self.foreign(&args, true); self.raise(s, Ty::ANY); vec![Ty::of(1 << code)] }
            _ => { self.unknown(s); vec![Ty::ANY] }
        }
    }

    /* Values builtin code takes: an instance whose class defines a dunder past `__init__`, or a function or class it `calls`,
       runs user code nothing follows; a generator it steps runs its own body. */
    fn foreign(&mut self, vals: &[Ty], calls: bool) {
        let user = |v: &Ty| match v.known {
            Known::Obj(c, _) => self.mro(c).is_none_or(|all| self.t.tables.keys().any(|&(o, n)| o >> 28 == MEMBER >> 28 && all.contains(&(o & 0xFFF_FFFF)) && self.p.names[n as usize].starts_with("__") && self.p.names[n as usize] != "__init__")),
            _ => calls && (v.tags & (self.p.tag("function").tags | self.p.tag("type").tags) != 0 || matches!(v.known, Known::Body(_) | Known::Bound(..))),
        };
        self.eff.wild |= vals.iter().any(user);
        self.eff.ran |= vals.iter().any(|v| matches!(v.known, Known::Gen(_)));
    }

    /* The classes a class derives from, itself first, None past a base that is no class of the program nor object. */
    fn mro(&self, c: u32) -> Option<Vec<u32>> {
        let mut out = vec![c];
        let mut i = 0;
        while let Some(&k) = out.get(i) {
            for j in 0..self.p.bodies[k as usize].chunk.bases as u32 {
                match self.table(BASES | k, j)?.known {
                    Known::Body(b) if self.p.is(b, Kind::Class) => if !out.contains(&b) { out.push(b) },
                    Known::Type(t) if self.p.leaf(t) == "object" => {}
                    _ => return None,
                }
            }
            i += 1;
        }
        Some(out)
    }

    /* What attribute `name` of class `c` may be: the first class defining it on a single line of bases, each one when they branch, anything past an unknown base. */
    fn lookup(&self, c: u32, name: &str) -> Vec<Ty> {
        let Some(all) = self.mro(c) else { return vec![Ty::ANY] };
        let id = self.p.names.iter().position(|n| n == name).map_or(NO, |i| i as u32);
        let found = all.iter().filter_map(|&k| self.table(MEMBER | k, id));
        if all.iter().all(|&k| self.p.bodies[k as usize].chunk.bases <= 1) { found.take(1).collect() } else { found.collect() }
    }

    /* Each value attribute `name` of `o` may be. */
    fn attr(&mut self, s: &State, o: Ty, name: u32) -> Vec<Ty> {
        let n = self.p.names[name as usize].clone();
        self.raise(s, self.p.tag("AttributeError"));
        match o.known {
            Known::Obj(c, at) if self.mro(c).is_some() && self.lookup(c, "__getattr__").is_empty() && self.lookup(c, "__getattribute__").is_empty() => {
                let own = self.table(ATTR | c, name);
                self.lookup(c, &n).into_iter().map(|m| match m.known { Known::Body(f) if self.p.is(f, Kind::Function(0)) => self.p.tag("function").with(Known::Bound(f, c, at)), _ => m }).chain(own).collect()
            }
            Known::Body(c) if self.p.is(c, Kind::Class) => self.lookup(c, &n),
            _ if o.tags.count_ones() == 1 && !self.p.rules.rows(&alloc::format!("{}.{n}", self.p.leaf(o.tags.trailing_zeros() as u8)), 0).is_empty() =>
                vec![self.p.tag("function").with(Known::Meth(o.tags.trailing_zeros() as u8, name, if let Known::Site(x) = o.known { x } else { NO }))],
            _ => { self.unknown(s); vec![Ty::ANY] }
        }
    }

    /* The items of a container. */
    fn item(&self, v: &Ty) -> Ty {
        match v.known { Known::Site(s) | Known::Obj(_, s) => self.table(s, ITEM).unwrap_or(Ty::ANY), Known::Gen(x) => self.table(YIELDS | x, ITEM).unwrap_or(Ty::NONE), _ => Ty::ANY }
    }

    fn global(&self, name: u32) -> Ty { self.table(GLOBAL | self.body().module, name).unwrap_or(Ty::NONE).join(&self.p.builtin(name)) }

    /* Name `i` of the body: a slot of its own frame (the module's names too), a member of a class body or else the module's,
       the module's name from a function. A closure's cell is anything. Raises when it may be unbound. */
    fn load(&mut self, s: &mut State, i: u16) -> Option<Entry> {
        let b = self.body();
        let own = s.slots.get(i as usize).copied().unwrap_or(Ty::of(UNBOUND));
        let (ty, local) = match b.binds.get(i as usize).copied().unwrap_or(Binding::Local) {
            Binding::Local => (own, true),
            Binding::Global(_) if b.kind == Kind::Module => (own, true),
            Binding::Member if own.tags & UNBOUND != 0 => (own.minus(UNBOUND).join(&self.global(self.name(i))), false),
            Binding::Member => (own, false),
            Binding::Global(_) => (self.global(self.name(i)), false),
            Binding::Cell | Binding::Lent => (Ty::ANY, false),
        };
        if ty.tags & UNBOUND != 0 { self.raise(s, Ty::of(self.p.tag("NameError").tags | self.p.tag("UnboundLocalError").tags)); }
        let ty = ty.minus(UNBOUND);
        if ty.tags == 0 { return None; }
        if local { s.slots[i as usize] = ty; }
        Some(Entry { from: if local { i as u32 } else { NO }, ..Entry::of(ty) })
    }

    fn store(&mut self, s: &mut State, i: u16, v: Ty, global: bool) {
        let b = self.body();
        // What testing a value learned of this slot no longer holds once it is written.
        for e in s.stack.iter_mut().filter(|e| e.from == i as u32 || e.of.contains(&(i as u32))) { (e.from, e.cond, e.of) = (if e.from == i as u32 { NO } else { e.from }, NO, [NO; 3]); }
        match if global { Binding::Global(0) } else { b.binds.get(i as usize).copied().unwrap_or(Binding::Local) } {
            Binding::Global(_) => { self.write(GLOBAL | b.module, self.name(i), v); if b.kind == Kind::Module && !global { s.slots[i as usize] = v; } }
            Binding::Member => { self.write(MEMBER | self.me(), self.name(i), v); s.slots[i as usize] = v; }
            Binding::Local => s.slots[i as usize] = v,
            // What a closure's cell holds is never read back, only kept for the objects that leave through it.
            _ => self.write(CELL | b.module, self.name(i), v),
        }
    }

    /* Narrows slot `i` to what one side learned, false when nothing of it is left. */
    fn learn(&self, s: &mut State, i: u32, l: &Learn, of: &[u32; 3]) -> bool {
        let r = match l { Learn::Rel(_, Rhs::A(k)) => of.get(*k).and_then(|&f| s.slots.get(f as usize).copied()).unwrap_or(Ty::ANY), Learn::Rel(_, Rhs::N(n)) => Ty::int(0, *n, *n), _ => Ty::ANY };
        let Some(v) = s.slots.get_mut(i as usize) else { return true };
        match *l {
            Learn::In(t) => v.tags &= t,
            Learn::Rel("<", _) => v.hi = v.hi.min(r.hi.saturating_sub(1)),
            Learn::Rel("<=", _) => v.hi = v.hi.min(r.hi),
            Learn::Rel(">", _) => v.lo = v.lo.max(r.lo.saturating_add(1)),
            Learn::Rel(">=", _) => v.lo = v.lo.max(r.lo),
            Learn::Rel("==", _) => (v.lo, v.hi) = (v.lo.max(r.lo), v.hi.min(r.hi)),
            _ => {}
        }
        // No integer is left of it, only its values of other tags.
        if v.lo > v.hi { *v = Ty::of(v.tags & !(self.p.tag("int").tags | self.p.tag("bool").tags)).with(v.known); }
        v.tags != 0
    }

    /* Tests `e` for truth by the Truth rows: each side it may take, with what the rows that made the test and the value learn there. */
    fn truth(&mut self, s: &State, ip: usize, e: Entry, mut each: impl FnMut(&mut Self, State, bool)) {
        let at = self.outs.len();
        self.rows(s, ip, self.p.rules.rows("Truth", 0), &[e], 1, ip);
        let int = self.p.tag("int").tags | self.p.tag("bool").tags;
        for (to, mut s) in self.outs.split_off(at) {
            if !matches!(to, To::Ip(i) if i == ip) { self.outs.push((to, s)); continue; }
            let Some(b) = s.stack.pop() else { continue };
            for side in [true, false] {
                // An integer of known sign goes one way only.
                if !(b.ty.lo <= side as i64 && side as i64 <= b.ty.hi) || e.ty.tags & !int == 0 && (if side { e.ty.lo == 0 && e.ty.hi == 0 } else { e.ty.lo > 0 || e.ty.hi < 0 }) { continue; }
                let mut s = s.clone();
                let mut ok = true;
                for (c, test) in [(b, true), (e, false)] {
                    let Some(ins) = self.body().chunk.instructions.get((c.cond >> 8) as usize).filter(|_| c.cond != NO) else { continue };
                    let name = if test { String::from("Truth") } else { alloc::format!("{:?}", ins.opcode) };
                    let Some(row) = self.p.rules.rows(&name, ins.operand).get((c.cond & 0xFF) as usize).copied() else { continue };
                    for (_, a, l) in row.learn.iter().filter(|l| l.0 == side) { if let Some(&f) = c.of.get(*a).filter(|&&f| f != NO) { ok &= self.learn(&mut s, f, l, &c.of); } }
                }
                if ok { each(self, s, side); }
            }
        }
    }

    /* Value `v` narrowed to pattern `p`, None when it takes none of it. */
    fn pat(&self, p: &Pat, v: &Ty) -> Option<Ty> {
        let n = |t: &str, lo: i64, hi: i64| Some(Ty { tags: v.tags & if t.is_empty() { ALL } else { self.p.tag(t).tags }, lo: v.lo.max(lo), hi: v.hi.min(hi), known: v.known }).filter(|t| t.tags != 0 && t.lo <= t.hi);
        match *p {
            Pat::In(t) => n("", i64::MIN, i64::MAX).map(|v| Ty { tags: v.tags & t, ..v }).filter(|v| v.tags != 0),
            Pat::Type(c) => (v.known == Known::Type(c)).then_some(*v),
            Pat::Int(k) => n("int", k, k),
            Pat::None => n("NoneType", i64::MIN, i64::MAX),
            Pat::Bool(b) => n("bool", b as i64, b as i64),
        }
    }

    /* Whether the rows take every value of `a[j..]`, among the rows still in play. */
    fn covers(&self, rows: &[&Row], a: &[Ty], j: usize) -> bool {
        let Some(v) = a.get(j) else { return !rows.is_empty() };
        (0..127).filter(|c| v.tags >> c & 1 == 1).all(|c| {
            let one = Ty { tags: 1 << c, ..*v };
            self.covers(&rows.iter().copied().filter(|r| r.ins.get(j).and_then(|p| self.pat(p, &one)) == Some(one)).collect::<Vec<_>>(), a, j + 1)
        })
    }

    /* An opcode, method or truth test by its rows, each row it meets a path of its own on to `then`; values no row takes give anything. */
    fn rows(&mut self, s: &State, ip: usize, rows: Vec<&Row>, a: &[Entry], m: usize, then: usize) {
        let tys: Vec<Ty> = a.iter().map(|e| e.ty).collect();
        self.foreign(&tys, false);
        if !self.covers(&rows, &tys, 0) {
            // Their containers may change, and code may run through their objects and functions.
            for v in &tys { match v.known { Known::Site(c) | Known::Obj(_, c) if c != NO => self.write(c, ITEM, Ty::ANY), _ => self.eff.wild |= v.tags & (self.p.tag("instance").tags | self.p.tag("function").tags) != 0 } }
            self.raise(s, Ty::ANY);
            let mut s = s.clone();
            s.stack.extend((0..m).map(|_| Entry::of(Ty::ANY)));
            self.go(then, s);
        }
        for (k, row) in rows.iter().enumerate() {
            let Some(ins) = row.ins.iter().zip(&tys).map(|(p, v)| self.pat(p, v)).collect::<Option<Vec<Ty>>>().filter(|i| i.len() == tys.len()) else { continue };
            self.foreign(&row.calls.iter().map(|&j| ins[j]).collect::<Vec<_>>(), true);
            let mut s = s.clone();
            for (e, t) in a.iter().zip(&ins) { if let Some(v) = s.slots.get_mut(e.from as usize) { *v = *t; } }
            let b = |r: Rhs, lo: bool| match r { Rhs::N(n) => n as i128, Rhs::A(j) => if lo { ins[j].lo as i128 } else { ins[j].hi as i128 } };
            // The exact integer of result `r`, its bounds past an i64 open.
            let exact = |r: usize| row.says.iter().find_map(|x| match x { (q, Out::Exact(op, a, c)) if *q == r => Some((*op, *a, *c)), _ => None }).map(|(op, x, y)| {
                let f = |a: i128, c: i128| match op { '+' => a + c, '-' => a - c, '*' => a * c, _ => a };
                let ps = [f(b(x, true), b(y, true)), f(b(x, true), b(y, false)), f(b(x, false), b(y, true)), f(b(x, false), b(y, false))];
                (i64::try_from(*ps.iter().min().unwrap_or(&0)).unwrap_or(i64::MIN), i64::try_from(*ps.iter().max().unwrap_or(&0)).unwrap_or(i64::MAX))
            });
            match row.unless { Some((r, lo, hi)) if exact(r).is_some_and(|(a, b)| lo <= a && b <= hi) => {} _ => self.raise(&s, Ty::of(row.raises)) }
            let Some(outs) = &row.outs else { continue };
            for &(j, v) in &row.puts {
                let v = v.map_or(Ty::ANY, |v| ins[v]);
                match j.map(|j| ins[j].known) { Some(Known::Site(c) | Known::Obj(_, c)) if c != NO => self.write(c, ITEM, v), _ => self.eff.wild = true }
            }
            for (r, &tags) in outs.iter().enumerate() {
                let mut ty = Ty::of(tags);
                if let Some((lo, hi)) = exact(r) { (ty.lo, ty.hi) = (lo, hi); }
                for (_, say) in row.says.iter().filter(|x| x.0 == r) {
                    let made = match say {
                        Out::Range(lo, hi) => { (ty.lo, ty.hi) = (ty.lo.max(*lo), ty.hi.min(*hi)); continue }
                        Out::Item(j) => { let it = self.item(&ins[*j]); ty = Ty { tags: ty.tags & it.tags, lo: ty.lo.max(it.lo), hi: ty.hi.min(it.hi), known: it.known }; continue }
                        // Each item written apart, so an object among them leaves as itself.
                        Out::Items(js) => { for &j in js { self.write(self.site(ip), ITEM, ins[j]); } Ty::NONE }
                        Out::Holds(t, lo, hi) => Ty::int(*t, b(*lo, true) as i64, b(*hi, false) as i64),
                        Out::Exact(..) => continue,
                    };
                    ty.known = Known::Site(self.site(ip));
                    self.write(self.site(ip), ITEM, made);
                }
                if ty.tags == 0 { continue; }
                let learns = r == 0 && !row.learn.is_empty();
                s.stack.push(Entry { ty, from: NO, cond: if learns { (ip as u32) << 8 | k as u32 } else { NO }, of: core::array::from_fn(|j| a.get(j).filter(|_| learns).map_or(NO, |e| e.from)) });
            }
            self.go(then, s);
        }
    }

    /* Each way the instruction at `ip` goes from `s`. */
    pub fn step(&mut self, ip: usize, s: &State) {
        let chunk = self.body().chunk;
        let Some(&ins) = chunk.instructions.get(ip) else { return self.ret(s.clone(), self.p.konst(&Value::None)) };
        let (op, k, next) = (ins.opcode, ins.operand, ip + 1);
        let mut s = s.clone();
        macro_rules! pop { () => { match s.stack.pop() { Some(e) => e, None => return self.give_up() } } }
        match op {
            O::LoadConst => { let v = chunk.constants.get(k as usize).map_or(Ty::ANY, |c| self.p.konst(c)); self.push(next, &s, [v]) }
            O::LoadTrue | O::LoadFalse | O::LoadNone => { let v = self.p.konst(&if op == O::LoadNone { Value::None } else { Value::Bool(op == O::LoadTrue) }); self.push(next, &s, [v]) }
            O::LoadName => if let Some(e) = self.load(&mut s, k) { s.stack.push(e); self.go(next, s) },
            O::LoadGlobal => { let v = self.global(self.name(k)); if v.tags & UNBOUND != 0 { self.raise(&s, self.p.tag("NameError")); } self.push(next, &s, [v.minus(UNBOUND)]) }
            O::StoreName | O::StoreGlobal => { let v = pop!(); self.store(&mut s, k, v.ty, op == O::StoreGlobal); self.go(next, s) }
            O::Del => { self.store(&mut s, k, Ty::of(UNBOUND), false); self.go(next, s) }
            // Versions of one name share its slot, so a join of two of them moves nothing.
            O::Phi if chunk.phi_sources.get(chunk.phi_map.get(ip).copied().unwrap_or(0)).is_some_and(|&(a, b)| a == k && b == k) => self.go(next, s),
            O::Phi => self.give_up(),
            O::Global | O::Nonlocal | O::BeginArgs => self.go(next, s),
            O::PopTop => { pop!(); self.go(next, s) }
            O::Dup => { let e = pop!(); s.stack.extend([e, e]); self.go(next, s) }
            O::Dup2 => { let (y, x) = (pop!(), pop!()); s.stack.extend([x, y, x, y]); self.go(next, s) }
            O::Swap => { let (y, x) = (pop!(), pop!()); s.stack.extend([y, x]); self.go(next, s) }
            O::Rot3 => { let (z, y, x) = (pop!(), pop!(), pop!()); s.stack.extend([y, z, x]); self.go(next, s) }
            O::Jump => self.go(k as usize, s),
            O::JumpIfFalse | O::JumpIfFalseOrPop | O::JumpIfTrueOrPop | O::Assert => {
                let e = pop!();
                self.truth(&s, ip, e, |me, mut s, side| match op {
                    O::JumpIfFalse => me.go(if side { next } else { k as usize }, s),
                    O::Assert => if side { me.go(next, s) } else { let a = me.p.tag("AssertionError"); me.raise(&s, a) },
                    _ if side == (op == O::JumpIfTrueOrPop) => { s.stack.push(Entry::of(e.ty)); me.go(k as usize, s) }
                    _ => me.go(next, s),
                })
            }
            O::GetIter if k != crate::parser::ITER_VALUE => {
                let (e, at) = (pop!(), self.outs.len());
                self.rows(&s, ip, self.p.rules.rows("GetIter", k), &[e], 0, next);
                // The iterated value moves to the loop on each way the rows go on.
                for (to, st) in &mut self.outs[at..] { if matches!(to, To::Ip(t) if *t == next) { st.iters.push(e.ty); } }
            }
            O::ForIter => {
                let Some(&it) = s.iters.last() else { return self.give_up() };
                if let Known::Gen(g) = it.known { self.eff.ran = true; self.raise(&s, self.t.ctxs[g as usize].raises); }
                self.push(next, &s, [self.item(&it)]);
                s.iters.pop();
                self.go(k as usize, s)
            }
            O::PopIter => { s.iters.pop(); self.go(next, s) }
            O::SetupExcept | O::SetupFinally => { s.tries.push(Try { handler: k, finally: op == O::SetupFinally, depth: [s.stack.len() as u16, s.iters.len() as u16, s.unwind.len() as u16] }); self.go(next, s) }
            O::PopExcept => { s.tries.pop(); self.go(next, s) }
            O::BeginFinally => { s.unwind.push(Why::Normal); self.go(next, s) }
            O::EndFinally => match s.unwind.pop() {
                None | Some(Why::Normal) => self.go(next, s),
                Some(Why::Return(v)) => self.ret(s, v),
                Some(Why::Goto(t, n)) => match if n > 0 { Self::cleanup(&mut s) } else { None } { Some(h) => { s.unwind.push(Why::Goto(t, n - 1)); self.go(h, s) } None => self.go(t as usize, s) },
                Some(Why::Reraise(e)) => self.raise(&s, e),
            },
            O::UnwindFinally => {
                let mut t = s.clone();
                match if k > 0 { Self::cleanup(&mut t) } else { None } { Some(h) => { t.unwind.push(Why::Goto(next as u16, k - 1)); self.go(h, t) } None => self.go(next, s) }
            }
            O::ReturnValue => { let v = s.stack.pop().map_or(self.p.konst(&Value::None), |e| e.ty); self.ret(s, v) }
            O::Raise | O::RaiseFrom => {
                if op == O::RaiseFrom { pop!(); }
                // A bare raise raises again whatever is being handled.
                let e = if op == O::Raise && k == 1 { Ty::ANY } else {
                    let v = pop!().ty;
                    match v.known { Known::Body(c) => self.p.tag("instance").with(Known::Obj(c, self.site(ip))), Known::Type(t) => Ty::of(1 << t), Known::Obj(..) => v, _ => Ty::ANY }
                };
                self.raise(&s, e.join(&self.p.tag("TypeError")))
            }
            O::Yield => {
                let v = pop!();
                self.write(YIELDS | self.x as u32, ITEM, v.ty);
                self.raise(&s, self.p.tag("GeneratorExit"));
                self.push(next, &s, [Ty::ANY])
            }
            O::MakeFunction | O::MakeCoroutine | O::MakeClass => {
                let class = op == O::MakeClass;
                let id = if class { &self.body().classes } else { &self.body().fns }.get(k as usize).copied();
                let (Some((n, _)), Some(id)) = (crate::parser::effect(&ins, chunk), id) else { return self.give_up() };
                let Some(at) = s.stack.len().checked_sub(n as usize) else { return self.give_up() };
                let dflt: Vec<u32> = self.p.bodies[id as usize].params.iter().enumerate().filter(|p| p.1.2).map(|p| p.0 as u32).collect();
                for (j, e) in s.stack.split_off(at).into_iter().enumerate() { self.write(if class { BASES } else { DEFAULTS } | id, if class { j as u32 } else { dflt.get(j).copied().unwrap_or(j as u32) }, e.ty); }
                if class && self.p.bodies[id as usize].chunk.class_kw > 0 { self.unknown(&s); }
                if class && self.call(&s, id, Vec::new()).is_none() { return; }
                self.push(next, &s, [self.p.tag(if class { "type" } else { "function" }).with(Known::Body(id))])
            }
            // An imported module and a `with` block's manager are anything, and so what reading or calling them gives.
            O::LoadModule => self.push(next, &s, [Ty::ANY]),
            O::WithEnter | O::WithExit => { if op == O::WithEnter { pop!(); } s.stack.extend([Entry::of(Ty::ANY); 5].into_iter().take(if op == O::WithEnter { 2 } else { 5 })); self.go(next, s) }
            O::StoreAttr => {
                let (v, o) = (pop!(), pop!());
                match o.ty.known {
                    Known::Obj(c, _) if self.mro(c).is_some() && self.lookup(c, "__setattr__").is_empty() => self.write(ATTR | c, self.name(k), v.ty),
                    _ => self.unknown(&s),
                }
                self.go(next, s)
            }
            O::LoadAttr => { let o = pop!(); let got = self.attr(&s, o.ty, self.name(k)); self.push(next, &s, got) }
            O::Call | O::CallSpread => {
                let Some(at) = crate::parser::effect(&ins, chunk).and_then(|(n, _)| s.stack.len().checked_sub(n as usize)) else { return self.give_up() };
                let a = s.stack.split_off(at);
                let rets = self.invoke(&s, ip, a[0].ty, &a[1..], op == O::Call && k >> 8 == 0, next);
                self.push(next, &s, rets);
            }
            O::WithJudge => {
                let r = pop!();
                if !matches!(s.unwind.last(), Some(Why::Reraise(_))) { return self.go(next, s); }
                self.truth(&s, ip, r, |me, mut s, side| { if side && let Some(w) = s.unwind.last_mut() { *w = Why::Normal; } me.go(next, s) })
            }
            _ => {
                let Some((n, m)) = crate::parser::effect(&ins, chunk) else { return self.give_up() };
                let Some(at) = s.stack.len().checked_sub(n as usize) else { return self.give_up() };
                let a = s.stack.split_off(at);
                self.rows(&s, ip, self.p.rules.rows(&alloc::format!("{op:?}"), k), &a, m as usize, next)
            }
        }
        // Past code that may write the module's names, each may hold what any code stores in it.
        let b = self.body();
        if !self.eff.ran || b.kind != Kind::Module { return; }
        for (_, st) in self.outs.iter_mut() {
            for (v, i) in st.slots.iter_mut().zip(0..).filter(|(_, i)| matches!(b.binds.get(*i), Some(Binding::Global(_)))) {
                *v = if self.t.wild { Ty::ANY } else { v.join(&self.t.tables.get(&(GLOBAL | b.module, b.names[i])).copied().unwrap_or(Ty::NONE)) };
            }
        }
    }
}

/* The state a context starts in: its parameters bound to its arguments, every other slot unbound, or a builtin in a module. */
pub fn entry(p: &Prog, body: u32, args: &[Ty]) -> State {
    let b = &p.bodies[body as usize];
    let mut slots: Vec<Ty> = b.names.iter().map(|&n| if b.kind == Kind::Module { p.builtin(n) } else { Ty::of(UNBOUND) }).collect();
    for (k, &(_, slot, _)) in b.params.iter().enumerate() {
        if let Some(v) = slots.get_mut(b.chunk.canonical.get(slot).map_or(slot, |&c| c as usize)) { *v = args.get(k).copied().unwrap_or(Ty::ANY); }
    }
    State { slots, ..Default::default() }
}
