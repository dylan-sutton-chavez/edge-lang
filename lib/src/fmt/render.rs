use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use edge::lang::{Args, Cx, Mode, Ret, Sig};
use edge::value::{HeapObj, HeapPool, Val, VmErr};

use crate::{Kernel, Lang, Site, uni};

// containers nested past this many levels show as the depth mark
const DEPTH: usize = 100;
// a container whose text passes this many bytes ends its items with the mark
const BREADTH: usize = 1_000_000;

/* how a float prints with fixed form from `low` to `high` digits before the point */
#[derive(Clone, Copy)]
pub struct Floats {
    pub nan: &'static str,
    pub inf: &'static str,
    pub neg_inf: &'static str,
    pub low: i32,
    pub high: i32,
    // an exponent of zero or more takes a plus sign
    pub plus: bool,
    // the fewest digits an exponent shows
    pub width: u8,
    // a whole float in fixed form ends with a point and a zero
    pub point: bool,
}

/* how a character a repr cannot print is escaped */
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    // two four or eight hex digits after x u or U
    Fixed,
    // its hex digits in braces after u
    Braced,
}

/* how a runtime shows each kind of value with defaults as rust writes them */
pub trait Style {
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
    const EMPTY_SET: &'static str = "{}";
    // the close of a tuple of one item
    const ONE: &'static str = ",)";
    const FLOATS: Floats = Floats { nan: "NaN", inf: "inf", neg_inf: "-inf", low: -4, high: 16, plus: false, width: 1, point: true };
    // what the open bracket of a container holding itself shows past it
    const AGAIN: &'static str = "..";

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

    /* what a bound method puts before its name before its receiver and after it */
    const BOUND: (&'static str, &'static str, &'static str) = ("fn ", " of ", "");

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

    /* what a slice puts before its bounds between them and after them */
    const SLICE: (&'static str, &'static str, &'static str) = ("(", "; ", ")");

    /* what an exception's repr puts around its arguments after its class */
    const RAISED: (&'static str, &'static str) = ("(", ")");

    /* what a numbered system error puts before its number after it and before its file */
    const NUMBERED: (&'static str, &'static str, &'static str) = ("[", "] ", ": ");

    /* whether an exception of class `name` shows its lone argument as its repr */
    fn keyed(_name: &str) -> bool { false }

    /* whether class `name` shows two or three args as a numbered system error */
    fn numbered(_name: &str) -> bool { false }
}

/* the pieces of `parts` one after another */
fn push(out: &mut String, parts: &[&str]) { for p in parts { out.push_str(p); } }

/* a style's mark pushed as a char when it is one byte */
#[inline(always)]
fn mark(out: &mut String, m: &str) { match m.as_bytes() { &[b] if b < 0x80 => out.push(char::from(b)), _ => out.push_str(m) } }

/* `n` as hex digits after `0x` */
pub fn hex(out: &mut String, n: u64) { let _ = write!(out, "0x{n:x}"); }

/* the display text of `v` charged for its length */
pub fn display<L: Lang>(cx: &mut Cx<'_, '_>, v: Val) -> Result<String, VmErr> {
    // inline values lone texts and foreign values get text made at its exact size
    if let Some(t) = inline_text::<L>(v).or_else(|| cx.text(v).map(String::from)).or_else(|| lone_text::<L>(cx, v)).map(Ok).or_else(|| kind_shown::<L>(cx, v)) {
        let t = t?;
        cx.charge(t.len())?;
        return Ok(t);
    }
    let mut out = String::new();
    display_into::<L>(cx, v, &mut out)?;
    Ok(out)
}

/* the text of a value off the heap made at its exact size */
fn inline_text<L: Lang>(v: Val) -> Option<String> {
    if v.is_int() { return Some(String::from(itoa::Buffer::new().format(v.as_int()))); }
    if v.is_float() { return Some(format_f64::<L>(v.as_float())); }
    if v.is_bool() { return Some(String::from(if v.as_bool() { L::TRUE } else { L::FALSE })); }
    if v.is_none() { return Some(String::from(L::NAMES.nil)); }
    None
}

/* appends the display text of `v` to `out` charged by length */
pub fn display_into<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, out: &mut String) -> Result<(), VmErr> {
    let start = out.len();
    if v.is_int() { out.push_str(itoa::Buffer::new().format(v.as_int())); }
    else if v.is_bool() { out.push_str(if v.as_bool() { L::TRUE } else { L::FALSE }); }
    else if v.is_none() { out.push_str(L::NAMES.nil); }
    else if v.is_float() { push_f64::<L>(out, v.as_float()); }
    else if let Some(s) = cx.text(v) { out.push_str(s); }
    else if container(cx, v) { walk::<L>(cx, out, v); }
    else {
        match kind_shown::<L>(cx, v) { Some(r) => out.push_str(&r?), None => push_text::<L>(cx, out, v, &mut Vec::new()) }
    }
    cx.charge(out.len() - start)
}

/* the text of an exception made with one text or inline value at its size */
fn lone_text<L: Lang>(cx: &Cx<'_, '_>, v: Val) -> Option<String> {
    let (name, args) = cx.heap().error_parts(v)?;
    let &[a] = args else { return None };
    if let Some(t) = inline_text::<L>(a) { return Some(t); }
    let s = cx.text(a)?;
    if !L::keyed(name) { return Some(String::from(s)); }
    let mut out = String::with_capacity(s.len() + 2);
    push_repr_str::<L>(&mut out, s);
    Some(out)
}

/* the text of `v` that reads back as it charged for its length */
pub fn repr<L: Lang>(cx: &mut Cx<'_, '_>, v: Val) -> Result<String, VmErr> {
    // an inline value is made at its size and a text at its quoted size
    let t = match (inline_text::<L>(v), cx.text(v)) {
        (Some(t), _) => t,
        (None, Some(s)) => { let mut out = String::with_capacity(s.len() + 2); push_repr_str::<L>(&mut out, s); out }
        (None, None) => match kind_text::<L>(cx, v) {
            Some(t) => t?,
            None => { let mut out = String::new(); repr_into::<L>(cx, v, &mut out)?; return Ok(out); }
        },
    };
    cx.charge(t.len())?;
    Ok(t)
}

/* appends the text of `v` that reads back as it to `out` charged by length */
pub fn repr_into<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, out: &mut String) -> Result<(), VmErr> {
    let start = out.len();
    match kind_text::<L>(cx, v) {
        Some(r) => out.push_str(&r?),
        None if container(cx, v) => walk::<L>(cx, out, v),
        None => push_repr::<L>(cx, out, v, &mut Vec::new()),
    }
    cx.charge(out.len() - start)
}

/* the repr with every character past ascii escaped */
pub fn ascii<L: Lang>(cx: &mut Cx<'_, '_>, v: Val) -> Result<String, VmErr> {
    let r = repr::<L>(cx, v)?;
    Ok(if r.is_ascii() { r } else { ascii_escape::<L>(&r) })
}

/* the render hook a runtime registers */
pub fn hook<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> {
    match mode {
        Mode::Str => display::<L>(cx, v),
        Mode::Repr => repr::<L>(cx, v),
        Mode::Ascii => ascii::<L>(cx, v),
    }
}

/* `v` in `mode` onto `out` which is lib's render for every runtime that keeps it */
pub fn show_into<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, mode: Mode, out: &mut String) -> Result<(), VmErr> {
    match mode {
        Mode::Str => display_into::<L>(cx, v, out),
        Mode::Repr => repr_into::<L>(cx, v, out),
        Mode::Ascii => { let t = ascii::<L>(cx, v)?; out.push_str(&t); Ok(()) }
    }
}

