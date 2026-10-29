use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

use edge::lang::{Acc, Args, CLASS, Cx, ErrKind, Flow, Fold, Item, KindId, Ret, STEPS, Sig, Step, TYPE};
use edge::value::{DictMap, DictPart, HeapObj, HeapPool, MAP_TABLE, Val, VmErr};

use crate::fail::{Fail, fail, receiver};
use crate::iter::items::{SHORT, drained, gather, generator, taken};
use crate::lang::Derived;
use crate::{Kernel, Site};

// the bytes a map holds for an entry with its hash before any index
const ENTRY: usize = 24;

/* a new map of a mapping or of pairs and then the keywords */
pub const fn new<S: Site>() -> Kernel {
    // keywords past the source are entries put after its own
    Kernel::new(Sig::range(0, 1).extra(), new_run::<S>).fast(new_fast).flags(TYPE | STEPS)
}

/* the value of a key or a default */
pub const fn get<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), get_run) }

/* the keys walked live */
pub const fn keys<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), keys_run::<S>) }

/* the values walked live */
pub const fn values<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), values_run::<S>) }

/* the entries as pairs walked live */
pub const fn items<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), items_run::<S>) }

/* removes a key and gives its value or a default */
pub const fn pop<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), pop_run::<S>) }

/* removes the last entry and gives it as a pair */
pub const fn pop_last<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), pop_last_run::<S>) }

/* the value of a key put first as a default when missing */
pub const fn set_default<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), set_default_run) }

/* merges a mapping or pairs and then the keywords */
pub const fn update<S: Site>() -> Kernel {
    // keywords past the source come as pairs merged after it
    Kernel::new(Sig::range(0, 1).extra(), update_run::<S>).flags(STEPS)
}

/* a new map of the same entries */
pub const fn copy<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), copy_run) }

/* removes every entry */
pub const fn clear<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), clear_run) }

/* a new map keying one value by every item which reads off the type itself */
pub const fn from_keys<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), from_keys_run).flags(CLASS | STEPS) }

/* a map of `pairs` in order keeping each first key with its last value */
pub fn map_of(cx: &Cx<'_, '_>, pairs: Vec<(Val, Val)>) -> Result<DictMap, VmErr> {
    for &(k, _) in pairs.iter().filter(|p| p.0.is_heap()) { cx.require_key(k)?; }
    table(cx.heap(), pairs.len(), pairs)
}

/* a map of `n` pairs sized at once or grown within the memory left */
pub fn table(heap: &HeapPool, n: usize, pairs: impl IntoIterator<Item = (Val, Val)>) -> Result<DictMap, VmErr> {
    // a short run holds too little to refuse or to spend on
    if n <= 64 {
        let mut d = DictMap::with_capacity(n);
        for (k, v) in pairs { d.insert(k, v, heap); }
        return Ok(d);
    }
    // the limit every host shares decides the sizing so each host pays the same probes
    let fits = n.saturating_mul(MAP_TABLE) <= heap.limit();
    if fits { heap.reserve(n.saturating_mul(MAP_TABLE))?; }
    let mut d = DictMap::with_capacity(if fits { n } else { 0 });
    for (k, v) in pairs {
        let had = d.len();
        d.insert(k, v, heap);
        // colliding keys stop the build once the budget is spent
        heap.within_budget()?;
        // each doubling is refused once the table it builds passes the memory left
        if !fits && d.len() > had && d.len().is_power_of_two() { heap.reserve(d.len().saturating_mul(2 * MAP_TABLE))?; }
    }
    Ok(d)
}

fn new_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(src) = a.get(0) else {
        let d = map_of(cx, a.named.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).collect())?;
        return Ok(cx.new_dict(d)?.into());
    };
    if a.named.is_empty() && let Some(v) = copy_of(cx.heap_mut(), src) { return Ok(v?.into()); }
    if let Some(entries) = cx.heap().dict_cell(src).map(|d| d.borrow().iter().collect::<Vec<_>>()) {
        cx.spend(entries.len());
        let mut pairs = entries;
        pairs.extend(a.named.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])));
        let d = map_of(cx, pairs)?;
        return Ok(cx.new_dict(d)?.into());
    }
    let items = if !generator(cx, src) { cx.items(src)? } else if cx.spent(src) { Some(Vec::new()) } else { None };
    let named = if a.named.is_empty() { Val::undef() } else { cx.new_tuple(a.named.to_vec())? };
    filled::<S>(cx, src, items, named, Val::undef())
}

