use alloc::string::String;

use crate::engine::lang::{Cx, ErrKind, KindId, Language, Names, iters};
use crate::engine::value::{HeapPool, Val, VmErr};

use crate::engine::lib::fail::{self, At, Class, Fail};
use crate::engine::lib::fmt::render::{Escape, Floats, hex, push};
use crate::engine::lib::uni;

/* what lib reads off any language which its names and policy decide */
pub(crate) trait Derived: Lang {
    // a bool's type beside the int type it counts as under the policy
    const ALSO: &'static [(&'static str, &'static str)] = if Self::POLICY.bool_is_int { &[(Self::NAMES.kinds[KindId::Bool as usize], Self::NAMES.kinds[KindId::Int as usize])] } else { &[] };
    // the names of the builtin iterators the kernels make
    const ITERS: Iters = Iters::of(Self::NAMES);
    // decimal text of an int past this many digits raises
    const DIGITS: Option<usize> = match Self::POLICY.max_int_digits { Some(d) => Some(d as usize), None => None };
    // the classes the type tests walk as the language's names give them
    const TREE: &'static Tree = &Tree::of::<Self>();
}

impl<L: Lang> Derived for L {}

/* every choice a runtime makes for the kernels its tables name past its language */
pub trait Lang: Language + Sized + 'static {
    // whether print writes scalars and texts on its fast path as the default render does
    const PLAIN_PRINT: bool = true;
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

    const TRUE: &'static str = "true";
    const FALSE: &'static str = "false";
    // a text's quote and the one it takes when the text holds only the first
    const QUOTE: char = '"';
    const SWITCH: Option<char> = None;
    const ESCAPE: Escape = Escape::Braced;
    // the prefix a bytes repr takes
    const BYTES: &'static str = "b";
    // what a mutable buffer's repr puts around its bytes repr
    const BUFFER: (&'static str, &'static str) = ("", "");
    // empty shows an empty set as its kind name and a bare call, apart from a map
    const EMPTY_SET: &'static str = "";
    // the close of a tuple of one item
    const ONE: &'static str = ",)";
    const FLOATS: Floats = Floats::PLAIN;
    // what the open bracket of a container holding itself shows past it
    const AGAIN: &'static str = "..";
    // what a map shows between each key and its value
    const PAIR: &'static str = ": ";
    /* what a bound method puts before its name before its receiver and after it */
    const BOUND: (&'static str, &'static str, &'static str) = ("fn ", " of ", "");
    /* what a slice puts before its bounds between them and after them */
    const SLICE: (&'static str, &'static str, &'static str) = ("(", "; ", ")");
    /* what an exception's repr puts around its arguments after its class */
    const RAISED: (&'static str, &'static str) = ("(", ")");
    /* what a numbered system error puts before its number after it and before its file */
    const NUMBERED: (&'static str, &'static str, &'static str) = ("[", "] ", ": ");

    /* the class a failure of class `c` raises */
    fn class(c: Class) -> &'static str { class::<Self>(c) }

    /* the error `f` raises where `at` is the native that failed */
    fn words(cx: &mut Cx<'_, '_>, at: At, f: Fail) -> VmErr { fail::english::<Self>(cx, at, f) }

    /* the error `f` raises when no native call runs */
    fn words_bare(f: Fail) -> VmErr { fail::bare_english::<Self>(f) }

    /* the codec a program names and none for a name the runtime does not know */
    fn codec(_name: &str) -> Option<Codec> { None }

    /* the handler a program names for what a codec cannot convert */
    fn handler(_name: &str) -> Option<OnError> { None }

    /* the name a codec failure gives `c` */
    fn codec_name(c: Codec) -> &'static str {
        match c {
            Codec::Utf8 => "utf-8", Codec::Ascii => "us-ascii", Codec::Latin1 => "iso-8859-1", Codec::Windows1252 => "windows-1252",
            Codec::Utf16(Some(Endian::Big)) => "utf-16be", Codec::Utf16(_) => "utf-16le", Codec::Utf32(Some(Endian::Big)) => "utf-32be", Codec::Utf32(_) => "utf-32le",
        }
    }

    /* the byte order a program names and none for a name the runtime lacks */
    fn endian(_name: &str) -> Option<Endian> { None }

    /* a type object named `name` */
    fn type_form(out: &mut String, name: &str) { out.push_str(name); }

    /* a native function named `name` */
    fn native_form(out: &mut String, name: &str) { push(out, &["fn ", name]); }

    /* native method `name` read off kind `kind` */
    fn method_form(out: &mut String, name: &str, kind: &str) { push(out, &["fn ", kind, "::", name]); }

    /* native method `name` bound to a value of type `ty` at `at` */
    fn bound_native_form(out: &mut String, name: &str, ty: &str, at: u64) { push(out, &["fn ", ty, "::", name, " bound at "]); hex(out, at); }

    /* a value of type `ty` at `at` with no text of its own */
    fn object_form(out: &mut String, ty: &str, at: u64) { push(out, &[ty, " at "]); hex(out, at); }

    /* a module named `name` */
    fn module_form(out: &mut String, name: &str) { push(out, &["mod ", name]); }

    /* a host function named `name` */
    fn extern_form(out: &mut String, name: &str) { push(out, &["extern fn ", name]); }

    /* a user function at `at` */
    fn function_form(out: &mut String, name: &str, at: u64) { push(out, &["fn ", name, " at "]); hex(out, at); }

    /* a user class named `name` */
    fn class_form(out: &mut String, name: &str) { out.push_str(name); }

    /* an instance of user class `name` at `at` */
    fn instance_form(out: &mut String, name: &str, at: u64) { push(out, &[name, " at "]); hex(out, at); }

    /* a generator or coroutine of type `ty` running `name` at `at` */
    fn coroutine_form(out: &mut String, ty: &str, name: &str, at: u64) { push(out, &[ty, " ", name, " at "]); hex(out, at); }

    /* a cell at `at` empty or holding a value of type `ty` at `held` */
    fn cell_form(out: &mut String, at: u64, held: Option<(&str, u64)>) {
        out.push_str("cell at ");
        hex(out, at);
        if let Some((ty, h)) = held { push(out, &[" holding ", ty, " at "]); hex(out, h); }
    }

    /* a range by its bounds and its step when it is not one */
    fn range_form(out: &mut String, start: &str, stop: &str, step: Option<&str>) {
        push(out, &[start, "..", stop]);
        if let Some(st) = step { push(out, &[" by ", st]); }
    }

    /* whether an exception of class `name` shows its lone argument as its repr */
    fn keyed(_name: &str) -> bool { false }

    /* whether class `name` shows two or three args as a numbered system error */
    fn numbered(_name: &str) -> bool { false }

    /* whether map key `key` shows bare as the runtime's record literal writes it */
    fn bare_key(_key: &str) -> bool { false }
}

/* the int a bool counts as where the policy counts bools as ints */
#[inline(always)]
pub(crate) fn bool_int(heap: &HeapPool, v: Val) -> Option<i64> { if v.is_bool() && heap.policy().bool_is_int { Some(v.as_bool() as i64) } else { None } }

/* whether `v` is an int of any width or a bool where bools are ints */
#[cfg(any(feature = "iter", feature = "num"))]
pub(crate) fn int_like(cx: &Cx<'_, '_>, v: Val) -> bool { v.is_int() || bool_int(cx.heap(), v).is_some() || cx.heap().is_wide(v) }

/* whether `c` is white space in the set runtime `L` reads */
#[inline(always)]
pub(crate) fn space<L: Lang>(c: char) -> bool { if L::SEPARATORS { uni::is_space_or_separator(c) } else { uni::is_space(c) } }

/* the white space test of the set runtime `L` reads */
#[inline(always)]
pub(crate) const fn spaces<L: Lang>() -> fn(char) -> bool { if L::SEPARATORS { uni::is_space_or_separator } else { uni::is_space } }

/* number text trimmed of `L`'s white space with the digits `D` reads as ascii */
#[inline(always)]
pub(crate) fn number_text<L: Lang, D: uni::Digits>(s: &str) -> alloc::borrow::Cow<'_, str> {
    // each set trims through its own function as a call the trim can inline
    if L::SEPARATORS { uni::number_text::<D>(s, uni::is_space_or_separator) } else { uni::number_text::<D>(s, uni::is_space) }
}

