use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use edge::lang::{Acc, Args, Cx, ErrKind, Flow, Fold, Item, KindId, Ret, STEPS, Sig, Step};
use edge::value::{HeapObj, Val, VmErr, cold_heap};

use crate::fail::{Error, Fail, fail};
use crate::iter::items::{PIECE, SHORT, gather};
use crate::lang::{Lang, Machine};
use crate::num::{bounds, ssize, word};
use crate::{Kernel, Site};

pub use edge::util::search;

/* a text or a buffer as a run of units the kernels read alike */
pub trait Hay {
    // a character of a text or a byte of a buffer
    type Unit: Copy + PartialEq;
    // what a kernel builds a new run in
    type Owned;

    fn bytes(&self) -> &[u8];

    /* the run between two unit boundaries given in bytes */
    fn cut(&self, r: Range<usize>) -> &Self;

    /* each unit beside the byte it starts at */
    fn units(&self) -> impl DoubleEndedIterator<Item = (usize, Self::Unit)>;

    /* each byte of a run whose units are all one byte read as a unit */
    fn bytes_as_units(&self) -> impl DoubleEndedIterator<Item = (usize, Self::Unit)> { self.bytes().iter().enumerate().map(|(i, &b)| (i, Self::from_ascii(b))) }

    /* whether unit `u` is one of the run's units */
    fn holds(&self, u: Self::Unit) -> bool;

    /* the bytes a unit takes */
    fn size(u: Self::Unit) -> usize;

    /* whether a unit is white space in the run's own class under runtime `L` */
    fn blank<L: Lang>(u: Self::Unit) -> bool;

    /* a unit as the character a line break set or a sign test reads */
    fn char(u: Self::Unit) -> char;

    /* the unit an ascii byte is */
    fn from_ascii(b: u8) -> Self::Unit;

    /* how many units the run holds where `ascii` means a unit is a byte */
    fn count(&self, ascii: bool) -> usize;

    /* the byte unit `i` starts at or the end past the last unit */
    fn at(&self, ascii: bool, i: usize) -> usize;

    /* how many units lie before byte `b` */
    fn before(&self, ascii: bool, b: usize) -> usize;

    fn owned(capacity: usize) -> Self::Owned;

    /* a new run of the same units */
    fn copied(&self) -> Self::Owned;

    fn push(out: &mut Self::Owned, piece: &Self);

    fn push_unit(out: &mut Self::Owned, u: Self::Unit, n: usize);

    /* the run with every byte `from` written as byte `to` both of one byte units */
    fn swapped(&self, from: u8, to: u8) -> Self::Owned;
}

impl Hay for str {
    type Unit = char;
    type Owned = String;

    #[inline(always)]
    fn bytes(&self) -> &[u8] { self.as_bytes() }

    #[inline(always)]
    fn cut(&self, r: Range<usize>) -> &str { self.get(r).unwrap_or_default() }

    #[inline(always)]
    fn units(&self) -> impl DoubleEndedIterator<Item = (usize, char)> { self.char_indices() }

    #[inline(always)]
    fn holds(&self, u: char) -> bool { if u.is_ascii() { self.as_bytes().contains(&(u as u8)) } else { self.chars().any(|c| c == u) } }

    #[inline(always)]
    fn size(u: char) -> usize { u.len_utf8() }

    #[inline(always)]
    fn blank<L: Lang>(u: char) -> bool { crate::lang::space::<L>(u) }

    #[inline(always)]
    fn char(u: char) -> char { u }

    #[inline(always)]
    fn from_ascii(b: u8) -> char { char::from(b) }

    #[inline(always)]
    fn count(&self, ascii: bool) -> usize { if ascii { self.len() } else { self.chars().count() } }

    #[inline(always)]
    fn at(&self, ascii: bool, i: usize) -> usize {
        if ascii { i.min(self.len()) } else { self.char_indices().nth(i).map_or(self.len(), |(b, _)| b) }
    }

    #[inline(always)]
    fn before(&self, ascii: bool, b: usize) -> usize { if ascii { b } else { self.get(..b).map_or(0, |s| s.chars().count()) } }

    #[inline(always)]
    fn owned(capacity: usize) -> String { String::with_capacity(capacity) }

    #[inline(always)]
    fn copied(&self) -> String { String::from(self) }

    #[inline(always)]
    fn push(out: &mut String, piece: &str) { out.push_str(piece); }

    #[inline(always)]
    fn push_unit(out: &mut String, u: char, n: usize) { out.extend(core::iter::repeat_n(u, n)); }

    #[inline(always)]
    fn swapped(&self, from: u8, to: u8) -> String {
        let mut to_text = [0u8; 4];
        self.replace(char::from(from), char::from(to).encode_utf8(&mut to_text))
    }
}

impl Hay for [u8] {
    type Unit = u8;
    type Owned = Vec<u8>;

    #[inline(always)]
    fn bytes(&self) -> &[u8] { self }

