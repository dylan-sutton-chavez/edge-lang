use super::prelude::*;
use crate::s;
use crate::util::uni;

use core::iter;

// `str.encode([encoding])`, UTF-8/ASCII only, other names error to block silent mismatches.
pub fn encode(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let s = recv_str(vm, recv)?;
    if let Some(arg) = pos.first() {
        let enc = val_to_str(vm, *arg)?;
        match enc.as_str() {
            "utf-8" | "utf8" => {}
            "ascii" if !s.is_ascii() => {
                return Err(VmErr::Raised("UnicodeEncodeError: 'ascii' codec can't encode non-ASCII characters".into()));
            }
            "ascii" => {}
            _ => return Err(cold_value("unsupported encoding (expected 'utf-8' or 'ascii')")),
        }
    }
    let v = vm.heap.alloc(HeapObj::Bytes(s.into_bytes()))?;
    vm.push(v); Ok(())
}

// str zero-arg transforms, the receiver read in place and the result pushed.
macro_rules! str_transform {
    ($name:ident, $f:expr) => {
        pub fn $name(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
            let out = $f(recv_str_ref(vm, recv)?);
            vm.alloc_and_push_str(out)
        }
    };
}
str_transform!(upper, |s: &str| s.to_uppercase());
str_transform!(lower, |s: &str| s.to_lowercase());
str_transform!(capitalize, capitalize_first);
str_transform!(title, title_case);

