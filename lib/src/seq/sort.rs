use alloc::vec::Vec;
use core::cmp::{Ordering, Reverse};

use edge::lang::{Acc, Args, Cx, Flow, Fold, Item, KindId, Ret, STEPS, Sig, Step};
use edge::value::{HeapObj, HeapPool, Val, VmErr, cold_runtime};

use super::timsort;
use crate::fail::{Fail, fail, receiver};
use crate::{Kernel, Site};

/* the items in a new list in order by a key that may run backward */
pub const fn sorted<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), sorted_run).flags(STEPS).reads(2) }

/* a list ordered in place by a key that may run backward */
pub const fn sort<S: Site>() -> Kernel {
    // the body checks its positionals before any other keyword in its own words
    Kernel::new(Sig::var(0).extra(), sort_run::<S>).flags(STEPS).reads(2)
}

// the bits of `acc.n` a keyed drain keeps
const REVERSE: i64 = 1;
const KEYING: i64 = 2;

/* the items in a new list with keys and generators run through a drain */
fn sorted_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (key, reverse) = options(cx, &a);
    let Some(mut items) = cx.items(a.pos[0])? else {
        let mut acc = Acc::new();
        (acc.v[0], acc.n) = (key.unwrap_or(Val::undef()), reverse as i64);
        // a key waits until every item is gathered
        let fold = if key.is_some() { Fold::with(keyed_fold, acc) } else { Fold::collect(gathered_fold, acc) };
        return Ok(Step::Drain { it: a.pos[0], fold }.into());
    };
    match key {
        Some(key) if !items.is_empty() => match plain_keys(cx, key, &items)? {
            Some(keys) => {
                let mut order = key_order(cx, &keys, reverse)?;
                arrange(&mut items, &mut order);
                Ok(cx.new_list(items)?.into())
            }
            None => {
                let list = cx.new_list(items)?;
                Ok(keyed(cx, list, key, reverse, Val::undef())?.into())
            }
        },
        _ => {
            order_vals(cx, &mut items, reverse)?;
            Ok(cx.new_list(items)?.into())
        }
    }
}

/* a list's sort in place where a key drains the items as they were */
fn sort_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    if a.pos.len() > 2 { return Err(fail::<S>(cx, Fail::SortCount(a.pos.len()))); }
    if !a.pos.is_empty() { return Err(fail::<S>(cx, Fail::SortPositional)); }
    if let Some(&name) = a.named.first() {
        let name = alloc::string::String::from(cx.kw_name(name).unwrap_or(""));
        return Err(fail::<S>(cx, Fail::SortKeyword(name)));
    }
    let (key, reverse) = options(cx, &a);
    let Some(HeapObj::List(rc)) = cx.heap().try_get(a.recv) else { return Err(receiver()) };
    let rc = rc.clone();
    if let Some(key) = key {
        let mut items = rc.borrow().clone();
        if items.is_empty() { return Ok(Val::none().into()); }
        let Some(keys) = plain_keys(cx, key, &items)? else {
            let snap = cx.new_list(items)?;
            return Ok(keyed(cx, snap, key, reverse, a.recv)?.into());
        };
        let mut order = key_order(cx, &keys, reverse)?;
        arrange(&mut items, &mut order);
        *rc.borrow_mut() = items;
        return Ok(Val::none().into());
    }
    afford_sort(cx.heap(), rc.borrow().len())?;
    if plain_sort(&mut rc.borrow_mut(), reverse, cx.heap()) { return Ok(Val::none().into()); }
    // a comparison may read this list so it orders a copy
    let mut items = rc.borrow().clone();
    let mut order = full_order(cx, &items, reverse)?;
    arrange(&mut items, &mut order);
    *rc.borrow_mut() = items;
    Ok(Val::none().into())
}

/* the key a call names unless nil and whether the order runs descending */
fn options(cx: &Cx<'_, '_>, a: &Args<'_>) -> (Option<Val>, bool) {
    (a.kw(0).filter(|k| !k.is_none()), a.kw(1).is_some_and(|r| cx.truthy(r)))
}

/* a builtin key's values at once and None for a key run as frames */
pub(crate) fn plain_keys(cx: &mut Cx<'_, '_>, key: Val, items: &[Val]) -> Result<Option<Vec<Val>>, VmErr> {
    if matches!(cx.kind(key), KindId::Func | KindId::Class) { return Ok(None); }
    cx.call_each(key, items)
}

/* a drain keying each item of the new `list` whose end orders `into` or `list` */
fn keyed(cx: &Cx<'_, '_>, list: Val, key: Val, reverse: bool, into: Val) -> Result<Step, VmErr> {
    let mut acc = Acc::new();
    (acc.v[1], acc.v[2], acc.n) = (list, into, reverse as i64 | KEYING);
    acc.items = room(cx, len(cx, list))?;
    Ok(Step::Drain { it: list, fold: Fold::keys(key, by_keys, acc) })
}

