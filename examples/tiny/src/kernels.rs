use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use edge::lang::{Args, Cx, KindId, Native, Ret, Sig};
use edge::value::{DictMap, HeapObj, Val, VmErr};
use lib::fail::{Error, Fail};

use crate::natives::{kind_name, wrong};
use crate::Tiny;

// the slots of a source's state list past its request id
const CARRY: usize = 1;
const DONE: usize = 2;
const FORMAT: usize = 3;
// a csv source's header or the field a line of text fills
const HEADER: usize = 4;
const COUNT: usize = 5;
const NAME: usize = 6;
// the bytes an array of records gathers until its source ends
const WHOLE: usize = 7;
// a csv record whose quote is still open and the line it started on
const PENDING: usize = 8;
const OPENED: usize = 9;

// the formats as the lowering passes them
pub const JSON_LINES: i64 = 0;
pub const CSV: i64 = 1;
pub const LINES: i64 = 2;
// json whose first byte past blank text tells an array from lines
pub const JSON_DOC: i64 = 3;
// json read whole as one array of records once a bracket opened it
const ARRAY: i64 = 4;

// how many lines of host input one batch reads so memory follows the batch
const BATCH: usize = 64;

// the kernels the lowering calls under names no expression can spell
pub const KERNELS: &[Native] = &[
    Native::new("@source", Sig::exact(3), source_run),
    Native::new("@status", Sig::exact(2), status_run),
    Native::new("@more", Sig::exact(1), more_run),
    Native::new("@feed", Sig::exact(2), feed_run),
    Native::new("@lines", Sig::exact(1), lines_run),
    Native::new("@count", Sig::var(1), count_run),
    Native::new("@sum", Sig::var(2), sum_run),
    Native::new("@average", Sig::var(2), average_run),
    Native::new("@min", Sig::var(2), min_run),
    Native::new("@max", Sig::var(2), max_run),
    Native::new("@rows", Sig::exact(4), rows_run),
    Native::new("@push", Sig::exact(2), push_run),
    Native::new("@first", Sig::exact(2), first_run),
    Native::new("@sink", Sig::exact(2), sink_run),
    Native::new("@emit", Sig::exact(2), emit_run),
    Native::new("@emit_all", Sig::exact(2), emit_all_run),
];

// the items of a list a kernel reads and writes in place
type Items = alloc::rc::Rc<core::cell::RefCell<Vec<Val>>>;

/* the items of the list `v` names */
fn cell(cx: &Cx<'_, '_>, v: Val) -> Result<Items, VmErr> {
    match cx.heap().try_get(v) {
        Some(HeapObj::List(rc)) => Ok(rc.clone()),
        _ => Err(VmErr::Runtime("a tiny kernel was handed something other than its state")),
    }
}

fn slot(cx: &Cx<'_, '_>, v: Val, i: usize) -> Result<Val, VmErr> { Ok(cell(cx, v)?.borrow().get(i).copied().unwrap_or(Val::none())) }

fn put(cx: &Cx<'_, '_>, v: Val, i: usize, x: Val) -> Result<(), VmErr> {
    let rc = cell(cx, v)?;
    let mut items = rc.borrow_mut();
    if let Some(at) = items.get_mut(i) { *at = x; }
    Ok(())
}

/* the state of request `id` as a source or of host input when none */
fn source_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    // the carry grows in place so a long line costs its length once
    let carry = cx.new_bytes(Vec::new(), true)?;
    let field = if a.pos[1].as_int() == LINES { cx.new_str(String::from("line"))? } else { Val::none() };
    let state = vec![a.pos[0], carry, Val::bool(false), a.pos[1], field, Val::int(0), a.pos[2], Val::none(), Val::none(), Val::int(0)];
    Ok(cx.new_list(state)?.into())
}

/* refuses a response whose status is not 200 */
fn status_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let head = match cx.heap().try_get(a.pos[1]) {
        Some(HeapObj::List(rc)) => rc.borrow().first().copied(),
        Some(HeapObj::Tuple(t)) => t.first().copied(),
        _ => None,
    };
    let status = head.and_then(|s| cx.int(s));
    if status == Some(200) { return Ok(Val::none().into()); }
    let name = slot(cx, a.pos[0], NAME)?;
    let name = String::from(cx.str_of(name).unwrap_or(""));
    let shown = status.map_or_else(|| String::from("no status"), |s| format!("{s}"));
    Err(VmErr::Raised(format!("HostFailed: {name} answered {shown}, not 200")))
}

