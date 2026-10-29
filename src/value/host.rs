use alloc::{string::{String, ToString}, vec::Vec};
use core::cmp::Ordering;
use super::*;
use crate::lang::{Act, Cmp, EngineError as E, KindId, Op, Use};

// what a builtin body reads past the heap so one body serves two hosts
pub trait Host {
    fn heap(&self) -> &HeapPool;
    fn heap_mut(&mut self) -> &mut HeapPool;
    // pays native work against the op budget
    fn charge(&mut self, n: usize) -> Result<(), VmErr>;
    // items only the vm can step and none for a builtin container
    fn vm_items(&mut self, _v: Val) -> Result<Option<Vec<Val>>, VmErr> { Ok(None) }
    fn type_name(&self, v: Val) -> &str { self.heap().type_name(v) }
    // the runtime's natives which name the builtins an error mentions
    fn natives(&self) -> &'static crate::lang::Natives { &crate::lang::Natives::EMPTY }
    // keeps `v` alive until `unroot` lets it go
    fn root(&mut self, _v: Val) {}
    fn unroot(&mut self, _n: usize) {}
    // runs `f` with `vals` kept alive
    fn with_roots<R>(&mut self, vals: impl IntoIterator<Item = Val>, f: impl FnOnce(&mut Self) -> R) -> R where Self: Sized {
        let mut n = 0;
        for v in vals { self.root(v); n += 1; }
        let r = f(self);
        self.unroot(n);
        r
    }

    // the key error of the missing key with the key's text
    fn key_error(&mut self, key: Val) -> VmErr;

    // engine error `e` in the runtime's words under the class its kind names
    fn fail(&mut self, e: crate::lang::EngineError) -> VmErr;

    #[inline] fn truthy(&self, v: Val) -> bool { self.heap().truthy(v) }
    #[inline] fn as_i128(&self, v: Val) -> Option<i128> { super::as_i128(v, self.heap()) }
    // the text of a str and none for any other value
    #[inline] fn text(&self, v: Val) -> Option<&str> { match self.heap().try_get(v) { Some(HeapObj::Str(s)) => Some(s), _ => None } }
    #[inline] fn alloc_list(&mut self, items: Vec<Val>) -> Result<Val, VmErr> { self.heap_mut().alloc(HeapObj::List(alloc::rc::Rc::new(core::cell::RefCell::new(items)))) }

    // the type error of a non-iterable operand
    fn not_iterable(&self, o: Val) -> VmErr { E::cannot(Act::Iterate, Some(o)) }

    fn str_to_char_vals(&mut self, s: &str) -> Result<Vec<Val>, VmErr> {
        // a long text whose characters pass the memory left is refused before any exists
        if s.len() > 64 && !text_fits(self.heap(), s) { return Err(cold_heap()); }
        // one op a byte pays for the slots its characters take
        self.charge(s.len())?;
        s.chars().map(|c| self.heap_mut().alloc(HeapObj::Str(c.to_string()))).collect()
    }

    // the items of `o` charged as native work and refused as `iter` refuses them
    fn extract_iter(&mut self, o: Val) -> Result<Vec<Val>, VmErr> {
        if !o.is_heap() {
            // a foreign kind's frame steps its items with the count it would hold checked first
            let Some(walk) = self.heap().foreign_frame(o) else { return Err(self.not_iterable(o)) };
            let n = match self.heap_mut().foreign_len(o) { Some(n) => n?, None => 0 };
            return self.frame_items(n, walk);
        }
        if let Some(items) = self.vm_items(o)? { return Ok(items); }
        // a wide range steps its cursor with the count it would hold checked first
        if let Some(&HeapObj::BigRange(cur, end, step)) = self.heap().try_get(o) {
            let n = super::wide::range_len(self.heap_mut(), o).unwrap_or(Ok(0))?;
            return self.frame_items(n, IterFrame::BigRange { cur, end, step });
        }
        // a long copy is refused past the memory or budget left before it starts
        let heap = self.heap();
        let fits = |n: usize| if n <= 64 { Ok(()) } else { long_fits(heap, n) };
        // the items are copied out so the heap borrow ends before any allocation
        let snapshot = match heap.get(o) {
            HeapObj::List(v) => { let v = v.borrow(); fits(v.len())?; Some(v.clone()) }
            HeapObj::Tuple(v) => { fits(v.len())?; Some(v.clone()) }
            HeapObj::Set(v) => { let v = v.borrow(); fits(v.len())?; Some(v.iter().cloned().collect()) }
            HeapObj::Range(s, e, st) => {
                let (mut cur, end, step) = (*s, *e, *st);
                let span = (end as i128 - cur as i128).unsigned_abs();
                // a range running away from its stop holds nothing
                let count = if step == 0 || (step > 0) != (end > cur) { 0 } else { span / (step as i128).unsigned_abs() };
                // an item past the inline ints holds an entry of the FOREIGN table too
                let each = VAL_BYTES + if [cur, end].iter().any(|x| !(Val::INT_MIN..=Val::INT_MAX).contains(x)) { foreign::ENTRY_BYTES } else { 0 };
                if count.saturating_mul(each as u128) > self.heap().room() as u128 { return Err(cold_heap()); }
                let mut out = Vec::new();
                if step > 0 {
                    while cur < end {
                        out.push(self.heap_mut().int(cur as i128)?);
                        match cur.checked_add(step) { Some(n) => cur = n, None => break }
                    }
                } else {
                    while cur > end {
                        out.push(self.heap_mut().int(cur as i128)?);
                        match cur.checked_add(step) { Some(n) => cur = n, None => break }
                    }
                }
                Some(out)
            }
            HeapObj::Dict(d) => { let d = d.borrow(); fits(d.len())?; Some(d.keys().collect()) }
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => { fits(b.len())?; Some(b.iter().map(|&x| Val::int(x as i64)).collect()) }
            // a text is handled below since each of its characters takes a slot
            HeapObj::Str(_) => None,
            _ => return Err(self.not_iterable(o)),
        };
        if let Some(v) = snapshot {
            // a copy pays one op an item so repeated copies stay bounded
            self.charge(v.len())?;
            return Ok(v);
        }
        // a text gives one value for each of its characters
        if let HeapObj::Str(s) = self.heap().get(o) {
            let s = s.clone();
            return self.str_to_char_vals(&s);
        }
        unreachable!()
    }