/* the shortest text reading back as `f` in the runtime's float forms */
pub fn format_f64<S: Style>(f: f64) -> String {
    if let Some(w) = f64_word::<S>(f) { return String::from(w); }
    let mut sci = Sci::new();
    let d = f64_digits(f, &mut sci);
    let mut out = String::with_capacity(d.len::<S>());
    d.push::<S>(&mut out);
    out
}

/* `text` with every character past ascii as an escape */
pub fn ascii_escape<S: Style>(text: &str) -> String {
    // ascii text is its own escape
    if text.is_ascii() { return String::from(text); }
    let mut out = String::with_capacity(text.len());
    let mut run = 0;
    for (i, c) in text.char_indices() {
        if c.is_ascii() { continue; }
        out.push_str(&text[run..i]);
        run = i + c.len_utf8();
        push_escape::<S>(&mut out, c as u32);
    }
    out.push_str(&text[run..]);
    out
}

fn container(cx: &Cx<'_, '_>, v: Val) -> bool {
    matches!(cx.heap().try_get(v), Some(HeapObj::List(_) | HeapObj::Tuple(_) | HeapObj::Dict(_) | HeapObj::Set(_)))
}

pub(crate) fn push_f64<S: Style>(out: &mut String, f: f64) {
    if let Some(w) = f64_word::<S>(f) { return out.push_str(w); }
    let mut sci = Sci::new();
    f64_digits(f, &mut sci).push::<S>(out);
}

/* the text of a nan or infinity or zero float which has no digits */
fn f64_word<S: Style>(f: f64) -> Option<&'static str> {
    let fl = S::FLOATS;
    if f.is_nan() { return Some(fl.nan); }
    if f.is_infinite() { return Some(if f > 0.0 { fl.inf } else { fl.neg_inf }); }
    (f == 0.0).then_some(match (f.is_sign_negative(), fl.point) { (true, true) => "-0.0", (false, true) => "0.0", (true, false) => "-0", (false, false) => "0" })
}

// a finite nonzero float's first digit and the rest with where its point goes
struct Digits<'a> { neg: bool, first: &'a str, rest: &'a str, point: i32 }

/* the shortest round trip digits of a finite nonzero `f` from its exponent form */
fn f64_digits(f: f64, sci: &mut Sci) -> Digits<'_> {
    let _ = write!(sci, "{:e}", f.abs());
    even_tie(f.abs(), sci);
    let text = core::str::from_utf8(&sci.buf[..sci.len]).unwrap_or("0e0");
    let (mant, power) = text.split_once('e').unwrap_or((text, "0"));
    let power = match power.strip_prefix('-') { Some(d) => -digits_value(d), None => digits_value(power) };
    Digits { neg: f < 0.0, first: mant.get(..1).unwrap_or("0"), rest: mant.get(2..).unwrap_or(""), point: power + 1 }
}