fn more_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(Val::bool(!slot(cx, a.pos[0], DONE)?.is_true()).into()) }

/* the records the lines of `chunk` complete after the carry where none ends the source */
fn feed_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (s, chunk) = (a.pos[0], a.pos[1]);
    let format = match cx.bytes_of(chunk) { Some(b) => decide(cx, s, b)?, None => slot(cx, s, FORMAT)?.as_int() };
    if format == ARRAY && (chunk.is_none() || cx.bytes_of(chunk).is_some()) {
        let more = cx.bytes_of(chunk).map(<[u8]>::to_vec).unwrap_or_default();
        cx.spend(more.len());
        if chunk.is_none() { put(cx, s, DONE, Val::bool(true))?; }
        return gather(cx, s, more, chunk.is_none());
    }
    let carry_val = slot(cx, s, CARRY)?;
    let done = chunk.is_none();
    let lines = if done {
        put(cx, s, DONE, Val::bool(true))?;
        // the bytes no line end closed are the source's last line
        let last = match cx.heap_mut().try_get_mut(carry_val) { Some(HeapObj::ByteArray(b)) => core::mem::take(b), _ => Vec::new() };
        if last.is_empty() { Vec::new() } else { vec![last] }
    } else {
        let Some(bytes) = cx.bytes_of(chunk) else { return Err(VmErr::Raised(format!("HostFailed: the source read {}, not bytes", kind_name(cx, chunk)))) };
        cx.spend(bytes.len());
        cx.reserve(bytes.len())?;
        let bytes = bytes.to_vec();
        match cx.heap_mut().try_get_mut(carry_val) { Some(HeapObj::ByteArray(carry)) => cut(carry, &bytes), _ => Vec::new() }
    };
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let n = bump(cx, s)?;
        let Ok(text) = core::str::from_utf8(&line) else { return Err(bad(cx, s, n, "is no utf-8 text")) };
        if let Some(r) = record(cx, s, n, text)? { out.push(r); }
    }
    if done { unclosed(cx, s)?; }
    Ok(cx.new_list(out)?.into())
}

/* the lines `chunk` completes after `carry` which keeps the bytes past the last line end */
fn cut(carry: &mut Vec<u8>, chunk: &[u8]) -> Vec<Vec<u8>> {
    // a carriage return right before the newline belongs to the break
    let line = |mut b: Vec<u8>| { if b.last() == Some(&b'\r') { b.pop(); } b };
    let Some(i) = chunk.iter().position(|&c| c == b'\n') else { carry.extend_from_slice(chunk); return Vec::new() };
    let mut first = core::mem::take(carry);
    first.extend_from_slice(&chunk[..i]);
    let (mut lines, mut start) = (vec![line(first)], i + 1);
    while let Some(j) = chunk[start..].iter().position(|&c| c == b'\n') {
        lines.push(line(chunk[start..start + j].to_vec()));
        start += j + 1;
    }
    carry.extend_from_slice(&chunk[start..]);
    lines
}

/* refuses a csv source that ended inside a record whose quote never closed */
fn unclosed(cx: &mut Cx<'_, '_>, s: Val) -> Result<(), VmErr> {
    let pending = slot(cx, s, PENDING)?;
    let Some(text) = cx.bytes_of(pending).map(|b| String::from_utf8_lossy(b).into_owned()) else { return Ok(()) };
    let at = lib::data::csv::fields(&text, ',').err().unwrap_or(0);
    let n = slot(cx, s, OPENED)?.as_int();
    Err(bad(cx, s, n, &format!("holds a quote at byte {at} that never closes")))
}