    /* the `n` items a frame steps refused past the memory left before any is made */
    #[inline(never)]
    fn frame_items(&mut self, n: i128, mut walk: IterFrame) -> Result<Vec<Val>, VmErr> {
        // each item may hold an entry of the FOREIGN table too
        if usize::try_from(n.max(0)).unwrap_or(usize::MAX).saturating_mul(VAL_BYTES + foreign::ENTRY_BYTES) > self.heap().room() { return Err(cold_heap()); }
        let mut out = Vec::new();
        while let Some(x) = walk.next_item(self.heap_mut())? { out.push(x); }
        self.charge(out.len())?;
        Ok(out)
    }

    /* rejects the mutable types a dict or set may not key by */
    fn require_hashable(&self, v: Val) -> Result<(), VmErr> { self.require_hashable_at(v, 0) }

    /* a set probing a set finds nothing so only other values must hash */
    fn require_set_probe(&self, v: Val) -> Result<(), VmErr> {
        if v.is_heap() && matches!(self.heap().get(v), HeapObj::Set(_)) { return Ok(()); }
        self.require_element(v)
    }

    /* `v` as a dict key refusing an unhashable one by that use and its type */
    fn require_key(&self, v: Val) -> Result<(), VmErr> { self.require_hashable(v).map_err(|e| self.unhashable_as(v, &e, Use::Key)) }

    /* `v` as a set element refusing an unhashable one by that use and its type */
    fn require_element(&self, v: Val) -> Result<(), VmErr> { self.require_hashable(v).map_err(|e| self.unhashable_as(v, &e, Use::Element)) }

    #[cold]
    fn unhashable_as(&self, v: Val, e: &VmErr, at: Use) -> VmErr {
        match e {
            VmErr::Engine(b) if let E::Cannot { act: Act::Hash { inner, .. }, .. } = **b => E::cannot(Act::Hash { at, inner }, Some(v)),
            e => e.clone(),
        }
    }

    fn require_hashable_at(&self, v: Val, depth: usize) -> Result<(), VmErr> {
        let heap = self.heap();
        match hashable(heap, v, depth) {
            Some(Ok(items)) => {
                let mut walk = Walk::<Bounded>::default();
                let mut r = hashable_items(heap, v, items, depth, &mut walk);
                let mut read = walk.read;
                if walk.long() {
                    let mut kept = Keeping::default();
                    r = hashable_items(heap, v, items, depth, &mut kept);
                    read = read.saturating_add(kept.read);
                }
                // many items count what the check reads as native work
                if read > 64 { heap.spend(read); }
                r
            }
            Some(Err(e)) => Err(e),
            None => Ok(()),
        }
    }

    /* `v` as one byte which a `bytearray` item must be */
    fn byte_value(&self, v: Val) -> Result<u8, VmErr> {
        let Some(n) = self.as_i128(v).filter(|_| !v.is_float()) else { return Err(E::cannot(Act::Index, Some(v))) };
        u8::try_from(n).map_err(|_| E::ByteRange.into())
    }