    #[inline(always)]
    fn cut(&self, r: Range<usize>) -> &[u8] { self.get(r).unwrap_or_default() }

    #[inline(always)]
    fn units(&self) -> impl DoubleEndedIterator<Item = (usize, u8)> { self.iter().copied().enumerate() }

    #[inline(always)]
    fn holds(&self, u: u8) -> bool { self.contains(&u) }

    #[inline(always)]
    fn size(_: u8) -> usize { 1 }

    #[inline(always)]
    fn blank<L: Lang>(u: u8) -> bool { crate::bytes::is_space(u) }

    #[inline(always)]
    fn char(u: u8) -> char { char::from(u) }

    #[inline(always)]
    fn from_ascii(b: u8) -> u8 { b }

    #[inline(always)]
    fn count(&self, _: bool) -> usize { self.len() }

    #[inline(always)]
    fn at(&self, _: bool, i: usize) -> usize { i.min(self.len()) }

    #[inline(always)]
    fn before(&self, _: bool, b: usize) -> usize { b }

    #[inline(always)]
    fn owned(capacity: usize) -> Vec<u8> { Vec::with_capacity(capacity) }

    #[inline(always)]
    fn copied(&self) -> Vec<u8> { self.to_vec() }

    #[inline(always)]
    fn push(out: &mut Vec<u8>, piece: &[u8]) { out.extend_from_slice(piece); }

    #[inline(always)]
    fn push_unit(out: &mut Vec<u8>, u: u8, n: usize) { out.extend(core::iter::repeat_n(u, n)); }

    #[inline(always)]
    fn swapped(&self, from: u8, to: u8) -> Vec<u8> { self.iter().map(|&b| if b == from { to } else { b }).collect() }
}

// what a kernel of family `F` builds a new run in
type Owned<F> = <<F as Family>::Hay as Hay>::Owned;

/* the part an argument plays which a family words its failure for */
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    // what a replace takes out and puts in
    Old,
    New,
    // the separator a split cuts at
    Sep,
    // the separator a partition cuts at
    Part,
    // a prefix or suffix to drop
    Affix,
    // the units a strip drops
    Chars,
    // one affix of a tuple a test tries
    Item,
}

/* the receivers of these kernels with the arguments they take and values they make */
pub trait Family: 'static {
    type Hay: Hay + ?Sized;
    // the kind a failure asks an argument to be
    const KIND: KindId;
    // whether a result equal to the receiver comes back as the receiver itself
    const SHARES: bool;

    /* whether each unit of receiver `v` is one byte */
    fn ascii(cx: &mut Cx<'_, '_>, v: Val) -> bool;

    /* the receiver charged as native work for its length */
    fn recv<'a>(cx: &'a Cx<'_, '_>, v: Val) -> Result<&'a Self::Hay, VmErr>;

    /* argument `v` in the role it plays charged for its length */
    fn arg<'a>(cx: &'a Cx<'_, '_>, v: Val, role: Role) -> Result<&'a Self::Hay, Error>;

    /* what a search looks for where `one` holds a byte an int names */
    fn needle<'a>(cx: &'a Cx<'_, '_>, v: Val, one: &'a mut [u8; 1]) -> Result<&'a [u8], Error>;

    /* the unit a pad fills with where none is the space */
    fn fill(cx: &Cx<'_, '_>, v: Option<Val>) -> Result<<Self::Hay as Hay>::Unit, Error>;

    /* `v` as a run of this family a join takes or none for another value */
    fn part<'a>(cx: &'a Cx<'_, '_>, v: Val) -> Option<&'a Self::Hay>;

    /* a new value holding `out` */
    fn give(cx: &mut Cx<'_, '_>, out: <Self::Hay as Hay>::Owned) -> Result<Val, VmErr>;

    /* `v` as a part of a result which a value that is not shared copies */
    fn keep(cx: &mut Cx<'_, '_>, v: Val) -> Result<Val, VmErr>;

    /* the failure of an index search that found nothing */
    fn missing() -> Fail;
}

/* a count argument where a negative one means no limit */
#[inline(always)]
pub(crate) fn limit(cx: &Cx<'_, '_>, v: Option<Val>) -> Result<usize, Error> {
    let n = match v { Some(v) => ssize(cx, v)?, None => -1 };
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

/* a width a result grows to reserved before the result exists */
#[inline(always)]
fn width(cx: &Cx<'_, '_>, v: Val) -> Result<usize, Error> {
    let w = if v.is_int() { v.as_int().max(0) } else { ssize(cx, v)?.max(0) };
    let w = usize::try_from(w).map_err(|_| cold_heap())?;
    cx.reserve(w)?;
    Ok(w)
}

/* the window two bounds give over the receiver in bytes and none past its end */
#[inline(always)]
fn window<H: Hay + ?Sized>(cx: &Cx<'_, '_>, h: &H, ascii: bool, start: Option<Val>, stop: Option<Val>) -> Result<Option<(usize, usize)>, Error> {
    // no bounds is the whole run with no count of its units
    if start.is_none() && stop.is_none() { return Ok(Some((0, h.bytes().len()))); }
    Ok(bounds(cx, start, stop, h.count(ascii))?.map(|(s, e)| (h.at(ascii, s), h.at(ascii, e))))
}

/* whether a window starts with an affix or one of a tuple of them */
pub const fn starts<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 3), affix::<S, F, false>) }