/* whether a quote `open` at the start of `line` is still open at its end */
fn open_after(line: &str, mut open: bool) -> bool {
    // a quote opens at a field start and a doubled one inside is one quote
    let (mut fresh, mut cs) = (!open, line.chars().peekable());
    while let Some(c) = cs.next() {
        match (open, c) {
            (true, '"') => if cs.next_if_eq(&'"').is_none() { open = false; },
            (true, _) => {}
            (false, ',') => { fresh = true; continue; }
            (false, '"') if fresh => open = true,
            (false, _) => {}
        }
        fresh = false;
    }
    open
}

/* the records of the next lines of host input */
fn lines_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let s = a.pos[0];
    let mut out = Vec::new();
    for _ in 0..BATCH {
        let Some(line) = cx.read_line() else {
            put(cx, s, DONE, Val::bool(true))?;
            if slot(cx, s, FORMAT)?.as_int() == ARRAY { return gather(cx, s, Vec::new(), true); }
            unclosed(cx, s)?;
            break;
        };
        let n = bump(cx, s)?;
        cx.spend(line.len());
        if decide(cx, s, line.as_bytes())? == ARRAY {
            // the blank lines before the array keep its lines numbered as the input's
            let fresh = slot(cx, s, WHOLE)?.is_none();
            let mut more = if fresh { "\n".repeat(usize::try_from(n - 1).unwrap_or(0)).into_bytes() } else { Vec::new() };
            more.extend_from_slice(line.as_bytes());
            more.push(b'\n');
            gather(cx, s, more, false)?;
            continue;
        }
        if let Some(r) = record(cx, s, n, &line)? { out.push(r); }
    }
    Ok(cx.new_list(out)?.into())
}

/* the format of source `s` once a json source's first byte decides it */
fn decide(cx: &Cx<'_, '_>, s: Val, text: &[u8]) -> Result<i64, VmErr> {
    let format = slot(cx, s, FORMAT)?.as_int();
    if format != JSON_DOC { return Ok(format); }
    let Some(&first) = text.iter().find(|b| !b.is_ascii_whitespace()) else { return Ok(JSON_DOC) };
    let format = if first == b'[' { ARRAY } else { JSON_LINES };
    put(cx, s, FORMAT, Val::int(format))?;
    Ok(format)
}

/* `more` added to an array's bytes whose items at the end are its records */
fn gather(cx: &mut Cx<'_, '_>, s: Val, more: Vec<u8>, end: bool) -> Ret {
    let mut whole = slot(cx, s, WHOLE)?;
    if whole.is_none() {
        whole = cx.new_bytes(Vec::new(), true)?;
        put(cx, s, WHOLE, whole)?;
    }
    cx.reserve(more.len())?;
    if let Some(HeapObj::ByteArray(b)) = cx.heap_mut().try_get_mut(whole) { b.extend_from_slice(&more); }
    if !end { return Ok(cx.new_list(Vec::new())?.into()); }
    let bytes = cx.bytes_of(whole).map(<[u8]>::to_vec).unwrap_or_default();
    let Ok(text) = String::from_utf8(bytes) else { return Err(bad(cx, s, 1, "is no utf-8 text")) };
    let doc = match lib::data::json::parse::<Tiny>(cx, &text) {
        Ok(v) => v,
        Err(Error::Fail(Fail::BadJson { at, why })) => {
            // the failure names the line of the input it falls on and its byte there
            let before = &text.as_bytes()[..at.min(text.len())];
            let line = before.iter().filter(|&&b| b == b'\n').count() as i64 + 1;
            let col = before.iter().rposition(|&b| b == b'\n').map_or(at, |i| at - i - 1);
            return Err(bad(cx, s, line, &format!("is no JSON, {why} at byte {col}")));
        }
        Err(Error::Fail(_)) => return Err(bad(cx, s, 1, "is no JSON")),
        Err(Error::Vm(e)) => return Err(e),
    };
    let items = match cx.heap().try_get(doc) { Some(HeapObj::List(rc)) => rc.borrow().clone(), _ => Vec::new() };
    Ok(cx.new_list(items)?.into())
}

/* counts one more line read by the source and gives its number */
fn bump(cx: &mut Cx<'_, '_>, s: Val) -> Result<i64, VmErr> {
    let n = slot(cx, s, COUNT)?.as_int() + 1;
    put(cx, s, COUNT, Val::int(n))?;
    Ok(n)
}