// str.strip / lstrip / rstrip trim whitespace, or any char in the optional arg.
enum Trim { Both, Start, End }
fn strip_impl(vm: &mut VM, recv: Val, pos: &[Val], mode: Trim) -> Result<(), VmErr> {
    let s = recv_str_ref(vm, recv)?;
    let out = if pos.is_empty() {
        match mode { Trim::Both => s.trim_matches(uni::is_space), Trim::Start => s.trim_start_matches(uni::is_space), Trim::End => s.trim_end_matches(uni::is_space) }.to_string()
    } else {
        let p = val_to_str(vm, pos[0])?;
        let f = |c: char| p.contains(c);
        match mode { Trim::Both => s.trim_matches(f), Trim::Start => s.trim_start_matches(f), Trim::End => s.trim_end_matches(f) }.to_string()
    };
    vm.alloc_and_push_str(out)
}
pub fn strip(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { strip_impl(vm, recv, pos, Trim::Both) }
pub fn lstrip(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { strip_impl(vm, recv, pos, Trim::Start) }
pub fn rstrip(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { strip_impl(vm, recv, pos, Trim::End) }

// str predicates, class tests need a non-empty string and case tests a cased char.
macro_rules! str_pred {
    ($name:ident, $f:expr) => {
        pub fn $name(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
            let hit = $f(recv_str_ref(vm, recv)?);
            vm.push(Val::bool(hit));
            Ok(())
        }
    };
}
fn all_chars(s: &str, f: fn(char) -> bool) -> bool { !s.is_empty() && s.chars().all(f) }
fn only_case(s: &str, other: fn(char) -> bool) -> bool { s.chars().any(|c| c.is_uppercase() || c.is_lowercase()) && !s.chars().any(other) }
str_pred!(isdigit, |s: &str| all_chars(s, uni::is_digit));
str_pred!(isalpha, |s: &str| all_chars(s, uni::is_alpha));
str_pred!(isalnum, |s: &str| all_chars(s, uni::is_alnum));
str_pred!(isspace, |s: &str| all_chars(s, uni::is_space));
str_pred!(isupper, |s: &str| only_case(s, char::is_lowercase));
str_pred!(islower, |s: &str| only_case(s, char::is_uppercase));

// Collect the affix argument as one or more strings (str, or a tuple of str).
fn affixes(vm: &VM, v: Val) -> Result<Vec<String>, VmErr> {
    if let Some(HeapObj::Tuple(items)) = vm.heap.try_get(v) {
        let items = items.clone();
        let mut out = Vec::with_capacity(items.len());
        for it in items { out.push(val_to_str(vm, it)?); }
        Ok(out)
    } else {
        Ok(alloc::vec![val_to_str(vm, v)?])
    }
}

// `str.startswith`/`endswith(affix[, start[, stop]])`, the optional window restricts where the affix must sit.
fn affix_impl(vm: &mut VM, recv: Val, pos: &[Val], end: bool) -> Result<(), VmErr> {
    let affixes = affixes(vm, pos[0])?;
    let ascii = vm.heap.str_is_ascii(recv);
    let s = recv_str_ref(vm, recv)?;
    let hit = window(s, ascii, pos, 1).is_some_and(|(lo, hi)| affixes.iter().any(|p| if end { s[lo..hi].ends_with(p.as_str()) } else { s[lo..hi].starts_with(p.as_str()) }));
    vm.push(Val::bool(hit));
    Ok(())
}
pub fn startswith(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { affix_impl(vm, recv, pos, false) }
pub fn endswith(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { affix_impl(vm, recv, pos, true) }

// Byte bounds of the optional char-index `start`/`stop` window (pos[from], pos[from + 1]), None when start lies past the end or past stop.
fn window(s: &str, ascii: bool, pos: &[Val], from: usize) -> Option<(usize, usize)> {
    let arg = |k: usize| pos.get(k).filter(|v| v.is_int()).map(|v| v.as_int());
    let len = s.len() as i64;
    // ASCII has one char per byte, other text walks chars from the end the index counts from, None past the end.
    let byte = |i: i64| -> Option<usize> {
        if ascii { if i < 0 { Some((len + i).max(0) as usize) } else { (i <= len).then_some(i as usize) } }
        else if i < 0 { Some(s.char_indices().rev().nth(usize::try_from(i.unsigned_abs() - 1).unwrap_or(usize::MAX)).map_or(0, |(b, _)| b)) }
        else { s.char_indices().map(|(b, _)| b).chain(iter::once(s.len())).nth(usize::try_from(i).unwrap_or(usize::MAX)) }
    };
    let lo = match arg(from) { Some(i) => byte(i)?, None => 0 };
    let hi = arg(from + 1).map_or(s.len(), |i| byte(i).unwrap_or(s.len()));
    (lo <= hi).then_some((lo, hi))
}

fn find_impl(vm: &mut VM, recv: Val, pos: &[Val], last: bool, raise: bool) -> Result<(), VmErr> {
    let sub = val_to_str(vm, pos[0])?;
    let ascii = vm.heap.str_is_ascii(recv);
    let s = recv_str_ref(vm, recv)?;
    let idx = window(s, ascii, pos, 1).and_then(|(lo, hi)| {
        let hay = &s[lo..hi];
        let local = if last { hay.rfind(sub.as_str()) } else { hay.find(sub.as_str()) };
        local.map(|b| if ascii { lo + b } else { s[..lo + b].chars().count() } as i64)
    }).unwrap_or(-1);
    if raise && idx < 0 { return Err(cold_value("substring not found")); }
    vm.push(Val::int(idx));
    Ok(())
}
pub fn find(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, false, false) }
pub fn rfind(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, true, false) }
pub fn index(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, false, true) }
pub fn rindex(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, true, true) }

pub fn count(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let sub = val_to_str(vm, pos[0])?;
    let ascii = vm.heap.str_is_ascii(recv);
    let s = recv_str_ref(vm, recv)?;
    let c = window(s, ascii, pos, 1).map_or(0, |(lo, hi)| {
        let hay = &s[lo..hi];
        if sub.is_empty() { hay.chars().count() + 1 } else { hay.matches(sub.as_str()).count() }
    });
    vm.push(Val::int(c as i64));
    Ok(())
}

// Whitespace split with at most `m` cuts and the rest kept whole, counted from the right when `rev`.
fn split_ws_max(s: &str, m: usize, rev: bool) -> Vec<String> {
    let mut chars: Vec<char> = s.chars().collect();
    if rev { chars.reverse(); }
    let piece = |cs: &[char]| -> String { if rev { cs.iter().rev().collect() } else { cs.iter().collect() } };
    let (n, mut i, mut out) = (chars.len(), 0, Vec::new());
    while i < n {
        while i < n && uni::is_space(chars[i]) { i += 1; }
        if i >= n { break; }
        if out.len() == m { out.push(piece(&chars[i..])); break; }
        let start = i;
        while i < n && !uni::is_space(chars[i]) { i += 1; }
        out.push(piece(&chars[start..i]));
    }
    if rev { out.reverse(); }
    out
}

// `str.split`/`rsplit([sep[, maxsplit]])`, `from_right` counts from the right and reverses sep-mode results.
fn split_impl(vm: &mut VM, recv: Val, pos: &[Val], from_right: bool) -> Result<(), VmErr> {
    let s = recv_str_ref(vm, recv)?;
    // Optional second arg is maxsplit (<0 means unlimited).
    let maxsplit: Option<usize> = match pos.get(1) {
        Some(n) if n.is_int() => { let m = n.as_int(); if m < 0 { None } else { Some(m as usize) } }
        Some(_) => return Err(cold_type("maxsplit must be an integer")),
        None => None,
    };
    let strs: Vec<String> = if pos.is_empty() || pos[0].is_none() {
        // No separator, split on runs of whitespace, dropping empties.
        match maxsplit {
            Some(m) => split_ws_max(s, m, from_right),
            None => s.split(uni::is_space).filter(|w| !w.is_empty()).map(String::from).collect(),
        }
    } else {
        let sep = val_to_str(vm, pos[0])?;
        if sep.is_empty() { return Err(cold_value("empty separator")); }
        match maxsplit {
            Some(m) if from_right => { let mut v: Vec<String> = s.rsplitn(m + 1, sep.as_str()).map(String::from).collect(); v.reverse(); v }
            Some(m) => s.splitn(m + 1, sep.as_str()).map(String::from).collect(),
            None => s.split(sep.as_str()).map(String::from).collect(),
        }
    };
    let parts: Vec<Val> = strs.into_iter().map(|p| vm.heap.alloc(HeapObj::Str(p))).collect::<Result<_, _>>()?;
    vm.alloc_and_push_list(parts)
}

pub fn split(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { split_impl(vm, recv, pos, false) }

pub fn join(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let items = vm.extract_iter(pos[0]).map_err(|e| if matches!(e, VmErr::TypeMsg(_)) { cold_type("join() argument must be iterable") } else { e })?;
    let sep = recv_str_ref(vm, recv)?;
    // The joined length is known before any copy, so a text past the memory limit fails first.
    let mut total = sep.len().saturating_mul(items.len().saturating_sub(1));
    for &v in &items {
        let Some(HeapObj::Str(s)) = vm.heap.try_get(v) else { return Err(cold_type("argument must be a string")) };
        total = total.saturating_add(s.len());
    }
    vm.heap.reserve(total)?;
    vm.heap.spend(total);
    let mut out = String::with_capacity(total);
    for (i, &v) in items.iter().enumerate() {
        if i > 0 { out.push_str(sep); }
        if let Some(HeapObj::Str(s)) = vm.heap.try_get(v) { out.push_str(s); }
    }
    vm.alloc_and_push_str(out)
}

pub fn replace(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let s = recv_str_ref(vm, recv)?;
    let old = val_to_str(vm, pos[0])?;
    let new = val_to_str(vm, pos[1])?;
    // A result past the memory limit fails before it allocates.
    if new.len() > old.len() {
        let hits = if old.is_empty() { s.chars().count() + 1 } else { s.matches(old.as_str()).count() };
        let hits = match pos.get(2) { Some(n) if n.is_int() && n.as_int() >= 0 => hits.min(n.as_int() as usize), _ => hits };
        vm.heap.reserve(s.len().saturating_add(hits.saturating_mul(new.len() - old.len())))?;
    }
    // Optional third arg is max replacements (<0 means all).
    let out = match pos.get(2) {
        Some(n) if n.is_int() && n.as_int() >= 0 => s.replacen(old.as_str(), new.as_str(), n.as_int() as usize),
        Some(n) if !n.is_int() => return Err(cold_type("replace count must be an integer")),
        _ => s.replace(old.as_str(), new.as_str()),
    };
    vm.heap.spend(out.len());
    vm.alloc_and_push_str(out)
}

// `str.rsplit([sep[, maxsplit]])`, like split but counts from the right.
pub fn rsplit(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { split_impl(vm, recv, pos, true) }

// `str.format(*args)` fills `{[idx][!r|!s|!a][:spec]}` fields, a spec may hold fields one level deep, keyword fields are not supported.
/* Where named fields come from, the keywords of `format` or the mapping of `format_map`. */
#[derive(Clone, Copy)]
pub(crate) enum Names<'a> { Kw(&'a [Val]), Map(Val) }

pub(crate) fn format(vm: &mut VM, recv: Val, pos: &[Val], names: Names, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
    let tmpl = recv_str(vm, recv)?;
    let (kw, map) = match names { Names::Kw(kw) => (kw, None), Names::Map(m) => (&[][..], Some(m)) };
    // Fields run user dunders, so the arguments stay rooted until the text is built.
    let out = vm.with_roots(pos.iter().chain(kw).copied().chain(map), |vm| expand_fields(vm, &tmpl, pos, names, &mut (0, None), 0, chunk))?;
    vm.alloc_and_push_str(out)
}

/* The value a field name reaches, a position or a name, then each `.attr` and `[key]`. */
fn field_value(vm: &mut VM, name: &str, pos: &[Val], names: Names, numbering: &mut (usize, Option<bool>), chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
    let head_end = name.find(['.', '[']).unwrap_or(name.len());
    let (head, mut rest) = name.split_at(head_end);
    let (auto, manual) = (&mut numbering.0, &mut numbering.1);
    let index = if head.is_empty() {
        if *manual == Some(true) { return Err(cold_value("cannot switch from manual field specification to automatic field numbering")); }
        *manual = Some(false);
        *auto += 1;
        Some(*auto - 1)
    } else if let Ok(i) = head.parse::<usize>() {
        if *manual == Some(false) { return Err(cold_value("cannot switch from automatic field numbering to manual field specification")); }
        *manual = Some(true);
        Some(i)
    } else { None };
    let mut v = match (index, names) {
        (Some(i), _) => *pos.get(i).ok_or_else(|| VmErr::Raised(s!("IndexError: Replacement index ", int i as i64, " out of range for positional args tuple")))?,
        (None, Names::Kw(kw)) => match kw.chunks(2).find(|p| matches!(vm.heap.try_get(p[0]), Some(HeapObj::Str(k)) if k == head)) {
            Some(p) => p[1],
            None => { let k = vm.heap.alloc(HeapObj::Str(head.into()))?; return Err(vm.key_error(k)); }
        },
        (None, Names::Map(m)) => { let k = vm.heap.alloc(HeapObj::Str(head.into()))?; item_of(vm, m, k, chunk)? }
    };
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            if end == 0 { return Err(cold_value("Empty attribute in format string")); }
            v = vm.with_roots([v], |vm| { vm.load_attr(v, &r[..end], chunk)?; vm.pop() })?;
            rest = &r[end..];
        } else {
            let close = rest.find(']').ok_or_else(|| cold_value("expected '}' before end of string"))?;
            let key = &rest[1..close];
            if key.is_empty() { return Err(cold_value("Empty attribute in format string")); }
            let k = match key.parse::<i64>() { Ok(i) if !key.starts_with(['-', '+']) => Val::int(i), _ => vm.heap.alloc(HeapObj::Str(key.into()))? };
            v = item_of(vm, v, k, chunk)?;
            rest = &rest[close + 1..];
            if !rest.is_empty() && !rest.starts_with(['.', '[']) { return Err(cold_value("Only '.' or '[' may follow ']' in format field specifier")); }
        }
    }
    Ok(v)
}

