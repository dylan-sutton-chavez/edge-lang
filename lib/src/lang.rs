use alloc::string::String;

use edge::lang::{Cx, ErrKind, KindId, Language, Mode, Names, iters};
use edge::value::{HeapPool, Val, VmErr};

use crate::fail::{self, At, Class, Fail};
use crate::fmt::render::{self, Style};
use crate::uni;

/* what lib reads off any language which its names and policy decide */
pub trait Derived: Language + Style {
    // a bool's type beside the int type it counts as under the policy
    const ALSO: &'static [(&'static str, &'static str)] = if Self::POLICY.bool_is_int { &[(Self::NAMES.kinds[KindId::Bool as usize], Self::NAMES.kinds[KindId::Int as usize])] } else { &[] };
    // the names of the builtin iterators the kernels make
    const ITERS: Iters = Iters::of(Self::NAMES);
    // decimal text of an int past this many digits raises
    const DIGITS: Option<usize> = match Self::POLICY.max_int_digits { Some(d) => Some(d as usize), None => None };
}

impl<L: Language + Style> Derived for L {}

/* every choice a runtime makes for the kernels its tables name past its language */
pub trait Lang: Derived + Sized + 'static {
    // the classes the type tests walk as `Tree::of` builds them from the language's names
    const TREE: &'static Tree;
    // whether scalars and texts show as the default render shows them
    const PLAIN: bool = true;
    // where a centred text puts the unit an odd pad leaves over
    const CENTRE: Centre = Centre::After;
    // how the text a number conversion reads may write its digits
    const NUMERALS: Numerals = Numerals::PLAIN;
    // whether the four information separators count as white space in text
    const SEPARATORS: bool = false;
    // the machine ints sizes must fit where none bounds them by memory alone
    const MACHINE: Option<Machine> = None;
    // the parts of a format spec the spec reads past its plain ones
    const SPECS: Specs = Specs::PLAIN;
    // whether a generator's throw takes the value to raise after its class
    const THROW_VALUE: bool = false;
    // whether iter takes a sentinel after a function it calls until one comes back
    const SENTINEL: bool = false;

    /* the class a failure of class `c` raises */
    fn class(c: Class) -> &'static str { class::<Self>(c) }

    /* the error `f` raises where `at` is the native that failed */
    fn words(cx: &mut Cx<'_, '_>, at: At, f: Fail) -> VmErr { fail::english::<Self>(cx, at, f) }

    /* the error `f` raises when no native call runs */
    fn words_bare(f: Fail) -> VmErr { fail::bare_english::<Self>(f) }

    /* `v` as text in `mode` onto `out` as the runtime renders it */
    fn show_into(cx: &mut Cx<'_, '_>, v: Val, mode: Mode, out: &mut String) -> Result<(), VmErr> { render::show_into::<Self>(cx, v, mode, out) }

    /* the codec a program names and none for a name the runtime does not know */
    fn codec(_name: &str) -> Option<Codec> { None }

    /* the handler a program names for what a codec cannot convert */
    fn handler(_name: &str) -> Option<OnError> { None }

    /* the name a codec failure gives `c` */
    fn codec_name(c: Codec) -> &'static str {
        match c {
            Codec::Utf8 => "utf-8", Codec::Ascii => "us-ascii", Codec::Latin1 => "iso-8859-1", Codec::Windows1252 => "windows-1252",
            Codec::Utf16(Order::Be) => "utf-16be", Codec::Utf16(_) => "utf-16le", Codec::Utf32(Order::Be) => "utf-32be", Codec::Utf32(_) => "utf-32le",
        }
    }

    /* the byte order a program names and none for a name the runtime lacks */
    fn endian(_name: &str) -> Option<Endian> { None }
}

/* whether `c` is white space in the set runtime `L` reads */
#[inline(always)]
pub fn space<L: Lang>(c: char) -> bool { if L::SEPARATORS { uni::is_space_or_separator(c) } else { uni::is_space(c) } }