    /* a set of `items` refusing any that does not hash */
    fn valset_of(&mut self, items: &[Val]) -> Result<ValSet, VmErr> {
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; }
        ValSet::build(items, self.heap())
    }

    /* a set of `items` deduped by content */
    fn alloc_set_result(&mut self, items: Vec<Val>) -> Result<Val, VmErr> {
        let s = ValSet::build(&items, self.heap())?;
        self.heap_mut().alloc(HeapObj::Set(Rc::new(RefCell::new(s))))
    }

    /* an i128 as the narrowest value with an overflow error for none */
    #[inline]
    fn int_to_val(&mut self, r: Option<i128>) -> Result<Val, VmErr> {
        self.heap_mut().int(r.ok_or_else(cold_overflow)?)
    }

    /* builtin indexing for callers without a bytecode frame and for `get_item` */
    fn get_item_builtin(&mut self, obj: Val, idx: Val) -> Result<Val, VmErr> {
        if let Some([start, stop, step]) = self.heap().slice_of(idx) { return self.slice_val(obj, start, stop, step); }

        let ascii = idx.is_int() && self.heap_mut().str_is_ascii(obj);
        if obj.is_heap() && idx.is_int()
            && let HeapObj::Str(s) = self.heap().get(obj) {
                let i = idx.as_int();
                let c = if ascii {
                    let b = s.as_bytes();
                    char::from(*b.get(normalize_index(i, b.len(), self.heap().policy)).ok_or_else(|| out_of_range(obj))?)
                } else if s.len() <= 256 {
                    short_char(s, i, self.heap().policy.neg_index).ok_or_else(|| out_of_range(obj))?
                } else {
                    self.heap().char_of(obj, s, i).ok_or_else(|| out_of_range(obj))?
                };
                let mut buf = [0u8; 4];
                let one = c.encode_utf8(&mut buf);
                // one byte shares the slot the heap interns so the char needs no allocation
                return if one.len() == 1 { self.heap_mut().intern_str(one) } else { self.heap_mut().alloc(HeapObj::Str(String::from(&*one))) };
        }

        // an index into bytes gives the byte as an int and not a text
        if obj.is_heap() && idx.is_int()
            && let HeapObj::Bytes(b) | HeapObj::ByteArray(b) = self.heap().get(obj) {
                let i = idx.as_int();
                let ui = normalize_index(i, b.len(), self.heap().policy);
                let byte = *b.get(ui).ok_or_else(|| out_of_range(obj))?;
                return Ok(Val::int(byte as i64));
        }

        self.getitem_val(obj, idx)
    }

    fn slice_val(&mut self, obj: Val, start: Val, stop: Val, step: Val) -> Result<Val, VmErr> {
        if !obj.is_heap() {
            // a foreign kind cuts its own values into a value of its own
            return self.heap_mut().foreign_cut(obj, [start, stop, step]).unwrap_or_else(|| Err(E::cannot(Act::Sequence, None)));
        }

        // the item count read with no copy of the source
        let ascii = self.heap_mut().str_is_ascii(obj);
        let len: i64 = match self.heap().get(obj) {
            HeapObj::List(v) => v.borrow().len() as i64,
            HeapObj::Tuple(v) => v.len() as i64,
            HeapObj::Str(s) if ascii => s.len() as i64,
            HeapObj::Str(_) => return self.text_cut(obj, start, stop, step),
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => b.len() as i64,
            // a range slices into another range with its bounds resolved at any width
            HeapObj::Range(..) | HeapObj::BigRange(..) => return super::wide::range_slice(self.heap_mut(), obj, start, stop, step).unwrap_or_else(|| Err(VmErr::Overflow)),
            _ => return Err(E::cannot(Act::Slice, None)),
        };
        // a boxed index lies past either end so it clamps to the inline extreme
        let clip = |v: Val| super::wide::clip_index(self.heap(), v);
        let (s, e, st) = slice_bounds(clip(start), clip(stop), clip(step), len, self.heap().policy.neg_index)?;

        // a step of one copies one contiguous run
        let contiguous = st == 1;
        let (lo, hi) = (s.max(0) as usize, e.max(s).max(0) as usize);
        let picks = if contiguous { hi - lo } else { slice_count(s, e, st) };
        // a long slice is refused past the memory left and counts the items it copies
        if picks > 64 { long_slice(self.heap(), obj, picks)?; }
        // a stepped pick finds each place from the first with no list of places
        let at = |k: usize| (s + k as i64 * st) as usize;
        let pick = |v: &[Val]| -> Vec<Val> {
            if contiguous { return v[lo.min(v.len())..hi.min(v.len())].to_vec(); }
            let mut out = Vec::with_capacity(picks);
            out.extend((0..picks).filter_map(|k| v.get(at(k)).copied()));
            out
        };
        let pick_bytes = |b: &[u8]| -> Vec<u8> {
            if contiguous { return b[lo.min(b.len())..hi.min(b.len())].to_vec(); }
            let mut out = Vec::with_capacity(picks);
            out.extend((0..picks).filter_map(|k| b.get(at(k)).copied()));
            out
        };

        // the picks are copied while the source is borrowed and allocated after
        enum Out { List(Vec<Val>), Tuple(Vec<Val>), Str(String), Bytes(Vec<u8>), ByteArray(Vec<u8>) }
        let out = match self.heap().get(obj) {
            HeapObj::List(v) => Out::List(pick(&v.borrow())),
            HeapObj::Tuple(v) => Out::Tuple(pick(v)),
            HeapObj::Str(text) => Out::Str(String::from_utf8(pick_bytes(text.as_bytes())).unwrap_or_default()),
            HeapObj::Bytes(buf) => Out::Bytes(pick_bytes(buf)),
            HeapObj::ByteArray(buf) => Out::ByteArray(pick_bytes(buf)),
            _ => return Err(E::cannot(Act::Slice, None)),
        };
        match out {
            Out::List(v) => self.heap_mut().alloc(HeapObj::List(Rc::new(RefCell::new(v)))),
            Out::Tuple(v) => self.heap_mut().alloc(HeapObj::Tuple(v)),
            Out::Str(s) => self.heap_mut().alloc(HeapObj::Str(s)),
            Out::Bytes(b) => self.heap_mut().alloc(HeapObj::Bytes(b)),
            Out::ByteArray(b) => self.heap_mut().alloc(HeapObj::ByteArray(b)),
        }
    }

    /* a cut of a text past ascii walked from the place it starts nearest */
    #[cold]
    #[inline(never)]
    fn text_cut(&mut self, obj: Val, start: Val, stop: Val, step: Val) -> Result<Val, VmErr> {
        let n = self.heap_mut().text_len(obj).unwrap_or(0);
        let clip = |v: Val| super::wide::clip_index(self.heap(), v);
        let (s, e, st) = slice_bounds(clip(start), clip(stop), clip(step), n as i64, self.heap().policy.neg_index)?;
        let picks = if st == 1 { e.max(s).abs_diff(s) as usize } else { slice_count(s, e, st) };
        if picks > 64 { self.heap().spend(picks); }
        let heap = self.heap();
        let Some(HeapObj::Str(text)) = heap.try_get(obj) else { return Err(E::cannot(Act::Slice, None)) };
        let first = if picks == 0 { 0 } else { heap.char_start(obj, text, s as usize, Some(n)).unwrap_or(text.len()) };
        let out = if picks == 0 { String::new() } else if st == 1 {
            let to = heap.char_start(obj, text, e as usize, Some(n)).unwrap_or(text.len());
            fits(heap, to - first)?;
            String::from(&text[first..to])
        } else {
            // the picks are walked twice so the copy is refused or sized by its bytes
            let mut bytes = 0;
            let walked = each_pick(text, first, st, picks, |c| bytes += c.len_utf8());
            if walked > 1024 { heap.spend(walked / 16); }
            fits(heap, bytes)?;
            let mut out = String::with_capacity(bytes);
            each_pick(text, first, st, picks, |c| out.push(c));
            out
        };
        self.heap_mut().alloc(HeapObj::Str(out))
    }

    fn getitem_val(&mut self, obj: Val, idx: Val) -> Result<Val, VmErr> {
        if !obj.is_heap() {
            // a foreign kind reads its own items with no item made of the value
            return self.heap_mut().foreign_item(obj, idx).unwrap_or_else(|| Err(E::cannot(Act::Subscript, Some(obj))));
        }
        let bad = |vm: &Self| -> VmErr { if vm.heap().is_wide(idx) { E::TooWide(ErrKind::Index).into() } else { E::bad_index(obj, idx) } };
        match self.heap().get(obj) {
            HeapObj::List(v) => {
                if !idx.is_int() { return Err(bad(self)); }
                let b = v.borrow(); let i = idx.as_int();
                let ui = normalize_index(i, b.len(), self.heap().policy);
                b.get(ui).copied().ok_or_else(|| out_of_range(obj))
            }
            HeapObj::Tuple(v) => {
                if !idx.is_int() { return Err(bad(self)); }
                let i = idx.as_int();
                let ui = normalize_index(i, v.len(), self.heap().policy);
                v.get(ui).copied().ok_or_else(|| out_of_range(obj))
            }
            HeapObj::Dict(p) => {
                let hit = p.borrow().get(&idx, self.heap()).copied();
                match hit {
                    Some(v) => Ok(v),
                    None => { self.require_key(idx)?; Err(self.key_error(idx)) }
                }
            }
            HeapObj::Str(_) | HeapObj::Bytes(_) | HeapObj::ByteArray(_) => Err(bad(self)),
            &HeapObj::BigRange(..) if idx.is_int() || self.heap().is_wide(idx) => super::wide::range_item(self.heap_mut(), obj, idx).unwrap_or_else(|| Err(VmErr::Overflow)),
            HeapObj::BigRange(..) => Err(bad(self)),
            // an item of a range is computed with no list built
            &HeapObj::Range(s, e, st) => {
                let (len, neg) = (range_len(s, e, st), self.heap().policy.neg_index);
                // an inline index into a range shorter than an i64 reads in i64 arithmetic
                if idx.is_int() && let Ok(n) = i64::try_from(len) {
                    let i = idx.as_int();
                    let i = if i < 0 && neg { i + n } else { i };
                    if !(0..n).contains(&i) { return Err(out_of_range(obj)); }
                    return self.heap_mut().int((s + i * st) as i128);
                }
                let wide = self.heap().is_wide(idx);
                if !idx.is_int() && !wide { return Err(bad(self)); }
                let Some(i) = (if wide { self.heap().foreign_i128(idx) } else { Some(idx.as_int() as i128) }) else { return Err(out_of_range(obj)) };
                let i = if i < 0 && neg { i + len } else { i };
                if !(0..len).contains(&i) { return Err(out_of_range(obj)); }
                self.heap_mut().int(s as i128 + i * st as i128)
            }
            _ => Err(E::cannot(Act::Subscript, Some(obj))),
        }
    }

    /* the length of a builtin container which the plugin abi shares */
    fn builtin_len(&mut self, o: Val) -> Result<i128, VmErr> {
        if let Some(n) = self.heap_mut().text_len(o) { return Ok(n as i128); }
        Ok(match self.heap().try_get(o) {
            Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => b.len() as i128,
            Some(HeapObj::List(v)) => v.borrow().len() as i128,
            Some(HeapObj::Tuple(v)) => v.len() as i128,
            Some(HeapObj::Dict(v)) => v.borrow().len() as i128,
            Some(HeapObj::Set(v)) => v.borrow().len() as i128,
            Some(&HeapObj::Range(_, _, 0)) => return Err(E::zero_step(true)),
            // a length past a word overflows as a wide range's does
            Some(&HeapObj::Range(s, e, st)) => Some(crate::value::eq::range_len(s, e, st)).filter(|&n| n <= i64::MAX as i128).ok_or_else(|| VmErr::Overflow)?,
            Some(HeapObj::BigRange(..)) => super::wide::range_len(self.heap_mut(), o).unwrap_or(Ok(0))?,
            // a foreign kind reports its own length
            _ => return self.heap_mut().foreign_len(o).unwrap_or_else(|| Err(E::cannot(Act::Size, Some(o)))),
        })
    }

    /* item presence in a container or a substring in text and a type error otherwise */
    fn contains(&mut self, container: Val, item: Val) -> Result<bool, VmErr> {
        if container.is_heap() {
            match self.heap().get(container) {
                // a scan counts what it may read as native work
                HeapObj::List(v) => { self.heap().spend(v.borrow().len()); return Ok(v.borrow().iter().any(|x| eq_member(*x, item, self.heap()))) }
                HeapObj::Tuple(v) => { self.heap().spend(v.len()); return Ok(v.iter().any(|x| eq_member(*x, item, self.heap()))) }
                // only a miss checks the key since an unhashable one is never inside
                HeapObj::Dict(p) => { let hit = p.borrow().contains_key(&item, self.heap()); if !hit { self.require_key(item)?; } return Ok(hit); }
                HeapObj::Set(s) => { let hit = s.borrow().contains(item, self.heap()); if !hit { self.require_set_probe(item)?; } return Ok(hit); }
                HeapObj::Str(s) => {
                    if let Some(HeapObj::Str(sub)) = self.heap().try_get(item) { self.heap().spend(s.len()); return Ok(s.contains(sub.as_str())); }
                    return Err(E::unsupported(Op::In, item, Some(container)));
                }
                HeapObj::Bytes(b) | HeapObj::ByteArray(b) => {
                    if let Some(sub) = self.heap().bytes_of(item) {
                        self.heap().spend(b.len());
                        return Ok(crate::util::search::find(b, sub).is_some());
                    }
                    let n = match as_i128(item, self.heap()).and_then(|n| i64::try_from(n).ok()) { Some(n) if !item.is_foreign() => n, _ => return Err(E::cannot(Act::Bytes, None)) };
                    if !(0..=255).contains(&n) { return Err(E::ByteRange.into()); }
                    return Ok(b.contains(&(n as u8)));
                }
                HeapObj::BigRange(..) => return super::wide::range_has(self.heap_mut(), container, item).unwrap_or(Ok(false)),
                HeapObj::Range(s, e, st) => {
                    let (s, e, st) = (*s as i128, *e as i128, *st as i128);
                    // membership in a range reads its bounds and step and a whole float matches too
                    let x = match as_i128(item, self.heap()) {
                        Some(i) => Some(i),
                        None if item.is_float() => {
                            let v = item.as_float();
                            if v.is_finite() && v == libm::trunc(v) { Some(v as i128) } else { None }
                        }
                        None => None,
                    };
                    return Ok(match x {
                        Some(x) => {
                            let in_bounds = if st > 0 { x >= s && x < e } else { x <= s && x > e };
                            in_bounds && st != 0 && (x - s) % st == 0
                        }
                        None => false,
                    });
                }
                _ => {}
            }
        }
        Err(E::cannot(Act::Contain, Some(container)))
    }

    // the index as a plain value or none for a heap one like an instance
    #[inline]
    fn plain_index(&self, cont: Val, idx: Val) -> Option<Val> {
        // a bool indexes as its int where bools are ints and a dict keeps it
        let keyed = self.heap().dict_cell(cont).is_some();
        if idx.is_bool() && !keyed && self.heap().policy.bool_is_int { return Some(Val::int(idx.as_bool() as i64)); }
        if !idx.is_heap() || keyed { return Some(idx); }
        None
    }

    /* the error of operator `op` of one operand that `v` does not take */
    #[cold]
    #[inline(never)]
    fn bad_operand(&self, op: Op, v: Val) -> VmErr { E::unsupported(op, v, None) }

    /* the user class an instance or a dict or list was made of or none */
    #[inline]
    fn instance_class(&self, v: Val) -> Option<Val> { crate::vm::sections::class_of(self.heap(), v) }

    /* a builtin iterator named `name` over `frame` */
    fn alloc_iterator(&mut self, frame: IterFrame, name: &'static str) -> Result<Val, VmErr> {
        self.heap_mut().alloc(HeapObj::Iter(alloc::rc::Rc::new(core::cell::RefCell::new(frame)), name))
    }

    /* a live walk over the `part` of each entry of dict `d` */
    fn dict_walk(&self, d: Val, part: DictPart) -> IterFrame {
        let len = self.heap().dict_cell(d).map_or(0, |m| m.borrow().len());
        IterFrame::DictWalk { of: d, idx: 0, left: len, len, part }
    }

    /* a frame over `items` of `o` watching a dict or set for a size change */
    fn watched(&self, o: Val, items: Vec<Val>) -> IterFrame {
        let of = match self.heap().try_get(o) {
            Some(HeapObj::Dict(_) | HeapObj::Set(_)) => o,
            _ => return IterFrame::Seq { items: items.into(), idx: 0 },
        };
        IterFrame::Watched { items: items.into(), idx: 0, of, len: crate::value::watched_len(self.heap(), of) }
    }

    // the interned keyword name a value holds and none for any other value
    fn kw_name(&self, k: Val) -> Option<&str> {
        match self.heap().try_get(k) {
            Some(HeapObj::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    fn add_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        // two inline ints add in place and an overflow falls to the i128 path
        if a.is_int() && b.is_int()
            && let Some(r) = a.as_int().checked_add(b.as_int())
            && (Val::INT_MIN..=Val::INT_MAX).contains(&r) {
            return Ok(Val::int(r));
        }
        if let Some(p) = coerce_floats(a, b, self.heap()) { let (af, bf) = p?; return Ok(Val::float(af + bf)); }
        if let (Some(ai), Some(bi)) = (as_i128(a, self.heap()), as_i128(b, self.heap())) && let Some(r) = ai.checked_add(bi) { return self.heap_mut().int(r); }
        if let Some(r) = self.wide_op(BinOp::Add, a, b)? { return Ok(r); }
        if a.is_heap() && b.is_heap() {
            // a join pays for the items it copies so joins in a loop stay bounded
            let copy_cost = match (self.heap().get(a), self.heap().get(b)) {
                (HeapObj::Str(sa), HeapObj::Str(sb)) => Some((sa.len() + sb.len(), 1)),
                (HeapObj::Bytes(ba) | HeapObj::ByteArray(ba), HeapObj::Bytes(bb) | HeapObj::ByteArray(bb)) => Some((ba.len() + bb.len(), 1)),
                (HeapObj::List(va), HeapObj::List(vb)) => Some((va.borrow().len() + vb.borrow().len(), VAL_BYTES)),
                (HeapObj::Tuple(va), HeapObj::Tuple(vb)) => Some((va.len() + vb.len(), VAL_BYTES)),
                _ => None,
            };
            // a long result is refused past the memory left before it is paid or built
            if let Some((n, unit)) = copy_cost { fits(self.heap(), n.saturating_mul(unit))?; self.charge(n)?; }
            match (self.heap().get(a), self.heap().get(b)) {
                (HeapObj::Str(sa), HeapObj::Str(sb)) => {
                    let mut r = String::with_capacity(sa.len() + sb.len());
                    r.push_str(sa); r.push_str(sb);
                    return self.heap_mut().alloc(HeapObj::Str(r));
                }
                // each result is sized once so no operand is copied twice
                (HeapObj::List(va), HeapObj::List(vb)) => {
                    let lst = [&va.borrow()[..], &vb.borrow()[..]].concat();
                    return self.heap_mut().alloc(HeapObj::List(Rc::new(RefCell::new(lst))));
                }
                (HeapObj::Tuple(va), HeapObj::Tuple(vb)) => {
                    let tup = [&va[..], &vb[..]].concat();
                    return self.heap_mut().alloc(HeapObj::Tuple(tup));
                }
                // the result takes the type of the left side
                (left @ (HeapObj::Bytes(ba) | HeapObj::ByteArray(ba)), HeapObj::Bytes(bb) | HeapObj::ByteArray(bb)) => {
                    let (r, array) = ([&ba[..], &bb[..]].concat(), matches!(left, HeapObj::ByteArray(_)));
                    return self.heap_mut().alloc(if array { HeapObj::ByteArray(r) } else { HeapObj::Bytes(r) });
                }
                _ => {}
            }
        }
        Err(self.unsupported(BinOp::Add, a, b))
    }

    /* the type error of a binary operator no operand type supports */
    #[cold]
    fn unsupported(&self, op: BinOp, a: Val, b: Val) -> VmErr { E::unsupported(Op::Bin(op), a, Some(b)) }

    /* `op` through the kind of a FOREIGN operand or none when nothing takes it */
    fn wide_op(&mut self, op: BinOp, a: Val, b: Val) -> Result<Option<Val>, VmErr> {
        if !a.is_foreign() && !b.is_foreign() { return Ok(None); }
        self.heap_mut().foreign_binop(op, a, b)
    }

    fn lt_vals(&self, a: Val, b: Val) -> Result<bool, VmErr> {
        self.lt_vals_d(a, b, 0)
    }

    /* depth tracked `<` raising a recursion error past the cap instead of overflowing the stack */
    fn lt_vals_d(&self, a: Val, b: Val, depth: usize) -> Result<bool, VmErr> {
        if depth > CMP_DEPTH_MAX { return Err(cold_depth()); }
        // a bool orders as its int only where the policy counts bools as ints
        let int = |v: Val| if v.is_bool() && self.heap().policy.bool_is_int { Val::int(v.as_bool() as i64) } else { v };
        let (a, b) = (int(a), int(b));
        if a.is_int() && b.is_int() { return Ok(a.as_int() < b.as_int()); }
        // two bools that are no ints still order with false first
        if a.is_bool() && b.is_bool() { return Ok(!a.as_bool() & b.as_bool()); }
        if (a.is_foreign() || b.is_foreign()) && let Some(o) = self.heap().foreign_cmp(a, b) { return Ok(o == Ordering::Less); }
        // a nan orders before nothing even beside an int too wide for a float
        match coerce_floats(a, b, self.heap()) { Some(Ok((af, bf))) => return Ok(af < bf), Some(Err(_)) if [a, b].iter().any(|v| v.is_float() && v.as_float().is_nan()) => return Ok(false), _ => {} }
        if a.is_heap() && b.is_heap() {
            match (self.heap().get(a), self.heap().get(b)) {
                // a long pair counts the bytes the order may read as native work
                (HeapObj::Str(x), HeapObj::Str(y)) => { long_pair(self.heap(), x.len(), y.len()); return Ok(x < y) }
                (HeapObj::Bytes(x) | HeapObj::ByteArray(x), HeapObj::Bytes(y) | HeapObj::ByteArray(y)) => { long_pair(self.heap(), x.len(), y.len()); return Ok(x < y) }
                // sequences compare in place since an order changes nothing it reads
                (HeapObj::List(x), HeapObj::List(y)) => return self.seq_lt_d(&x.borrow(), &y.borrow(), depth + 1),
                (HeapObj::Tuple(x), HeapObj::Tuple(y)) => return self.seq_lt_d(x, y, depth + 1),
                _ => {}
            }
        }
        Err(E::unsupported(Op::Cmp(Cmp::Lt), a, Some(b)))
    }

    /* `<` of two sequences decided by the first pair that differs or else by length */
    fn seq_lt_d(&self, xs: &[Val], ys: &[Val], depth: usize) -> Result<bool, VmErr> { self.seq_order_d(xs, ys, depth, false, |x, y, d| self.lt_vals_d(x, y, d)) }

    /* sequences ordered by their first unequal pair or by length with `equal` taking a tie */
    fn seq_order_d(&self, xs: &[Val], ys: &[Val], depth: usize, equal: bool, pair: impl FnOnce(Val, Val, usize) -> Result<bool, VmErr>) -> Result<bool, VmErr> {
        if depth > CMP_DEPTH_MAX { return Err(cold_depth()); }
        // the equal pairs read before the first that differs count as native work
        let same = xs.iter().zip(ys).take_while(|&(&x, &y)| eq_member(x, y, self.heap())).count();
        if same > 64 { self.heap().spend(same); }
        match (xs.get(same), ys.get(same)) {
            (Some(&x), Some(&y)) => pair(x, y, depth + 1),
            _ => Ok(if equal { xs.len() <= ys.len() } else { xs.len() < ys.len() }),
        }
    }

    /* integer division or remainder rounded as the policy says with floats from either side */
    fn floor_op(&mut self, op: BinOp, a: Val, b: Val) -> Result<Val, VmErr> {
        let modulo = op == BinOp::Mod;
        let policy = self.heap().policy;
        if a.is_float() || b.is_float() {
            let (Some(af), Some(bf)) = (num_f64(a, self.heap()), num_f64(b, self.heap())) else { return Err(self.unsupported(op, a, b)); };
            let (af, bf) = (af?, bf?);
            let Some((q, r)) = policy.float_divmod(af, bf) else { return Err(VmErr::ZeroDiv) };
            return Ok(Val::float(if modulo { r } else { q }));
        }
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) && y != 0 && let Some((q, r)) = policy.int_divmod(x, y) {
            return self.heap_mut().int(if modulo { r } else { q });
        }
        self.heap_mut().foreign_binop(op, a, b)?.ok_or_else(|| self.unsupported(op, a, b))
    }

    /* the power `pow` and `**` give exact for ints and a float otherwise */
    fn pow_vals(&mut self, a: Val, b: Val) -> Result<Val, VmErr> {
        if let (Some(ai), Some(exp)) = (self.as_i128(a), self.as_i128(b).and_then(|e| i64::try_from(e).ok())) {
            if exp >= 0 {
                if let Some(r) = checked_pow(ai, exp as u64) { return self.heap_mut().int(r); }
                return self.heap_mut().foreign_binop(BinOp::Pow, a, b)?.ok_or_else(|| self.heap().past_range());
            }
            if ai == 0 { return Err(E::ZeroPow.into()); }
            if !self.heap().policy.negative_powers() { return Err(E::NegativePower.into()); }
            // bases one and minus one stay exact and any other underflows toward zero
            let r = match ai {
                1 => 1.0,
                -1 => if exp % 2 == 0 { 1.0 } else { -1.0 },
                _ => libm::pow(ai as f64, exp as f64),
            };
            return Ok(Val::float(r));
        }
        if let Some(r) = self.wide_op(BinOp::Pow, a, b)? { return Ok(r); }
        let (Some(fa), Some(fb)) = (num_f64(a, self.heap()), num_f64(b, self.heap())) else { return Err(self.unsupported(BinOp::Pow, a, b)); };
        let (fa, fb) = (fa?, fb?);
        if fa == 0.0 && fb < 0.0 { return Err(E::ZeroPow.into()); }
        // a negative base under a fractional power has only a complex result
        if crate::value::math::complex_power(fa, fb) { return Err(E::cannot(Act::Complex, None)); }
        fpowf(fa, fb).map(Val::float).ok_or_else(|| E::FloatRange.into())
    }

}