/* the failure naming line `n` of the source */
fn bad(cx: &Cx<'_, '_>, s: Val, n: i64, why: &str) -> VmErr {
    let name = slot(cx, s, NAME).ok().and_then(|v| cx.str_of(v).map(String::from)).unwrap_or_default();
    VmErr::Raised(format!("BadData: line {n} of {name} {why}"))
}

/* line `n` as a record of its format or none for a line holding none */
fn record(cx: &mut Cx<'_, '_>, s: Val, n: i64, text: &str) -> Result<Option<Val>, VmErr> {
    match slot(cx, s, FORMAT)?.as_int() {
        JSON_LINES | JSON_DOC => {
            if text.trim().is_empty() { return Ok(None); }
            match lib::data::json::parse::<Tiny>(cx, text) {
                Ok(v) => Ok(Some(v)),
                Err(Error::Fail(Fail::BadJson { at, why })) => Err(bad(cx, s, n, &format!("is no JSON, {why} at byte {at}"))),
                Err(Error::Fail(_)) => Err(bad(cx, s, n, "is no JSON")),
                Err(Error::Vm(e)) => Err(e),
            }
        }
        CSV => {
            // a record whose quote is open takes each next line with the break it crossed
            let pending = slot(cx, s, PENDING)?;
            if !pending.is_none() {
                cx.reserve(text.len() + 1)?;
                cx.spend(text.len());
                if let Some(HeapObj::ByteArray(b)) = cx.heap_mut().try_get_mut(pending) { b.push(b'\n'); b.extend_from_slice(text.as_bytes()); }
                if open_after(text, true) { return Ok(None); }
                put(cx, s, PENDING, Val::none())?;
                let whole = cx.bytes_of(pending).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                return csv_record(cx, s, slot(cx, s, OPENED)?.as_int(), &whole);
            }
            if text.is_empty() { return Ok(None); }
            if open_after(text, false) {
                let held = cx.new_bytes(text.as_bytes().to_vec(), true)?;
                put(cx, s, PENDING, held)?;
                put(cx, s, OPENED, Val::int(n))?;
                return Ok(None);
            }
            csv_record(cx, s, n, text)
        }
        // a line of text is a record whose one field holds it
        _ => {
            let (key, line) = (slot(cx, s, HEADER)?, cx.new_str(String::from(text))?);
            let mut d = DictMap::with_capacity(1);
            d.insert(key, line, cx.heap());
            Ok(Some(cx.new_dict(d)?))
        }
    }
}

/* the record of csv text `text` that line `n` started or none for the header */
fn csv_record(cx: &mut Cx<'_, '_>, s: Val, n: i64, text: &str) -> Result<Option<Val>, VmErr> {
    let fields = match lib::data::csv::fields(text, ',') {
        Ok(f) => f,
        Err(at) => return Err(bad(cx, s, n, &format!("holds a quote at byte {at} that never closes"))),
    };
    let header = slot(cx, s, HEADER)?;
    if header.is_none() {
        let mut names = Vec::with_capacity(fields.len());
        for f in fields { names.push(cx.new_str(f)?); }
        let list = cx.new_list(names)?;
        put(cx, s, HEADER, list)?;
        return Ok(None);
    }
    let names = cell(cx, header)?.borrow().clone();
    if names.len() != fields.len() {
        return Err(bad(cx, s, n, &format!("holds {} fields where the header names {}", fields.len(), names.len())));
    }
    let mut d = DictMap::with_capacity(names.len());
    for (k, f) in names.into_iter().zip(fields) {
        let v = cx.new_str(f)?;
        d.insert(k, v, cx.heap());
        // a header of colliding names stops the record once its probes spend the budget
        cx.heap().within_budget()?;
    }
    Ok(Some(cx.new_dict(d)?))
}

