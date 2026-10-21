/* The tree of a program's paths: every state each instruction can see, two paths joined only when their types are the
   same and their integers then hulled, loops and recursion run until nothing grows, a context of its own for each
   function and the argument types it is called with. The reviewer (check.rs) decides whether to trust it. */

use alloc::{vec, vec::Vec};
use crate::util::hash::FxHashMap as Map;
use super::check::{Prog, Step, To, entry};
use super::*;

/* One body run with one tuple of argument types, every state the tree found at each of its instructions. */
pub struct Ctx { pub body: u32, pub args: Vec<Ty>, pub seen: Vec<Vec<State>>, pub ret: Ty, pub raises: Ty, pub gave_up: bool }

/* The contexts, the module's first, the tables every body shares (names of modules and cells, members, attributes, items,
   yields, defaults and bases), and whether code nothing knows ran, which makes every table anything. */
#[derive(Default)]
pub struct Tree { pub ctxs: Vec<Ctx>, pub tables: Map<(u32, u32), Ty>, pub wild: bool }

/* `old` joined with `new`, with a bound that grew opened when `widen`, so a loop's integers stop growing. */
fn hull(old: &Ty, new: &Ty, widen: bool) -> Ty {
    let j = old.join(new);
    if !widen || old.tags.is_empty() { return j; }
    Ty { lo: if j.lo < old.lo { i64::MIN } else { j.lo }, hi: if j.hi > old.hi { i64::MAX } else { j.hi }, ..j }
}

fn hull_all(a: &[Ty], b: &[Ty], widen: bool) -> Vec<Ty> { a.iter().zip(b).map(|(x, y)| hull(x, y, widen)).collect() }

/* Joins `new` into `at`, false when it held it already. */
fn grow_into(at: &mut Ty, new: &Ty) -> bool {
    if new.within(at) { return false; }
    *at = hull(at, new, true);
    true
}

impl Tree {
    /* Takes `s` at `ip` of context `x`, the state to go on from, None when the tree holds it already. */
    fn admit(&mut self, p: &Prog, x: usize, ip: usize, s: State) -> Option<State> {
        let widen = p.bodies[self.ctxs[x].body as usize].heads.get(ip).copied().unwrap_or(false);
        let seen = &mut self.ctxs[x].seen[ip];
        let Some(old) = seen.iter_mut().find(|o| o.same(&s)) else { seen.push(s.clone()); return Some(s) };
        if s.within(old) { return None; }
        old.slots = hull_all(&old.slots, &s.slots, widen);
        old.iters = hull_all(&old.iters, &s.iters, widen);
        old.withs = hull_all(&old.withs, &s.withs, widen);
        for (o, n) in old.stack.iter_mut().zip(&s.stack) { o.ty = hull(&o.ty, &n.ty, widen); }
        Some(old.clone())
    }

    fn open(&mut self, p: &Prog, body: u32, args: Vec<Ty>, work: &mut Vec<(usize, usize, State)>) {
        let n = p.bodies[body as usize].chunk.instructions.len() + 1;
        work.push((self.ctxs.len(), 0, entry(p, body, &args)));
        self.ctxs.push(Ctx { body, args, seen: vec![Vec::new(); n], ret: Ty::NONE, raises: Ty::NONE, gave_up: false });
    }
}

/* Grows the tree of the program until a pass over every context finds nothing new. */
pub fn grow(p: &Prog) -> Tree {
    let mut t = Tree::default();
    let mut work = Vec::new();
    t.open(p, 0, Vec::new(), &mut work);
    loop {
        let mut changed = false;
        if work.is_empty() {
            for (x, c) in t.ctxs.iter_mut().enumerate() {
                c.seen.iter_mut().for_each(Vec::clear);
                if !c.gave_up { work.push((x, 0, entry(p, c.body, &c.args))); }
            }
        }
        while let Some((x, ip, s)) = work.pop() {
            if t.ctxs[x].gave_up { continue; }
            let Some(s) = t.admit(p, x, ip, s) else { continue };
            let mut st = Step::new(p, &t, x);
            st.step(ip, &s);
            let (outs, eff) = (st.outs, st.eff);
            if eff.gave_up {
                let c = &mut t.ctxs[x];
                (c.gave_up, c.ret, c.raises, t.wild) = (true, Ty::ANY, Ty::ANY, true);
                changed = true;
                continue;
            }
            if eff.wild && !t.wild { t.wild = true; changed = true; }
            for (o, f, v) in eff.writes {
                // An entry made empty still says something: a container built with no items holds none.
                let new = !t.tables.contains_key(&(o, f));
                changed |= grow_into(t.tables.entry((o, f)).or_insert(Ty::NONE), &v) || new;
            }
            for (f, args) in eff.calls {
                match t.ctxs.iter().position(|c| c.body == f && c.args.len() == args.len() && c.args.iter().zip(&args).all(|(a, b)| a.same(b))) {
                    Some(i) => { let c = &mut t.ctxs[i]; c.args = hull_all(&c.args, &args, true); }
                    None => t.open(p, f, args, &mut work),
                }
                changed = true;
            }
            for (to, n) in outs {
                match to {
                    To::Ip(i) => work.push((x, i, n)),
                    To::Ret(v) => changed |= grow_into(&mut t.ctxs[x].ret, &v),
                    To::Raise(e) => changed |= grow_into(&mut t.ctxs[x].raises, &e),
                }
            }
        }
        if !changed { return t; }
    }
}