/* where a centred pad puts the unit an odd pad leaves over */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
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
    internal! {
        #[inline(always)]
        pub const fn before(self, pad: usize, width: usize) -> usize {
            match self {
                Centre::After => pad / 2,
                Centre::Before => pad - pad / 2,
                Centre::ByWidth => pad / 2 + (pad & width & 1),
            }
        }
    }
}

/* how the digits of the text a number conversion reads may be written */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct Numerals {
    // the ascii separator that may stand between two digits where none takes no separator
    pub(crate) separator: Option<u8>,
    // whether separators may stand side by side
    pub(crate) doubled: bool,
    // whether one separator may follow a base prefix
    pub(crate) after_prefix: bool,
    // whether a base read off the text takes decimal text that opens with a zero
    pub(crate) leading_zero: bool,
    // whether a base the call names still reads the prefix of that base
    pub(crate) prefixed_base: bool,
}

impl Numerals {
    setters! {
        separator: Option<u8>,
        doubled: bool,
        after_prefix: bool,
        leading_zero: bool,
        prefixed_base: bool,
    }
}

impl Numerals {
    // digits alone where a prefix names the base only when the call names none
    pub const PLAIN: Numerals = Numerals { separator: None, doubled: false, after_prefix: false, leading_zero: true, prefixed_base: false };
}