// type names and truth read off the heap alone
impl HeapPool {
    pub fn truthy(&self, v: Val) -> bool {
        // the mask is read only where a value could read as false
        if v.is_none() { return self.falsy.nil_truth(); }
        if v.is_false() { return self.falsy.bool_truth(false); }
        if v.is_true() { return true; }
        if v.is_int() { return v.as_int() != 0 || self.falsy.int_truth(0); }
        if v.is_float() { let x = v.as_float(); return x != 0.0 && !x.is_nan() || self.falsy.float_truth(x); }
        if v.is_foreign() { return self.foreign_truthy(v).unwrap_or(true); }
        let full = match self.get(v) {
            HeapObj::Str(s) => return self.falsy.text_truth(s.is_empty()),
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => !b.is_empty(),
            HeapObj::List(l) => !l.borrow().is_empty(),
            HeapObj::Tuple(t) => !t.is_empty(),
            HeapObj::Dict(d) => !d.borrow().is_empty(),
            HeapObj::Set(s) => !s.borrow().is_empty(),
            HeapObj::Range(s,e,st) => if *st > 0 { s < e } else { s > e },
            HeapObj::BigRange(..) => super::wide::range_empty(self, v) == Some(false),
            HeapObj::Func { .. }
            | HeapObj::Native(_) | HeapObj::Bound { .. }
            | HeapObj::Object { .. } | HeapObj::Coroutine(..)
            | HeapObj::Module(..)
            | HeapObj::Iter(..) | HeapObj::Cell(_) => true,
        };
        full || self.falsy.container_truth(true)
    }