/* the white space test of the set runtime `L` reads */
#[inline(always)]
pub const fn spaces<L: Lang>() -> fn(char) -> bool { if L::SEPARATORS { uni::is_space_or_separator } else { uni::is_space } }

/* number text trimmed of `L`'s white space with the digits `D` reads as ascii */
#[inline(always)]
pub fn number_text<L: Lang, D: uni::Digits>(s: &str) -> alloc::borrow::Cow<'_, str> {
    // each set trims through its own function as a call the trim can inline
    if L::SEPARATORS { uni::number_text::<D>(s, uni::is_space_or_separator) } else { uni::number_text::<D>(s, uni::is_space) }
}

/* where a centred pad puts the unit an odd pad leaves over */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Centre {
    // after the text which leans to the start
    After,
    // before the text which leans to the end
    Before,
    // before the text on an odd width and after it on an even one
    ByWidth,
}

impl Centre {
    /* the units of a pad of `pad` that go before a text centred in `width` */
    #[inline(always)]
    pub const fn before(self, pad: usize, width: usize) -> usize {
        match self {
            Centre::After => pad / 2,
            Centre::Before => pad - pad / 2,
            Centre::ByWidth => pad / 2 + (pad & width & 1),
        }
    }
}

/* how the digits of the text a number conversion reads may be written */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Numerals {
    // the ascii separator that may stand between two digits where none takes no separator
    pub separator: Option<u8>,
    // whether separators may stand side by side
    pub doubled: bool,
    // whether one separator may follow a base prefix
    pub after_prefix: bool,
    // whether a base read off the text takes decimal text that opens with a zero
    pub leading_zero: bool,
    // whether a base the call names still reads the prefix of that base
    pub prefixed_base: bool,
}

impl Numerals {
    // digits alone where a prefix names the base only when the call names none
    pub const PLAIN: Numerals = Numerals { separator: None, doubled: false, after_prefix: false, leading_zero: true, prefixed_base: false };
}

/* the bits of the signed machine ints one implementation holds sizes in */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Machine {
    // the int a tab size is held in
    pub int: u32,
    // the int a count of bytes made or taken is held in
    pub index: u32,
}

impl Machine {
    /* whether `n` fits a signed int of `bits` bits */
    #[inline]
    pub const fn fits(n: i128, bits: u32) -> bool {
        let top = 1i128 << (bits - 1);
        n >= -top && n < top
    }
}

/* the parts of a format spec past fill align sign width precision and plain types */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Specs {
    // a `z` that turns a negative zero after rounding positive
    pub z: bool,
    // a comma or an underscore that groups the digits
    pub groups: bool,
    // the type `%` that writes a hundred times the number with a percent sign
    pub percent: bool,
    // the type `n` that reads as `d` for ints and `g` for floats
    pub n: bool,
    // the align `=` that pads between the sign and the digits
    pub eq: bool,
}

impl Specs {
    // none of the parts past the plain spec
    pub const PLAIN: Specs = Specs { z: false, groups: false, percent: false, n: false, eq: false };
    // every part the spec reads
    pub const ALL: Specs = Specs { z: true, groups: true, percent: true, n: true, eq: true };
}

/* the order an int's bytes are written in */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Endian { Big, Little }

/* the class of failure class `c` by the error kind it matches in `L`'s names */
#[inline]
pub fn class<L: Language>(c: Class) -> &'static str {
    // each arm reads one fixed place so the match folds to a table of constants
    let names = L::NAMES;
    match c {
        Class::Value | Class::Decode | Class::Encode | Class::End => names.error(ErrKind::Value),
        Class::Overflow => names.error(ErrKind::Overflow),
        Class::Index => names.error(ErrKind::Index),
        Class::Key | Class::Lookup => names.error(ErrKind::Key),
        Class::Exit => names.error(ErrKind::Exit),
        Class::Runtime => names.error(ErrKind::Runtime),
        Class::ZeroDiv => names.error(ErrKind::ZeroDiv),
    }
}

/* the encodings text and bytes convert through */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Codec { Utf8, Ascii, Latin1, Windows1252, Utf16(Order), Utf32(Order) }