/* `obj[key]` with a user `__getitem__` first, the way a subscript reads it. */
fn item_of(vm: &mut VM, obj: Val, key: Val, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
    if let Some(v) = vm.try_call_dunder(obj, "__getitem__", &[key], chunk)? { return Ok(v); }
    // A class over dict answers a missing key through its `__missing__`.
    if vm.heap.classed(obj).is_some() && let HeapObj::Dict(d) = vm.heap.get(obj) && d.borrow().get(&key, &vm.heap).is_none()
        && let Some(v) = vm.try_call_dunder(obj, "__missing__", &[key], chunk)? { return Ok(v); }
    vm.get_item_builtin(obj, key)?;
    vm.pop()
}

/* Fills the fields of `tmpl`, where `depth` 1 is a spec and a field nested past it is refused as Python refuses it. */
fn expand_fields(vm: &mut VM, tmpl: &str, pos: &[Val], names: Names, numbering: &mut (usize, Option<bool>), depth: u8, chunk: &crate::parser::SSAChunk) -> Result<String, VmErr> {
    if depth > 1 { return Err(cold_value("Max string recursion exceeded")); }
    let chars: Vec<char> = tmpl.chars().collect();
    let mut out = String::with_capacity(tmpl.len());
    let mut ci = 0;
    while ci < chars.len() {
        let c = chars[ci];
        if c == '{' {
            if chars.get(ci + 1) == Some(&'{') { out.push('{'); ci += 2; continue; }
            // The field ends at its matching brace, so a field nested in its spec stays inside it.
            let (mut j, mut nest) = (ci + 1, 0usize);
            let mut field = String::new();
            while j < chars.len() && (chars[j] != '}' || nest > 0) {
                match chars[j] { '{' => nest += 1, '}' => nest -= 1, _ => {} }
                field.push(chars[j]);
                j += 1;
            }
            if j >= chars.len() { return Err(cold_value("Single '{' encountered in format string")); }
            ci = j + 1;
            // The name ends at the first `!` or `:` outside its brackets.
            let mut depth_br = 0i32;
            let cut = field.char_indices().find(|&(_, ch)| { match ch { '[' => depth_br += 1, ']' => depth_br -= 1, _ => {} } depth_br == 0 && matches!(ch, '!' | ':') }).map_or(field.len(), |(i, _)| i);
            let (name, tail) = field.split_at(cut);
            let (conv, spec) = match tail.strip_prefix('!') {
                Some(t) => match t.split_once(':') { Some((c, sp)) => (Some(c), sp.to_string()), None => (Some(t), String::new()) },
                None => (None, tail.strip_prefix(':').unwrap_or("").to_string()),
            };
            let val = field_value(vm, name, pos, names, numbering, chunk)?;
            // The fields of a spec fill after the field that holds it, numbered on from it.
            let spec = if spec.contains('{') { vm.with_roots([val], |vm| expand_fields(vm, &spec, pos, names, numbering, depth + 1, chunk))? } else { spec };
            // A conversion renders to a string first, then the spec applies to that.
            let text = match conv {
                None => None,
                Some("r") => Some(vm.repr_op(val, chunk)?),
                Some("s") => Some(vm.display_op(val, chunk)?),
                Some("a") => Some(crate::vm::format_spec::ascii_escape(&vm.repr_op(val, chunk)?)),
                Some(c) => return Err(VmErr::Raised(s!("ValueError: Unknown conversion specifier ", str c))),
            };
            let target = match text { Some(t) => vm.heap.alloc(HeapObj::Str(t))?, None => val };
            let rendered = vm.format_op(target, &spec, chunk)?;
            out.push_str(&rendered);
        } else if c == '}' {
            if chars.get(ci + 1) == Some(&'}') { out.push('}'); ci += 2; continue; }
            return Err(cold_value("Single '}' encountered in format string"));
        } else {
            out.push(c);
            ci += 1;
        }
    }
    Ok(out)
}