/* whether a window ends with an affix or one of a tuple of them */
pub const fn ends<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 3), affix::<S, F, true>) }

/* where the first match in a window starts or minus one */
pub const fn find<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 3), seek::<S, F, false, false>) }

/* where the last match in a window starts or minus one */
pub const fn rfind<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 3), seek::<S, F, true, false>) }

/* where the first match in a window starts or a failure */
pub const fn index<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 3), seek::<S, F, false, true>) }

/* where the last match in a window starts or a failure */
pub const fn rindex<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 3), seek::<S, F, true, true>) }

/* how many matches a window holds without overlap */
pub const fn count<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 3), count_run::<S, F>) }

/* the receiver with its first matches up to a count replaced */
pub const fn replace<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(2, 3), replace_run::<S, F>) }

/* the pieces between separators or white space runs cut from the start */
pub const fn split<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(0, 2), split_run::<S, F, false>) }

/* the pieces between separators or white space runs cut from the end */
pub const fn rsplit<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(0, 2), split_run::<S, F, true>) }

/* the lines at every break `B` names where a crlf counts as one */
pub const fn lines<S: Site, F: Family, B: Breaks>() -> Kernel { Kernel::new(Sig::range(0, 1), lines_run::<F, B>) }

/* white space or the argument's units dropped from both ends */
pub const fn trim<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(0, 1), trim_run::<S, F, true, true>) }

/* white space or the argument's units dropped from the start */
pub const fn trim_start<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(0, 1), trim_run::<S, F, true, false>) }

/* white space or the argument's units dropped from the end */
pub const fn trim_end<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(0, 1), trim_run::<S, F, false, true>) }

/* the items of an iterable joined by the receiver */
pub const fn join<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::exact(1), join_run::<S, F>).flags(STEPS) }

/* the parts around the first separator */
pub const fn partition<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::exact(1), halves::<S, F, false>) }

/* the parts around the last separator */
pub const fn rpartition<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::exact(1), halves::<S, F, true>) }

/* the receiver without a prefix when it starts with one */
pub const fn drop_prefix<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::exact(1), unaffixed::<S, F, false>) }

/* the receiver without a suffix when it ends with one */
pub const fn drop_suffix<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::exact(1), unaffixed::<S, F, true>) }

/* the receiver centred in a width with an odd pad split as the runtime chooses */
pub const fn center<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 2), justify::<S, F, b'^'>) }

/* the receiver padded at its end to a width */
pub const fn pad_end<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 2), justify::<S, F, b'<'>) }

/* the receiver padded at its start to a width */
pub const fn pad_start<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(1, 2), justify::<S, F, b'>'>) }

/* zeros after any leading sign up to a width */
pub const fn zeros<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::exact(1), zeros_run::<S, F>) }

/* every tab padded to the next stop where a line break starts the column again */
pub const fn tabs<S: Site, F: Family>() -> Kernel { Kernel::new(Sig::range(0, 1), tabs_run::<S, F>) }

/* which characters end a line */
pub trait Breaks: 'static { const BREAKS: &'static [char]; }

/* a newline a carriage return or both */
pub struct Newline;

/* the newline breaks and the other mandatory breaks unicode names */
pub struct Unicode;

impl Breaks for Newline { const BREAKS: &'static [char] = &['\n', '\r']; }

impl Breaks for Unicode { const BREAKS: &'static [char] = &['\n', '\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}']; }

fn affix<S: Site, F: Family, const END: bool>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = F::ascii(cx, a.recv);
    let hit = {
        let cx = &*cx;
        (|| -> Result<bool, Error> {
            let h = F::recv(cx, a.recv)?;
            let span = window(cx, h, ascii, a.get(1), a.get(2))?;
            let hay = span.map(|(s, e)| h.bytes().get(s..e).unwrap_or_default());
            let hit = |p: &[u8]| hay.is_some_and(|w| if END { w.ends_with(p) } else { w.starts_with(p) });
            // a tuple's affixes are checked as each is tried up to the first hit
            if let Some(HeapObj::Tuple(items)) = cx.heap().try_get(a.pos[0]) {
                for &x in items {
                    if hit(F::arg(cx, x, Role::Item)?.bytes()) { return Ok(true); }
                }
                return Ok(false);
            }
            match F::arg(cx, a.pos[0], Role::Item) {
                Ok(p) => Ok(hit(p.bytes())),
                Err(Error::Fail(_)) => Err(Error::Fail(Fail::AffixType { v: a.pos[0], want: F::KIND })),
                Err(e) => Err(e),
            }
        })()
    };
    Ok(Val::bool(hit.map_err(|e| e.raise::<S>(cx))?).into())
}

