use alloc::string::String;
use alloc::vec::Vec;

use edge::lang::{Args, Cx, Ret, Sig};

use super::{new_str, push_lower, push_upper, recv_str};
use crate::{Kernel, Site, uni};

/* the text in uppercase as `C` maps each character */
pub const fn upper<S: Site, C: Case>() -> Kernel { Kernel::new(Sig::exact(0), upper_run::<C>) }

/* the text in lowercase as `C` maps each character */
pub const fn lower<S: Site, C: Case>() -> Kernel { Kernel::new(Sig::exact(0), lower_run::<C>) }

/* the first character in titlecase and the rest lowercased */
pub const fn capitalize<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), capitalize_run) }

/* each run of cased characters titlecased first and lowercased after */
pub const fn title<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), title_run) }

/* each cased character turned to the other case */
pub const fn swap<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), swap_run) }

/* the text folded so texts that differ only by case compare equal */
pub const fn fold<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), fold_run) }

/* how a case map treats a character whose other case is several characters */
pub trait Case: 'static {
    fn upper(s: &str) -> String;
    fn lower(s: &str) -> String;
}

/* one character maps to one and a longer mapping leaves it as it is */
pub struct Simple;

/* unicode's full mappings where one character may become several */
pub struct Full;

impl Case for Simple {
    fn upper(s: &str) -> String { s.chars().map(|c| one(c, c.to_uppercase())).collect() }
    fn lower(s: &str) -> String { s.chars().map(|c| one(c, c.to_lowercase())).collect() }
}

impl Case for Full {
    fn upper(s: &str) -> String { s.to_uppercase() }
    fn lower(s: &str) -> String { s.to_lowercase() }
}

/* the one character a mapping gives or `c` when it gives several */
fn one(c: char, mut mapped: impl Iterator<Item = char>) -> char {
    match (mapped.next(), mapped.next()) { (Some(m), None) => m, _ => c }
}

fn upper_run<C: Case>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { let out = C::upper(recv_str(cx, a.recv)?); new_str(cx, out) }

fn lower_run<C: Case>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { let out = C::lower(recv_str(cx, a.recv)?); new_str(cx, out) }

fn capitalize_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let out = {
        let s = recv_str(cx, a.recv)?;
        let mut cs = s.chars();
        let mut out = String::with_capacity(s.len());
        if let Some(c) = cs.next() {
            uni::push_title(&mut out, c);
            let rest = cs.as_str();
            // a capital sigma lowers by its place in the whole text first character included
            if rest.bytes().any(|b| b == SIGMA_END) { out.push_str(s.to_lowercase().get(lowered_len(c)..).unwrap_or("")); } else { out.push_str(&rest.to_lowercase()); }
        }
        out
    };
    new_str(cx, out)
}

/* the bytes first character `c` takes once its text is lowercased */
fn lowered_len(c: char) -> usize { if c == SIGMA { 'σ'.len_utf8() } else { c.to_lowercase().map(char::len_utf8).sum() } }

fn title_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = cx.is_ascii(a.recv);
    let out = {
        let s = recv_str(cx, a.recv)?;
        // ascii text maps byte by byte with no case tables
        if ascii { let out = ascii_title(s); return new_str(cx, out); }
        let mut finals = None;
        let (mut out, mut cased, mut cs) = (String::with_capacity(s.len()), false, s.chars());
        while let Some(c) = cs.next() {
            if !uni::is_cased(c) { out.push(c); cased = false; continue; }
            if !cased { uni::push_title(&mut out, c); } else if c == SIGMA { out.push(sigma(finals.get_or_insert_with(|| ends(s)), s.len() - cs.as_str().len() - c.len_utf8())); } else { push_lower(&mut out, c); }
            cased = true;
        }
        out
    };
    new_str(cx, out)
}

// the capital sigma whose lowercase takes its final form at the end of a word
const SIGMA: char = 'Σ';
// the last byte of a capital sigma which a scan finds with no decoding
const SIGMA_END: u8 = 0xA3;

/* an ascii text with each run of letters started uppercase and continued lowercase */
#[inline(never)]
fn ascii_title(s: &str) -> String {
    let mut cased = false;
    let out: Vec<u8> = s.bytes().map(|b| {
        let c = if !b.is_ascii_alphabetic() { b } else if cased { b.to_ascii_lowercase() } else { b.to_ascii_uppercase() };
        cased = b.is_ascii_alphabetic();
        c
    }).collect();
    String::from_utf8(out).unwrap_or_default()
}

/* an ascii text with each letter turned to the other case */
#[inline(never)]
fn ascii_swap(s: &str) -> String {
    let out: Vec<u8> = s.bytes().map(|b| if b.is_ascii_uppercase() { b.to_ascii_lowercase() } else { b.to_ascii_uppercase() }).collect();
    String::from_utf8(out).unwrap_or_default()
}

/* where capital sigmas of `s` take the final form as its whole lowercase tells */
fn ends(s: &str) -> Vec<usize> {
    let lowered = s.to_lowercase();
    let mut low = lowered.chars();
    let mut out = Vec::new();
    for (i, c) in s.char_indices() {
        // every other character lowers on its own so the two texts walk together
        if c != SIGMA { low.by_ref().take(c.to_lowercase().count()).for_each(drop); continue; }
        if low.next() == Some('ς') { out.push(i); }
    }
    out
}

/* the lowercase of the capital sigma at byte `at` given where the final forms fall */
fn sigma(finals: &[usize], at: usize) -> char { if finals.binary_search(&at).is_ok() { 'ς' } else { 'σ' } }

fn fold_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let out = {
        let s = recv_str(cx, a.recv)?;
        let mut out = String::with_capacity(s.len());
        s.chars().for_each(|c| uni::push_fold(&mut out, c));
        out
    };
    new_str(cx, out)
}

fn swap_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let ascii = cx.is_ascii(a.recv);
    let out = {
        let s = recv_str(cx, a.recv)?;
        // ascii text maps byte by byte with no case tables
        if ascii { let out = ascii_swap(s); return new_str(cx, out); }
        let mut finals = None;
        let (mut out, mut cs) = (String::with_capacity(s.len()), s.chars());
        while let Some(c) = cs.next() {
            if c == SIGMA { out.push(sigma(finals.get_or_insert_with(|| ends(s)), s.len() - cs.as_str().len() - c.len_utf8())); }
            else if c.is_uppercase() { push_lower(&mut out, c); } else if c.is_lowercase() { push_upper(&mut out, c); } else { out.push(c); }
        }
        out
    };
    new_str(cx, out)
}