/* the even of two equally near shortest digit runs of `a` written into `sci` */
fn even_tie(a: f64, sci: &mut Sci) {
    let bits = a.to_bits();
    let (biased, frac) = ((bits >> 52) as i32 & 0x7ff, bits & ((1 << 52) - 1));
    let (m, e) = if biased == 0 { (frac, -1074) } else { (frac | 1 << 52, biased - 1075) };
    let fractional = -(e + m.trailing_zeros() as i32);
    // a tie needs exact digits of 17 or 18 which few fractional bits give
    if !(1..=25).contains(&fractional) { return; }
    // the odd part times the fraction's fives must reach the bits 10^16 takes
    let odd = m >> m.trailing_zeros();
    if (64 - odd.leading_zeros() as i32) * 1000 + fractional * 2322 < 53_000 { return; }
    let exact = u128::from(odd) * 5u128.pow(fractional as u32);
    if !(10u128.pow(16)..10u128.pow(18)).contains(&exact) { return; }
    let text = core::str::from_utf8(&sci.buf[..sci.len]).unwrap_or("0e0");
    let (mant, power) = text.split_once('e').unwrap_or((text, "0"));
    let n = mant.len() - usize::from(mant.len() > 1);
    let point = match power.strip_prefix('-') { Some(d) => -digits_value(d), None => digits_value(power) } + 1;
    // the exact value ends one decimal past the shortest digits in a 5
    if fractional != n as i32 + 1 - point { return; }
    let mut low: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(20);
    let mut wide = exact;
    while wide > 0 { low.push(b'0' + (wide % 10) as u8); wide /= 10; }
    low.reverse();
    low.pop();
    // the upper run is the lower one up by one in its last place
    let mut high = low.clone();
    let Some(last) = high.iter().rposition(|&d| d != b'9') else { return };
    high[last] += 1;
    high[last + 1..].fill(b'0');
    let even = if low.last().is_some_and(|d| d % 2 == 0) { low } else { high };
    let (head, tail) = even.split_at(1);
    let tail = core::str::from_utf8(tail).unwrap_or("").trim_end_matches('0');
    let mut out = Sci::new();
    let _ = out.write_str(core::str::from_utf8(head).unwrap_or("0"));
    if !tail.is_empty() { let _ = write!(out, ".{tail}"); }
    let _ = write!(out, "e{power}");
    let again = core::str::from_utf8(&out.buf[..out.len]).unwrap_or("");
    if again != text && again.parse::<f64>().ok() == Some(a) { *sci = out; }
}

impl Digits<'_> {
    /* how many bytes the text takes so its string is made at its size */
    fn len<S: Style>(&self) -> usize {
        let fl = S::FLOATS;
        let (n, p) = (1 + self.rest.len() as i32, self.point);
        let body = if (fl.low..=0).contains(&p) { 2 + p.abs() + n }
            else if (n..=fl.high).contains(&p) { p + if fl.point { 2 } else { 0 } }
            else if p < n && (1..=fl.high).contains(&p) { n + 1 }
            else {
                let e = p - 1;
                let digits = if e.unsigned_abs() < 10 { 1 } else if e.unsigned_abs() < 100 { 2 } else { 3 };
                n + i32::from(!self.rest.is_empty()) + 1 + i32::from(e < 0 || fl.plus) + digits.max(fl.width as i32)
            };
        body as usize + usize::from(self.neg)
    }

    /* the text with fixed form between the style's exponents and exponent form past them */
    fn push<S: Style>(&self, out: &mut String) {
        let fl = S::FLOATS;
        let (first, rest, point) = (self.first, self.rest, self.point);
        let n = 1 + rest.len() as i32;
        if self.neg { out.push('-'); }
        if (fl.low..=fl.high).contains(&point) {
            if point <= 0 {
                out.push_str("0.");
                out.push_str(&ZEROS[..point.unsigned_abs() as usize]);
                out.push_str(first);
                out.push_str(rest);
            } else if point >= n {
                out.push_str(first);
                out.push_str(rest);
                out.push_str(&ZEROS[..(point - n) as usize]);
                if fl.point { out.push_str(".0"); }
            } else {
                let (head, tail) = rest.split_at(point as usize - 1);
                out.push_str(first);
                out.push_str(head);
                out.push('.');
                out.push_str(tail);
            }
        } else {
            out.push_str(first);
            if !rest.is_empty() { out.push('.'); out.push_str(rest); }
            let e = point - 1;
            out.push_str(if e < 0 { "e-" } else if fl.plus { "e+" } else { "e" });
            let digits = if e.unsigned_abs() < 10 { 1 } else if e.unsigned_abs() < 100 { 2 } else { 3 };
            for _ in digits..fl.width as u32 { out.push('0'); }
            out.push_str(itoa::Buffer::new().format(e.unsigned_abs()));
        }
    }
}

// the zeros a fixed form pads with which never pass the style's exponents
const ZEROS: &str = "00000000000000000000000000000000";

// a stack buffer the exponent form writes into so no allocation is made
struct Sci { buf: [u8; 32], len: usize }

impl Sci {
    const fn new() -> Self { Self { buf: [0; 32], len: 0 } }
}