/* the end of a keys drain whose keys the engine gathered */
fn by_keys(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item { Item::End(_) => ordered_by_keys(acc, cx), _ => Ok(Flow::More) }
}

fn len(cx: &Cx<'_, '_>, list: Val) -> usize { match cx.heap().try_get(list) { Some(HeapObj::List(rc)) => rc.borrow().len(), _ => 0 } }

/* a buffer for `n` values refused past the memory left */
pub(crate) fn room(cx: &Cx<'_, '_>, n: usize) -> Result<Vec<Val>, VmErr> {
    if n.saturating_mul(core::mem::size_of::<Val>()) >= cx.room() { return Err(VmErr::Heap); }
    Ok(Vec::with_capacity(n))
}

/* calls the key on each item once any generator's items are gathered */
fn keyed_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) if acc.n & KEYING != 0 => Ok(Flow::Call(acc.v[0], v)),
        Item::Next(v) => {
            if (acc.items.len() + 1).saturating_mul(core::mem::size_of::<Val>()) >= cx.room() { return Err(VmErr::Heap); }
            acc.items.push(v);
            Ok(Flow::More)
        }
        Item::Called(k) => {
            acc.items.push(k);
            Ok(Flow::More)
        }
        Item::End(_) if acc.n & KEYING != 0 => ordered_by_keys(acc, cx),
        Item::End(_) => {
            let list = cx.new_list(core::mem::take(&mut acc.items))?;
            (acc.v[1], acc.n) = (list, acc.n | KEYING);
            acc.items = room(cx, len(cx, list))?;
            Ok(Flow::Drain(list))
        }
    }
}

/* the end of a gathered drain with no key ordering what the engine collected */
fn gathered_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let Item::End(_) = item else { return Ok(Flow::More) };
    let mut items = core::mem::take(&mut acc.items);
    order_vals(cx, &mut items, acc.n & REVERSE != 0)?;
    Ok(Flow::Stop(cx.new_list(items)?))
}

/* orders the drained list by its keys in place or into the receiver */
fn ordered_by_keys(acc: &mut Acc, cx: &mut Cx<'_, '_>) -> Result<Flow, VmErr> {
    let keys = core::mem::take(&mut acc.items);
    let order = key_order(cx, &keys, acc.n & REVERSE != 0)?;
    let heap = cx.heap();
    let Some(HeapObj::List(rc)) = heap.try_get(acc.v[1]) else { return Err(cold_runtime("a sort lost its items")) };
    if rc.borrow().len() != order.len() { return Err(cold_runtime("a sort lost its items")); }
    let Some(HeapObj::List(out)) = heap.try_get(acc.v[2]) else {
        let sorted: Vec<Val> = { let items = rc.borrow(); order.iter().map(|&i| items[i]).collect() };
        *rc.borrow_mut() = sorted;
        return Ok(Flow::Stop(acc.v[1]));
    };
    // the copy the keys walked hands its items over so nothing waits for a sweep
    let items = core::mem::take(&mut *rc.borrow_mut());
    let mut l = out.borrow_mut();
    l.clear();
    l.extend(order.iter().map(|&i| items[i]));
    Ok(Flow::Stop(Val::none()))
}

/* sorts values by `<` in place where only mixed kinds take the full comparison */
fn order_vals(cx: &Cx<'_, '_>, items: &mut [Val], reverse: bool) -> Result<(), VmErr> {
    afford_sort(cx.heap(), items.len())?;
    if plain_sort(items, reverse, cx.heap()) { return Ok(()); }
    let mut order = full_order(cx, items, reverse)?;
    arrange(items, &mut order);
    Ok(())
}

/* sorts `items` in place when one plain kind orders them and false otherwise */
fn plain_sort(items: &mut [Val], reverse: bool, heap: &HeapPool) -> bool {
    if items.len() < 2 { return true; }
    if items.iter().all(Val::is_int) {
        pay_sort(heap, items.len(), 0, items.len());
        // equal ints are one value so no order among them can show
        if reverse { items.sort_unstable_by_key(|v| Reverse(v.as_int())); } else { items.sort_unstable_by_key(|v| v.as_int()); }
        return true;
    }
    let Some(mut order) = plain_order(items, reverse, heap) else { return false };
    arrange(items, &mut order);
    true
}

/* the comparisons `n` keys holding `units` make in a pick among `among` */
fn compares(n: usize, units: usize, among: usize) -> usize {
    // each key meets about log2 of `among` others reading what the keys hold
    n.saturating_add(units).saturating_mul((usize::BITS - among.leading_zeros()) as usize)
}