fn seek<S: Site, F: Family, const LAST: bool, const RAISE: bool>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = F::ascii(cx, a.recv);
    let found = {
        let cx = &*cx;
        (|| -> Result<Option<usize>, Error> {
            let h = F::recv(cx, a.recv)?;
            let span = window(cx, h, ascii, a.get(1), a.get(2))?;
            let mut one = [0];
            let sub = F::needle(cx, a.pos[0], &mut one)?;
            let at = span.and_then(|(s, e)| {
                let hay = h.bytes().get(s..e).unwrap_or_default();
                if LAST { search::rfind(hay, sub) } else { search::find(hay, sub) }.map(|i| s + i)
            });
            Ok(at.map(|b| h.before(ascii, b)))
        })()
    };
    match found.map_err(|e| e.raise::<S>(cx))? {
        Some(i) => Ok(Val::int(i as i64).into()),
        None if RAISE => Err(fail::<S>(cx, F::missing())),
        None => Ok(Val::int(-1).into()),
    }
}

fn count_run<S: Site, F: Family>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = F::ascii(cx, a.recv);
    let n = {
        let cx = &*cx;
        (|| -> Result<usize, Error> {
            let h = F::recv(cx, a.recv)?;
            let span = window(cx, h, ascii, a.get(1), a.get(2))?;
            let mut one = [0];
            let sub = F::needle(cx, a.pos[0], &mut one)?;
            let Some((s, e)) = span else { return Ok(0) };
            Ok(match sub {
                // an empty needle matches before each unit and once at the end
                [] => h.cut(s..e).count(ascii) + 1,
                &[c] => search::count_byte(h.bytes().get(s..e).unwrap_or_default(), c),
                _ => search::Matches::new(h.bytes().get(s..e).unwrap_or_default(), sub).count(),
            })
        })()
    };
    Ok(Val::int(n.map_err(|e| e.raise::<S>(cx))? as i64).into())
}

/* `h` with `new` at each of `hits` of `len` bytes and none without hits */
#[inline(always)]
fn swapped_at<H: Hay + ?Sized>(h: &H, hits: impl Iterator<Item = usize>, len: usize, new: &H) -> Option<H::Owned> {
    let mut hits = hits.peekable();
    hits.peek()?;
    let end = h.bytes().len();
    let (mut out, mut at) = (H::owned(end), 0);
    for j in hits {
        H::push(&mut out, h.cut(at..j));
        H::push(&mut out, new);
        at = j + len;
    }
    H::push(&mut out, h.cut(at..end));
    Some(out)
}

/* `old` swapped for `new` up to a count refusing a result past the memory left */
fn replace_run<S: Site, F: Family>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let out = {
        let cx = &*cx;
        (|| -> Result<Option<Owned<F>>, Error> {
            let (old, new) = (F::arg(cx, a.pos[0], Role::Old)?, F::arg(cx, a.pos[1], Role::New)?);
            let max = limit(cx, a.get(2))?;
            let h = F::recv(cx, a.recv)?;
            let (ob, nb, len) = (old.bytes(), new.bytes(), h.bytes().len());
            // one byte for another with no count maps the run in one pass
            if let (&[from], &[to], usize::MAX) = (ob, nb, max) && from.is_ascii() && to.is_ascii() {
                if !search::contains(h.bytes(), from) { return Ok(if F::SHARES { None } else { Some(h.copied()) }); }
                return Ok(Some(h.swapped(from, to)));
            }
            // an empty `old` matches before each unit and once at the end
            let boundaries = || h.units().map(|(b, _)| b).chain(core::iter::once(len)).take(max);
            let matches = || search::Matches::new(h.bytes(), ob).take(max);
            // a result past the memory left fails before it is built
            if nb.len() > ob.len() {
                let n = if ob.is_empty() { boundaries().count() } else { matches().count() };
                cx.reserve(len.saturating_add(n.saturating_mul(nb.len() - ob.len())))?;
            }
            let out = if ob.is_empty() { swapped_at(h, boundaries(), 0, new) } else { swapped_at(h, matches(), ob.len(), new) };
            Ok(match out { Some(out) => Some(out), None if F::SHARES => None, None => Some(h.copied()) })
        })()
    };
    match out.map_err(|e| e.raise::<S>(cx))? { Some(out) => Ok(F::give(cx, out)?.into()), None => Ok(a.recv.into()) }
}

