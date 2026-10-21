/* The reviewer: one step of the machine on a typed state, by the rules of `edge.ept` and the VM's own control, and the
   check that a tree is closed under it, so a tree it accepts holds every path the program can take. */

use alloc::{string::String, vec, vec::Vec};
use crate::code::{Binding, BodyKind, ParamKind, Program};
use crate::parser::{ImportKind, OpCode as O, SSAChunk, Value, ssa_strip};
use crate::util::hash::FxHashMap as Map;
use super::read::{Exact, Learn, Pat, Rhs, Row, Rules};
use super::tree::Tree;
use super::*;

// Not a runtime type: the code a slot holds while unbound.
pub const UNBOUND: u8 = 255;
// The owners of table entries by kind in the top bits, a site (body << 16 | ip) or a body below them.
pub const GLOBAL: u32 = 1 << 28;
pub const CELL: u32 = 2 << 28;
pub const MEMBER: u32 = 3 << 28;
pub const ATTR: u32 = 4 << 28;
pub const YIELDS: u32 = 5 << 28;
pub const DEFAULTS: u32 = 6 << 28;
pub const BASES: u32 = 7 << 28;
const KIND: u32 = 0xF << 28;
pub const ITEM: u32 = u32::MAX;

/* One body of the program as the tree reads it. */
pub struct Body<'c> {
    pub chunk: &'c SSAChunk,
    pub kind: BodyKind,
    pub binds: Vec<Binding>,
    pub module: u32,
    // The bodies its MakeFunction and MakeClass operands make, and the program-wide id of each of its names.
    pub fns: Vec<u32>,
    pub classes: Vec<u32>,
    pub names: Vec<u32>,
    // Each parameter's kind, slot and whether it takes a default.
    pub params: Vec<(ParamKind, usize, bool)>,
    // Where a back edge lands, the only places the tree widens integers.
    pub heads: Vec<bool>,
}

pub struct Prog<'c> {
    pub rules: &'c Rules,
    pub bodies: Vec<Body<'c>>,
    pub names: Vec<String>,
}

fn walk<'c>(p: &mut Prog<'c>, vm: &Program<'c>, c: &'c SSAChunk, module: u32, defaults: &[String], ids: &mut Map<String, u32>) -> u32 {
    let me = p.bodies.len() as u32;
    let (kind, binds) = vm.bindings_of(c).map_or((BodyKind::Module, Vec::new()), |(k, b)| (k, b.to_vec()));
    let module = if kind == BodyKind::Module { me } else { module };
    let names = c.names.iter().map(|n| { let n = ssa_strip(n); let k = ids.len() as u32; *ids.entry(n.into()).or_insert_with(|| { p.names.push(n.into()); k }) }).collect();
    let mut heads = vec![false; c.instructions.len() + 1];
    for (i, x) in c.instructions.iter().enumerate() { if x.opcode == O::Jump && (x.operand as usize) <= i { heads[x.operand as usize] = true; } }
    let params = match kind { BodyKind::Function(fi) => vm.params_of(fi).iter().zip(defaults).map(|(&(k, s), d)| (k, s, d.ends_with('='))).collect(), _ => Vec::new() };
    p.bodies.push(Body { chunk: c, kind, binds, module, fns: Vec::new(), classes: Vec::new(), names, params, heads });
    let fns = c.functions.iter().map(|f| walk(p, vm, &f.1, module, &f.0, ids)).collect();
    let classes = c.classes.iter().map(|k| walk(p, vm, k, module, &[], ids)).collect();
    for e in &c.imports { if let ImportKind::Code(sub) = &e.kind && !p.bodies.iter().any(|b| core::ptr::eq(b.chunk, &**sub)) { walk(p, vm, sub, module, &[], ids); } }
    let b = &mut p.bodies[me as usize];
    (b.fns, b.classes) = (fns, classes);
    me
}

impl<'c> Prog<'c> {
    pub fn new(chunk: &'c SSAChunk, rules: &'c Rules) -> Self {
        let mut p = Prog { rules, bodies: Vec::new(), names: Vec::new() };
        walk(&mut p, &Program::new(chunk), chunk, 0, &[], &mut Map::default());
        p
    }
    // The codes of a tag, every code when the file has no such tag.
    pub fn tag(&self, name: &str) -> Tags { self.rules.tag(name).unwrap_or(Tags::ALL) }
    fn code(&self, name: &str) -> Option<u8> { self.rules.tags.iter().find(|t| t.0 == name && t.1 == t.2).map(|t| t.1) }
    fn leaf(&self, c: u8) -> &str { self.rules.tags.iter().find(|t| t.1 == c && t.2 == c).map_or("", |t| &t.0) }
    pub fn exc(&self, name: &str) -> Ty { Ty::of(self.tag(name)) }
    fn name(&self, b: u32, i: u16) -> u32 { self.bodies[b as usize].names.get(i as usize).copied().unwrap_or(NO) }
    fn id(&self, n: &str) -> u32 { self.names.iter().position(|m| m == n).map_or(NO, |i| i as u32) }
    fn is_fn(&self, b: u32) -> bool { matches!(self.bodies[b as usize].kind, BodyKind::Function(_)) }
    fn is_class(&self, b: u32) -> bool { self.bodies[b as usize].kind == BodyKind::Class }
    /* What a name no code of its module binds is: a builtin type or function, else unbound. */
    pub fn builtin(&self, name: u32) -> Ty {
        let n = self.names.get(name as usize).map_or("", String::as_str);
        match self.code(n).filter(|_| crate::parser::builtin_type(n).is_some()) {
            Some(c) => Ty::of(self.tag("type")).with(Known::Type(c)),
            None if crate::value::NativeFnId::from_name(n).is_some() => Ty::of(self.tag("builtin_function_or_method")),
            None => Ty::of(unb()),
        }
    }
    pub fn konst(&self, v: &Value) -> Ty {
        let (n, r) = match v {
            Value::Int(i) => ("int", Some(*i)),
            Value::BigInt(_) => ("int", None),
            Value::Bool(b) => ("bool", Some(*b as i64)),
            Value::Float(_) => ("float", None),
            Value::Imag(_) => ("complex", None),
            Value::Str(_) => ("str", None),
            Value::Bytes(_) => ("bytes", None),
            Value::None => ("NoneType", None),
        };
        r.map_or(Ty::of(self.tag(n)), |i| Ty::int(self.tag(n), i, i))
    }
}