impl Write for Sci {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        self.buf.get_mut(self.len..end).ok_or(core::fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn digits_value(d: &str) -> i32 { d.bytes().fold(0i32, |n, b| n.wrapping_mul(10).wrapping_add(i32::from(b.wrapping_sub(b'0')))) }

/* `n` as `width` lowercase hex digits */
fn push_hex(out: &mut String, n: u32, width: u32) {
    for k in (0..width).rev() { out.push(char::from(b"0123456789abcdef"[(n >> (4 * k) & 0xF) as usize])); }
}

/* the bytes repr of `buf` keeping printable ascii where a quote may switch */
pub fn push_bytes<S: Style>(out: &mut String, buf: &[u8], escape_quote: bool) {
    let quote = match S::SWITCH { Some(alt) if buf.contains(&(S::QUOTE as u8)) && !buf.contains(&(alt as u8)) => alt as u8, _ => S::QUOTE as u8 };
    out.reserve(buf.len() + 3);
    out.push_str(S::BYTES);
    out.push(char::from(quote));
    for &b in buf {
        match b {
            b'\\' => out.push_str("\\\\"),
            _ if b == S::QUOTE as u8 && (quote == b || escape_quote) => { out.push('\\'); out.push(char::from(b)); }
            _ if b == quote => { out.push('\\'); out.push(char::from(b)); }
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7E => out.push(char::from(b)),
            _ => { out.push_str("\\x"); push_hex(out, u32::from(b), 2); }
        }
    }
    out.push(char::from(quote));
}

/* the quoted repr of `s` escaping what does not print and copying printable runs whole */
pub fn push_repr_str<S: Style>(out: &mut String, s: &str) {
    let quote = match S::SWITCH { Some(alt) if s.contains(S::QUOTE) && !s.contains(alt) => alt, _ => S::QUOTE };
    out.reserve(s.len() + 2);
    out.push(quote);
    let (bytes, q) = (s.as_bytes(), quote as u8);
    let plain = |b: u8| (b' '..=b'~').contains(&b) && b != b'\\' && b != q;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if plain(b) || b >= 0x80 {
            // a run of printable characters copies whole
            let mut end = s.len();
            for (k, c) in s[i..].char_indices() {
                if !(if c.is_ascii() { plain(c as u8) } else { uni::is_printable(c) }) { end = i + k; break; }
            }
            if end > i { out.push_str(&s[i..end]); i = end; continue; }
            let c = s[i..].chars().next().unwrap_or(char::REPLACEMENT_CHARACTER);
            i += c.len_utf8();
            push_escape::<S>(out, c as u32);
            continue;
        }
        i += 1;
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            _ if b == q => { out.push('\\'); out.push(quote); }
            _ => push_escape::<S>(out, u32::from(b)),
        }
    }
    out.push(quote);
}

/* a code point a repr cannot print as the style's escape */
fn push_escape<S: Style>(out: &mut String, n: u32) {
    match (S::ESCAPE, n) {
        (Escape::Fixed, 0..=0xff) => { out.push_str("\\x"); push_hex(out, n, 2); }
        (Escape::Fixed, 0x100..=0xffff) => { out.push_str("\\u"); push_hex(out, n, 4); }
        (Escape::Fixed, _) => { out.push_str("\\U"); push_hex(out, n, 8); }
        (Escape::Braced, _) => { let _ = write!(out, "\\u{{{n:x}}}"); }
    }
}

/* the text of a value off the heap which repr and str show alike */
fn push_scalar<L: Lang>(cx: &Cx<'_, '_>, out: &mut String, v: Val) {
    if v.is_int() { out.push_str(itoa::Buffer::new().format(v.as_int())); }
    else if v.is_float() { push_f64::<L>(out, v.as_float()); }
    else if v.is_bool() { out.push_str(if v.as_bool() { L::TRUE } else { L::FALSE }); }
    else if v.is_none() { out.push_str(L::NAMES.nil); }
    else if v.is_foreign() { foreign_text::<L>(cx, out, v); }
    else { out.push_str("<undefined>"); }
}

/* a foreign value's own text or its type where the kind refuses */
fn foreign_text<L: Lang>(cx: &Cx<'_, '_>, out: &mut String, v: Val) {
    // the ellipsis shows its name with no text made for it
    if let (Some(k), Some(e)) = (cx.heap().foreign_kind(v), L::BUILT.ellipsis) && core::ptr::eq(k, e) { return out.push_str(L::NAMES.ellipsis); }
    match kind_text::<L>(cx, v) { Some(Ok(s)) => out.push_str(&s), _ => L::object_form(out, cx.type_name(v), v.raw()) }
}

#[derive(Clone, Copy, PartialEq)]
enum Shape { List, Tuple, Set, Dict }

// a container the walk has open with its items and where its text starts
struct Level { items: Vec<Val>, at: usize, shape: Shape, start: usize }