/* the pieces between the matches of `sep` with at most `max` cuts from either end */
fn by_sep<H: Hay + ?Sized>(h: &H, sep: &[u8], max: usize, right: bool, out: &mut Vec<H::Owned>) {
    let (hay, n) = (h.bytes(), sep.len());
    if right {
        let mut end = hay.len();
        for j in search::Matches::back(hay, sep).take(max) {
            out.push(h.cut(j + n..end).copied());
            end = j;
        }
        out.push(h.cut(0..end).copied());
        out.reverse();
    } else {
        let mut at = 0;
        for j in search::Matches::new(hay, sep).take(max) {
            out.push(h.cut(at..j).copied());
            at = j + n;
        }
        out.push(h.cut(at..hay.len()).copied());
    }
}

/* the runs between white space with at most `max` cuts read in one pass */
fn by_space<L: Lang, H: Hay + ?Sized>(h: &H, ascii: bool, max: usize, right: bool, out: &mut Vec<H::Owned>) {
    // a run of one byte units walks its bytes with no decoding
    if ascii { runs::<L, H>(h, h.bytes_as_units(), max, right, out) } else { runs::<L, H>(h, h.units(), max, right, out) }
}

/* the runs between white space of the units `units` walks */
#[inline(always)]
fn runs<L: Lang, H: Hay + ?Sized>(h: &H, units: impl DoubleEndedIterator<Item = (usize, H::Unit)>, max: usize, right: bool, out: &mut Vec<H::Owned>) {
    let len = h.bytes().len();
    // the edge of the run being read and none inside white space
    let mut edge: Option<usize> = None;
    if right {
        for (b, u) in units.rev() {
            let after = b + H::size(u);
            if H::blank::<L>(u) {
                if let Some(e) = edge.take() { out.push(h.cut(after..e).copied()); }
            } else if edge.is_none() {
                // past the cut count the rest stays whole with its leading white space
                if out.len() == max { out.push(h.cut(0..after).copied()); out.reverse(); return; }
                edge = Some(after);
            }
        }
        if let Some(e) = edge { out.push(h.cut(0..e).copied()); }
        out.reverse();
    } else {
        for (b, u) in units {
            if H::blank::<L>(u) {
                if let Some(s) = edge.take() { out.push(h.cut(s..b).copied()); }
            } else if edge.is_none() {
                // past the cut count the rest stays whole with its trailing white space
                if out.len() == max { out.push(h.cut(b..len).copied()); return; }
                edge = Some(b);
            }
        }
        if let Some(s) = edge { out.push(h.cut(s..len).copied()); }
    }
}

/* refuses the pieces of a long receiver once their count passes the room */
#[cold]
#[inline(never)]
fn too_many(cx: &Cx<'_, '_>, len: usize, pieces: usize) -> Result<(), VmErr> {
    // the count read the receiver once more so it pays for it
    cx.spend(len);
    if pieces.saturating_mul(PIECE) > cx.room().saturating_sub(len) { Err(cold_heap()) } else { Ok(()) }
}

/* how many runs between white space a split makes with at most `max` cuts */
fn count_runs<L: Lang, H: Hay + ?Sized>(h: &H, max: usize) -> usize {
    let (mut n, mut inside) = (0usize, false);
    for (_, u) in h.units() {
        let blank = H::blank::<L>(u);
        if !blank && !inside { n += 1; if n > max { break; } }
        inside = !blank;
    }
    n
}

/* each part a new value of its family in a new list */
fn listed<F: Family>(cx: &mut Cx<'_, '_>, parts: Vec<Owned<F>>) -> Ret {
    let mut items = Vec::with_capacity(parts.len());
    for p in parts { items.push(F::give(cx, p)?); }
    Ok(cx.new_list(items)?.into())
}

fn split_run<S: Site, F: Family, const RIGHT: bool>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = F::ascii(cx, a.recv);
    let parts = {
        let cx = &*cx;
        (|| -> Result<Vec<Owned<F>>, Error> {
            let max = limit(cx, a.get(1))?;
            let sep = match a.get(0).filter(|v| !v.is_none()) { Some(v) => Some(F::arg(cx, v, Role::Sep)?), None => None };
            if sep.is_some_and(|s| s.bytes().is_empty()) { return Err(Error::Fail(Fail::EmptySeparator)); }
            let h = F::recv(cx, a.recv)?;
            let len = h.bytes().len();
            // a short receiver or one with room for a piece per byte needs no count
            if len > SHORT && (len + 1).saturating_mul(PIECE) > cx.room() {
                let n = match sep { Some(sep) => search::Matches::new(h.bytes(), sep.bytes()).take(max).count() + 1, None => count_runs::<S::Lang, F::Hay>(h, max) };
                too_many(cx, len, n)?;
            }
            let mut parts = Vec::new();
            match sep {
                Some(sep) => by_sep(h, sep.bytes(), max, RIGHT, &mut parts),
                None => by_space::<S::Lang, F::Hay>(h, ascii, max, RIGHT, &mut parts),
            }
            Ok(parts)
        })()
    };
    let parts = parts.map_err(|e| e.raise::<S>(cx))?;
    listed::<F>(cx, parts)
}