/* a fresh map filled from `src` or its items then merged into `into` when given */
fn filled<S: Site>(cx: &mut Cx<'_, '_>, src: Val, items: Option<Vec<Val>>, named: Val, into: Val) -> Ret {
    let mut acc = Acc::new();
    acc.v[0] = cx.new_dict(DictMap::new())?;
    (acc.v[2], acc.v[3]) = (named, into);
    let Some(items) = items else {
        // the source's items come first and only then its pairs
        acc.n = -1;
        return Ok(drained(src, fill_end::<S>, acc));
    };
    match put_pairs::<S>(cx, acc.v[0], &items, 0)? {
        None => Ok(finish(cx, &acc)?.into()),
        Some(i) => Ok(drained(items[i], fill_end::<S>, keep_items(cx, acc, items, i)?)),
    }
}

/* the end of a fill drain over its source or one pair needing frames */
fn fill_end<S: Site>(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    let Item::End(_) = item else { return taken(acc, cx, item) };
    let got = core::mem::take(&mut acc.items);
    let (items, at) = if acc.n < 0 {
        (got, 0)
    } else {
        put_pair::<S>(cx, acc.v[0], &got, acc.n as usize)?;
        let kept = match cx.heap().try_get(acc.v[1]) { Some(HeapObj::Tuple(t)) => t.clone(), _ => Vec::new() };
        (kept, acc.n as usize + 1)
    };
    match put_pairs::<S>(cx, acc.v[0], &items, at)? {
        None => Ok(Flow::Stop(finish(cx, acc)?)),
        Some(i) => {
            let next = items[i];
            // the source's items go into a tuple the first time a pair needs frames
            if acc.v[1].is_undef() { *acc = keep_items(cx, core::mem::take(acc), items, i)?; } else { acc.n = i as i64; }
            Ok(Flow::Drain(next))
        }
    }
}

/* the filled map with the keywords put last or nil once merged into the receiver */
fn finish(cx: &Cx<'_, '_>, acc: &Acc) -> Result<Val, VmErr> {
    put_named(cx, acc.v[0], acc.v[2])?;
    if acc.v[3].is_undef() { return Ok(acc.v[0]); }
    let found = entries(cx, acc.v[0]).unwrap_or_default();
    merge(cx, acc.v[3], &found, &[])?;
    Ok(Val::none())
}

/* a fill drain's state walking pair `i` with every item kept in a tuple */
fn keep_items(cx: &mut Cx<'_, '_>, mut acc: Acc, items: Vec<Val>, i: usize) -> Result<Acc, VmErr> {
    acc.n = i as i64;
    acc.v[1] = cx.new_tuple(items)?;
    Ok(acc)
}

/* puts each pair from `at` into map `d` giving the first that needs frames */
fn put_pairs<S: Site>(cx: &mut Cx<'_, '_>, d: Val, items: &[Val], at: usize) -> Result<Option<usize>, VmErr> {
    for (i, &item) in items.iter().enumerate().skip(at) {
        // a tuple or list of two reads in place charged as a copy would be
        let two = match cx.heap().try_get(item) {
            Some(HeapObj::Tuple(t)) => two_of(t),
            Some(HeapObj::List(rc)) => two_of(&rc.borrow()),
            _ => None,
        };
        if let Some(kv) = two {
            cx.charge(2)?;
            put_pair::<S>(cx, d, &kv, i)?;
            continue;
        }
        if cx.is_coroutine(item) { return Err(fail::<S>(cx, Fail::PairNotIterable)); }
        let pair = match cx.items(item) {
            Ok(Some(pair)) => pair,
            Ok(None) => return Ok(Some(i)),
            Err(e) if e.is_kind(ErrKind::Type) => return Err(fail::<S>(cx, Fail::PairNotIterable)),
            Err(e) => return Err(e),
        };
        put_pair::<S>(cx, d, &pair, i)?;
    }
    Ok(None)
}

fn two_of(items: &[Val]) -> Option<[Val; 2]> { match *items { [k, v] => Some([k, v]), _ => None } }

