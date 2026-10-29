use alloc::string::String;
use alloc::vec::Vec;

use edge::lang::{Args, Cx, Ret, Sig};
use edge::value::{DictMap, HeapObj, Val, VmErr};

use crate::fail::{Error, Fail, Json, ToJson, fail};
use crate::fmt::render::format_f64;
use crate::{Kernel, Lang, Site};

// containers nested past this many levels fail to read and to write
const DEPTH: usize = 512;

/* the value JSON text or utf-8 bytes spell */
pub const fn read<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), read_run::<S>) }

/* a value as compact JSON text */
pub const fn write<S: Site>() -> Kernel { Kernel::new(Sig::exact(1), write_run::<S>) }

fn read_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let src = match source(cx, a.pos[0]) { Ok(s) => s, Err(f) => return Err(fail::<S>(cx, f)) };
    cx.spend(src.len());
    let mut r = Reader { src: &src, at: 0 };
    match r.document::<S::Lang>(cx) { Ok(v) => Ok(v.into()), Err(e) => Err(e.raise::<S>(cx)) }
}

/* the value JSON text spells for the runtime `L` with its work charged */
pub fn parse<L: Lang>(cx: &mut Cx<'_, '_>, src: &str) -> Result<Val, Error> {
    cx.spend(src.len());
    Reader { src, at: 0 }.document::<L>(cx)
}

/* `v` as compact JSON onto `out` for the runtime `L` */
pub fn write_into<L: Lang>(cx: &Cx<'_, '_>, v: Val, out: &mut String) -> Result<(), Error> { written::<L>(cx, v, out) }

/* the text a source holds copied off the heap so reading may make values */
fn source(cx: &Cx<'_, '_>, v: Val) -> Result<String, Fail> {
    if let Some(s) = cx.text(v) { return Ok(String::from(s)); }
    let Some(b) = cx.bytes_of(v) else { return Err(Fail::TextOrBytes(v)) };
    match core::str::from_utf8(b) {
        Ok(s) => Ok(String::from(s)),
        Err(e) => Err(Fail::BadJson { at: e.valid_up_to(), why: Json::Utf8 }),
    }
}

/* the failure at byte `at` for the reason `why` */
fn bad(at: usize, why: Json) -> Error { Error::Fail(Fail::BadJson { at, why }) }

/* a container whose items are still being read */
enum Open {
    List(Vec<Val>),
    // the entries so far and the key the next value goes under
    Map(Vec<(Val, Val)>, Val),
}