/* lines broken at each break `B` names with their ends kept when asked */
fn lines_run<F: Family, B: Breaks>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let parts = {
        let cx = &*cx;
        let keep = a.get(0).is_some_and(|v| cx.truthy(v));
        let h = F::recv(cx, a.recv)?;
        let len = h.bytes().len();
        // a short text or one with room for a line per byte needs no count
        if len > SHORT && (len + 1).saturating_mul(PIECE) > cx.room() { too_many(cx, len, h.units().filter(|&(_, u)| B::BREAKS.contains(&<F::Hay as Hay>::char(u))).count() + 1)?; }
        let (mut parts, mut start) = (Vec::new(), 0);
        let mut units = h.units().peekable();
        while let Some((i, u)) = units.next() {
            let c = <F::Hay as Hay>::char(u);
            if !B::BREAKS.contains(&c) { continue; }
            let mut end = i + <F::Hay as Hay>::size(u);
            if c == '\r' && units.next_if(|&(_, d)| <F::Hay as Hay>::char(d) == '\n').is_some() { end += 1; }
            parts.push(h.cut(start..if keep { end } else { i }).copied());
            start = end;
        }
        if start < h.bytes().len() { parts.push(h.cut(start..h.bytes().len()).copied()); }
        parts
    };
    listed::<F>(cx, parts)
}

/* the receiver without leading or trailing white space or units of the argument */
fn trim_run<S: Site, F: Family, const LEFT: bool, const RIGHT: bool>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = F::ascii(cx, a.recv);
    let out = {
        let cx = &*cx;
        (|| -> Result<Option<Owned<F>>, Error> {
            let set = match a.get(0).filter(|v| !v.is_none()) { Some(v) => Some(F::arg(cx, v, Role::Chars)?), None => None };
            let h = F::recv(cx, a.recv)?;
            let gone = |u: <F::Hay as Hay>::Unit| match set { Some(set) => set.holds(u), None => <F::Hay as Hay>::blank::<S::Lang>(u) };
            // a run of one byte units walks its bytes with no decoding
            let (s, e) = if ascii { kept::<F::Hay, LEFT, RIGHT, _>(h, || h.bytes_as_units(), gone) } else { kept::<F::Hay, LEFT, RIGHT, _>(h, || h.units(), gone) };
            if F::SHARES && e - s == h.bytes().len() { return Ok(None); }
            Ok(Some(h.cut(s..e).copied()))
        })()
    };
    match out.map_err(|e| e.raise::<S>(cx))? { Some(out) => Ok(F::give(cx, out)?.into()), None => Ok(a.recv.into()) }
}

/* the bytes of `h` left once units `gone` holds leave the ends asked */
#[inline(always)]
fn kept<H: Hay + ?Sized, const LEFT: bool, const RIGHT: bool, I: DoubleEndedIterator<Item = (usize, H::Unit)>>(h: &H, units: impl Fn() -> I, gone: impl Fn(H::Unit) -> bool) -> (usize, usize) {
    let len = h.bytes().len();
    let s = if LEFT { units().find(|&(_, u)| !gone(u)).map_or(len, |(b, _)| b) } else { 0 };
    let e = if RIGHT { units().rev().find(|&(_, u)| !gone(u)).map_or(s, |(b, u)| b + H::size(u)).max(s) } else { len };
    (s, e)
}

/* the items joined by the receiver once each is of its family */
fn joined<F: Family>(cx: &Cx<'_, '_>, sep: Val, items: &[Val]) -> Result<Owned<F>, Error> {
    let sep = F::recv(cx, sep)?;
    let mut total = sep.bytes().len().saturating_mul(items.len().saturating_sub(1));
    for (i, &v) in items.iter().enumerate() {
        let Some(p) = F::part(cx, v) else { return Err(Error::Fail(Fail::JoinItem { at: i, v, want: F::KIND })) };
        total = total.saturating_add(p.bytes().len());
    }
    cx.reserve(total)?;
    cx.spend(total);
    let mut out = <F::Hay as Hay>::owned(total);
    let glue = !sep.bytes().is_empty();
    for (i, &v) in items.iter().enumerate() {
        if i > 0 && glue { <F::Hay as Hay>::push(&mut out, sep); }
        if let Some(p) = F::part(cx, v) { <F::Hay as Hay>::push(&mut out, p); }
    }
    Ok(out)
}