/* the row table `g` keeps for `keys` made with `init` after them when new */
fn row(cx: &mut Cx<'_, '_>, g: Val, keys: &[Val], init: &[Val]) -> Result<Items, VmErr> {
    let key = match keys {
        [] => Val::none(),
        [one] => *one,
        many => cx.new_tuple(many.to_vec())?,
    };
    if cx.require_key(key).is_err() {
        let bad = keys.iter().copied().find(|&k| cx.require_key(k).is_err()).unwrap_or(key);
        return Err(VmErr::Raised(format!("WrongType: a key is a text a number or a bool, not a {}", kind_name(cx, bad))));
    }
    let Some(table) = cx.heap().dict_cell(g).cloned() else { return Err(VmErr::Runtime("a report lost its rows")) };
    let found = table.borrow().get(&key, cx.heap()).copied();
    if let Some(r) = found { return cell(cx, r); }
    let mut items = Vec::with_capacity(keys.len() + init.len());
    items.extend_from_slice(keys);
    items.extend_from_slice(init);
    let r = cx.new_list(items)?;
    table.borrow_mut().insert(key, r, cx.heap());
    cell(cx, r)
}

/* counts one more record in the row of its keys */
fn count_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let rc = row(cx, a.pos[0], &a.pos[1..], &[Val::int(0)])?;
    let mut items = rc.borrow_mut();
    let at = items.len() - 1;
    items[at] = Val::int(items[at].as_int() + 1);
    Ok(Val::none().into())
}

/* adds a number to its row's total where none adds nothing */
fn sum_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let v = a.pos[1];
    let rc = row(cx, a.pos[0], &a.pos[2..], &[Val::none()])?;
    if v.is_none() { return Ok(Val::none().into()); }
    number(cx, v)?;
    let at = rc.borrow().len() - 1;
    let held = rc.borrow()[at];
    let total = if held.is_none() { v } else { cx.add(held, v)? };
    rc.borrow_mut()[at] = total;
    Ok(Val::none().into())
}

/* adds a number to its row's total and counts it in the slot after */
fn average_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let v = a.pos[1];
    let rc = row(cx, a.pos[0], &a.pos[2..], &[Val::none(), Val::int(0)])?;
    if v.is_none() { return Ok(Val::none().into()); }
    number(cx, v)?;
    let at = rc.borrow().len() - 2;
    let (held, k) = { let items = rc.borrow(); (items[at], items[at + 1].as_int()) };
    let total = if held.is_none() { v } else { cx.add(held, v)? };
    let mut items = rc.borrow_mut();
    items[at] = total;
    items[at + 1] = Val::int(k + 1);
    Ok(Val::none().into())
}

/* keeps the least value of its row where none is skipped */
fn min_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { extreme(cx, a, true) }

/* keeps the greatest value of its row where none is skipped */
fn max_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { extreme(cx, a, false) }

fn extreme(cx: &mut Cx<'_, '_>, a: Args<'_>, least: bool) -> Ret {
    let v = a.pos[1];
    let rc = row(cx, a.pos[0], &a.pos[2..], &[Val::none()])?;
    if v.is_none() { return Ok(Val::none().into()); }
    let at = rc.borrow().len() - 1;
    let held = rc.borrow()[at];
    let better = held.is_none() || if least { cx.lt(v, held)? } else { cx.lt(held, v)? };
    if better { rc.borrow_mut()[at] = v; }
    Ok(Val::none().into())
}

/* refuses a value an aggregate adds that is no number */
fn number(cx: &Cx<'_, '_>, v: Val) -> Result<(), VmErr> {
    match cx.kind(v) {
        KindId::Int | KindId::Float => Ok(()),
        _ => Err(VmErr::Raised(format!("WrongType: cannot add {} to a total of numbers", kind_name(cx, v)))),
    }
}

