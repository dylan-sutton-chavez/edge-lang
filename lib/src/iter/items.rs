use alloc::rc::Rc;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::RefCell;

use alloc::boxed::Box;

use edge::lang::{Acc, Cx, Flow, Fold, FoldFn, Item, Out, Step};
use edge::value::{DictPart, HeapObj, HeapPool, IterFrame, Val, VmErr};

use crate::Lang;

/* takes item `v` into the drain's gathered items refusing a list past the memory left */
pub fn gather(acc: &mut Acc, cx: &Cx<'_, '_>, v: Val) -> Result<Flow, VmErr> {
    if (acc.items.len() + 1).saturating_mul(core::mem::size_of::<Val>()) >= cx.room() { return Err(VmErr::Heap); }
    acc.items.push(v);
    Ok(Flow::More)
}

// what a new value takes in its slot and in the list holding it
pub(crate) const PIECE: usize = core::mem::size_of::<HeapObj>() + 2 * core::mem::size_of::<Val>();

// a result this small fits a heap near its limit so no room check runs
pub(crate) const SHORT: usize = 1 << 16;

/* a container's items in order each read spent and none past what is left */
pub fn plain_items(heap: &mut HeapPool, v: Val) -> Option<Vec<Val>> {
    let items: Vec<Val> = match heap.try_get(v)? {
        HeapObj::List(rc) => { let l = rc.borrow(); fits(heap, l.len(), 1)?; l.clone() }
        HeapObj::Tuple(t) => { fits(heap, t.len(), 1)?; t.clone() }
        &HeapObj::Range(start, stop, step) => range_items(heap, start, stop, step)?,
        HeapObj::Bytes(b) | HeapObj::ByteArray(b) => { fits(heap, b.len(), 1)?; b.iter().map(|&x| Val::int(x as i64)).collect() }
        HeapObj::Dict(d) => { let d = d.borrow(); fits(heap, d.len(), 1)?; d.keys().collect() }
        HeapObj::Set(s) => { let s = s.borrow(); fits(heap, s.len(), 1)?; s.iter().copied().collect() }
        HeapObj::Str(s) => {
            // a character past ascii becomes a text in a slot of its own
            if s.is_ascii() { fits(heap, s.len(), 1)?; } else { fits(heap, s.chars().count(), PIECE / core::mem::size_of::<Val>())?; }
            let s = s.clone();
            // the bytes are spent as each character becomes a text of its own
            heap.spend(s.len());
            return s.chars().map(|c| heap.alloc(HeapObj::Str(c.to_string())).ok()).collect();
        }
        _ => return None,
    };
    heap.spend(items.len());
    Some(items)
}

/* none once `n` items of `words` values pass what is left */
#[inline(always)]
fn fits(heap: &HeapPool, n: usize, words: usize) -> Option<()> {
    let bytes = n.saturating_mul(words * core::mem::size_of::<Val>());
    if bytes > SHORT && (heap.reserve(bytes).is_err() || heap.afford(n).is_err()) { return None; }
    Some(())
}

/* the iterator over a container made with no call and none for the full call */
pub fn plain_iter<L: Lang>(heap: &mut HeapPool, v: Val) -> Option<Val> {
    let names = L::ITERS;
    let (frame, name) = match heap.try_get(v)? {
        HeapObj::Iter(..) => return Some(v),
        HeapObj::List(rc) => (IterFrame::List { rc: rc.clone(), idx: 0 }, names.list),
        &HeapObj::Range(cur, end, step) => (IterFrame::Range { cur, end, step }, names.range),
        HeapObj::Dict(d) => {
            let len = d.borrow().len();
            (IterFrame::DictWalk { of: v, idx: 0, left: len, len, part: DictPart::Keys }, names.keys)
        }
        HeapObj::Tuple(_) => (seq(plain_items(heap, v)?), names.tuple),
        // octets and a long text's characters are copied once and read one a step
        HeapObj::Bytes(b) => (IterFrame::Octets { bytes: b.as_slice().into(), idx: 0 }, names.bytes),
        HeapObj::ByteArray(b) => (IterFrame::Octets { bytes: b.as_slice().into(), idx: 0 }, names.buffer),
        HeapObj::Str(s) if s.len() > edge::value::SHORT_TEXT => { let text = s.as_str().into(); (IterFrame::Chars { text, at: 0 }, if heap.str_is_ascii(v) { names.ascii } else { names.text }) }
        HeapObj::Str(_) => (seq(plain_items(heap, v)?), if heap.str_is_ascii(v) { names.ascii } else { names.text }),
        // a set fails its walk once its size changes
        HeapObj::Set(_) => {
            let items = plain_items(heap, v)?;
            let len = items.len();
            (IterFrame::Watched { items: items.into(), idx: 0, of: v, len }, names.set)
        }
        _ => return None,
    };
    heap.alloc(HeapObj::Iter(Rc::new(RefCell::new(frame)), name)).ok()
}

/* a walk over items read up front */
pub fn seq(items: Vec<Val>) -> IterFrame { IterFrame::Seq { items: items.into(), idx: 0 } }

/* whether `src` is a generator which only the drain steps unless it is spent */
#[inline]
pub fn generator(cx: &Cx<'_, '_>, src: Val) -> bool { matches!(cx.heap().try_get(src), Some(HeapObj::Coroutine(_))) }

/* the drain of `src` gathering every item for the end `on` built in its box */
#[inline(always)]
pub fn drained(src: Val, on: FoldFn, acc: Acc) -> Out { Out::Step(Box::new(Step::Drain { it: src, fold: Fold::collect(on, acc) })) }

/* an item a gathering drain hands its function rather than taking it itself */
pub fn taken(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => gather(acc, cx, v),
        Item::Called(_) | Item::End(_) => Ok(Flow::More),
    }
}

/* the inline ints of a range or none past the memory left */
pub fn range_items(heap: &HeapPool, start: i64, stop: i64, step: i64) -> Option<Vec<Val>> {
    let count = range_len(start, stop, step);
    if count == 0 { return Some(Vec::new()); }
    if count.saturating_mul(core::mem::size_of::<Val>() as u64) > heap.room() as u64 { return None; }
    // the memory left keeps the count far below a word
    let last = step.checked_mul(count as i64 - 1).and_then(|d| start.checked_add(d))?;
    let inline = Val::INT_MIN..=Val::INT_MAX;
    if !(inline.contains(&start) && inline.contains(&last)) { return None; }
    Some((0..count as i64).map(|k| Val::int(start + k * step)).collect())
}

/* how many ints a range of a nonzero step yields counted in words */
pub fn range_len(start: i64, stop: i64, step: i64) -> u64 {
    let (lo, hi) = if step > 0 { (start, stop) } else { (stop, start) };
    if hi <= lo { return 0; }
    hi.abs_diff(lo).div_ceil(step.unsigned_abs())
}