/* the parts joined by the receiver with lists read in place and generators drained */
fn join_run<S: Site, F: Family>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let it = a.pos[0];
    let (n, generator) = match cx.heap().try_get(it) {
        Some(HeapObj::List(rc)) => (Some(rc.borrow().len()), false),
        Some(HeapObj::Tuple(t)) => (Some(t.len()), false),
        Some(HeapObj::Coroutine(_)) => (None, true),
        _ => (None, false),
    };
    if let Some(n) = n {
        // the charge a copy of the items would have paid
        cx.charge(n)?;
        let out = match cx.heap().try_get(it) {
            Some(HeapObj::List(rc)) => joined::<F>(cx, a.recv, &rc.borrow()),
            Some(HeapObj::Tuple(t)) => joined::<F>(cx, a.recv, t),
            _ => joined::<F>(cx, a.recv, &[]),
        };
        let out = out.map_err(|e| e.raise::<S>(cx))?;
        return Ok(F::give(cx, out)?.into());
    }
    let parts = if generator { None } else {
        match cx.items(it) {
            Ok(p) => p,
            Err(e) if e.is_kind(ErrKind::Type) => return Err(fail::<S>(cx, Fail::JoinIterable)),
            Err(e) => return Err(e),
        }
    };
    let Some(parts) = parts else {
        // a coroutine only steps under await so it is no iterable here
        if generator && cx.is_coroutine(it) { return Err(fail::<S>(cx, Fail::JoinIterable)); }
        let mut acc = Acc::new();
        acc.v[0] = a.recv;
        return Ok(Step::Drain { it, fold: Fold::with(join_fold::<S, F>, acc) }.into());
    };
    let out = joined::<F>(cx, a.recv, &parts).map_err(|e| e.raise::<S>(cx))?;
    Ok(F::give(cx, out)?.into())
}

/* the drain of `join` that gathers every part and joins them at the end */
fn join_fold<S: Site, F: Family>(acc: &mut Acc, cx: &mut Cx<'_, '_>, item: Item) -> Result<Flow, VmErr> {
    match item {
        Item::Next(v) => {
            // one allocation for the parts of a short generator where growth would take two
            if acc.items.capacity() == 0 { acc.items.reserve(8); }
            gather(acc, cx, v)
        }
        Item::End(_) => {
            let out = joined::<F>(cx, acc.v[0], &acc.items).map_err(|e| e.raise::<S>(cx))?;
            Ok(Flow::Stop(F::give(cx, out)?))
        }
        Item::Called(_) => Ok(Flow::More),
    }
}

/* the parts around the first or last separator keeping shared values as given */
fn halves<S: Site, F: Family, const LAST: bool>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let found = {
        let cx = &*cx;
        (|| -> Result<(Owned<F>, Option<Owned<F>>), Error> {
            let sep = F::arg(cx, a.pos[0], Role::Part)?;
            if sep.bytes().is_empty() { return Err(Error::Fail(Fail::EmptySeparator)); }
            let h = F::recv(cx, a.recv)?;
            let (hay, n) = (h.bytes(), sep.bytes().len());
            let at = if LAST { search::rfind(hay, sep.bytes()) } else { search::find(hay, sep.bytes()) };
            Ok(match at {
                Some(i) => (h.cut(0..i).copied(), Some(h.cut(i + n..hay.len()).copied())),
                None => (<F::Hay as Hay>::owned(0), None),
            })
        })()
    };
    let (head, tail) = found.map_err(|e| e.raise::<S>(cx))?;
    let parts = match tail {
        Some(tail) => [F::give(cx, head)?, F::keep(cx, a.pos[0])?, F::give(cx, tail)?],
        None if LAST => [F::give(cx, <F::Hay as Hay>::owned(0))?, F::give(cx, head)?, F::keep(cx, a.recv)?],
        None => [F::keep(cx, a.recv)?, F::give(cx, head)?, F::give(cx, <F::Hay as Hay>::owned(0))?],
    };
    Ok(cx.new_tuple(parts.to_vec())?.into())
}

/* the receiver without the affix where a shared receiver whole comes back as itself */
fn unaffixed<S: Site, F: Family, const SUFFIX: bool>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let out = {
        let cx = &*cx;
        (|| -> Result<Option<Owned<F>>, Error> {
            let affix = F::arg(cx, a.pos[0], Role::Affix)?;
            let h = F::recv(cx, a.recv)?;
            let (hay, p) = (h.bytes(), affix.bytes());
            let rest = if SUFFIX { hay.strip_suffix(p).map(|r| 0..r.len()) } else { hay.strip_prefix(p).map(|_| p.len()..hay.len()) };
            Ok(match rest {
                Some(r) if !p.is_empty() => Some(h.cut(r).copied()),
                _ if F::SHARES => None,
                _ => Some(h.copied()),
            })
        })()
    };
    match out.map_err(|e| e.raise::<S>(cx))? { Some(out) => Ok(F::give(cx, out)?.into()), None => Ok(a.recv.into()) }
}