/* refuses a sort of `n` keys whose comparisons pass the budget left before it starts */
#[inline]
pub(crate) fn afford_sort(heap: &HeapPool, n: usize) -> Result<(), VmErr> {
    if n > 64 { heap.afford(compares(n, 0, n))?; }
    Ok(())
}

/* counts the comparisons of a sort of `n` keys holding `units` as native work */
#[inline]
fn pay_sort(heap: &HeapPool, n: usize, units: usize, among: usize) {
    let work = compares(n, units, among);
    if work > 64 { heap.spend(work); }
}

/* refuses comparisons past the budget left and counts them as native work */
fn sorting(heap: &HeapPool, n: usize, among: usize) -> Result<(), VmErr> {
    if n > 64 { heap.afford(compares(n, 0, among))?; }
    pay_sort(heap, n, 0, among);
    Ok(())
}

/* the stable order of `keys` by `<` where the first failing comparison ends the sort */
pub(crate) fn key_order(cx: &Cx<'_, '_>, keys: &[Val], reverse: bool) -> Result<Vec<usize>, VmErr> {
    afford_sort(cx.heap(), keys.len())?;
    match plain_order(keys, reverse, cx.heap()) {
        Some(order) => Ok(order),
        None => full_order(cx, keys, reverse),
    }
}

/* the stable order of keys of one plain kind and None for any other keys */
fn plain_order(keys: &[Val], reverse: bool, heap: &HeapPool) -> Option<Vec<usize>> {
    let (kind, units) = plain_kind(keys, heap)?;
    let n = keys.len();
    let mut order: Vec<usize> = match kind {
        Plain::Int | Plain::Num => {
            pay_sort(heap, n, 0, n);
            return Some(if kind == Plain::Int { by_word(keys.iter().map(|v| v.as_int()), reverse) } else { by_word(keys.iter().map(|&v| rank(num(v))), reverse) });
        }
        Plain::Str | Plain::Tuple => (0..n).collect(),
    };
    if kind == Plain::Str {
        let k: Vec<&str> = keys.iter().map(|&v| text(v, heap)).collect();
        // a compare reads the bytes two texts share which count sixteen at a time
        pay_sort(heap, n, k.iter().map(|s| s.len() / 16).fold(0, usize::saturating_add), n);
        order.sort_by(|&x, &y| facing(k[x].cmp(k[y]), reverse));
    } else {
        let k: Vec<&[Val]> = keys.iter().map(|&v| members(v, heap)).collect();
        pay_sort(heap, n, units, n);
        order.sort_by(|&x, &y| facing(seq_cmp(k[x], k[y], heap), reverse));
    }
    Some(order)
}

/* the `k` greatest keys in the order a stable descending sort of all gives */
pub(crate) fn top_order(cx: &Cx<'_, '_>, keys: &[Val], k: usize) -> Result<Vec<usize>, VmErr> {
    if k >= keys.len() { return key_order(cx, keys, true); }
    sorting(cx.heap(), keys.len(), k)?;
    let mut kept = super::top::greatest(keys.len(), k, |x, y| cx.lt(keys[x], keys[y]))?;
    // the kept keys in their first order so equal keys keep it
    kept.sort_unstable();
    let sub: Vec<Val> = kept.iter().map(|&i| keys[i]).collect();
    let order = key_order(cx, &sub, true)?;
    Ok(order.into_iter().map(|j| kept[j]).collect())
}

/* the stable order of keys that order as words */
fn by_word<K: Ord + Copy>(keys: impl Iterator<Item = K>, reverse: bool) -> Vec<usize> {
    let mut k: Vec<(K, usize)> = keys.zip(0..).collect();
    if reverse { k.sort_by_key(|e| Reverse(e.0)); } else { k.sort_by_key(|e| e.0); }
    k.into_iter().map(|e| e.1).collect()
}

/* the order the full comparison gives `keys` as the merge of natural runs finds it */
fn full_order(cx: &Cx<'_, '_>, keys: &[Val], reverse: bool) -> Result<Vec<usize>, VmErr> {
    sorting(cx.heap(), keys.len(), keys.len())?;
    // a descending sort turns the items first so equal keys keep their order
    let mut order: Vec<usize> = if reverse { (0..keys.len()).rev().collect() } else { (0..keys.len()).collect() };
    let sorted = timsort::sort(&mut order, &mut |x: usize, y: usize| cx.lt(keys[x], keys[y]));
    if reverse { order.reverse(); }
    sorted?;
    Ok(order)
}