/* a JSON document read once from its first byte */
struct Reader<'s> {
    src: &'s str,
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> { self.src.as_bytes().get(self.at).copied() }

    fn skip(&mut self) { while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) { self.at += 1; } }

    /* the one value the document holds with only white space around it */
    fn document<L: Lang>(&mut self, cx: &mut Cx<'_, '_>) -> Result<Val, Error> {
        // containers open on a stack of their own so no depth reaches the native stack
        let mut open: Vec<Open> = Vec::new();
        'value: loop {
            self.skip();
            let mut v = match self.peek() {
                Some(b'[' | b'{') if open.len() >= DEPTH => return Err(bad(self.at, Json::Deep)),
                Some(b'[') => {
                    self.at += 1;
                    self.skip();
                    if self.peek() != Some(b']') { open.push(Open::List(Vec::new())); continue 'value; }
                    self.at += 1;
                    cx.new_list(Vec::new())?
                }
                Some(b'{') => {
                    self.at += 1;
                    self.skip();
                    if self.peek() != Some(b'}') { let k = self.key(cx)?; open.push(Open::Map(Vec::new(), k)); continue 'value; }
                    self.at += 1;
                    cx.new_dict(DictMap::new())?
                }
                Some(b'"') => { let s = self.string()?; cx.new_str(s)? }
                Some(b't') => self.word("true", Val::bool(true))?,
                Some(b'f') => self.word("false", Val::bool(false))?,
                Some(b'n') => self.word("null", Val::none())?,
                Some(b'-' | b'0'..=b'9') => self.number::<L>(cx)?,
                Some(_) => return Err(bad(self.at, Json::Unexpected)),
                None => return Err(bad(self.at, Json::End)),
            };
            // a finished value closes each container it was the last item of
            loop {
                let Some(mut top) = open.pop() else {
                    self.skip();
                    if self.at < self.src.len() { return Err(bad(self.at, Json::Extra)); }
                    return Ok(v);
                };
                let close = match &mut top {
                    Open::List(items) => { items.push(v); b']' }
                    Open::Map(pairs, k) => { pairs.push((*k, v)); b'}' }
                };
                self.skip();
                match self.peek() {
                    Some(b',') => {
                        self.at += 1;
                        if let Open::Map(_, k) = &mut top { self.skip(); *k = self.key(cx)?; }
                        open.push(top);
                        continue 'value;
                    }
                    Some(c) if c == close => {
                        self.at += 1;
                        v = match top {
                            Open::List(items) => cx.new_list(items)?,
                            // a key given twice keeps its first place and its last value
                            Open::Map(pairs, _) => { let d = crate::map::table(cx.heap(), pairs.len(), pairs)?; cx.new_dict(d)? }
                        };
                    }
                    _ if close == b']' => return Err(bad(self.at, Json::CommaOrBracket)),
                    _ => return Err(bad(self.at, Json::CommaOrBrace)),
                }
            }
        }
    }

    /* the key at the reader as a new text with the colon after it read */
    fn key(&mut self, cx: &mut Cx<'_, '_>) -> Result<Val, Error> {
        if self.peek() != Some(b'"') { return Err(bad(self.at, Json::Key)); }
        let s = self.string()?;
        self.skip();
        if self.peek() != Some(b':') { return Err(bad(self.at, Json::Colon)); }
        self.at += 1;
        Ok(cx.new_str(s)?)
    }

    /* the value of the literal `lit` at the reader */
    fn word(&mut self, lit: &str, v: Val) -> Result<Val, Error> {
        if !self.src.as_bytes().get(self.at..).is_some_and(|r| r.starts_with(lit.as_bytes())) { return Err(bad(self.at, Json::Literal)); }
        self.at += lit.len();
        Ok(v)
    }

    /* the text of the string at the reader with every escape read */
    fn string(&mut self) -> Result<String, Error> {
        let src = self.src;
        let b = src.as_bytes();
        let start = self.at + 1;
        let mut i = start;
        // a string with no escape is one copy of its bytes
        loop {
            match b.get(i) {
                Some(b'"') => { self.at = i + 1; return Ok(String::from(src.get(start..i).unwrap_or(""))); }
                Some(b'\\') => break,
                Some(&c) if c < 0x20 => return Err(bad(i, Json::Control)),
                Some(_) => i += 1,
                None => return Err(bad(i, Json::Unterminated)),
            }
        }
        let mut out = String::from(src.get(start..i).unwrap_or(""));
        loop {
            match b.get(i) {
                Some(b'"') => { self.at = i + 1; return Ok(out); }
                Some(b'\\') => {
                    let c = match b.get(i + 1) {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => { let (c, len) = self.unicode(i)?; out.push(c); i += len; continue; }
                        _ => return Err(bad(i, Json::Escape)),
                    };
                    out.push(c);
                    i += 2;
                }
                Some(&c) if c < 0x20 => return Err(bad(i, Json::Control)),
                Some(_) => {
                    let run = i;
                    while b.get(i).is_some_and(|&c| c != b'"' && c != b'\\' && c >= 0x20) { i += 1; }
                    out.push_str(src.get(run..i).unwrap_or(""));
                }
                None => return Err(bad(i, Json::Unterminated)),
            }
        }
    }

    /* the character the unicode escape at byte `i` spells and the bytes it takes */
    fn unicode(&self, i: usize) -> Result<(char, usize), Error> {
        let unit = self.hex4(i + 2).ok_or_else(|| bad(i, Json::Unicode))?;
        let (code, len) = match unit {
            // a high surrogate takes the low one that must follow it
            0xD800..=0xDBFF => match (self.src.as_bytes().get(i + 6..i + 8), self.hex4(i + 8)) {
                (Some(b"\\u"), Some(low @ 0xDC00..=0xDFFF)) => (0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00), 12),
                _ => return Err(bad(i, Json::Surrogate)),
            },
            0xDC00..=0xDFFF => return Err(bad(i, Json::Surrogate)),
            u => (u, 6),
        };
        char::from_u32(code).map(|c| (c, len)).ok_or_else(|| bad(i, Json::Unicode))
    }

    /* the four hex digits at byte `at` as one code unit */
    fn hex4(&self, at: usize) -> Option<u32> {
        self.src.as_bytes().get(at..at + 4)?.iter().try_fold(0, |n, &c| Some(n * 16 + char::from(c).to_digit(16)?))
    }

    /* the number at the reader as an int when it has no fraction or exponent */
    fn number<L: Lang>(&mut self, cx: &mut Cx<'_, '_>) -> Result<Val, Error> {
        let (src, start) = (self.src.as_bytes(), self.at);
        let digit = |i: usize| src.get(i).is_some_and(u8::is_ascii_digit);
        let first = start + usize::from(src.get(start) == Some(&b'-'));
        let mut i = first;
        if !digit(i) { return Err(bad(i, Json::Number)); }
        while digit(i) { i += 1; }
        if src[first] == b'0' && i > first + 1 { return Err(bad(start, Json::LeadingZero)); }
        let whole = i;
        if src.get(i) == Some(&b'.') {
            i += 1;
            if !digit(i) { return Err(bad(i, Json::Number)); }
            while digit(i) { i += 1; }
        }
        if matches!(src.get(i), Some(b'e' | b'E')) {
            i += 1;
            if matches!(src.get(i), Some(b'+' | b'-')) { i += 1; }
            if !digit(i) { return Err(bad(i, Json::Number)); }
            while digit(i) { i += 1; }
        }
        self.at = i;
        if i > whole { return self.src.get(start..i).unwrap_or("").parse::<f64>().map(Val::float).map_err(|_| bad(start, Json::Number)); }
        int::<L>(cx, first > start, &src[first..whole])
    }
}