str_transform!(casefold, |s: &str| s.to_lowercase());
str_transform!(swapcase, |s: &str| s.chars().map(|c| if c.is_uppercase() { c.to_lowercase().to_string() } else if c.is_lowercase() { c.to_uppercase().to_string() } else { c.to_string() }).collect::<String>());

/* Parse a pad-method `(width[, fill])` arg pair, `width_err` names the method for the width TypeError. */
fn width_and_fill(vm: &mut VM, pos: &[Val], width_err: &'static str) -> Result<(usize, char), VmErr> {
    if !pos[0].is_int() { return Err(cold_type(width_err)); }
    let width = pos[0].as_int().max(0) as usize;
    // User-controlled width drives the output size, cap it so a huge value errors instead of aborting in the allocator.
    vm.heap.reserve(width)?;
    vm.heap.spend(width);
    let fill = if pos.len() > 1 {
        let f = val_to_str(vm, pos[1])?;
        let mut cs = f.chars();
        match (cs.next(), cs.next()) { (Some(c), None) => c, _ => return Err(cold_type("The fill character must be exactly one character long")) }
    } else { ' ' };
    Ok((width, fill))
}

// `str.ljust`/`rjust`/`center(width[, fill])` pad to width in code points, an odd centre pad leans left on an odd width.
fn justify(vm: &mut VM, recv: Val, pos: &[Val], align: u8) -> Result<(), VmErr> {
    let (width, fill) = width_and_fill(vm, pos, "width must be an integer")?;
    let s = recv_str_ref(vm, recv)?;
    let pad = width.saturating_sub(s.chars().count());
    let left = match align { b'<' => 0, b'>' => pad, _ => pad / 2 + (pad & width & 1) };
    let out: String = iter::repeat_n(fill, left).chain(s.chars()).chain(iter::repeat_n(fill, pad - left)).collect();
    vm.alloc_and_push_str(out)
}
pub fn ljust(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { justify(vm, recv, pos, b'<') }
pub fn rjust(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { justify(vm, recv, pos, b'>') }
pub fn center(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { justify(vm, recv, pos, b'^') }

pub fn expandtabs(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let s = recv_str_ref(vm, recv)?;
    // Holds tabsize in a C int, out-of-range is an OverflowError before any expansion.
    let ts = match pos.first() {
        Some(n) if n.is_int() => {
            let raw = n.as_int();
            if !(i32::MIN as i64..=i32::MAX as i64).contains(&raw) { return Err(cold_overflow()); }
            raw.max(0) as usize
        }
        _ => 8,
    };
    let limit = vm.heap.room();
    let mut out = String::with_capacity(s.len());
    let mut col = 0usize;
    for c in s.chars() {
        match c {
            // A large tabsize can blow the column count past the memory left, bail before materialising the spaces.
            '\t' => {
                let n = if ts == 0 { 0 } else { ts - (col % ts) };
                if col.saturating_add(n) > limit { return Err(cold_heap()); }
                for _ in 0..n { out.push(' '); }
                col += n;
            }
            '\n' | '\r' => { out.push(c); col = 0; }
            _ => { if col >= limit { return Err(cold_heap()); } out.push(c); col += 1; }
        }
    }
    vm.heap.spend(out.len());
    vm.alloc_and_push_str(out)
}

pub fn istitle(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let s = recv_str_ref(vm, recv)?;
    // Titlecased means every cased run starts uppercase and continues lowercase, needs >=1 cased char.
    let mut prev_cased = false;
    let mut any_cased = false;
    let mut ok = true;
    for c in s.chars() {
        if c.is_uppercase() || uni::is_titlecase(c) {
            if prev_cased { ok = false; break; }
            prev_cased = true; any_cased = true;
        } else if c.is_lowercase() {
            if !prev_cased { ok = false; break; }
            prev_cased = true; any_cased = true;
        } else {
            prev_cased = false;
        }
    }
    vm.push(Val::bool(ok && any_cased));
    Ok(())
}

// `str.removeprefix` / `removesuffix`, strip if present, else return unchanged.
pub fn removeprefix(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let p = val_to_str(vm, pos[0])?;
    // A string without the prefix is its own result.
    match recv_str_ref(vm, recv)?.strip_prefix(p.as_str()).map(String::from) { Some(out) => vm.alloc_and_push_str(out), None => { vm.push(recv); Ok(()) } }
}

pub fn removesuffix(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let suf = val_to_str(vm, pos[0])?;
    match recv_str_ref(vm, recv)?.strip_suffix(suf.as_str()).map(String::from) { Some(out) => vm.alloc_and_push_str(out), None => { vm.push(recv); Ok(()) } }
}

// `str.splitlines(keepends=False)`, split on \n / \r / \r\n and the other line breaks, each kept on its line when `keepends` holds.
pub fn splitlines(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    // line boundaries \n \r \r\n \v \f \x1c \x1d \x1e \x85.
    const BREAKS: &[char] = &['\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}'];
    let keep = pos.first().is_some_and(|&k| vm.truthy(k));
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = recv_str_ref(vm, recv)?.chars().peekable();
    while let Some(c) = chars.next() {
        if BREAKS.contains(&c) {
            if keep { cur.push(c); }
            // \r\n counts as a single boundary.
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
                if keep { cur.push('\n'); }
            }
            lines.push(core::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() { lines.push(cur); }
    let parts = lines.into_iter().map(|l| vm.heap.alloc(HeapObj::Str(l))).collect::<Result<Vec<_>, _>>()?;
    vm.alloc_and_push_list(parts)
}

// `str.partition` / `rpartition`, (head, sep, tail), on miss returns (s,"","") / ("","",s).
fn partition_impl(vm: &mut VM, recv: Val, pos: &[Val], from_right: bool) -> Result<(), VmErr> {
    let sep = val_to_str(vm, pos[0])?;
    if sep.is_empty() { return Err(cold_value("empty separator")); }
    let s = recv_str_ref(vm, recv)?;
    let hit = if from_right { s.rfind(sep.as_str()) } else { s.find(sep.as_str()) };
    let pieces = hit.map(|i| (s[..i].to_string(), s[i + sep.len()..].to_string()));
    let empty = |vm: &mut VM| vm.heap.alloc(HeapObj::Str(String::new()));
    let (av, bv, cv) = match pieces {
        Some((a, c)) => (vm.heap.alloc(HeapObj::Str(a))?, vm.heap.alloc(HeapObj::Str(sep))?, vm.heap.alloc(HeapObj::Str(c))?),
        // A miss keeps the original string whole on its side.
        None if from_right => (empty(vm)?, empty(vm)?, recv),
        None => (recv, empty(vm)?, empty(vm)?),
    };
    vm.alloc_and_push_tuple(vec![av, bv, cv])
}
pub fn partition(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { partition_impl(vm, recv, pos, false) }
pub fn rpartition(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { partition_impl(vm, recv, pos, true) }

// str padding.
pub fn zfill(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    if !pos[0].is_int() { return Err(cold_type("zfill() requires an integer argument")); }
    let width = pos[0].as_int().max(0) as usize;
    // User-controlled width drives the output size, cap it so a huge value errors instead of aborting in the allocator.
    vm.heap.reserve(width)?;
    vm.heap.spend(width);
    let s = recv_str_ref(vm, recv)?;
    let nchars = s.chars().count();
    if nchars >= width { vm.push(recv); return Ok(()); }
    let pad = "0".repeat(width - nchars);
    let out = if s.starts_with('+') || s.starts_with('-') { s[..1].to_string() + &pad + &s[1..] } else { pad + s };
    vm.alloc_and_push_str(out)
}