/* a wide codec's byte order where `Mark` writes and reads a byte order mark */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Order { Mark, Le, Be }

/* what a codec does with what it cannot convert */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OnError {
    // fails at the first span
    Strict,
    // drops the span
    Skip,
    // writes a question mark or the replacement character
    Replace,
    // writes each unit as a backslash escape
    Escape,
    // writes each character as a numeric character reference
    CharRef,
    // writes each character by its unicode name which no table here holds
    Named,
}

/* the names of the iterators the kernels make by their role */
#[derive(Clone, Copy)]
pub struct Iters {
    pub list: &'static str,
    pub list_back: &'static str,
    pub range: &'static str,
    pub tuple: &'static str,
    pub text: &'static str,
    pub ascii: &'static str,
    pub bytes: &'static str,
    pub buffer: &'static str,
    pub set: &'static str,
    pub keys: &'static str,
    pub values: &'static str,
    pub items: &'static str,
    pub keys_back: &'static str,
    pub calls: &'static str,
    pub numbered: &'static str,
    pub zipped: &'static str,
    pub mapped: &'static str,
    pub filtered: &'static str,
    pub backward: &'static str,
}

impl Iters {
    /* each role's name at its place in `names` where a snapshot also finds it */
    pub const fn of(names: &Names) -> Iters {
        let n = &names.iters;
        Iters {
            list: n[iters::LIST], list_back: n[iters::LIST_BACK], range: n[iters::RANGE], tuple: n[iters::TUPLE], text: n[iters::TEXT],
            ascii: n[iters::ASCII], bytes: n[iters::BYTES], buffer: n[iters::BYTE_ARRAY], set: n[iters::SET], keys: n[iters::KEYS],
            values: n[iters::VALUES], items: n[iters::ITEMS], keys_back: n[iters::KEYS_BACK], calls: n[iters::CALL],
            numbered: n[iters::COUNT], zipped: n[iters::ZIP], mapped: n[iters::MAP], filtered: n[iters::FILTER], backward: n[iters::BACK],
        }
    }
}

// the most classes a tree holds
const MOST: usize = 64;
// the longest class name a lookup compares a word at a time
const LONGEST: usize = 24;

/* a runtime's classes each beside its parent with the lookups built at compile time */
pub struct Tree {
    // the type every value is an instance of
    pub root: &'static str,
    // a kind's type name beside the type it also counts as
    pub also: &'static [(&'static str, &'static str)],
    classes: [(&'static str, &'static str); MOST],
    len: usize,
    // where the names of each length lie which run shortest first
    runs: [(u8, u8); LONGEST + 1],
    // each name as the three words a lookup compares
    words: [[u64; 3]; MOST],
    // the place of each parent where the root class is its own parent
    up: [u8; MOST],
    // the first bytes of the names so other names leave at once
    first: [bool; 128],
}

impl Tree {
    /* the engine's classes of language `L` under the type every object is */
    pub const fn of<L: Derived>() -> Tree { Tree::new(L::NAMES.kinds[KindId::Instance as usize], L::ALSO, L::NAMES.tree.pairs) }