/* an int of decimal digits boxed by the runtime's wide kind past an i128 */
fn int<L: Lang>(cx: &mut Cx<'_, '_>, neg: bool, digits: &[u8]) -> Result<Val, Error> {
    // up to 38 digits always fit an i128
    if digits.len() <= 38 {
        let n = digits.iter().fold(0i128, |n, &d| n * 10 + i128::from(d - b'0'));
        return Ok(cx.new_int(if neg { -n } else { n })?);
    }
    if L::DIGITS.is_some_and(|limit| digits.len() > limit) { return Err(Error::Fail(Fail::Digits(Some(digits.len())))); }
    // a language with no wide kind holds no int this long
    let Some(make) = L::WIDE.and_then(|k| k.from_digits) else { return Err(Error::Fail(Fail::TooBig)) };
    // the conversion is quadratic in the digits as the int kind's own is
    let work = digits.len().saturating_mul(digits.len()) / 64;
    cx.heap().afford(work)?;
    cx.spend(work);
    let values: Vec<u8> = digits.iter().map(|d| d - b'0').collect();
    Ok(make(cx.heap_mut(), neg, &values, 10)?)
}

fn write_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mut out = String::new();
    if let Err(e) = written::<S::Lang>(cx, a.pos[0], &mut out) { return Err(e.raise::<S>(cx)); }
    Ok(cx.new_str(out)?.into())
}

/* the failure of writing `v` for the reason `why` */
fn not(v: Val, why: ToJson) -> Error { Error::Fail(Fail::NotJson { v, why }) }

/* what a container still writes after the items before `at` */
enum Rest {
    Items(Vec<Val>, usize),
    Entries(Vec<(Val, Val)>, usize),
}

/* a container's items or entries refused past the memory left and none for other values */
fn contents(cx: &Cx<'_, '_>, v: Val) -> Result<Option<Rest>, Error> {
    let each = core::mem::size_of::<Val>();
    Ok(match cx.heap().try_get(v) {
        Some(HeapObj::List(rc)) => { cx.reserve(rc.borrow().len().saturating_mul(each))?; Some(Rest::Items(rc.borrow().clone(), 0)) }
        Some(HeapObj::Tuple(items)) => { cx.reserve(items.len().saturating_mul(each))?; Some(Rest::Items(items.clone(), 0)) }
        Some(HeapObj::Dict(rc)) => { cx.reserve(rc.borrow().len().saturating_mul(2 * each))?; Some(Rest::Entries(rc.borrow().iter().collect(), 0)) }
        _ => None,
    })
}

/* `root` as JSON onto `out` paying for its walk whether it ends or fails */
fn written<L: Lang>(cx: &Cx<'_, '_>, root: Val, out: &mut String) -> Result<(), Error> {
    let mut read = 0;
    let done = walked::<L>(cx, root, out, &mut read);
    cx.spend(read.saturating_add(out.len()));
    done
}

