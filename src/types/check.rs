/* The reviewer: accepts a tree only when it is closed under `step`, so it holds every path the program can take, and
   says which objects may live off the heap. It compares and nothing else; the logic is `step`'s. */

use alloc::{string::String, vec::Vec};
use crate::parser::OpCode as O;
use super::step::{Prog, Step, To, entry, ATTR};
use super::tree::Tree;
use super::*;

/* Every state the tree holds, stepped: each next state inside one it holds at its target, each result, exception and table
   entry inside what it claims, each call to a context it holds, nothing nothing covers unless its context gave up. */
#[unsafe(no_mangle)]
pub fn types_check(p: &Prog, t: &Tree) -> Result<(), String> {
    let fail = |x: usize, ip: usize, why: &str| Err(alloc::format!("types_check: context {x} at {ip}: {why}"));
    if t.ctxs.first().is_none_or(|c| c.body != 0 || !c.args.is_empty()) { return fail(0, 0, "the module is not the first context"); }
    for (x, c) in t.ctxs.iter().enumerate() {
        if c.gave_up { if t.wild && Ty::ANY.within(&c.ret) && Ty::ANY.within(&c.raises) { continue; } return fail(x, 0, "a context given up claims less than anything"); }
        let e = entry(p, c.body, &c.args);
        if !c.seen.first().is_some_and(|s| s.iter().any(|s| e.within(s))) { return fail(x, 0, "its entry is not held"); }
        for (ip, states) in c.seen.iter().enumerate() {
            for s in states {
                let mut st = Step::new(p, t, x);
                st.step(ip, s);
                let held = |to: &To, n: &State| match to { To::Ip(i) => c.seen.get(*i).is_some_and(|ss| ss.iter().any(|w| n.within(w))), To::Ret(v) => v.within(&c.ret), To::Raise(e) => e.within(&c.raises) };
                let why = if st.eff.gave_up { "a step no rule covers" }
                    else if st.eff.wild && !t.wild { "code nothing knows ran in a tree that says none did" }
                    else if !st.eff.calls.is_empty() { "a call to a context the tree does not hold" }
                    // Defaults and bases are read as written even after unknown code ran.
                    else if st.eff.writes.iter().any(|(o, f, v)| (!t.wild || o >> 28 >= 5) && !t.tables.get(&(*o, *f)).is_some_and(|w| v.within(w))) { "a write outside its table" }
                    else if !st.outs.iter().all(|(to, n)| held(to, n)) { "a next state the tree does not hold" }
                    else { continue };
                return fail(x, ip, why);
            }
        }
    }
    Ok(())
}

/* The instance sites whose objects may live off the heap: of fixed size known at build (the attributes their class takes,
   at most 256 bytes with the class), never leaving the frames they are made and passed in (written into no table, never
   a result nor an exception out of a body, never held by a generator), never compared by identity, and never two of one
   site alive at once. Each step is run again to see what leaves, with the values exactly as they leave. */
pub fn no_heap(p: &Prog, t: &Tree) -> Vec<u32> {
    let hits = |v: &Ty, at: u32, c: u32| matches!(v.known, Known::Obj(d, s) | Known::Bound(_, d, s) if d == c && (s == at || s == NO));
    let (mut sites, mut left, mut alive) = (Vec::<(u32, u32)>::new(), Vec::<Ty>::new(), Vec::<(u32, Ty)>::new());
    for (x, c) in t.ctxs.iter().enumerate().filter(|_| !t.wild) {
        let b = &p.bodies[c.body as usize];
        for (ip, s) in c.seen.iter().enumerate().flat_map(|(ip, ss)| ss.iter().map(move |s| (ip, s))) {
            let op = b.chunk.instructions.get(ip).map(|i| i.opcode);
            let vals: Vec<Ty> = s.stack.iter().map(|e| e.ty).chain(s.slots.iter().copied()).chain(s.iters.iter().copied()).collect();
            for v in &vals { if let Known::Obj(k, at) = v.known && at != NO && !sites.contains(&(at, k)) { sites.push((at, k)); } }
            if b.chunk.is_generator || b.coro { left.extend(&vals); }
            if matches!(op, Some(O::Is | O::IsNot)) { left.extend(s.stack.iter().rev().take(2).map(|e| e.ty)); }
            if op == Some(O::Call) { alive.extend(vals.iter().map(|&v| (c.body << 16 | ip as u32, v))); }
            let mut st = Step::new(p, t, x);
            st.step(ip, s);
            left.extend(st.eff.writes.iter().map(|w| w.2).chain(st.outs.iter().filter_map(|o| match o.0 { To::Ret(v) | To::Raise(v) => Some(v), _ => None })));
        }
    }
    sites.retain(|&(at, c)| t.tables.keys().filter(|k| k.0 == ATTR | c).count() <= 31 && !left.iter().any(|v| hits(v, at, c)) && !alive.iter().any(|(m, v)| *m == at && hits(v, at, c)));
    sites.into_iter().map(|s| s.0).collect()
}