/* puts the key and value of `pair` into map `d` as source element `i` */
fn put_pair<S: Site>(cx: &mut Cx<'_, '_>, d: Val, pair: &[Val], i: usize) -> Result<(), VmErr> {
    let [k, v] = *pair else { return Err(fail::<S>(cx, Fail::PairLength { at: i, len: pair.len() })) };
    cx.require_key(k)?;
    let heap = cx.heap();
    if let Some(m) = heap.dict_cell(d) { m.borrow_mut().insert(k, v, heap); }
    heap.within_budget()
}

/* puts the keyword pairs a tuple holds into map `d` after every other entry */
fn put_named(cx: &Cx<'_, '_>, d: Val, named: Val) -> Result<(), VmErr> {
    let heap = cx.heap();
    let (Some(HeapObj::Tuple(t)), Some(m)) = (heap.try_get(named), heap.dict_cell(d)) else { return Ok(()) };
    let mut m = m.borrow_mut();
    for p in t.as_chunks::<2>().0 {
        m.insert(p[0], p[1], heap);
        heap.within_budget()?;
    }
    Ok(())
}

/* the copy `copy_of` makes and none where its refusal ends the full call */
fn new_fast(heap: &mut HeapPool, x: Val) -> Option<Val> { copy_of(heap, x)?.ok() }

/* a map of a map copied with the hashes it stored */
fn copy_of(heap: &mut HeapPool, x: Val) -> Option<Result<Val, VmErr>> {
    let d = {
        let d = heap.dict_cell(x)?.borrow();
        // a long copy is refused before it is built and pays for its entries
        if d.len().saturating_mul(ENTRY) > SHORT && let Err(e) = heap.reserve(d.len().saturating_mul(MAP_TABLE)) { return Some(Err(e)); }
        if d.len() > 64 { heap.spend(d.len()); }
        d.clone()
    };
    Some(heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(d)))))
}

fn get_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (key, heap) = (a.pos[0], cx.heap());
    let Some(d) = heap.dict_cell(a.recv) else { return Err(receiver()) };
    let found = d.borrow().get(&key, heap).copied();
    if let Some(v) = found { return Ok(v.into()); }
    cx.require_key(key)?;
    Ok(a.pos.get(1).copied().unwrap_or(Val::none()).into())
}

fn keys_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { walk(cx, a.recv, DictPart::Keys, <S::Lang as Derived>::ITERS.keys) }

fn values_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { walk(cx, a.recv, DictPart::Values, <S::Lang as Derived>::ITERS.values) }

fn items_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { walk(cx, a.recv, DictPart::Items, <S::Lang as Derived>::ITERS.items) }

/* an iterator walking the map live so nothing is copied */
fn walk(cx: &mut Cx<'_, '_>, d: Val, part: DictPart, name: &'static str) -> Ret {
    let frame = cx.dict_walk(d, part);
    Ok(cx.new_iter(frame, name)?.into())
}

fn pop_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let key = a.pos[0];
    if let Some(v) = with_map(cx, a.recv, |d, heap| d.remove(&key, heap))? { return Ok(v.into()); }
    cx.require_key(key)?;
    match a.pos.get(1) {
        Some(&default) => Ok(default.into()),
        None => Err(fail::<S>(cx, Fail::MissingKey(key))),
    }
}

fn pop_last_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let last = with_map(cx, a.recv, |d, heap| d.pop_last(heap))?;
    let Some((k, v)) = last else { return Err(fail::<S>(cx, Fail::EmptyMap)) };
    Ok(cx.new_tuple(alloc::vec![k, v])?.into())
}

fn set_default_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (key, default) = (a.pos[0], a.pos.get(1).copied().unwrap_or(Val::none()));
    cx.require_key(key)?;
    let v = with_map(cx, a.recv, |d, heap| {
        if let Some(v) = d.get(&key, heap).copied() { return v; }
        d.insert(key, default, heap);
        default
    })?;
    Ok(v.into())
}

/* merges a mapping or pairs then keywords through a fresh map when pairs run code */
fn update_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(src) = a.get(0) else { return merge(cx, a.recv, &[], a.named).map(Into::into) };
    if let Some(found) = entries(cx, src) { return merge(cx, a.recv, &found, a.named).map(Into::into); }
    let items = cx.items(src)?;
    if let Some(items) = &items && let Some(found) = pairs::<S>(cx, items)? { return merge(cx, a.recv, &found, a.named).map(Into::into); }
    let named = if a.named.is_empty() { Val::undef() } else { cx.new_tuple(a.named.to_vec())? };
    filled::<S>(cx, src, items, named, a.recv)
}

