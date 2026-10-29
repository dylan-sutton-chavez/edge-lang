use alloc::vec::Vec;
use core::cmp::Ordering;

use edge::lang::{Acc, Args, Cx, Flow, Fold, Item, Ret, STEPS, Sig, Step};
use edge::value::{HeapObj, Val, VmErr};

use super::sort::{plain_keys, room, top_order};
use crate::fail::{Fail, fail};
use crate::iter::items::gather;
use crate::{Kernel, Site};

/* the `k` greatest items first with ties kept where a map ranks entries by value */
pub const fn top<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), top_run::<S>).flags(STEPS).reads(1) }

fn top_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (src, k) = (a.pos[0], a.pos[1]);
    let Some(k) = count(cx, k) else { return Err(fail::<S>(cx, Fail::NotIndex(k))) };
    let key = a.kw(0).filter(|f| !f.is_none());
    let mut acc = Acc::new();
    (acc.v[0], acc.n) = (key.unwrap_or(Val::undef()), i64::try_from(k).unwrap_or(i64::MAX));
    // a map ranks its entries by their values unless a key ranks the entries
    if let Some(entries) = entries(cx, src) {
        let (items, values) = entries?;
        return match key {
            None => Ok(pick(cx, &items, &values, k)?.into()),
            Some(f) => ranked(cx, items, f, acc),
        };
    }
    let Some(items) = cx.items(src)? else { return Ok(Step::Drain { it: src, fold: Fold::with(top_fold, acc) }.into()) };
    match key {
        None => Ok(pick(cx, &items, &items, k)?.into()),
        Some(f) => ranked(cx, items, f, acc),
    }
}

/* how many items a count asks for where a negative one asks for none */
fn count(cx: &Cx<'_, '_>, v: Val) -> Option<usize> {
    if let Some(b) = crate::num::bool_int(cx.heap(), v) { return Some(b as usize); }
    if v.is_int() { return Some(usize::try_from(v.as_int()).unwrap_or(0)); }
    if !cx.heap().is_wide(v) { return None; }
    Some(if cx.heap().foreign_cmp(v, Val::int(0)) == Some(Ordering::Less) { 0 } else { usize::MAX })
}

// a map's entries as pairs beside its values
type Entries = (Vec<Val>, Vec<Val>);

/* a map's entries as pairs beside its values and none for anything else */
fn entries(cx: &mut Cx<'_, '_>, src: Val) -> Option<Result<Entries, VmErr>> {
    let pairs: Vec<(Val, Val)> = cx.heap().dict_cell(src)?.borrow().iter().collect();
    cx.spend(pairs.len());
    let mut items = Vec::with_capacity(pairs.len());
    for &(k, v) in &pairs {
        match cx.new_tuple(alloc::vec![k, v]) { Ok(t) => items.push(t), Err(e) => return Some(Err(e)) }
    }
    Some(Ok((items, pairs.into_iter().map(|p| p.1).collect())))
}

/* `items` ranked by key `f` at once or by a drain running it as frames */
fn ranked(cx: &mut Cx<'_, '_>, items: Vec<Val>, f: Val, mut acc: Acc) -> Ret {
    if let Some(keys) = plain_keys(cx, f, &items)? { return Ok(pick(cx, &items, &keys, acc.n as usize)?.into()); }
    let list = cx.new_list(items)?;
    acc.v[1] = list;
    acc.v[2] = Val::int(1);
    acc.items = room(cx, length(cx, list))?;
    Ok(Step::Drain { it: list, fold: Fold::with(top_fold, acc) }.into())
}

/* gathers the items then runs the key on each once a frame key needs it */
fn top_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let keying = !acc.v[2].is_undef();
    match item {
        Item::Next(v) if keying => Ok(Flow::Call(acc.v[0], v)),
        Item::Next(v) => gather(acc, cx, v),
        Item::Called(k) => { acc.items.push(k); Ok(Flow::More) }
        Item::End(_) if keying => {
            let keys = core::mem::take(&mut acc.items);
            let items = match cx.heap().try_get(acc.v[1]) { Some(HeapObj::List(rc)) => rc.borrow().clone(), _ => Vec::new() };
            Ok(Flow::Stop(pick(cx, &items, &keys, acc.n as usize)?))
        }
        Item::End(_) => {
            let items = core::mem::take(&mut acc.items);
            if acc.v[0].is_undef() { return Ok(Flow::Stop(pick(cx, &items, &items, acc.n as usize)?)); }
            if let Some(keys) = plain_keys(cx, acc.v[0], &items)? { return Ok(Flow::Stop(pick(cx, &items, &keys, acc.n as usize)?)); }
            let list = cx.new_list(items)?;
            acc.v[1] = list;
            acc.v[2] = Val::int(1);
            acc.items = room(cx, length(cx, list))?;
            Ok(Flow::Drain(list))
        }
    }
}

fn length(cx: &Cx<'_, '_>, list: Val) -> usize { match cx.heap().try_get(list) { Some(HeapObj::List(rc)) => rc.borrow().len(), _ => 0 } }

/* a new list of the `k` items of greatest key in stable descending order */
fn pick(cx: &mut Cx<'_, '_>, items: &[Val], keys: &[Val], k: usize) -> Result<Val, VmErr> {
    if keys.len() != items.len() { return Err(edge::value::cold_runtime("a ranking lost its items")); }
    let order = top_order(cx, keys, k)?;
    let best: Vec<Val> = order.iter().map(|&i| items[i]).collect();
    cx.new_list(best)
}

/* the indices of the `k` greatest of `n` keys ordered by `lt` */
pub fn greatest<E>(n: usize, k: usize, mut lt: impl FnMut(usize, usize) -> Result<bool, E>) -> Result<Vec<usize>, E> {
    // a heap of the kept keys with the one that ranks lowest on top
    let mut kept: Vec<usize> = Vec::with_capacity(k.min(n));
    if k == 0 { return Ok(kept); }
    for i in 0..n {
        if kept.len() < k {
            kept.push(i);
            let mut at = kept.len() - 1;
            while at > 0 && below(&mut lt, kept[at], kept[(at - 1) / 2])? {
                kept.swap(at, (at - 1) / 2);
                at = (at - 1) / 2;
            }
            continue;
        }
        // a key equal to the lowest kept came later so it ranks below it
        if !lt(kept[0], i)? { continue; }
        kept[0] = i;
        let mut at = 0;
        while 2 * at + 1 < kept.len() {
            let (l, r) = (2 * at + 1, 2 * at + 2);
            let low = if r < kept.len() && below(&mut lt, kept[r], kept[l])? { r } else { l };
            if !below(&mut lt, kept[low], kept[at])? { break; }
            kept.swap(at, low);
            at = low;
        }
    }
    Ok(kept)
}

/* whether key `x` ranks below key `y` with the later of two equal keys lower */
fn below<E>(lt: &mut impl FnMut(usize, usize) -> Result<bool, E>, x: usize, y: usize) -> Result<bool, E> { Ok(lt(x, y)? || (x > y && !lt(y, x)?)) }