/* a report's rows as records of its keys at their paths then its aggregate */
fn rows_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (g, paths, word) = (a.pos[0], parts(cx, a.pos[1])?, a.pos[3]);
    let letter = cx.str_of(a.pos[2]).and_then(|s| s.chars().next()).unwrap_or('c');
    let Some(table) = cx.heap().dict_cell(g).cloned() else { return Err(VmErr::Runtime("a report lost its rows")) };
    let mut all: Vec<Vec<Val>> = table.borrow().iter().filter_map(|(_, r)| match cx.heap().try_get(r) { Some(HeapObj::List(rc)) => Some(rc.borrow().clone()), _ => None }).collect();
    // an aggregate with no keys gives one row as a total over nothing does
    if all.is_empty() && paths.is_empty() {
        all.push(match letter { 'c' => vec![Val::int(0)], 'a' => vec![Val::none(), Val::int(0)], _ => vec![Val::none()] });
    }
    let mut out = Vec::with_capacity(all.len());
    for row in all {
        let rec = cx.new_dict(DictMap::new())?;
        for (path, &v) in paths.iter().zip(row.iter()) { place(cx, rec, path, v)?; }
        let at = paths.len();
        let v = if letter != 'a' { row[at] } else {
            let (total, n) = (row[at], row[at + 1].as_int());
            if n == 0 { Val::none() } else {
                let t = match cx.float(total) { Some(Ok(f)) => f, Some(Err(e)) => return Err(e), None => 0.0 };
                Val::float(t / n as f64)
            }
        };
        place(cx, rec, &[word], v)?;
        out.push(rec);
    }
    Ok(cx.new_list(out)?.into())
}

/* the paths a list of lists of texts spells */
fn parts(cx: &Cx<'_, '_>, v: Val) -> Result<Vec<Vec<Val>>, VmErr> {
    let outer = cell(cx, v)?.borrow().clone();
    outer.into_iter().map(|p| cell(cx, p).map(|rc| rc.borrow().clone())).collect()
}

/* sets `v` at `path` of record `rec` making the records between */
fn place(cx: &mut Cx<'_, '_>, rec: Val, path: &[Val], v: Val) -> Result<(), VmErr> {
    let mut at = rec;
    for (i, &k) in path.iter().enumerate() {
        let Some(d) = cx.heap().dict_cell(at).cloned() else { return Err(VmErr::Runtime("a row lost its record")) };
        if i + 1 == path.len() { d.borrow_mut().insert(k, v, cx.heap()); return Ok(()); }
        let next = d.borrow().get(&k, cx.heap()).copied();
        at = match next {
            Some(n) if cx.kind(n) == KindId::Dict => n,
            _ => { let n = cx.new_dict(DictMap::new())?; d.borrow_mut().insert(k, n, cx.heap()); n }
        };
    }
    Ok(())
}

fn push_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    cell(cx, a.pos[0])?.borrow_mut().push(a.pos[1]);
    Ok(Val::none().into())
}

/* the first `n` items of a list */
fn first_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let items = cell(cx, a.pos[0])?.borrow().clone();
    let n = usize::try_from(a.pos[1].as_int()).unwrap_or(0).min(items.len());
    Ok(cx.new_list(items[..n].to_vec())?.into())
}

// how a sink writes its records
pub const JSON: i64 = 0;
pub const TABLE: i64 = 1;

/* the state of a sink writing to host channel `channel` */
fn sink_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    Ok(cx.new_list(vec![a.pos[0], a.pos[1], Val::none()])?.into())
}

fn emit_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    emit(cx, a.pos[0], a.pos[1])?;
    Ok(Val::none().into())
}

fn emit_all_run(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let items = cell(cx, a.pos[1])?.borrow().clone();
    for v in items {
        dated(cx, v)?;
        emit(cx, a.pos[0], v)?;
    }
    Ok(Val::none().into())
}

/* whether `v` is a time or a duration */
fn is_date(cx: &Cx<'_, '_>, v: Val) -> bool {
    cx.heap().foreign_kind(v).is_some_and(|k| core::ptr::eq(k, &crate::lang::TIME) || core::ptr::eq(k, &crate::lang::DURATION))
}

/* each time or duration of a row made its text for a line of JSON */
fn dated(cx: &mut Cx<'_, '_>, rec: Val) -> Result<(), VmErr> {
    let Some(d) = cx.heap().dict_cell(rec).cloned() else { return Ok(()) };
    let entries: Vec<(Val, Val)> = d.borrow().iter().collect();
    for (k, v) in entries {
        if cx.kind(v) == KindId::Dict { dated(cx, v)?; continue; }
        if !is_date(cx, v) { continue; }
        let text = lib::fmt::render::display::<Tiny>(cx, v)?;
        let t = cx.new_str(text)?;
        d.borrow_mut().insert(k, t, cx.heap());
        cx.heap().within_budget()?;
    }
    Ok(())
}