/* the entries of a map source charged for their count */
fn entries(cx: &Cx<'_, '_>, src: Val) -> Option<Vec<(Val, Val)>> {
    let heap = cx.heap();
    let d = heap.dict_cell(src)?.borrow();
    heap.spend(d.len());
    Some(d.iter().collect())
}

/* each item as a key and value or none before stepping one needs frames */
fn pairs<S: Site>(cx: &mut Cx<'_, '_>, items: &[Val]) -> Result<Option<Vec<(Val, Val)>>, VmErr> {
    let mut out = Vec::with_capacity(items.len());
    for &item in items {
        // an iterator is left unread so a drain over the items still finds it whole
        if framed(cx, item) { return Ok(None); }
        let pair = match cx.items(item) {
            Ok(Some(pair)) => pair,
            Ok(None) => return Ok(None),
            Err(e) if e.is_kind(ErrKind::Type) => return Err(fail::<S>(cx, Fail::PairNotIterable)),
            Err(e) => return Err(e),
        };
        let [k, v] = pair[..] else { return Err(fail::<S>(cx, Fail::PairLength { at: out.len(), len: pair.len() })) };
        cx.require_key(k)?;
        out.push((k, v));
    }
    Ok(Some(out))
}

/* puts `found` and then the keyword pairs `named` into map `recv` */
fn merge(cx: &Cx<'_, '_>, recv: Val, found: &[(Val, Val)], named: &[Val]) -> Result<Val, VmErr> {
    let heap = cx.heap();
    let Some(d) = heap.dict_cell(recv) else { return Err(VmErr::Runtime("a merge lost its map")) };
    let mut d = d.borrow_mut();
    // colliding keys stop the merge once the budget is spent
    for &(k, v) in found { d.insert(k, v, heap); heap.within_budget()?; }
    for &[k, v] in named.as_chunks::<2>().0 { d.insert(k, v, heap); heap.within_budget()?; }
    Ok(Val::none())
}

fn copy_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let heap = cx.heap();
    let Some(src) = heap.dict_cell(a.recv) else { return Err(receiver()) };
    let src = src.borrow();
    heap.spend(src.len());
    // a copy is refused past the memory left and keeps the places the map found
    heap.reserve(src.len().saturating_mul(MAP_TABLE))?;
    let d = src.clone();
    drop(src);
    Ok(cx.new_dict(d)?.into())
}

fn clear_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    with_map(cx, a.recv, |d, _| d.clear())?;
    Ok(Val::none().into())
}

/* a new map keying `value` by every item draining keys that may run user code */
fn from_keys_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let value = a.pos.get(1).copied().unwrap_or(Val::none());
    if let Some(keys) = cx.items(a.pos[0])? { return Ok(keyed(cx, &keys, value)?.into()); }
    let mut acc = Acc::new();
    acc.v[0] = value;
    Ok(Step::Drain { it: a.pos[0], fold: Fold::with(from_keys_fold, acc) }.into())
}

/* the drain of `from_keys` whose end keys the map by every item */
fn from_keys_fold(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => gather(acc, cx, v),
        Item::Called(_) => Ok(Flow::More),
        Item::End(_) => {
            let keys = core::mem::take(&mut acc.items);
            Ok(Flow::Stop(keyed(cx, &keys, acc.v[0])?))
        }
    }
}

/* a map of each of `keys` to `value` once every key hashes */
fn keyed(cx: &mut Cx<'_, '_>, keys: &[Val], value: Val) -> Result<Val, VmErr> {
    for &k in keys.iter().filter(|k| k.is_heap()) { cx.require_key(k)?; }
    let d = table(cx.heap(), keys.len(), keys.iter().map(|&k| (k, value)))?;
    cx.new_dict(d)
}

/* whether stepping pair `v` may need frames so the fill drains it */
fn framed(cx: &Cx<'_, '_>, v: Val) -> bool { matches!(cx.kind(v), KindId::Coro | KindId::Iter) }

/* the map `recv` holds changed through `f` where any other receiver fails */
fn with_map<R>(cx: &mut Cx<'_, '_>, recv: Val, f: impl FnOnce(&mut DictMap, &HeapPool) -> R) -> Result<R, VmErr> {
    let heap = cx.heap();
    match heap.dict_cell(recv) {
        Some(d) => Ok(f(&mut d.borrow_mut(), heap)),
        None => Err(receiver()),
    }
}