pub fn unb() -> Tags { Tags::range(UNBOUND, UNBOUND) }

/* Where a step goes. */
#[derive(Clone, Debug)]
pub enum To { Ip(usize), Ret(Ty), Raise(Ty) }

/* What a step does beside its states: entries it joins into the tables, calls it needs a context for, whether code nothing knows or any code ran. */
#[derive(Default, Debug)]
pub struct Eff { pub writes: Vec<(u32, u32, Ty)>, pub calls: Vec<(u32, Vec<Ty>)>, pub wild: bool, pub gave_up: bool, pub ran: bool }

pub struct Step<'a, 'c> { pub p: &'a Prog<'c>, pub t: &'a Tree, pub x: usize, pub outs: Vec<(To, State)>, pub eff: Eff }

impl<'a, 'c> Step<'a, 'c> {
    pub fn new(p: &'a Prog<'c>, t: &'a Tree, x: usize) -> Self { Self { p, t, x, outs: Vec::new(), eff: Eff::default() } }
    fn me(&self) -> u32 { self.t.ctxs[self.x].body }
    fn body(&self) -> &'a Body<'c> { &self.p.bodies[self.me() as usize] }
    fn site(&self, ip: usize) -> u32 { (self.me() << 16) | ip as u32 }
    fn table(&self, owner: u32, field: u32) -> Option<Ty> {
        if self.t.wild && !matches!(owner & KIND, DEFAULTS | BASES) { return Some(Ty::ANY); }
        self.t.tables.get(&(owner, field)).copied()
    }
    fn write(&mut self, owner: u32, field: u32, ty: Ty) { self.eff.writes.push((owner, field, ty)); }
    fn go(&mut self, ip: usize, s: State) { self.outs.push((To::Ip(ip), s)); }

    /* The exception `e` rises here: to the innermost handler, or out of the body. */
    fn raise(&mut self, s: &State, e: Ty) {
        if e.tags.is_empty() { return; }
        let mut s = s.clone();
        let Some(f) = s.tries.pop() else { return self.outs.push((To::Raise(e), s)) };
        Self::cut(&mut s, &f);
        if f.finally { s.unwind.push(Why::Reraise(e)); } else { s.stack.push(Entry::of(e)); }
        self.go(f.handler as usize, s);
    }

    fn cut(s: &mut State, f: &Try) {
        s.stack.truncate(f.stack as usize);
        s.iters.truncate(f.iters as usize);
        s.withs.truncate(f.withs as usize);
        s.unwind.truncate(f.unwind as usize);
    }

    /* The innermost finally handler, popping the frames above it as the VM does. */
    fn cleanup(s: &mut State) -> Option<usize> {
        while let Some(f) = s.tries.pop() { Self::cut(s, &f); if f.finally { return Some(f.handler as usize); } }
        None
    }

    fn ret(&mut self, mut s: State, v: Ty) {
        match Self::cleanup(&mut s) { Some(h) => { s.unwind.push(Why::Return(v)); self.go(h, s) } None => self.outs.push((To::Ret(v), s)) }
    }

    /* A call of user body `f` with these arguments, its result and exceptions, None while the tree holds no context for them. */
    fn call(&mut self, f: u32, args: Vec<Ty>) -> Option<(Ty, Ty)> {
        self.eff.ran = true;
        let at = self.t.ctxs.iter().position(|c| c.body == f && c.args.len() == args.len() && c.args.iter().zip(&args).all(|(a, b)| a.same(b)));
        match at {
            Some(i) if args.iter().zip(&self.t.ctxs[i].args).all(|(a, b)| a.within(b)) => {
                let c = &self.t.ctxs[i];
                let made = self.p.bodies[f as usize].chunk.is_generator;
                Some(if made { (Ty::of(self.p.tag("generator")).with(Known::Gen(i as u32)), Ty::NONE) } else { (c.ret, c.raises) })
            }
            _ => { self.eff.calls.push((f, args)); None }
        }
    }

    /* The arguments of body `f` from values given by position, None when it binds them any other way. */
    fn bind(&self, f: u32, given: &[Ty]) -> Option<Vec<Ty>> {
        let ps = &self.p.bodies[f as usize].params;
        if ps.iter().any(|p| p.0 != ParamKind::Normal) || given.len() > ps.len() { return None; }
        let mut out = given.to_vec();
        for k in given.len()..ps.len() { out.push(if ps[k].2 { self.table(DEFAULTS | f, k as u32)? } else { return None }); }
        Some(out)
    }

    fn user(&mut self, s: &State, f: u32, args: &[Ty], plain: bool) -> Option<Ty> {
        let n = self.p.bodies[f as usize].params.len();
        let args = plain.then(|| self.bind(f, args)).flatten().unwrap_or(vec![Ty::ANY; n]);
        let (r, e) = self.call(f, args)?;
        self.raise(s, e);
        Some(r)
    }

    /* Calls `callee` with these values, by position or not: each value it may return. */
    fn invoke(&mut self, s: &State, ip: usize, callee: Ty, a: &[Entry], plain: bool, then: usize) -> Vec<Ty> {
        let args: Vec<Ty> = a.iter().map(|e| e.ty).collect();
        let inst = |me: &Self, c, at| Ty::of(me.p.tag("instance")).with(Known::Obj(c, at));
        match callee.known {
            Known::Body(c) if self.p.is_class(c) => {
                let me = inst(self, c, self.site(ip));
                for init in self.lookup(c, "__init__") {
                    let Known::Body(f) = init.known else { self.unknown(s); continue };
                    if self.user(s, f, &[&[me][..], &args].concat(), plain).is_none() { return Vec::new(); }
                }
                vec![me]
            }
            Known::Body(f) if self.p.is_fn(f) => self.user(s, f, &args, plain).into_iter().collect(),
            Known::Bound(f, c, at) => self.user(s, f, &[&[inst(self, c, at)][..], &args].concat(), plain).into_iter().collect(),
            Known::Meth(code, name, at) if plain => {
                let recv = Entry::of(Ty::of(Tags::range(code, code)).with(if at == NO { Known::No } else { Known::Site(at) }));
                let ins = [&[recv][..], a].concat();
                let rows: Vec<&Row> = self.p.rules.method(self.p.leaf(code), &self.p.names[name as usize]).filter(|r| r.ins.len() == ins.len()).collect();
                self.rows(s, ip, rows, &ins, 1, Some(then));
                Vec::new()
            }
            Known::Type(code) => { self.foreign(&args); self.raise(s, Ty::ANY); vec![Ty::of(Tags::range(code, code))] }
            _ => { self.foreign(&args); self.unknown(s); vec![Ty::ANY] }
        }
    }

    /* Code nothing knows ran: it may raise anything and write any table. */
    fn unknown(&mut self, s: &State) { self.eff.wild = true; self.eff.ran = true; self.raise(s, Ty::ANY); }

    /* Values builtin code takes, which may call their functions and the dunders of their instances with anything. */
    fn foreign(&mut self, vals: &[Ty]) {
        for v in vals {
            let run = |me: &mut Self, f: u32, first: Option<Ty>| {
                let n = me.p.bodies[f as usize].params.len();
                let mut a = vec![Ty::ANY; n];
                if let (Some(x), Some(f0)) = (first, a.first_mut()) { *f0 = x; }
                let _ = me.call(f, a);
            };
            match v.known {
                Known::Body(f) if self.p.is_fn(f) => run(self, f, None),
                Known::Bound(f, c, at) => run(self, f, Some(Ty::of(self.p.tag("instance")).with(Known::Obj(c, at)))),
                Known::Obj(c, _) => for (n, m) in self.members(c) {
                    if let Known::Body(f) = m.known && self.p.is_fn(f) && self.p.names[n as usize].starts_with("__") { run(self, f, Some(*v)); }
                },
                _ => {}
            }
        }
    }

    /* The classes a class derives from, itself first, None past a base that is no class of the program nor object. */
    fn mro(&self, c: u32) -> Option<Vec<u32>> {
        let mut out = vec![c];
        let mut i = 0;
        while let Some(&k) = out.get(i) {
            let n = self.p.bodies[k as usize].chunk.bases;
            if n == crate::parser::SPREAD_BASES { return None; }
            for j in 0..n as u32 {
                match self.table(BASES | k, j)?.known {
                    Known::Body(b) if self.p.is_class(b) => if !out.contains(&b) { out.push(b) },
                    Known::Type(t) if Some(t) == self.p.code("object") => {}
                    _ => return None,
                }
            }
            i += 1;
        }
        Some(out)
    }

    fn members(&self, c: u32) -> Vec<(u32, Ty)> {
        let all = self.mro(c).unwrap_or_default();
        self.t.tables.iter().filter(|((o, _), _)| o & KIND == MEMBER && all.contains(&(o & !KIND))).map(|((_, n), t)| (*n, *t)).collect()
    }

    /* What attribute `name` of class `c` may be: the first class on a single line of bases defining it, every one when the bases branch, anything past an unknown base. */
    fn lookup(&self, c: u32, name: &str) -> Vec<Ty> {
        let Some(all) = self.mro(c) else { return vec![Ty::ANY] };
        let id = self.p.id(name);
        let found: Vec<Ty> = all.iter().filter_map(|&k| self.table(MEMBER | k, id)).collect();
        if all.iter().all(|&k| self.p.bodies[k as usize].chunk.bases <= 1) { found.into_iter().take(1).collect() } else { found }
    }

    /* Each value attribute `name` of `o` may be, raising what reading it may raise. */
    fn attr(&mut self, s: &State, o: Ty, name: u32) -> Vec<Ty> {
        let n = self.p.names[name as usize].clone();
        let mut got = Vec::new();
        match o.known {
            Known::Obj(c, at) if self.mro(c).is_some() && self.lookup(c, "__getattr__").is_empty() && self.lookup(c, "__getattribute__").is_empty() => {
                got.extend(self.table(ATTR | c, name));
                for m in self.lookup(c, &n) { got.push(match m.known { Known::Body(f) if self.p.is_fn(f) => Ty::of(self.p.tag("method")).with(Known::Bound(f, c, at)), _ => m }); }
            }
            Known::Body(c) if self.p.is_class(c) => got = self.lookup(c, &n),
            Known::Module(m) => got.push(self.table(GLOBAL | m, name).unwrap_or(Ty::NONE)),
            _ => match o.tags.runs()[..] {
                [(c, d)] if c == d && self.p.rules.method(self.p.leaf(c), &n).next().is_some() =>
                    got.push(Ty::of(self.p.tag("builtin_function_or_method")).with(Known::Meth(c, name, match o.known { Known::Site(x) => x, _ => NO }))),
                _ => { self.foreign(&[o]); self.unknown(s); got.push(Ty::ANY) }
            },
        }
        if !matches!(o.known, Known::Meth(..)) { self.raise(s, self.p.exc("AttributeError")); }
        got.into_iter().filter(|t| !t.tags.is_empty()).collect()
    }

    /* The type of the items of a container. */
    fn item(&self, v: &Ty) -> Ty {
        match v.known {
            Known::Site(s) | Known::Obj(_, s) => self.table(s, ITEM).unwrap_or(Ty::ANY),
            Known::Gen(x) => self.table(YIELDS | x, ITEM).unwrap_or(Ty::NONE),
            _ => Ty::ANY,
        }
    }

    fn global(&self, module: u32, name: u32) -> Ty { self.table(GLOBAL | module, name).unwrap_or(Ty::NONE).join(&self.p.builtin(name)) }

    /* Reads name `i` of the body, raising when it may be unbound. */
    fn load(&mut self, s: &mut State, i: u16) -> Option<Entry> {
        let b = self.body();
        let name = self.p.name(self.me(), i);
        let bind = b.binds.get(i as usize).copied().unwrap_or(Binding::Local);
        let slot = s.slots.get(i as usize).copied().unwrap_or(Ty::of(unb()));
        let ty = match bind {
            Binding::Local | Binding::Member => slot,
            Binding::Global(_) if b.kind == BodyKind::Module => slot,
            Binding::Global(_) => self.global(b.module, name),
            Binding::Cell => self.table(CELL | b.module, name).unwrap_or(Ty::NONE).join(&Ty::of(unb())),
            Binding::Lent => Ty::ANY,
        };
        // A class body reads the module's name where its own is unbound.
        let ty = if bind == Binding::Member && ty.tags.meets(unb()) { Ty { tags: ty.tags.minus(unb()), ..ty }.join(&self.global(b.module, name)) } else { ty };
        if ty.tags.meets(unb()) { self.raise(s, self.p.exc("NameError")); }
        let ty = Ty { tags: ty.tags.minus(unb()), ..ty };
        if ty.tags.is_empty() { return None; }
        let local = bind == Binding::Local || b.kind == BodyKind::Module && matches!(bind, Binding::Global(_));
        if local && let Some(v) = s.slots.get_mut(i as usize) { *v = ty; }
        Some(Entry { ty, from: if local { i as u32 } else { NO }, ..Entry::of(ty) })
    }

    fn store(&mut self, s: &mut State, i: u16, v: Ty, global: bool) {
        let b = self.body();
        let name = self.p.name(self.me(), i);
        // What testing a value learns of this slot no longer holds once it is written.
        for e in s.stack.iter_mut() { if e.from == i as u32 { e.from = NO; } if e.of.contains(&(i as u32)) { e.cond = NO; e.of = [NO; 3]; } }
        let bind = if global { Binding::Global(0) } else { b.binds.get(i as usize).copied().unwrap_or(Binding::Local) };
        match bind {
            Binding::Global(_) => { self.write(GLOBAL | b.module, name, v); if b.kind == BodyKind::Module && !global { s.slots[i as usize] = v; } }
            Binding::Cell => self.write(CELL | b.module, name, v),
            Binding::Member => { self.write(MEMBER | self.me(), name, v); s.slots[i as usize] = v; }
            _ => if let Some(x) = s.slots.get_mut(i as usize) { *x = v },
        }
    }

    /* Narrows slot `i` to what a branch learned, false when nothing of it is left. */
    fn learn(&self, s: &mut State, i: u32, l: &Learn, rhs: Option<Ty>) -> bool {
        let Some(v) = s.slots.get_mut(i as usize) else { return true };
        match (l, rhs) {
            (Learn::In(t), _) => v.tags = v.tags & *t,
            (Learn::Rel(rel, _), Some(r)) => {
                match *rel {
                    "<" => v.hi = v.hi.min(r.hi.saturating_sub(1)),
                    "<=" => v.hi = v.hi.min(r.hi),
                    ">" => v.lo = v.lo.max(r.lo.saturating_add(1)),
                    ">=" => v.lo = v.lo.max(r.lo),
                    "==" => (v.lo, v.hi) = (v.lo.max(r.lo), v.hi.min(r.hi)),
                    _ => {}
                }
                // No integer is left, only the values of other tags.
                if v.lo > v.hi { v.tags = v.tags.minus(self.p.tag("int") | self.p.tag("bool")); (v.lo, v.hi) = (i64::MIN, i64::MAX); }
            }
            _ => {}
        }
        !v.tags.is_empty()
    }

    /* Tests `e` for truth, each way it may go with what that way learns. */
    fn truth(&mut self, s: &State, e: Entry, mut each: impl FnMut(&mut Self, State, bool)) {
        let any = Row { ins: vec![Pat::In(Tags::ALL)], ..Default::default() };
        let mut rows: Vec<&Row> = self.p.rules.truth.iter().collect();
        if !self.covers(&rows, &[e.ty], 0) { rows.push(&any); }
        let (int, none) = (self.p.tag("int") | self.p.tag("bool"), self.p.tag("NoneType"));
        for row in rows {
            let Some(ty) = self.pat(&row.ins[0], &e.ty) else { continue };
            for side in [true, false] {
                // An int or bool of known value, or None, goes one way only.
                if ty.tags.within(int) && (side && ty.lo == 0 && ty.hi == 0 || !side && (ty.lo > 0 || ty.hi < 0)) || ty.tags.within(none) && side { continue; }
                let mut s = s.clone();
                let mut ok = true;
                if e.from != NO && let Some(v) = s.slots.get_mut(e.from as usize) { v.tags = v.tags & ty.tags; }
                for l in row.learn.iter().filter(|l| l.0 == side && l.1 == 0) { if e.from != NO { ok &= self.learn(&mut s, e.from, &l.2, None); } }
                // What the row that made the value learns of its operands on this side.
                let made = self.body().chunk.instructions.get((e.cond >> 8) as usize).filter(|i| e.cond != NO && !matches!(i.opcode, O::Call | O::CallMethod));
                if let Some(ins) = made {
                    if let Some(r) = self.p.rules.rows(ins.opcode, ins.operand).nth((e.cond & 0xFF) as usize) {
                        for (_, a, l) in r.learn.iter().filter(|l| l.0 == side) {
                            let rhs = match l { Learn::Rel(_, Rhs::A(k)) => e.of.get(*k).and_then(|&f| s.slots.get(f as usize).copied()), Learn::Rel(_, Rhs::N(n)) => Some(Ty::int(Tags::NONE, *n, *n)), _ => None };
                            if let Some(&f) = e.of.get(*a) && f != NO { ok &= self.learn(&mut s, f, l, rhs); }
                        }
                    }
                }
                if ok { each(self, s, side); }
            }
        }
    }

    /* Value `v` narrowed to pattern `p`, None when it takes none of it. */
    fn pat(&self, p: &Pat, v: &Ty) -> Option<Ty> {
        let n = |t: Tags, lo: i64, hi: i64| Some(Ty { tags: v.tags & t, lo: v.lo.max(lo), hi: v.hi.min(hi), known: v.known }).filter(|t| !t.tags.is_empty() && t.lo <= t.hi);
        match *p {
            Pat::In(t) => n(t, i64::MIN, i64::MAX),
            Pat::Type(c) => (v.known == Known::Type(c)).then_some(*v),
            Pat::Int(k) => n(self.p.tag("int"), k, k),
            Pat::None => n(self.p.tag("NoneType"), i64::MIN, i64::MAX),
            Pat::Bool(b) => n(self.p.tag("bool"), b as i64, b as i64),
        }
    }

    /* Whether the rows take every value of `a[j..]`, among the rows still in play. */
    fn covers(&self, rows: &[&Row], a: &[Ty], j: usize) -> bool {
        let Some(v) = a.get(j) else { return !rows.is_empty() };
        (0..UNBOUND).filter(|&c| v.tags.meets(Tags::range(c, c))).all(|c| {
            let one = Ty { tags: Tags::range(c, c), ..*v };
            let rs: Vec<&Row> = rows.iter().copied().filter(|r| r.ins.get(j).and_then(|p| self.pat(p, &one)).is_some_and(|t| t == one)).collect();
            self.covers(&rs, a, j + 1)
        })
    }

    /* An opcode or method by its rows, each row it meets a path of its own, going on to `then` (or to after a call). */
    fn rows(&mut self, s: &State, ip: usize, rows: Vec<&Row>, a: &[Entry], m: usize, then: Option<usize>) {
        let then = then.unwrap_or(ip + 1);
        let tys: Vec<Ty> = a.iter().map(|e| e.ty).collect();
        self.foreign(&tys);
        if !self.covers(&rows, &tys, 0) {
            let mut s = s.clone();
            // Values no row takes: anything comes out, user code may run through their objects and functions, and their containers may change.
            let user = self.p.tag("instance") | self.p.tag("function") | self.p.tag("method");
            for v in &tys {
                match v.known { Known::Site(c) | Known::Obj(_, c) => self.write(c, ITEM, Ty::ANY), _ => self.eff.wild |= v.tags.meets(user) }
            }
            self.raise(&s, Ty::ANY);
            s.stack.extend((0..m).map(|_| Entry::of(Ty::ANY)));
            self.go(then, s);
        }
        for (k, row) in rows.iter().enumerate() {
            let Some(ins) = row.ins.iter().zip(&tys).map(|(p, v)| self.pat(p, v)).collect::<Option<Vec<Ty>>>() else { continue };
            if ins.len() != tys.len() { continue; }
            let mut s = s.clone();
            for (e, t) in a.iter().zip(&ins) { if e.from != NO && let Some(v) = s.slots.get_mut(e.from as usize) { *v = *t; } }
            let span = |j: usize| (ins[j].lo as i128, ins[j].hi as i128);
            let exact = |r: usize| row.exact.iter().find(|x| x.0 == r).map(|x| {
                let (lo, hi) = match x.1 {
                    Exact::A(j) => span(j),
                    Exact::Neg(j) => (-span(j).1, -span(j).0),
                    Exact::Add(j, k) => (span(j).0 + span(k).0, span(j).1 + span(k).1),
                    Exact::Sub(j, k) => (span(j).0 - span(k).1, span(j).1 - span(k).0),
                    Exact::Mul(j, k) => { let (a, b) = (span(j), span(k)); let p = [a.0 * b.0, a.0 * b.1, a.1 * b.0, a.1 * b.1]; (*p.iter().min().unwrap_or(&0), *p.iter().max().unwrap_or(&0)) }
                    Exact::N(n) => (n as i128, n as i128),
                };
                // Past what an i64 holds the bound is open.
                (i64::try_from(lo).unwrap_or(i64::MIN), i64::try_from(hi).unwrap_or(i64::MAX))
            });
            let raises = match row.unless { Some((r, lo, hi)) if exact(r).is_some_and(|(a, b)| lo <= a && b <= hi) => Tags::NONE, _ => row.raises };
            self.raise(&s, Ty::of(raises));
            let Some(outs) = &row.outs else { continue };
            for &(j, b) in &row.puts {
                let v = b.map_or(Ty::ANY, |b| ins[b]);
                match j.map(|j| ins[j].known) { Some(Known::Site(c) | Known::Obj(_, c)) => self.write(c, ITEM, v), _ => self.eff.wild = true }
            }
            for (r, &tags) in outs.iter().enumerate() {
                let mut ty = Ty::of(tags);
                if let Some((lo, hi)) = exact(r) { (ty.lo, ty.hi) = (lo, hi); }
                for &(_, lo, hi) in row.ranges.iter().filter(|x| x.0 == r) { (ty.lo, ty.hi) = (ty.lo.max(lo), ty.hi.min(hi)); }
                for &(_, j) in row.item.iter().filter(|x| x.0 == r) { let it = self.item(&ins[j]); ty = Ty { tags: ty.tags & it.tags, lo: ty.lo.max(it.lo), hi: ty.hi.min(it.hi), known: it.known }; }
                for (_, js) in row.items.iter().filter(|x| x.0 == r) {
                    ty.known = Known::Site(self.site(ip));
                    self.write(self.site(ip), ITEM, js.iter().fold(Ty::NONE, |t, &j| t.join(&ins[j])));
                }
                if ty.tags.is_empty() { continue; }
                let learns = r == 0 && !row.learn.is_empty();
                let of = core::array::from_fn(|j| a.get(j).map_or(NO, |e| e.from));
                s.stack.push(Entry { ty, from: NO, cond: if learns { (ip as u32) << 8 | k as u32 } else { NO }, of: if learns { of } else { [NO; 3] } });
            }
            self.go(then, s);
        }
    }

    /* Each way the instruction at `ip` goes from `s`. */
    pub fn step(&mut self, ip: usize, s: &State) {
        let b = self.body();
        let chunk = b.chunk;
        let Some(&ins) = chunk.instructions.get(ip) else { return self.ret(s.clone(), self.p.konst(&Value::None)) };
        let (op, k, next) = (ins.opcode, ins.operand, ip + 1);
        let mut s = s.clone();
        macro_rules! pop { () => { match s.stack.pop() { Some(e) => e, None => return self.give_up() } } }
        match op {
            O::LoadConst => { s.stack.push(Entry::of(chunk.constants.get(k as usize).map_or(Ty::ANY, |c| self.p.konst(c)))); self.go(next, s) }
            O::LoadTrue | O::LoadFalse => { s.stack.push(Entry::of(self.p.konst(&Value::Bool(op == O::LoadTrue)))); self.go(next, s) }
            O::LoadNone => { s.stack.push(Entry::of(self.p.konst(&Value::None))); self.go(next, s) }
            O::LoadName => if let Some(e) = self.load(&mut s, k) { s.stack.push(e); self.go(next, s) },
            O::LoadGlobal => {
                let v = self.global(b.module, self.p.name(self.me(), k));
                if v.tags.meets(unb()) { self.raise(&s, self.p.exc("NameError")); }
                let v = Ty { tags: v.tags.minus(unb()), ..v };
                if !v.tags.is_empty() { s.stack.push(Entry::of(v)); self.go(next, s) }
            }
            O::StoreName | O::StoreGlobal => { let v = pop!(); self.store(&mut s, k, v.ty, op == O::StoreGlobal); self.go(next, s) }
            O::Del => { self.store(&mut s, k, Ty::of(unb()), false); self.go(next, s) }
            O::Phi => {
                let (a, c) = chunk.phi_sources.get(chunk.phi_map.get(ip).copied().unwrap_or(0)).copied().unwrap_or((k, k));
                let at = |i: u16| s.slots.get(i as usize).copied().unwrap_or(Ty::of(unb()));
                let (va, vc) = (at(a), at(c));
                let v = if va.tags.meets(unb()) { Ty { tags: va.tags.minus(unb()), ..va }.join(&vc) } else { va };
                if let Some(x) = s.slots.get_mut(k as usize) { *x = v; }
                self.go(next, s)
            }
            O::Global | O::Nonlocal | O::BeginArgs => self.go(next, s),
            O::PopTop => { pop!(); self.go(next, s) }
            O::Dup => { let e = pop!(); s.stack.extend([e, e]); self.go(next, s) }
            O::Dup2 => { let (y, x) = (pop!(), pop!()); s.stack.extend([x, y, x, y]); self.go(next, s) }
            O::Swap => { let (y, x) = (pop!(), pop!()); s.stack.extend([y, x]); self.go(next, s) }
            O::Rot3 => { let (z, y, x) = (pop!(), pop!(), pop!()); s.stack.extend([y, z, x]); self.go(next, s) }
            O::Jump => self.go(k as usize, s),
            O::JumpIfFalse | O::JumpIfFalseOrPop | O::JumpIfTrueOrPop | O::Assert => {
                let e = pop!();
                self.truth(&s, e, |me, mut s, side| match op {
                    O::JumpIfFalse => me.go(if side { next } else { k as usize }, s),
                    O::Assert => if side { me.go(next, s) } else { let a = me.p.exc("AssertionError"); me.raise(&s, a) },
                    _ if side == (op == O::JumpIfTrueOrPop) => { s.stack.push(Entry::of(e.ty)); me.go(k as usize, s) }
                    _ => me.go(next, s),
                })
            }
            O::GetIter if k != crate::parser::ITER_VALUE => {
                let e = pop!();
                let rows: Vec<&Row> = self.p.rules.rows(op, k).collect();
                let at = self.outs.len();
                self.rows(&s, ip, rows, &[e], 0, None);
                // The iterated value moves to the loop on each way the rows go on.
                for (to, st) in &mut self.outs[at..] { if matches!(to, To::Ip(t) if *t == next) { st.iters.push(e.ty); } }
            }
            O::ForIter => {
                let Some(&it) = s.iters.last() else { return self.give_up() };
                if let Known::Gen(g) = it.known { self.eff.ran = true; let e = self.t.ctxs[g as usize].raises; self.raise(&s, e); }
                let item = self.item(&it);
                if !item.tags.is_empty() { let mut l = s.clone(); l.stack.push(Entry::of(item)); self.go(next, l); }
                s.iters.pop();
                self.go(k as usize, s)
            }
            O::PopIter => { s.iters.pop(); self.go(next, s) }
            O::SetupExcept | O::SetupFinally => {
                s.tries.push(Try { handler: k, finally: op == O::SetupFinally, stack: s.stack.len() as u16, iters: s.iters.len() as u16, withs: s.withs.len() as u16, unwind: s.unwind.len() as u16 });
                self.go(next, s)
            }
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
                let e = if op == O::Raise && k == 1 { Ty::of(self.p.tag("BaseException") | self.p.tag("instance")) } else { let v = pop!(); self.exc_of(v.ty, ip) };
                self.raise(&s, e.join(&self.p.exc("TypeError")))
            }
            O::Yield => {
                let v = pop!();
                self.write(YIELDS | self.x as u32, ITEM, v.ty);
                self.raise(&s, self.p.exc("GeneratorExit"));
                s.stack.push(Entry::of(Ty::ANY));
                self.go(next, s)
            }
            O::MakeFunction | O::MakeCoroutine | O::MakeClass => {
                let class = op == O::MakeClass;
                let id = if class { b.classes.get(k as usize) } else { b.fns.get(k as usize) }.copied();
                let (Some((n, _)), Some(id)) = (self.p.rules.effect(op, k, chunk), id) else { return self.give_up() };
                let Some(at) = s.stack.len().checked_sub(n) else { return self.give_up() };
                let dflt: Vec<u32> = self.p.bodies[id as usize].params.iter().enumerate().filter(|p| p.1.2).map(|p| p.0 as u32).collect();
                for (j, e) in s.stack.split_off(at).into_iter().enumerate() { self.write(if class { BASES } else { DEFAULTS } | id, if class { j as u32 } else { dflt.get(j).copied().unwrap_or(j as u32) }, e.ty); }
                if class {
                    if self.p.bodies[id as usize].chunk.class_kw > 0 { self.unknown(&s); }
                    let (_, e) = match self.call(id, Vec::new()) { Some(r) => r, None => return };
                    self.raise(&s, e);
                }
                s.stack.push(Entry::of(Ty::of(self.p.tag(if class { "type" } else { "function" })).with(Known::Body(id))));
                self.go(next, s)
            }
            O::LoadModule => {
                let m = match chunk.imports.get(k as usize).map(|e| &e.kind) { Some(ImportKind::Code(c)) => self.p.bodies.iter().position(|b| core::ptr::eq(b.chunk, &**c)), _ => None };
                s.stack.push(Entry::of(m.map_or(Ty::ANY, |m| Ty::of(self.p.tag("module")).with(Known::Module(m as u32)))));
                self.go(next, s)
            }
            O::StoreAttr => {
                let (v, o) = (pop!(), pop!());
                match o.ty.known {
                    Known::Obj(c, _) if self.mro(c).is_some() && self.lookup(c, "__setattr__").is_empty() => self.write(ATTR | c, self.p.name(self.me(), k), v.ty),
                    _ => { self.foreign(&[o.ty, v.ty]); self.unknown(&s) }
                }
                self.go(next, s)
            }
            O::LoadAttr => { let o = pop!(); for g in self.attr(&s, o.ty, self.p.name(self.me(), k)) { let mut s = s.clone(); s.stack.push(Entry::of(g)); self.go(next, s); } }
            O::Call | O::CallMethod => {
                let method = op == O::CallMethod;
                let counts = if method { chunk.instructions.get(next).map_or(0, |i| i.operand) } else { k };
                let Some((n, _)) = self.p.rules.effect(O::Call, counts, chunk) else { return self.give_up() };
                let Some(at) = s.stack.len().checked_sub(n) else { return self.give_up() };
                let a = s.stack.split_off(at);
                let then = if method { next + 1 } else { next };
                // A fused method call has its receiver where a call has its callee, read as the attribute it names.
                let callees = if method { self.attr(&s, a[0].ty, self.p.name(self.me(), k)) } else { vec![a[0].ty] };
                for callee in callees {
                    for r in self.invoke(&s, ip, callee, &a[1..], counts >> 8 == 0, then) { let mut s = s.clone(); s.stack.push(Entry::of(r)); self.go(then, s); }
                }
            }
            O::WithEnter | O::WithExit => {
                let enter = op == O::WithEnter;
                let cm = if enter { pop!().ty } else { match s.withs.pop() { Some(c) => c, None => return self.give_up() } };
                let f = match cm.known { Known::Obj(c, _) => match self.lookup(c, if enter { "__enter__" } else { "__exit__" })[..] { [m] => m, _ => Ty::ANY }, _ => Ty::ANY };
                if enter { s.withs.push(cm); s.stack.extend([Entry::of(f), Entry::of(cm)]); }
                else { s.stack.extend([f, cm, Ty::ANY, Ty::ANY, self.p.konst(&Value::None)].map(Entry::of)); }
                self.go(next, s)
            }
            O::WithJudge => {
                let r = pop!();
                if matches!(s.unwind.last(), Some(Why::Reraise(_))) { self.truth(&s, r, |me, mut s, side| { if side && let Some(w) = s.unwind.last_mut() { *w = Why::Normal; } me.go(next, s) }) } else { self.go(next, s) }
            }
            _ => {
                let Some((n, m)) = self.p.rules.effect(op, k, chunk) else { return self.give_up() };
                let Some(at) = s.stack.len().checked_sub(n) else { return self.give_up() };
                let a = s.stack.split_off(at);
                let rows: Vec<&Row> = self.p.rules.rows(op, k).collect();
                self.rows(&s, ip, rows, &a, m, None)
            }
        }
        if self.eff.ran && self.body().kind == BodyKind::Module { self.havoc(); }
    }

