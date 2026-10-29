use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use edge::lang::{Args, Cx, KindId, Ret, Sig};
use edge::value::Val;

use crate::fail::{Fail, fail};
use crate::{Kernel, Site};

/* the lines a chunk completes after the last call's rest beside the new rest */
pub const fn lines<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), lines_run::<S>) }

/* a stream of byte chunks cut into lines with the unfinished line carried */
#[derive(Clone, Debug, Default)]
pub struct Lines {
    carry: Vec<u8>,
}

impl Lines {
    pub const fn new() -> Self { Lines { carry: Vec::new() } }

    /* the lines `chunk` completes with each newline and a carriage return before it dropped */
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Vec<u8>> {
        // the carry holds no newline so the search starts at the new bytes
        let from = self.carry.len();
        self.carry.extend_from_slice(chunk);
        let (ranges, rest) = cut(&self.carry, from);
        let lines = ranges.into_iter().map(|r| self.carry[r].to_vec()).collect();
        self.carry.drain(..rest);
        lines
    }

    /* the trailing line no newline ended and none when nothing is carried */
    pub fn finish(self) -> Option<Vec<u8>> { (!self.carry.is_empty()).then_some(self.carry) }
}

/* the ranges of the lines `buf` completes and where its unfinished rest starts */
fn cut(buf: &[u8], from: usize) -> (Vec<Range<usize>>, usize) {
    let (mut lines, mut start, mut at) = (Vec::new(), 0, from);
    while let Some(i) = buf.get(at..).and_then(|b| b.iter().position(|&c| c == b'\n')) {
        let end = at + i;
        // a carriage return right before the newline belongs to the break
        let stop = if end > start && buf[end - 1] == b'\r' { end - 1 } else { end };
        lines.push(start..stop);
        start = end + 1;
        at = start;
    }
    (lines, start)
}

/* how a chunk holds its bytes which its lines and its rest keep */
#[derive(Clone, Copy, PartialEq, Eq)]
enum Form { Text, Bytes, Buffer }

fn form(cx: &Cx<'_, '_>, v: Val) -> Option<Form> {
    match cx.kind(v) {
        KindId::Str => Some(Form::Text),
        KindId::Bytes => Some(Form::Bytes),
        KindId::ByteArray => Some(Form::Buffer),
        _ => None,
    }
}

/* the form of `chunk` with `rest` absent or text with text or bytes with bytes */
fn matched(cx: &Cx<'_, '_>, chunk: Val, rest: Option<Val>) -> Option<Form> {
    let f = form(cx, chunk)?;
    match rest.map(|r| form(cx, r)) {
        None => Some(f),
        Some(Some(g)) if (g == Form::Text) == (f == Form::Text) => Some(f),
        _ => None,
    }
}

fn lines_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (chunk, rest) = (a.pos[0], a.get(1));
    let Some(shape) = matched(cx, chunk, rest) else { return Err(fail::<S>(cx, Fail::LineKinds { chunk, rest: rest.unwrap_or(chunk) })) };
    let (items, tail) = if shape == Form::Text {
        let mut s = String::from(rest.and_then(|r| cx.text(r)).unwrap_or(""));
        s.push_str(cx.text(chunk).unwrap_or(""));
        cx.spend(s.len());
        let (ranges, tail) = cut(s.as_bytes(), 0);
        let mut items = Vec::with_capacity(ranges.len());
        // a cut at a newline leaves whole characters on both sides
        for r in ranges { items.push(cx.new_str(String::from(s.get(r).unwrap_or("")))?); }
        (items, cx.new_str(String::from(s.get(tail..).unwrap_or("")))?)
    } else {
        let mut b = rest.and_then(|r| cx.bytes_of(r)).unwrap_or(&[]).to_vec();
        b.extend_from_slice(cx.bytes_of(chunk).unwrap_or(&[]));
        cx.spend(b.len());
        let (ranges, tail) = cut(&b, 0);
        let mutable = shape == Form::Buffer;
        let mut items = Vec::with_capacity(ranges.len());
        for r in ranges { items.push(cx.new_bytes(b[r].to_vec(), mutable)?); }
        (items, cx.new_bytes(b[tail..].to_vec(), mutable)?)
    };
    let list = cx.new_list(items)?;
    Ok(cx.new_tuple(alloc::vec![list, tail])?.into())
}