/* moves each item to its place in `order` following the cycles with nothing copied */
fn arrange(items: &mut [Val], order: &mut [usize]) {
    for start in 0..order.len() {
        if order[start] == usize::MAX { continue; }
        let (first, mut at) = (items[start], start);
        loop {
            let from = core::mem::replace(&mut order[at], usize::MAX);
            if from == start { items[at] = first; break; }
            items[at] = items[from];
            at = from;
        }
    }
}

fn facing(o: Ordering, reverse: bool) -> Ordering { if reverse { o.reverse() } else { o } }

/* how keys of one kind order with no comparison that could fail */
#[derive(Clone, Copy, PartialEq)]
enum Plain { Int, Num, Str, Tuple }

/* the plain kind of `v` and None for one only the full comparison orders */
fn plain(v: Val, heap: &HeapPool) -> Option<Plain> {
    if v.is_int() { return Some(Plain::Int); }
    if v.is_bool() && heap.policy().bool_is_int || v.is_float() && !v.as_float().is_nan() { return Some(Plain::Num); }
    match heap.try_get(v)? {
        HeapObj::Str(_) => Some(Plain::Str),
        HeapObj::Tuple(_) => Some(Plain::Tuple),
        _ => None,
    }
}

/* the plain kind all keys share with tuples alike and what their compares read */
fn plain_kind(keys: &[Val], heap: &HeapPool) -> Option<(Plain, usize)> {
    let (&first, rest) = keys.split_first()?;
    let mut kind = plain(first, heap)?;
    for &k in rest {
        kind = match (kind, plain(k, heap)?) {
            (a, b) if a == b => a,
            (Plain::Int | Plain::Num, Plain::Int | Plain::Num) => Plain::Num,
            _ => return None,
        };
    }
    let mut units = 0usize;
    if kind == Plain::Tuple {
        for &k in keys { units = units.saturating_add(same_shape(first, k, heap, 0)?); }
    }
    Some((kind, units))
}

/* what `b` holds when plain like `a` at every tuple position four deep */
fn same_shape(a: Val, b: Val, heap: &HeapPool, depth: usize) -> Option<usize> {
    match (plain(a, heap)?, plain(b, heap)?) {
        (Plain::Int | Plain::Num, Plain::Int | Plain::Num) => Some(0),
        // a text inside a tuple counts its bytes sixteen at a time
        (Plain::Str, Plain::Str) => Some(if depth > 0 { text(b, heap).len() / 16 } else { 0 }),
        (Plain::Tuple, Plain::Tuple) if depth < 4 => match (heap.try_get(a), heap.try_get(b)) {
            (Some(HeapObj::Tuple(x)), Some(HeapObj::Tuple(y))) if x.len() == y.len() => {
                let mut units = y.len();
                for (&p, &q) in x.iter().zip(y) { units = units.saturating_add(same_shape(p, q, heap, depth + 1)?); }
                Some(units)
            }
            _ => None,
        },
        _ => None,
    }
}

/* a float but NaN as a word in its order with both zeros alike */
fn rank(f: f64) -> u64 {
    let b = if f == 0.0 { 0 } else { f.to_bits() };
    if b >> 63 == 1 { !b } else { b | 1 << 63 }
}

/* a plain number as the float it orders by */
fn num(v: Val) -> f64 { if v.is_float() { v.as_float() } else if v.is_bool() { v.as_bool() as i64 as f64 } else { v.as_int() as f64 } }

fn text(v: Val, heap: &HeapPool) -> &str { match heap.try_get(v) { Some(HeapObj::Str(s)) => s.as_str(), _ => "" } }

fn members(v: Val, heap: &HeapPool) -> &[Val] { match heap.try_get(v) { Some(HeapObj::Tuple(t)) => t.as_slice(), _ => &[] } }

/* two plain tuples item by item and then by length */
fn seq_cmp(x: &[Val], y: &[Val], heap: &HeapPool) -> Ordering {
    for (&p, &q) in x.iter().zip(y) {
        let o = plain_cmp(p, q, heap);
        if o != Ordering::Equal { return o; }
    }
    x.len().cmp(&y.len())
}

/* two plain keys of one shape as `<` orders them */
fn plain_cmp(a: Val, b: Val, heap: &HeapPool) -> Ordering {
    if a.is_int() && b.is_int() { return a.as_int().cmp(&b.as_int()); }
    match (heap.try_get(a), heap.try_get(b)) {
        (Some(HeapObj::Str(x)), Some(HeapObj::Str(y))) => x.as_str().cmp(y.as_str()),
        (Some(HeapObj::Tuple(x)), Some(HeapObj::Tuple(y))) => seq_cmp(x, y, heap),
        _ => num(a).partial_cmp(&num(b)).unwrap_or(Ordering::Equal),
    }
}
