use alloc::string::String;
use alloc::vec::Vec;

use edge::lang::{Args, Cx, Language, Mode, Ret, Sig};
use edge::value::{HeapObj, Val, VmErr};

use crate::fail::{Csv, Fail, fail};
use crate::{Kernel, Site};

/* the fields of one record of text split at a separator with quoted fields read */
pub const fn read<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), read_run::<S>) }

/* one record of text holding the values with each field quoted that needs it */
pub const fn write<S: Site>() -> Kernel { Kernel::new(Sig::range(1, 2), write_run::<S>) }

/* the call's separator as one character that is no quote or line break */
fn separator<S: Site>(cx: &mut Cx<'_, '_>, v: Option<Val>) -> Result<char, VmErr> {
    let Some(v) = v else { return Ok(',') };
    let Some(s) = cx.text(v) else { return Err(fail::<S>(cx, Fail::TextArg { at: 2, v })) };
    let mut cs = s.chars();
    match (cs.next(), cs.next()) {
        (Some(c), None) if !matches!(c, '"' | '\r' | '\n') => Ok(c),
        _ => Err(fail::<S>(cx, Fail::SepChar(v))),
    }
}

fn read_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let line = a.pos[0];
    if cx.text(line).is_none() { return Err(fail::<S>(cx, Fail::TextArg { at: 1, v: line })); }
    let sep = separator::<S>(cx, a.get(1))?;
    let found = fields(cx.str_of(line).unwrap_or(""), sep);
    let parts = match found { Ok(parts) => parts, Err(at) => return Err(fail::<S>(cx, Fail::BadCsv { at, why: Csv::Unclosed })) };
    let mut vals = Vec::with_capacity(parts.len());
    for p in parts { vals.push(cx.new_str(p)?); }
    Ok(cx.new_list(vals)?.into())
}

/* the fields of `line` per RFC 4180 or where a quote that never closes opens */
pub fn fields(line: &str, sep: char) -> Result<Vec<String>, usize> {
    // a line break ending the record is no part of its last field
    let line = line.strip_suffix('\n').map_or(line, |l| l.strip_suffix('\r').unwrap_or(l));
    if line.is_empty() { return Ok(Vec::new()); }
    let (mut out, mut field) = (Vec::new(), String::new());
    // where the quote of the field being read opened while it is open
    let mut open = None;
    // whether the field being read has taken nothing yet
    let mut fresh = true;
    let mut cs = line.char_indices().peekable();
    while let Some((i, c)) = cs.next() {
        match open {
            // a doubled quote inside quotes is one quote
            Some(_) if c == '"' => if cs.next_if(|&(_, d)| d == '"').is_some() { field.push('"'); } else { open = None; },
            Some(_) => field.push(c),
            None if c == sep => { out.push(core::mem::take(&mut field)); fresh = true; continue; }
            None if c == '"' && fresh => open = Some(i),
            None => field.push(c),
        }
        fresh = false;
    }
    if let Some(at) = open { return Err(at); }
    out.push(field);
    Ok(out)
}

fn write_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let v = a.pos[0];
    let items = match cx.heap().try_get(v) {
        Some(HeapObj::List(rc)) => rc.borrow().clone(),
        Some(HeapObj::Tuple(t)) => t.clone(),
        _ => return Err(fail::<S>(cx, Fail::NotRecord(v))),
    };
    let sep = separator::<S>(cx, a.get(1))?;
    let mut out = String::new();
    for (i, &item) in items.iter().enumerate() {
        if i > 0 { out.push(sep); }
        let text = <S::Lang as Language>::show(cx, item, Mode::Str)?;
        // one empty field is quoted so it reads back apart from no field
        let lone = items.len() == 1 && text.is_empty();
        field(&mut out, &text, sep, lone);
    }
    cx.spend(out.len());
    Ok(cx.new_str(out)?.into())
}

/* one field onto `out` quoted when it holds the separator a quote or line break */
pub fn field(out: &mut String, text: &str, sep: char, quote: bool) {
    if !quote && !text.contains([sep, '"', '\r', '\n']) { return out.push_str(text); }
    out.push('"');
    for c in text.chars() {
        if c == '"' { out.push('"'); }
        out.push(c);
    }
    out.push('"');
}