/* hands one record to the sink's channel as a line of its format */
fn emit(cx: &mut Cx<'_, '_>, k: Val, rec: Val) -> Result<(), VmErr> {
    let channel = slot(cx, k, 0)?;
    let channel = String::from(cx.str_of(channel).unwrap_or(""));
    let mut out = String::new();
    if slot(cx, k, 1)?.as_int() == TABLE {
        let mut fields: Vec<(String, Val)> = Vec::new();
        flatten(cx, rec, &mut String::new(), &mut fields)?;
        let header = slot(cx, k, 2)?;
        if header.is_none() {
            let mut names = Vec::with_capacity(fields.len());
            let mut line = String::new();
            for (i, (name, _)) in fields.iter().enumerate() {
                if i > 0 { line.push(','); }
                lib::data::csv::field(&mut line, name, ',', false);
                names.push(cx.new_str(name.clone())?);
            }
            let list = cx.new_list(names)?;
            put(cx, k, 2, list)?;
            send(cx, &channel, &line)?;
        } else {
            let names: Vec<String> = cell(cx, header)?.borrow().iter().map(|&n| String::from(cx.str_of(n).unwrap_or(""))).collect();
            if let Some((extra, _)) = fields.iter().find(|(f, _)| !names.contains(f)) {
                return Err(VmErr::Raised(format!("BadData: a record holds the field {extra} which the csv header of {channel} lacks")));
            }
            fields = names.into_iter().map(|n| { let v = fields.iter().find(|(f, _)| *f == n).map_or(Val::none(), |(_, v)| *v); (n, v) }).collect();
        }
        for (i, (_, v)) in fields.iter().enumerate() {
            if i > 0 { out.push(','); }
            let text = if v.is_none() { String::new() } else if cx.kind(*v) == KindId::Str { String::from(cx.str_of(*v).unwrap_or("")) } else { json(cx, *v)? };
            lib::data::csv::field(&mut out, &text, ',', false);
        }
    } else {
        out = json(cx, rec)?;
    }
    send(cx, &channel, &out)
}

/* `v` as compact JSON text */
fn json(cx: &Cx<'_, '_>, v: Val) -> Result<String, VmErr> {
    let mut out = String::new();
    match lib::data::json::write_into::<Tiny>(cx, v, &mut out) {
        Ok(()) => Ok(out),
        Err(Error::Vm(e)) => Err(e),
        Err(Error::Fail(Fail::NotJson { v, why })) => Err(VmErr::Raised(format!("BadData: a {} {why} so no line of JSON holds it", kind_name(cx, v)))),
        Err(Error::Fail(_)) => Err(VmErr::Raised("BadData: a value no line of JSON holds".into())),
    }
}

/* the fields of a record at their dotted paths with nested records opened */
fn flatten(cx: &Cx<'_, '_>, rec: Val, prefix: &mut String, out: &mut Vec<(String, Val)>) -> Result<(), VmErr> {
    let Some(d) = cx.heap().dict_cell(rec) else {
        if prefix.is_empty() { return Err(wrong(cx, "a csv file", "records", rec)); }
        out.push((prefix.clone(), rec));
        return Ok(());
    };
    let entries: Vec<(Val, Val)> = d.borrow().iter().collect();
    for (k, v) in entries {
        let name = cx.str_of(k).map(String::from).unwrap_or_else(|| String::from("?"));
        let len = prefix.len();
        if !prefix.is_empty() { prefix.push('.'); }
        prefix.push_str(&name);
        if cx.kind(v) == KindId::Dict { flatten(cx, v, prefix, out)?; } else { out.push((prefix.clone(), v)); }
        prefix.truncate(len);
    }
    Ok(())
}

/* hands a line to the host's channel or fails naming it */
fn send(cx: &mut Cx<'_, '_>, channel: &str, line: &str) -> Result<(), VmErr> {
    cx.spend(line.len());
    if cx.send(channel, line) { Ok(()) } else { Err(VmErr::Raised(format!("HostFailed: the host keeps no channel {channel}"))) }
}
