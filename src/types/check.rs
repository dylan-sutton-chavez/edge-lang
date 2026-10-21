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
                    else if !t.wild && st.eff.writes.iter().any(|(o, f, v)| !t.tables.get(&(*o, *f)).is_some_and(|w| v.within(w))) { "a write outside its table" }
                    else if !st.outs.iter().all(|(to, n)| held(to, n)) { "a next state the tree does not hold" }
                    else { continue };
                return fail(x, ip, why);
            }
        }
    }
    Ok(())
}

/* The instance sites whose objects may live off the heap: of fixed size known at build (the attributes their class takes,
   at most 256 bytes with the class), never leaving the frames they are made and passed in (no result, exception, table,
   finally reason or generator holds one), never compared by identity, and never two of one site alive at once. A value
   that may be an instance of a class made at no one known site may be any of them. */
pub fn no_heap(p: &Prog, t: &Tree) -> Vec<u32> {
    let inst = p.tag("instance").tags;
    let hits = |v: &Ty, s: u32, c: u32| match v.known { Known::Obj(d, at) | Known::Bound(_, d, at) => d == c && (at == s || at == NO), _ => v.tags & inst != 0 };
    let states = || t.ctxs.iter().flat_map(|c| c.seen.iter().enumerate().flat_map(move |(ip, ss)| ss.iter().map(move |s| (c, ip, s))));
    let vals = |s: &State| s.stack.iter().map(|e| e.ty).chain(s.slots.iter().copied()).chain(s.iters.iter().copied()).collect::<Vec<Ty>>();
    let mut sites: Vec<(u32, u32)> = Vec::new();
    for (_, _, s) in states().filter(|_| !t.wild) { for v in vals(s) { if let Known::Obj(c, at) = v.known && at != NO && !sites.contains(&(at, c)) { sites.push((at, c)); } } }
    sites.retain(|&(at, c)| {
        let left = t.tables.values().chain(t.ctxs.iter().flat_map(|x| [&x.ret, &x.raises])).any(|v| hits(v, at, c));
        !(left || t.tables.keys().filter(|k| k.0 == ATTR | c).count() > 31 || states().any(|(x, ip, s)| {
            let b = &p.bodies[x.body as usize];
            let op = b.chunk.instructions.get(ip).map(|i| i.opcode);
            s.unwind.iter().any(|w| matches!(w, Why::Return(v) | Why::Reraise(v) if hits(v, at, c)))
                || matches!(op, Some(O::Is | O::IsNot)) && s.stack.iter().rev().take(2).any(|e| hits(&e.ty, at, c))
                || (b.chunk.is_generator || b.coro || x.body << 16 | ip as u32 == at && op == Some(O::Call)) && vals(s).iter().any(|v| hits(v, at, c))
        }))
    });
    sites.into_iter().map(|s| s.0).collect()
}