/* `root` as JSON onto `out` keeping the open containers on a stack of their own */
fn walked<L: Lang>(cx: &Cx<'_, '_>, root: Val, out: &mut String, read: &mut usize) -> Result<(), Error> {
    let mut open: Vec<(Val, Rest)> = Vec::new();
    let mut next = Some(root);
    loop {
        if let Some(v) = next.take() {
            match contents(cx, v)? {
                Some(rest) => {
                    if open.len() >= DEPTH { return Err(not(v, ToJson::Deep)); }
                    if open.iter().any(|(c, _)| c.raw() == v.raw()) { return Err(not(v, ToJson::Cycle)); }
                    // a shared part is written each time it shows so each copy pays
                    *read = read.saturating_add(match &rest { Rest::Items(items, _) => items.len(), Rest::Entries(pairs, _) => pairs.len() });
                    cx.heap().afford(read.saturating_add(out.len()))?;
                    out.push(if matches!(rest, Rest::Items(..)) { '[' } else { '{' });
                    open.push((v, rest));
                }
                None => scalar::<L>(cx, v, out)?,
            }
            // the text is refused once it passes the memory left
            if out.len() > cx.room() { return Err(VmErr::Heap.into()); }
        }
        let Some((_, rest)) = open.last_mut() else { return Ok(()) };
        match rest {
            Rest::Items(items, at) if *at < items.len() => {
                if *at > 0 { out.push(','); }
                next = Some(items[*at]);
                *at += 1;
            }
            Rest::Entries(pairs, at) if *at < pairs.len() => {
                if *at > 0 { out.push(','); }
                let (k, item) = pairs[*at];
                *at += 1;
                let Some(key) = cx.text(k) else { return Err(not(k, ToJson::Key)) };
                quote(out, key);
                out.push(':');
                next = Some(item);
            }
            Rest::Items(..) => { out.push(']'); open.pop(); }
            Rest::Entries(..) => { out.push('}'); open.pop(); }
        }
    }
}

/* a value with no items as JSON onto `out` */
fn scalar<L: Lang>(cx: &Cx<'_, '_>, v: Val, out: &mut String) -> Result<(), Error> {
    if v.is_int() { out.push_str(itoa::Buffer::new().format(v.as_int())); }
    else if v.is_bool() { out.push_str(if v.as_bool() { "true" } else { "false" }); }
    else if v.is_none() { out.push_str("null"); }
    else if v.is_float() { float::<L>(v, out)?; }
    else if let Some(s) = cx.text(v) { quote(out, s); }
    else if let Some(digits) = cx.heap().is_wide(v).then(|| crate::fmt::render::kind_text::<L>(cx, v)).flatten() { out.push_str(&digits?); }
    else { return Err(not(v, ToJson::Kind)); }
    Ok(())
}

/* a finite float in the runtime's shortest text which JSON reads back as a float */
fn float<L: Lang>(v: Val, out: &mut String) -> Result<(), Error> {
    let f = v.as_float();
    if !f.is_finite() { return Err(not(v, ToJson::Infinite)); }
    let text = format_f64::<L>(f);
    // a style whose text is no JSON number gives way to the debug form
    if !number(&text) { out.push_str(&alloc::format!("{f:?}")); return Ok(()); }
    out.push_str(&text);
    if !text.contains(['.', 'e', 'E']) { out.push_str(".0"); }
    Ok(())
}

/* whether `s` is a number as JSON spells one */
fn number(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |from: usize| { let mut i = from; while b.get(i).is_some_and(u8::is_ascii_digit) { i += 1; } (i, i > from) };
    let first = usize::from(b.first() == Some(&b'-'));
    let (mut i, any) = digits(first);
    if !any || (b[first] == b'0' && i > first + 1) { return false; }
    if b.get(i) == Some(&b'.') {
        let (j, any) = digits(i + 1);
        if !any { return false; }
        i = j;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(b.get(i + 1), Some(b'+' | b'-')));
        let (j, any) = digits(i + 1 + sign);
        if !any { return false; }
        i = j;
    }
    i == b.len()
}

/* `s` as a JSON string with quotes backslashes and control characters escaped */
fn quote(out: &mut String, s: &str) {
    out.reserve(s.len() + 2);
    out.push('"');
    let mut run = 0;
    for (i, c) in s.bytes().enumerate() {
        let short = match c {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0..=0x1f => "",
            _ => continue,
        };
        out.push_str(s.get(run..i).unwrap_or(""));
        run = i + 1;
        if short.is_empty() { out.push_str("\\u00"); out.push(hex(c >> 4)); out.push(hex(c & 15)); } else { out.push_str(short); }
    }
    out.push_str(s.get(run..).unwrap_or(""));
    out.push('"');
}

fn hex(n: u8) -> char { char::from(b"0123456789abcdef"[usize::from(n & 15)]) }