/* the repr of container `v` onto `out` walked without recursion so depth needs no stack */
fn walk<L: Lang>(cx: &mut Cx<'_, '_>, out: &mut String, v: Val) {
    let (mut levels, mut seen) = (Vec::new(), Vec::new());
    open::<L>(cx, out, &mut levels, &mut seen, v);
    while let Some(level) = levels.last_mut() {
        let (i, shape, n) = (level.at, level.shape, level.items.len());
        let checks = i > 0 && i < n && match shape { Shape::Dict => i % 2 == 0, Shape::Tuple => n != 1, _ => true };
        if checks && out.len() - level.start > BREADTH { out.push_str(", "); out.push_str(L::AGAIN); level.at = n; }
        if level.at >= n {
            out.push_str(match shape { Shape::List => "]", Shape::Tuple if n == 1 => L::ONE, Shape::Tuple => ")", Shape::Set | Shape::Dict => "}" });
            levels.pop();
            seen.pop();
            continue;
        }
        if i > 0 { out.push_str(if shape == Shape::Dict && i % 2 == 1 { ": " } else { ", " }); }
        let x = level.items[i];
        level.at += 1;
        // a slice off the heap still shows its bounds through the walk
        if !x.is_heap() && cx.heap().slice_of(x).is_none() { push_scalar::<L>(cx, out, x); continue; }
        let mark = match cx.heap().try_get(x) {
            Some(HeapObj::Str(s)) => { push_repr_str::<L>(out, s); continue; }
            Some(HeapObj::List(_)) => ("[", "]"),
            Some(HeapObj::Tuple(_)) => ("(", ")"),
            Some(HeapObj::Dict(_) | HeapObj::Set(_)) => ("{", "}"),
            _ => { push_repr::<L>(cx, out, x, &mut seen); continue; }
        };
        if seen.contains(&x.as_heap()) { push(out, &[mark.0, L::AGAIN, mark.1]); }
        else if seen.len() > DEPTH { out.push_str(L::AGAIN); }
        else if !flat::<L>(cx, out, x) { open::<L>(cx, out, &mut levels, &mut seen, x); }
    }
}

/* opens container `o` on `out` with an empty set written whole */
fn open<L: Lang>(cx: &Cx<'_, '_>, out: &mut String, levels: &mut Vec<Level>, seen: &mut Vec<u32>, o: Val) {
    let (items, shape) = match cx.heap().try_get(o) {
        Some(HeapObj::List(rc)) => (rc.borrow().clone(), Shape::List),
        Some(HeapObj::Tuple(t)) => (t.clone(), Shape::Tuple),
        Some(HeapObj::Set(s)) => (s.borrow().iter().copied().collect(), Shape::Set),
        Some(HeapObj::Dict(d)) => (d.borrow().iter().flat_map(|(k, v)| [k, v]).collect(), Shape::Dict),
        _ => return,
    };
    let start = out.len();
    if shape == Shape::Set && items.is_empty() { return out.push_str(L::EMPTY_SET); }
    out.push_str(match shape { Shape::List => "[", Shape::Tuple => "(", Shape::Set | Shape::Dict => "{" });
    seen.push(o.as_heap());
    levels.push(Level { items, at: 0, shape, start });
}

/* writes list or tuple `o` whole when it holds no heap value */
fn flat<L: Lang>(cx: &Cx<'_, '_>, out: &mut String, o: Val) -> bool {
    match cx.heap().try_get(o) {
        Some(HeapObj::List(rc)) => flat_items::<L>(cx, out, &rc.borrow(), false),
        Some(HeapObj::Tuple(t)) => flat_items::<L>(cx, out, t, true),
        _ => false,
    }
}

fn flat_items<L: Lang>(cx: &Cx<'_, '_>, out: &mut String, items: &[Val], tuple: bool) -> bool {
    if items.iter().any(Val::is_heap) { return false; }
    let start = out.len();
    out.push(if tuple { '(' } else { '[' });
    for (i, &x) in items.iter().enumerate() {
        if i > 0 {
            if !(tuple && items.len() == 1) && out.len() - start > BREADTH { out.push_str(", "); out.push_str(L::AGAIN); break; }
            out.push_str(", ");
        }
        // an inline int writes in place where a flat container holds mostly ints
        if x.is_int() { out.push_str(itoa::Buffer::new().format(x.as_int())); }
        // a slice leaves for the walk that shows its bounds
        else if cx.heap().slice_of(x).is_some() { out.truncate(start); return false; }
        else { push_scalar::<L>(cx, out, x); }
    }
    out.push_str(if !tuple { "]" } else if items.len() == 1 { L::ONE } else { ")" });
    true
}

/* item `i` of a list tuple or exception read afresh so no borrow lingers */
fn item(cx: &Cx<'_, '_>, v: Val, i: usize) -> Option<Val> {
    match cx.heap().try_get(v)? {
        HeapObj::List(rc) => rc.borrow().get(i).copied(),
        HeapObj::Tuple(t) => t.get(i).copied(),
        _ => None,
    }
}

/* reprs of the items `at` gives cut once text past `start` passes the breadth */
fn reprs<L: Lang>(cx: &mut Cx<'_, '_>, out: &mut String, start: usize, seen: &mut Vec<u32>, at: impl Fn(&Cx<'_, '_>, usize) -> Option<Val>) {
    let mut i = 0;
    while let Some(x) = at(cx, i) {
        if out.len() - start > BREADTH { out.push_str(", "); out.push_str(L::AGAIN); break; }
        if i > 0 { out.push_str(", "); }
        push_repr::<L>(cx, out, x, seen);
        i += 1;
    }
}