/* one fill unit pads to a width splitting a centred pad as the runtime chooses */
fn justify<S: Site, F: Family, const ALIGN: u8>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = F::ascii(cx, a.recv);
    let out = {
        let cx = &*cx;
        (|| -> Result<Option<Owned<F>>, Error> {
            let width = width(cx, a.pos[0])?;
            let fill = F::fill(cx, a.get(1))?;
            let h = F::recv(cx, a.recv)?;
            let pad = width.saturating_sub(h.count(ascii));
            if pad == 0 && F::SHARES { return Ok(None); }
            // a fill of several bytes takes that many for each unit of the pad
            let size = <F::Hay as Hay>::size(fill);
            if size > 1 { cx.reserve(h.bytes().len().saturating_add(pad.saturating_mul(size)))?; }
            // the pad is charged once nothing refuses it
            cx.spend(width);
            let left = match ALIGN { b'<' => 0, b'>' => pad, _ => <S::Lang as Lang>::CENTRE.before(pad, width) };
            let mut out = <F::Hay as Hay>::owned(h.bytes().len() + pad * <F::Hay as Hay>::size(fill));
            <F::Hay as Hay>::push_unit(&mut out, fill, left);
            <F::Hay as Hay>::push(&mut out, h);
            <F::Hay as Hay>::push_unit(&mut out, fill, pad - left);
            Ok(Some(out))
        })()
    };
    match out.map_err(|e| e.raise::<S>(cx))? { Some(out) => Ok(F::give(cx, out)?.into()), None => Ok(a.recv.into()) }
}

fn zeros_run<S: Site, F: Family>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = F::ascii(cx, a.recv);
    let out = {
        let cx = &*cx;
        (|| -> Result<Option<Owned<F>>, Error> {
            let width = width(cx, a.pos[0])?;
            cx.spend(width);
            let h = F::recv(cx, a.recv)?;
            let (n, len) = (h.count(ascii), h.bytes().len());
            if n >= width && F::SHARES { return Ok(None); }
            let signed = usize::from(matches!(h.bytes().first(), Some(b'+' | b'-')));
            let mut out = <F::Hay as Hay>::owned(len + width.saturating_sub(n));
            <F::Hay as Hay>::push(&mut out, h.cut(0..signed));
            <F::Hay as Hay>::push_unit(&mut out, <F::Hay as Hay>::from_ascii(b'0'), width.saturating_sub(n));
            <F::Hay as Hay>::push(&mut out, h.cut(signed..len));
            Ok(Some(out))
        })()
    };
    match out.map_err(|e| e.raise::<S>(cx))? { Some(out) => Ok(F::give(cx, out)?.into()), None => Ok(a.recv.into()) }
}

/* a tab size refused past the machine int a runtime holds it in */
#[inline(always)]
fn tab_size<L: Lang>(cx: &Cx<'_, '_>, v: Val) -> Result<usize, Error> {
    let n = match word(cx, v) {
        Some(Ok(n)) => n,
        Some(Err(e)) => return Err(match L::MACHINE { Some(_) => Error::Fail(Fail::TabsWide), None => Error::Vm(e) }),
        None => return Err(Error::Fail(Fail::NotIndex(v))),
    };
    if let Some(m) = L::MACHINE && !Machine::fits(i128::from(n), m.int) { return Err(Error::Fail(Fail::TabsWide)); }
    // a size no usize holds saturates and the sizing pass refuses the pad it asks
    Ok(usize::try_from(n.max(0)).unwrap_or(usize::MAX))
}

fn tabs_run<S: Site, F: Family>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let out = {
        let cx = &*cx;
        (|| -> Result<Owned<F>, Error> {
            let size = match a.get(0) {
                None => 8,
                Some(v) => tab_size::<S::Lang>(cx, v)?,
            };
            let h = F::recv(cx, a.recv)?;
            let (room, len) = (cx.room(), h.bytes().len());
            let stop = |col: usize| if size == 0 { 0 } else { size - col % size };
            // a first pass sizes the text so it is refused before any of it exists
            let (mut total, mut col) = (0usize, 0usize);
            for (_, u) in h.units() {
                let c = <F::Hay as Hay>::char(u);
                let n = if c == '\t' { stop(col) } else { 1 };
                total = total.saturating_add(if c == '\t' { n } else { <F::Hay as Hay>::size(u) });
                if total > room { return Err(Error::Vm(cold_heap())); }
                col = if c == '\n' || c == '\r' { 0 } else { col + n };
            }
            let (mut out, mut col, mut run, mut written) = (<F::Hay as Hay>::owned(total), 0usize, 0, 0usize);
            for (i, u) in h.units() {
                let c = <F::Hay as Hay>::char(u);
                if c == '\t' {
                    let n = stop(col);
                    <F::Hay as Hay>::push(&mut out, h.cut(run..i));
                    <F::Hay as Hay>::push_unit(&mut out, <F::Hay as Hay>::from_ascii(b' '), n);
                    written += i - run + n;
                    run = i + 1;
                    col += n;
                } else {
                    col = if c == '\n' || c == '\r' { 0 } else { col + 1 };
                }
            }
            <F::Hay as Hay>::push(&mut out, h.cut(run..len));
            cx.spend(written + len - run);
            Ok(out)
        })()
    };
    let out = out.map_err(|e| e.raise::<S>(cx))?;
    Ok(F::give(cx, out)?.into())
}