/* the bits of the signed machine ints one implementation holds sizes in */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct Machine {
    // the int a tab size is held in
    pub int: u32,
    // the int a count of bytes made or taken is held in
    pub index: u32,
}

impl Machine {
    pub const fn new(int: u32, index: u32) -> Machine { Machine { int, index } }

    /* whether `n` fits a signed int of `bits` bits */
    #[inline]
    pub(crate) const fn fits(n: i128, bits: u32) -> bool {
        let top = 1i128 << (bits - 1);
        n >= -top && n < top
    }
}

/* the parts of a format spec past fill align sign width precision and plain types */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct Specs {
    // a `z` that turns a negative zero after rounding positive
    pub(crate) z: bool,
    // a comma or an underscore that groups the digits
    pub(crate) groups: bool,
    // the type `%` that writes a hundred times the number with a percent sign
    pub(crate) percent: bool,
    // the type `n` that reads as `d` for ints and `g` for floats
    pub(crate) n: bool,
    // the align `=` that pads between the sign and the digits
    pub(crate) eq: bool,
}

impl Specs {
    setters! {
        z: bool,
        groups: bool,
        percent: bool,
        n: bool,
        eq: bool,
    }
}

impl Specs {
    // none of the parts past the plain spec
    pub const PLAIN: Specs = Specs { z: false, groups: false, percent: false, n: false, eq: false };
    // every part the spec reads
    pub const ALL: Specs = Specs { z: true, groups: true, percent: true, n: true, eq: true };
}

/* the byte order of an int or of a wide codec's units */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[expect(clippy::exhaustive_enums, reason = "two byte orders are all there are")]
pub enum Endian { Big, Little }

/* the class of failure class `c` by the error kind it matches in `L`'s names */
#[inline]
pub(crate) fn class<L: Language>(c: Class) -> &'static str {
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

/* text and bytes encodings where a wide one without an order reads a mark */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Codec { Utf8, Ascii, Latin1, Windows1252, Utf16(Option<Endian>), Utf32(Option<Endian>) }

/* what a codec does with what it cannot convert */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
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
pub(crate) struct Iters {
    pub(crate) list: &'static str,
    pub(crate) list_back: &'static str,
    pub(crate) range: &'static str,
    pub(crate) tuple: &'static str,
    pub(crate) text: &'static str,
    pub(crate) ascii: &'static str,
    pub(crate) bytes: &'static str,
    pub(crate) buffer: &'static str,
    pub(crate) set: &'static str,
    pub(crate) keys: &'static str,
    pub(crate) values: &'static str,
    pub(crate) items: &'static str,
    pub(crate) keys_back: &'static str,
    pub(crate) calls: &'static str,
    pub(crate) numbered: &'static str,
    pub(crate) zipped: &'static str,
    pub(crate) mapped: &'static str,
    pub(crate) filtered: &'static str,
    pub(crate) backward: &'static str,
}

impl Iters {
    /* each role's name at its place in `names` where a snapshot also finds it */
    pub(crate) const fn of(names: &Names) -> Iters {
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
    #[cfg_attr(not(feature = "unstable-internals"), allow(dead_code))]
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
    pub const fn of<L: Lang>() -> Tree { Tree::new(L::NAMES.kinds[KindId::Instance as usize], <L as Derived>::ALSO, L::NAMES.tree.pairs) }

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
        assert!(crate::engine::lang::Tree::rooted(&t.up, classes.len()), "a class tree holds a cycle");
        t
    }

    /* every class beside its parent shortest name first */
    #[cfg(feature = "unstable-internals")]
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
    #[cfg(feature = "unstable-internals")]
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