    /* the rules of the runtime these values belong to */
    #[inline(always)]
    pub fn policy(&self) -> &'static crate::lang::Policy { self.policy }

    /* the values the runtime reads as false as the mask its tests read */
    #[inline(always)]
    pub fn falsy(&self) -> crate::lang::Falsy { self.falsy }

    /* the truth of an inline value or a text or none for any other value */
    #[inline(always)]
    pub fn bare_truth(&self, v: Val) -> Option<bool> {
        let f = self.falsy;
        if v.is_bool() { return Some(f.bool_truth(v.as_bool())); }
        if v.is_int() { return Some(f.int_truth(v.as_int())); }
        if v.is_none() { return Some(f.nil_truth()); }
        match self.try_get(v) { Some(HeapObj::Str(s)) => Some(f.text_truth(s.is_empty())), _ => None }
    }

    #[inline]
    pub fn type_name(&self, v: Val) -> &str { self.type_name_by(v, &|_| self.names.coroutine) }

    /* what the runtime calls the engine's kinds errors and conventions */
    #[inline(always)]
    pub fn names(&self) -> &'static crate::lang::Names { self.names }

    // `coro` names a generator or a coroutine by the body it runs
    #[inline(always)]
    pub fn type_name_by<'h>(&'h self, v: Val, coro: &impl Fn(&super::Coro) -> &'h str) -> &'h str {
        use crate::lang::KindId as K;
        let (names, k) = (self.names, &self.names.kinds);
        let scalar = |v: Val| if v.is_bool() { Some(k[K::Bool as usize]) }
            else if v.is_int() { Some(k[K::Int as usize]) }
            else if v.is_float() { Some(k[K::Float as usize]) }
            else if v.is_none() { Some(k[K::None as usize]) }
            else if v.is_foreign() { Some(self.foreign_kind(v).map_or(k[K::Foreign as usize], |f| f.type_name)) }
            else { None };
        if let Some(n) = scalar(v) { n }
        else { match self.get(v) {
            HeapObj::Str(_) => k[K::Str as usize],
            HeapObj::Bytes(_) => k[K::Bytes as usize],
            HeapObj::ByteArray(_) => k[K::ByteArray as usize],
            HeapObj::List(_) => k[K::List as usize],
            HeapObj::Dict(_) => k[K::Dict as usize],
            HeapObj::Set(_) => k[K::Set as usize],
            HeapObj::Tuple(_) => k[K::Tuple as usize],
            HeapObj::Func { .. } => k[K::Func as usize],
            HeapObj::Range(..) | HeapObj::BigRange(..) => k[K::Range as usize],
            // a method off a type is a descriptor until it binds
            &HeapObj::Native(id) => if self.natives.owner(id).is_some() { names.descriptor } else { names.native },
            &HeapObj::Bound { func, .. } => if func.as_native_ref().is_some() { names.native } else { names.method },
            // classes and types read as the type of types and instances as their maker
            &HeapObj::Object { meta, .. } => if meta.is_heap() { self.class_name(meta).unwrap_or(k[K::Instance as usize]) } else { k[K::Type as usize] },
            HeapObj::Coroutine(c) => coro(c),
            HeapObj::Module(..) => k[K::Module as usize],
            HeapObj::Iter(_, name) => name,
            HeapObj::Cell(_) => k[K::Cell as usize],
        }}
    }
}

