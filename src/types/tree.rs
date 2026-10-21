/* The tree of a program's paths: every state each instruction can see, two paths joined only when their types are the
   same and their integers then hulled, loops and recursion run until nothing grows, a context of its own for each
   function and the argument types it is called with. The reviewer (check.rs) decides whether to trust it. */

use alloc::{vec, vec::Vec};
use crate::util::hash::FxHashMap as Map;
use super::step::{Prog, Step, To, entry};
use super::*;

/* One body run with one tuple of argument types, every state the tree found at each of its instructions. */
pub struct Ctx { pub body: u32, pub args: Vec<Ty>, pub seen: Vec<Vec<State>>, pub ret: Vec<Ty>, pub raises: Ty, pub gave_up: bool }

/* The contexts, the module's first, the tables every body shares (names of modules and cells, members, attributes, items,
   yields, defaults and bases), and whether code nothing knows ran, which makes every table anything. An entry and a
   result hold one value per value known of them, so a call through a list, a name or a result steps each body it may be. */
#[derive(Default)]
pub struct Tree { pub ctxs: Vec<Ctx>, pub tables: Map<(u32, u32), Vec<Ty>>, pub wild: bool }

/* `old` joined with `new`, with a bound that grew opened when `widen`, so a loop's integers stop growing. */
fn hull(old: &Ty, new: &Ty, widen: bool) -> Ty {
    let j = old.join(new);
    if !widen || old.tags == 0 { return j; }
    Ty { lo: if j.lo < old.lo { i64::MIN } else { j.lo }, hi: if j.hi > old.hi { i64::MAX } else { j.hi }, ..j }
}

fn hull_all(a: &[Ty], b: &[Ty], widen: bool) -> Vec<Ty> { a.iter().zip(b).map(|(x, y)| hull(x, y, widen)).collect() }

/* Joins `new` into `at`, false when it held it already. */
fn grow_into(at: &mut Ty, new: &Ty) -> bool {
    if new.within(at) { return false; }
    *at = hull(at, new, true);
    true
}

/* Joins `new` into the value of the entry it is known as (one for every value no one is known as), false when held. */
fn hold(at: &mut Vec<Ty>, new: &Ty) -> bool {
    match at.iter_mut().find(|v| v.known == new.known || v.known.join(new.known) != Known::No) { Some(v) => grow_into(v, new), None => { at.push(*new); true } }
}

impl Tree {
    /* Takes `s` at `ip` of context `x`, the state to go on from, None when the tree holds it already. */
    fn admit(&mut self, p: &Prog, x: usize, ip: usize, s: State) -> Option<State> {
        let widen = p.bodies[self.ctxs[x].body as usize].heads.get(ip).copied().unwrap_or(false);
        let seen = &mut self.ctxs[x].seen[ip];
        // Once code nothing knows ran every table is anything, and paths of one shape join: splitting them keeps nothing.
        let Some(old) = seen.iter_mut().find(|o| if self.wild { o.all(&s, |_, _| true, |_, _| true) } else { o.same(&s) }) else { seen.push(s.clone()); return Some(s) };
        if s.within(old) { return None; }
        old.slots = hull_all(&old.slots, &s.slots, widen);
        old.iters = hull_all(&old.iters, &s.iters, widen);
        for (o, n) in old.stack.iter_mut().zip(&s.stack) {
            o.ty = hull(&o.ty, &n.ty, widen);
            // Paths joined with another slot or learning keep neither (wild only, where splitting them keeps nothing).
            if o.from != n.from { o.from = NO; }
            if (o.cond, o.of) != (n.cond, n.of) { (o.cond, o.of) = (NO, [NO; 3]); }
        }
        Some(old.clone())
    }

    fn open(&mut self, p: &Prog, body: u32, args: Vec<Ty>, work: &mut Vec<(usize, usize, State, bool)>) {
        let n = p.bodies[body as usize].chunk.instructions.len() + 1;
        work.push((self.ctxs.len(), 0, entry(p, body, &args), false));
        self.ctxs.push(Ctx { body, args, seen: vec![Vec::new(); n], ret: Vec::new(), raises: Ty::NONE, gave_up: false });
    }
}

/* Grows the tree of the program until a pass over every context finds nothing new. A state that read a context's result
   or raises is stepped again (without the tree's check that it holds it) when they grow, and one that called a context
   the tree did not hold once it holds it, so only a table that grows, or the program going wild, starts a new pass. */
pub fn grow(p: &Prog) -> Tree {
    let mut t = Tree::default();
    let (mut work, mut readers) = (Vec::new(), Map::<usize, Vec<(usize, usize, State)>>::default());
    t.open(p, 0, Vec::new(), &mut work);
    loop {
        let mut changed = false;
        if work.is_empty() {
            readers.clear();
            for (x, c) in t.ctxs.iter_mut().enumerate() {
                c.seen.iter_mut().for_each(Vec::clear);
                if !c.gave_up { work.push((x, 0, entry(p, c.body, &c.args), false)); }
            }
        }
        while let Some((x, ip, s, again)) = work.pop() {
            if t.ctxs[x].gave_up { continue; }
            let Some(s) = (if again { Some(s) } else { t.admit(p, x, ip, s) }) else { continue };
            let mut st = Step::new(p, &t, x);
            st.step(ip, &s);
            let (outs, eff) = (st.outs, st.eff);
            if eff.gave_up {
                let c = &mut t.ctxs[x];
                (c.gave_up, c.ret, c.raises, t.wild, changed) = (true, vec![Ty::ANY], Ty::ANY, true, true);
                continue;
            }
            if eff.wild && !t.wild { t.wild = true; changed = true; }
            for (o, f, v) in eff.writes {
                // An entry made empty still says something: a container built with no items holds none.
                let new = !t.tables.contains_key(&(o, f));
                changed |= hold(t.tables.entry((o, f)).or_default(), &v) || new;
            }
            if !eff.calls.is_empty() { work.push((x, ip, s.clone(), true)); }
            for (f, args) in eff.calls {
                match t.ctxs.iter().position(|c| c.body == f && c.args.len() == args.len() && c.args.iter().zip(&args).all(|(a, b)| a.same(b))) {
                    Some(i) => { let c = &mut t.ctxs[i]; c.args = hull_all(&c.args, &args, true); let e = entry(p, f, &c.args); work.push((i, 0, e, false)); }
                    None => t.open(p, f, args, &mut work),
                }
            }
            for i in eff.reads { let r = readers.entry(i).or_default(); if !r.iter().any(|w| (w.0, w.1) == (x, ip) && w.2 == s) { r.push((x, ip, s.clone())); } }
            for (to, n) in outs {
                let grew = match to {
                    To::Ip(i) => { work.push((x, i, n, false)); false }
                    To::Ret(v) => hold(&mut t.ctxs[x].ret, &v),
                    To::Raise(e) => grow_into(&mut t.ctxs[x].raises, &e),
                };
                if grew { work.extend(readers.get(&x).into_iter().flatten().map(|r| (r.0, r.1, r.2.clone(), true))); }
            }
        }
        if !changed { return t; }
    }
}
