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
   finally reason or generator holds one), never compared by identity, and never two of one site alive at once. */
pub fn no_heap(p: &Prog, t: &Tree) -> Vec<u32> {
    let site = |v: &Ty| match v.known { Known::Obj(_, s) | Known::Bound(_, _, s) => Some(s), _ => None };
    let mut bad: Vec<u32> = t.tables.values().chain(t.ctxs.iter().flat_map(|c| [&c.ret, &c.raises])).filter_map(site).collect();
    let mut out: Vec<u32> = Vec::new();
    for c in t.ctxs.iter().filter(|_| !t.wild) {
        let b = &p.bodies[c.body as usize];
        for (ip, s) in c.seen.iter().enumerate().flat_map(|(ip, ss)| ss.iter().map(move |s| (ip, s))) {
            let op = b.chunk.instructions.get(ip).map(|i| i.opcode);
            let whys = s.unwind.iter().filter_map(|w| match w { Why::Return(v) | Why::Reraise(v) => Some(v), _ => None });
            for (k, v) in s.stack.iter().rev().map(|e| &e.ty).chain(&s.slots).chain(&s.iters).chain(whys).enumerate() {
                let Some(at) = site(v) else { continue };
                let size = match v.known { Known::Obj(c, _) => t.tables.keys().filter(|(o, _)| *o == ATTR | c).count(), _ => 0 };
                let identity = k < 2 && matches!(op, Some(O::Is | O::IsNot));
                if size > 31 || identity || b.chunk.is_generator || k >= s.stack.len() + s.slots.len() + s.iters.len() || at == (c.body << 16 | ip as u32) && op == Some(O::Call) { bad.push(at); }
                if !out.contains(&at) { out.push(at); }
            }
        }
    }
    out.retain(|s| !bad.contains(s));
    out
}