/* the repr of `v` onto `out` with `seen` the containers open around it */
fn push_repr<L: Lang>(cx: &mut Cx<'_, '_>, out: &mut String, v: Val, seen: &mut Vec<u32>) {
    match cx.heap().try_get(v) {
        Some(HeapObj::Str(s)) => return push_repr_str::<L>(out, s),
        Some(&HeapObj::Object { meta: cls, .. }) if raised_base::<L>(cx, cls).is_some() => {
            // an error cuts only where it holds itself so a container cuts its own cycle
            if seen.len() > DEPTH || seen.last() == Some(&v.as_heap()) { return push(out, &[shown(cx, v, cls), L::RAISED.0, L::AGAIN, L::RAISED.1]); }
            let (t, held) = exc_tuple(cx, v);
            if held { seen.push(v.as_heap()); }
            let start = out.len();
            out.push_str(shown(cx, v, cls));
            mark(out, L::RAISED.0);
            reprs::<L>(cx, out, start, seen, |cx, i| item(cx, t, i));
            mark(out, L::RAISED.1);
            if held { seen.pop(); }
            return;
        }
        _ => {}
    }
    push_text::<L>(cx, out, v, seen)
}

/* the str of `v` onto `out` with `seen` the containers open around it */
fn push_text<L: Lang>(cx: &mut Cx<'_, '_>, out: &mut String, v: Val, seen: &mut Vec<u32>) {
    if seen.len() > DEPTH { return out.push_str(L::AGAIN); }
    if !v.is_heap() {
        let Some([a, b, c]) = cx.heap().slice_of(v) else { return push_scalar::<L>(cx, out, v) };
        // each bound shows as its repr so a text bound keeps its quotes
        out.push_str(L::SLICE.0);
        push_repr::<L>(cx, out, a, seen);
        out.push_str(L::SLICE.1);
        push_repr::<L>(cx, out, b, seen);
        out.push_str(L::SLICE.1);
        push_repr::<L>(cx, out, c, seen);
        return out.push_str(L::SLICE.2);
    }
    let Some(obj) = cx.heap().try_get(v) else { return out.push_str("<undefined>") };
    match obj {
        HeapObj::Str(s) => out.push_str(s),
        HeapObj::Bytes(b) => push_bytes::<L>(out, b, false),
        HeapObj::ByteArray(b) => { out.push_str(L::BUFFER.0); push_bytes::<L>(out, b, true); out.push_str(L::BUFFER.1); }
        &HeapObj::Range(a, b, c) => {
            let (mut ia, mut ib, mut ic) = (itoa::Buffer::new(), itoa::Buffer::new(), itoa::Buffer::new());
            let (a, b) = (ia.format(a), ib.format(b));
            L::range_form(out, a, b, (c != 1).then(|| ic.format(c)));
        }
        &HeapObj::BigRange(a, b, c) => wide_range::<L>(cx, out, a, b, c),
        HeapObj::List(_) => seq::<L>(cx, out, v, false, seen),
        HeapObj::Tuple(_) => seq::<L>(cx, out, v, true, seen),
        HeapObj::Dict(d) => {
            let pairs: Vec<(Val, Val)> = d.borrow().iter().collect();
            dict_text::<L>(cx, out, v, &pairs, seen);
        }
        HeapObj::Set(st) => {
            let items: Vec<Val> = st.borrow().iter().copied().collect();
            if items.is_empty() { return out.push_str(L::EMPTY_SET); }
            let start = out.len();
            out.push('{');
            if seen.contains(&v.as_heap()) { out.push_str(L::AGAIN); } else { seen.push(v.as_heap()); reprs::<L>(cx, out, start, seen, |_, i| items.get(i).copied()); seen.pop(); }
            out.push('}');
        }
        &HeapObj::Native(id) => match cx.natives().native(id) {
            Some(n) => match n.owner { None => L::native_form(out, n.name), Some(k) => L::method_form(out, n.name, cx.names().kind(k)) },
            None => L::extern_form(out, cx.heap().host_at(id).map_or("", |f| f.name.as_str())),
        },
        &HeapObj::Bound { recv, func } if !func.is_heap() => L::bound_native_form(out, func.as_native_ref().and_then(|id| cx.natives().native(id)).map_or("", |n| n.name), cx.type_name(recv), recv.raw()),
        HeapObj::Iter(_, name) => L::object_form(out, name, v.raw()),
        HeapObj::Module(name, _) => L::module_form(out, name),
        &HeapObj::Cell(c) if c.is_undef() => L::cell_form(out, v.raw(), None),
        &HeapObj::Cell(c) => L::cell_form(out, v.raw(), Some((cx.type_name(c), c.raw()))),
        HeapObj::Func { .. } => L::function_form(out, cx.fn_name(v).unwrap_or(L::NAMES.anonymous), v.raw()),
        &HeapObj::Object { meta, .. } => match cx.heap().type_object_name(v) {
            Some(name) => L::type_form(out, name),
            None if cx.heap().is_class(v) => L::class_form(out, cx.fn_name(v).unwrap_or("")),
            None => instance_text::<L>(cx, out, v, meta, seen),
        },
        &HeapObj::Bound { recv, .. } => {
            out.push_str(L::BOUND.0);
            out.push_str(cx.fn_name(v).unwrap_or("?"));
            out.push_str(L::BOUND.1);
            push_repr::<L>(cx, out, recv, seen);
            out.push_str(L::BOUND.2);
        }
        HeapObj::Coroutine(_) => L::coroutine_form(out, cx.type_name(v), cx.fn_name(v).unwrap_or(L::NAMES.module_body), v.raw()),
    }
}