/* whether the characters of `s` fit the memory left at a slot past ascii */
#[cold]
#[inline(never)]
fn text_fits(heap: &HeapPool, s: &str) -> bool {
    let (n, each) = if s.is_ascii() { (s.len(), VAL_BYTES) } else { (s.chars().count(), VAL_BYTES + core::mem::size_of::<HeapObj>()) };
    n.saturating_mul(each) <= heap.room()
}

/* refuses `n` copied values past the memory or budget left with the matching error */
#[cold]
#[inline(never)]
fn long_fits(heap: &HeapPool, n: usize) -> Result<(), VmErr> { if n.saturating_mul(VAL_BYTES) > heap.room() { Err(cold_heap()) } else { heap.afford(n) } }

/* refuses a slice of `n` items past the memory left and counts its copy */
#[cold]
#[inline(never)]
fn long_slice(heap: &HeapPool, obj: Val, n: usize) -> Result<(), VmErr> {
    let values = matches!(heap.get(obj), HeapObj::List(_) | HeapObj::Tuple(_));
    fits(heap, n.saturating_mul(if values { VAL_BYTES } else { 1 }))?;
    heap.spend(n);
    Ok(())
}

/* refuses a result of `bytes` past the memory left where a small one goes unchecked */
#[inline]
fn fits(heap: &HeapPool, bytes: usize) -> Result<(), VmErr> { if bytes > 1 << 16 { heap.reserve(bytes) } else { Ok(()) } }