    /* After code that may write the module's names, each may hold what any code stores in it. */
    fn havoc(&mut self) {
        let b = self.body();
        for (_, st) in self.outs.iter_mut() {
            for (i, bind) in b.binds.iter().enumerate() {
                let Some(v) = st.slots.get_mut(i) else { continue };
                if matches!(bind, Binding::Global(_)) { *v = if self.t.wild { Ty::ANY } else { v.join(&self.t.tables.get(&(GLOBAL | b.module, b.names[i])).copied().unwrap_or(Ty::NONE)) }; }
            }
        }
    }

    /* The value `raise v` raises, an instance when it names a class. */
    fn exc_of(&self, v: Ty, ip: usize) -> Ty {
        let ty = self.p.tag("type");
        match v.known {
            Known::Body(c) => Ty::of(self.p.tag("instance")).with(Known::Obj(c, self.site(ip))),
            Known::Type(t) => Ty::of(Tags::range(t, t)),
            _ => Ty { tags: v.tags.minus(ty) | if v.tags.meets(ty) { self.p.tag("BaseException") | self.p.tag("instance") } else { Tags::NONE }, ..v },
        }
    }

    /* A path through an opcode the rules do not cover: the VM runs this body. */
    fn give_up(&mut self) { self.eff.gave_up = true; self.eff.wild = true; }
}

/* The state a context starts in: its parameters bound to its arguments, every other slot unbound, or a builtin in a module. */
pub fn entry(p: &Prog, body: u32, args: &[Ty]) -> State {
    let b = &p.bodies[body as usize];
    let mut slots: Vec<Ty> = b.names.iter().map(|&n| if b.kind == BodyKind::Module { p.builtin(n) } else { Ty::of(unb()) }).collect();
    for (k, &(_, slot, _)) in b.params.iter().enumerate() {
        let c = b.chunk.canonical.get(slot).map_or(slot, |&c| c as usize);
        if let Some(v) = slots.get_mut(c) { *v = args.get(k).copied().unwrap_or(Ty::ANY); }
    }
    State { slots, ..Default::default() }
}

/* Accepts a tree only when every step of every state it holds lands inside it: each next state inside a state at its target,
   each result, exception and table entry inside what the tree claims, each call to a context it holds. */
#[unsafe(no_mangle)]
pub fn types_check(p: &Prog, t: &Tree) -> Result<(), String> {
    let fail = |x: usize, ip: usize, why: &str| Err(alloc::format!("types_check: context {x} at {ip}: {why}"));
    if t.ctxs.first().is_none_or(|c| c.body != 0 || !c.args.is_empty()) { return fail(0, 0, "the module is not the first context"); }
    for (x, c) in t.ctxs.iter().enumerate() {
        if c.gave_up {
            if !(t.wild && Ty::ANY.within(&c.ret) && Ty::ANY.within(&c.raises)) { return fail(x, 0, "a context given up claims less than anything"); }
            continue;
        }
        let e = entry(p, c.body, &c.args);
        if !c.seen.first().is_some_and(|s| s.iter().any(|s| e.within(s))) { return fail(x, 0, "its entry is not held"); }
        for (ip, states) in c.seen.iter().enumerate() {
            for s in states {
                let mut st = Step::new(p, t, x);
                st.step(ip, s);
                if st.eff.gave_up { return fail(x, ip, "a step no rule covers"); }
                if st.eff.wild && !t.wild { return fail(x, ip, "code nothing knows ran in a tree that says none did"); }
                if !st.eff.calls.is_empty() { return fail(x, ip, "a call to a context the tree does not hold"); }
                if !t.wild && st.eff.writes.iter().any(|(o, f, v)| !t.tables.get(&(*o, *f)).is_some_and(|w| v.within(w))) { return fail(x, ip, "a write outside its table"); }
                for (to, n) in &st.outs {
                    let ok = match to {
                        To::Ip(i) => c.seen.get(*i).is_some_and(|ss| ss.iter().any(|w| n.within(w))),
                        To::Ret(v) => v.within(&c.ret),
                        To::Raise(e) => e.within(&c.raises),
                    };
                    if !ok { return fail(x, ip, "a next state the tree does not hold"); }
                }
            }
        }
    }
    Ok(())
}