/* list or tuple `v` in brackets with the mark where it holds itself */
fn seq<L: Lang>(cx: &mut Cx<'_, '_>, out: &mut String, v: Val, tuple: bool, seen: &mut Vec<u32>) {
    let id = v.as_heap();
    if seen.contains(&id) { return push(out, &[if tuple { "(" } else { "[" }, L::AGAIN, if tuple { ")" } else { "]" }]); }
    seen.push(id);
    let one = if tuple { item(cx, v, 0).filter(|_| item(cx, v, 1).is_none()) } else { None };
    if let Some(x) = one {
        out.push('(');
        push_repr::<L>(cx, out, x, seen);
        out.push_str(L::ONE);
    } else {
        let start = out.len();
        out.push(if tuple { '(' } else { '[' });
        reprs::<L>(cx, out, start, seen, |cx, i| item(cx, v, i));
        out.push(if tuple { ')' } else { ']' });
    }
    seen.pop();
}

/* the entries of map `v` in braces with the mark where it holds itself */
fn dict_text<L: Lang>(cx: &mut Cx<'_, '_>, out: &mut String, v: Val, pairs: &[(Val, Val)], seen: &mut Vec<u32>) {
    let id = v.as_heap();
    if seen.contains(&id) { return push(out, &["{", L::AGAIN, "}"]); }
    seen.push(id);
    let start = out.len();
    out.push('{');
    for (i, &(k, x)) in pairs.iter().enumerate() {
        if i > 0 {
            if out.len() - start > BREADTH { out.push_str(", "); out.push_str(L::AGAIN); break; }
            out.push_str(", ");
        }
        push_repr::<L>(cx, out, k, seen);
        out.push_str(": ");
        push_repr::<L>(cx, out, x, seen);
    }
    out.push('}');
    seen.pop();
}

/* a range past a word by its digits or the mark past the digit limit */
fn wide_range<L: Lang>(cx: &Cx<'_, '_>, out: &mut String, a: Val, b: Val, c: Val) {
    let digits = |x: Val| match cx.int(x) {
        Some(i) => Some(String::from(itoa::Buffer::new().format(i))),
        None => kind_text::<L>(cx, x).and_then(Result::ok),
    };
    let step = if cx.int(c) == Some(1) { Some(None) } else { digits(c).map(Some) };
    match (digits(a), digits(b), step) {
        (Some(a), Some(b), Some(st)) => L::range_form(out, &a, &b, st.as_deref()),
        _ => L::range_form(out, L::AGAIN, L::AGAIN, None),
    }
}

/* a user class instance as its exception text or by its class and place */
fn instance_text<L: Lang>(cx: &mut Cx<'_, '_>, out: &mut String, v: Val, cls: Val, seen: &mut Vec<u32>) {
    if let Some(base) = raised_base::<L>(cx, cls) {
        let (t, held) = exc_tuple(cx, v);
        let n = tuple_of(cx.heap(), t).map_or(0, <[Val]>::len);
        let (key, numbered) = (L::keyed(base), (n == 2 || n == 3) && L::numbered(base));
        if seen.len() > DEPTH || seen.last() == Some(&v.as_heap()) { return push(out, &[shown(cx, v, cls), L::RAISED.0, L::AGAIN, L::RAISED.1]); }
        if held { seen.push(v.as_heap()); }
        exc_text::<L>(cx, out, t, n, key, numbered, seen);
        if held { seen.pop(); }
        return;
    }
    if cx.heap().is_class(cls) { return L::instance_form(out, cx.fn_name(cls).unwrap_or(""), v.raw()); }
    L::object_form(out, cx.heap().type_object_name(cls).unwrap_or(L::TREE.root), v.raw())
}

/* the tree class an instance of `cls` raises as or its engine error class */
fn raised_base<'c, L: Lang>(cx: &'c Cx<'_, '_>, cls: Val) -> Option<&'c str> {
    let heap = cx.heap();
    // a type object of the tree or of the engine's errors raises as itself
    if let Some(n) = heap.type_named(cls) { return (heap.is_exc_type(cls) || L::TREE.has(n) || L::NAMES.error_named(n).is_some()).then_some(n); }
    L::TREE.base_of(heap, cls)
}

/* the name an exception of `cls` shows where a builtin error shows its type's own */
fn shown<'c>(cx: &'c Cx<'_, '_>, v: Val, cls: Val) -> &'c str { cx.heap().type_named(cls).unwrap_or_else(|| cx.type_name(v)) }

/* the args tuple of exception `inst` and whether it holds a container a cycle passes */
fn exc_tuple(cx: &Cx<'_, '_>, inst: Val) -> (Val, bool) {
    let heap = cx.heap();
    let t = heap.error_tuple(inst).unwrap_or(Val::undef());
    let held = tuple_of(heap, t).is_some_and(|a| a.iter().any(|&x| x.is_heap() && !matches!(heap.try_get(x), Some(HeapObj::Str(_)))));
    (t, held)
}

/* the items of tuple `t` and none for any other value */
fn tuple_of(heap: &HeapPool, t: Val) -> Option<&[Val]> { match heap.try_get(t)? { HeapObj::Tuple(items) => Some(items), _ => None } }