/* how many items a slice of nonzero step `st` from `s` to `e` picks */
#[inline(always)]
fn slice_count(s: i64, e: i64, st: i64) -> usize {
    let (span, step) = if st > 0 { (e - s, st) } else { (s - e, -st) };
    if span <= 0 { 0 } else { ((span - 1) / step + 1) as usize }
}

/* counts the bytes an order of two long runs may read */
#[inline(always)]
pub(crate) fn long_pair(heap: &HeapPool, x: usize, y: usize) { let n = x.min(y); if n > 64 { heap.spend(n); } }

/* the items of a tuple to check or the refusal of an unhashable kind */
#[inline(always)]
fn hashable(heap: &HeapPool, v: Val, depth: usize) -> Option<Result<&[Val], VmErr>> {
    use crate::lang::KindId as K;
    if !v.is_heap() || depth > EQ_DEPTH_MAX { return None; }
    Some(Err(unhashable(v, match heap.get(v) {
        // a tuple hashes through its items as deep as the hash looks
        HeapObj::Tuple(items) => return Some(Ok(items)),
        HeapObj::List(_) => K::List,
        HeapObj::ByteArray(_) => K::ByteArray,
        HeapObj::Dict(_) => K::Dict,
        HeapObj::Set(_) => K::Set,
        _ => return None,
    })))
}