    /* the tree of `classes` beside their parents with the top's parent empty */
    const fn new(root: &'static str, also: &'static [(&'static str, &'static str)], classes: &[(&'static str, &'static str)]) -> Tree {
        assert!(classes.len() <= MOST, "a tree past the classes a lookup holds");
        let (len, sorted) = (classes.len(), shortest_first(classes));
        let classes = sorted.split_at(len).0;
        let mut t = Tree { root, also, classes: [("", ""); MOST], len, runs: [(0, 0); LONGEST + 1], words: [[0; 3]; MOST], up: [0; MOST], first: [false; 128] };
        let mut i = 0;
        while i < classes.len() {
            let name = classes[i].0.as_bytes();
            let n = name.len();
            assert!(n > 0 && n <= LONGEST && name[0] < 128, "a class name a lookup cannot compare");
            t.classes[i] = classes[i];
            if t.runs[n].1 == 0 { t.runs[n].0 = i as u8; }
            t.runs[n].1 = i as u8 + 1;
            let mut k = 0;
            while k < n { t.words[i][k / 8] |= (name[k] as u64) << (8 * (k % 8)); k += 1; }
            t.first[name[0] as usize] = true;
            i += 1;
        }
        let mut i = 0;
        while i < classes.len() {
            t.up[i] = i as u8;
            let mut j = 0;
            while j < classes.len() {
                if same(classes[j].0, classes[i].1) { t.up[i] = j as u8; }
                j += 1;
            }
            assert!(classes[i].1.is_empty() || t.up[i] != i as u8, "a class names a parent outside the tree");
            i += 1;
        }
        assert!(edge::lang::Tree::rooted(&t.up, classes.len()), "a class tree holds a cycle");
        t
    }

    /* every class beside its parent shortest name first */
    pub fn classes(&self) -> &[(&'static str, &'static str)] { self.classes.get(..self.len).unwrap_or(&[]) }

    /* the place of class `name` found a word at a time */
    pub fn place(&self, name: &str) -> Option<usize> {
        let &(lo, hi) = self.runs.get(name.len())?;
        let &first = name.as_bytes().first()?;
        if lo == hi || !self.first.get(first as usize).copied().unwrap_or(false) { return None; }
        let mut buf = [0u8; LONGEST];
        buf.get_mut(..name.len())?.copy_from_slice(name.as_bytes());
        let (&[a, b, c], _) = buf.as_chunks::<8>() else { return None };
        let (a, b, c) = (u64::from_le_bytes(a), u64::from_le_bytes(b), u64::from_le_bytes(c));
        (lo as usize..hi as usize).find(|&i| self.words[i][0] == a && self.words[i][1] == b && self.words[i][2] == c)
    }

    /* the parent of class `name` and none for the top class or any other name */
    pub fn parent(&self, name: &str) -> Option<&'static str> {
        let at = self.place(name)?;
        let up = self.up[at] as usize;
        (up != at).then_some(self.classes[up].0)
    }

    /* whether `name` is a class of the tree */
    pub fn has(&self, name: &str) -> bool { self.place(name).is_some() }

    /* whether `name` is `ancestor` or reaches it through its parents */
    pub fn derives(&self, name: &str, ancestor: &str) -> bool {
        if core::ptr::eq(name, ancestor) || name == ancestor { return true; }
        let Some(mut at) = self.place(name) else { return false };
        let Some(to) = self.place(ancestor) else { return false };
        loop {
            let up = self.up[at] as usize;
            if up == at { return false; }
            if up == to { return true; }
            at = up;
        }
    }

    /* the first class of the tree the bases of `cls` reach in their order */
    pub fn base_of<'h>(&self, heap: &'h HeapPool, cls: Val) -> Option<&'h str> {
        let m = heap.meta_of(cls)?;
        if m.is_type_meta() { return heap.meta_name(m).filter(|n| heap.is_exc_type(cls) || self.has(n)); }
        heap.bases_of(m).iter().find_map(|&b| self.base_of(heap, b))
    }

    /* whether a value of kind type `ty` counts as type `name` through `also` */
    #[inline]
    pub fn counts_as(&self, ty: &str, name: &str) -> bool { self.also.iter().any(|&(k, as_)| k == ty && as_ == name) }
}

/* `classes` ordered by the length of their names keeping the order within one length */
const fn shortest_first(classes: &[(&'static str, &'static str)]) -> [(&'static str, &'static str); MOST] {
    let mut out = [("", ""); MOST];
    let mut i = 0;
    while i < classes.len() {
        let mut j = i;
        while j > 0 && out[j - 1].0.len() > classes[i].0.len() { out[j] = out[j - 1]; j -= 1; }
        out[j] = classes[i];
        i += 1;
    }
    out
}

/* whether two names spell the same bytes */
const fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() { return false; }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] { return false; }
        i += 1;
    }
    true
}