/* an exception's text from its args tuple `t` of `n` items keying a lone one */
fn exc_text<L: Lang>(cx: &mut Cx<'_, '_>, out: &mut String, t: Val, n: usize, key: bool, numbered: bool, seen: &mut Vec<u32>) {
    let arg = |cx: &Cx<'_, '_>, i: usize| item(cx, t, i).unwrap_or(Val::none());
    match n {
        0 => {}
        1 if key => { let a = arg(cx, 0); push_repr::<L>(cx, out, a, seen); }
        1 => { let a = arg(cx, 0); push_text::<L>(cx, out, a, seen); }
        2 | 3 if numbered => {
            out.push_str(L::NUMBERED.0);
            let no = arg(cx, 0);
            push_text::<L>(cx, out, no, seen);
            out.push_str(L::NUMBERED.1);
            let msg = arg(cx, 1);
            push_text::<L>(cx, out, msg, seen);
            if n == 3 { out.push_str(L::NUMBERED.2); let file = arg(cx, 2); push_repr::<L>(cx, out, file, seen); }
        }
        _ => {
            let start = out.len();
            out.push('(');
            reprs::<L>(cx, out, start, seen, |cx, i| item(cx, t, i));
            out.push(')');
        }
    }
}

/* a foreign value's text in `mode` kept whole and charged for its length */
pub(crate) fn foreign_owned<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Option<Result<String, VmErr>> {
    if !v.is_foreign() { return None; }
    let text = if matches!(mode, Mode::Str) { kind_shown::<L>(cx, v) } else { kind_text::<L>(cx, v) }?;
    Some(text.and_then(|t| cx.charge(t.len()).map(|()| t)))
}

/* a foreign value's display text through its kind's own where it gives one */
fn kind_shown<L: Lang>(cx: &Cx<'_, '_>, v: Val) -> Option<Result<String, VmErr>> {
    if !v.is_foreign() { return None; }
    cx.heap().foreign_str(v).or_else(|| kind_text::<L>(cx, v))
}

/* a foreign value's text with a wide int's digit work charged first */
#[inline(never)]
pub(crate) fn kind_text<L: Lang>(cx: &Cx<'_, '_>, v: Val) -> Option<Result<String, VmErr>> {
    // a slice shows through the walk that reads its bounds as text
    if !v.is_foreign() || cx.heap().slice_of(v).is_some() { return None; }
    #[cfg(feature = "num")]
    if let Err(e) = crate::num::big::charge_text::<L>(cx, v) { return Some(Err(e)); }
    cx.heap().foreign_repr(v)
}

/* a text as itself and a value off the heap as its words */
pub fn text_fast<L: Lang>(heap: &mut HeapPool, x: Val) -> Option<Val> {
    if x.is_heap() { return matches!(heap.try_get(x), Some(HeapObj::Str(_))).then_some(x); }
    // the digits of an int are ascii so its text is made marked
    if x.is_int() { let mut buf = itoa::Buffer::new(); let t = buf.format(x.as_int()); return if t.len() == 1 { heap.intern_str(t).ok() } else { heap.alloc_ascii(String::from(t)).ok() }; }
    if x.is_float() { return heap.alloc(HeapObj::Str(format_f64::<L>(x.as_float()))).ok(); }
    if x.is_bool() { return text_val(heap, if x.as_bool() { L::TRUE } else { L::FALSE }); }
    if x.is_none() { return text_val(heap, L::NAMES.nil); }
    None
}

/* a new text of `text` where one byte shares the slot the heap interns */
pub fn text_val(heap: &mut HeapPool, text: &str) -> Option<Val> {
    if text.len() == 1 { return heap.intern_str(text).ok(); }
    heap.alloc(HeapObj::Str(String::from(text))).ok()
}

/* the text that reads back as the value */
pub const fn repr_of<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), repr_run::<S>).fast(repr_fast::<S::Lang>) }

fn repr_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let x = a.pos[0];
    if let Some(v) = repr_fast::<S::Lang>(cx.heap_mut(), x) { return Ok(v.into()); }
    if let Some(text) = foreign_owned::<S::Lang>(cx, x, Mode::Repr) { return Ok(cx.new_str(text?)?.into()); }
    // a text with nothing to escape takes its length and two quotes
    let mut out = String::with_capacity(cx.text(x).map_or(0, |s| s.len() + 2));
    <S::Lang as Lang>::show_into(cx, x, Mode::Repr, &mut out)?;
    Ok(cx.new_str(out)?.into())
}

/* the repr with every character past ascii escaped */
pub const fn ascii_of<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), ascii_run::<S>).fast(repr_fast::<S::Lang>) }

fn ascii_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    if let Some(v) = repr_fast::<S::Lang>(cx.heap_mut(), a.pos[0]) { return Ok(v.into()); }
    let mut out = String::new();
    <S::Lang as Lang>::show_into(cx, a.pos[0], Mode::Ascii, &mut out)?;
    Ok(cx.new_str(out)?.into())
}

/* the repr of a value off the heap which is its text charged by length */
fn repr_fast<L: Lang>(heap: &mut HeapPool, x: Val) -> Option<Val> {
    if x.is_heap() { return None; }
    let v = text_fast::<L>(heap, x)?;
    if let Some(HeapObj::Str(s)) = heap.try_get(v) { heap.spend(s.len()); }
    Some(v)
}