/* the items of tuple `v` checked where a long walk keeps each hashable tuple */
fn hashable_items<M: Kept<(u64, usize), ()>>(heap: &HeapPool, v: Val, items: &[Val], depth: usize, walk: &mut Walk<M>) -> Result<(), VmErr> {
    if walk.kept.get(&(v.raw(), depth)).is_some() || !walk.read(items.len()) { return Ok(()); }
    for &item in items {
        if let Some(t) = hashable(heap, item, depth + 1) { hashable_items(heap, item, t?, depth + 1, walk)?; }
    }
    walk.kept.put((v.raw(), depth), ());
    Ok(())
}

/* the type error of `v` holding an unhashable value of kind `inner` */
#[cold]
#[inline(never)]
fn unhashable(v: Val, inner: KindId) -> VmErr { E::cannot(Act::Hash { at: Use::Plain, inner }, Some(v)) }

/* the index error of `recv` read or written past its end */
#[cold]
#[inline(never)]
fn out_of_range(recv: Val) -> VmErr { E::out_of_range(recv, false) }

/* character `i` of a short text walked from the end it counts from */
#[inline]
fn short_char(s: &str, i: i64, neg: bool) -> Option<char> {
    match usize::try_from(i) {
        Ok(i) => s.chars().nth(i),
        Err(_) if neg => s.chars().nth_back(usize::try_from(i.unsigned_abs()).ok()? - 1),
        Err(_) => None,
    }
}

/* index `i` into `len` items counting a negative one from the end by the policy */
#[inline(always)]
pub(crate) fn normalize_index(i: i64, len: usize, policy: &crate::lang::Policy) -> usize {
    if i >= 0 { return i as usize; }
    if policy.neg_index { (len as i64 + i) as usize } else { usize::MAX }
}

/* slice bounds clamped to `len` items counting a negative bound from the end with `neg` */
pub(crate) fn slice_bounds(start: Val, stop: Val, step: Val, len: i64, neg: bool) -> Result<(i64, i64, i64), VmErr> {
    let st = if step.is_none() { 1 } else if step.is_int() { step.as_int() } else {
        return Err(E::bad_slice(step, true));
    };
    if st == 0 { return Err(E::zero_step(false)); }
    // a bound neither an int nor nil is refused as a range refuses it
    for v in [start, stop] { if !v.is_none() && !v.is_int() { return Err(E::bad_slice(v, false)); } }
    let clamp = |v: Val, def: i64| -> i64 {
        if v.is_none() { def }
        else { let i = v.as_int(); if i < 0 { if neg { (len + i).max(0) } else { 0 } } else { i.min(len) } }
    };
    // a negative step clamps bounds between minus one and the last index
    let clamp_neg = |v: Val, def: i64| -> i64 {
        if v.is_none() { def }
        else { let i = v.as_int(); (if i < 0 && neg { len + i } else { i }).clamp(-1, len - 1) }
    };
    Ok(if st > 0 { (clamp(start, 0), clamp(stop, len), st) } else { (clamp_neg(start, len - 1), clamp_neg(stop, -1), st) })
}

/* the bytes a stepped cut walks from byte `first` handing `f` each character it picks */
fn each_pick(text: &str, first: usize, st: i64, picks: usize, mut f: impl FnMut(char)) -> usize {
    let (step, mut last) = (usize::try_from(st.unsigned_abs()).unwrap_or(usize::MAX), first);
    if st > 0 {
        for (k, c) in text[first..].char_indices().step_by(step).take(picks) { f(c); last = first + k; }
    } else {
        let end = first + text[first..].chars().next().map_or(0, char::len_utf8);
        for (k, c) in text[..end].char_indices().rev().step_by(step).take(picks) { f(c); last = k; }
    }
    last.abs_diff(first)
}

/* the cap on `<` descent so self referential sequences raise a recursion error */
pub(crate) const CMP_DEPTH_MAX: usize = 100;

/* a numeric pair as two floats and none when neither operand is a float */
pub(crate) fn coerce_floats(a: Val, b: Val, heap: &HeapPool) -> Option<Result<(f64, f64), VmErr>> {
    if !a.is_float() && !b.is_float() { return None; }
    let (x, y) = (num_f64(a, heap)?, num_f64(b, heap)?);
    Some(x.and_then(|x| Ok((x, y?))))
}

/* `base ** e` by squaring or none once a step leaves i128 */
pub(crate) fn checked_pow(mut base: i128, mut e: u64) -> Option<i128> {
    let mut acc: i128 = 1;
    while e > 0 {
        if e & 1 == 1 { acc = acc.checked_mul(base)?; }
        e >>= 1;
        if e > 0 { base = base.checked_mul(base)?; }
    }
    Some(acc)
}
