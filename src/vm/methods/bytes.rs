use super::prelude::*;
use crate::vm::opcodes::subscript::slice_bounds;

// `bytes.decode([encoding[, errors]])`. 'strict' raises on invalid UTF-8, 'ignore'/'replace' recover.
pub fn decode<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let (encoding, errors) = super::string::codec_args(vm, pos)?;
    let text = crate::vm::codecs::decode(recv_bytes_ref(vm, recv)?, encoding, errors)?;
    let v = vm.heap_mut().alloc(HeapObj::Str(text))?;
    vm.push(v); Ok(())
}

// `bytes.hex([sep[, bytes_per_sep]])`, lowercase hex of every byte, `sep` between groups counted from the right, or the left when negative.
pub fn hex<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let sep = match pos.first().filter(|v| !v.is_none()) {
        None => None,
        Some(&v) => {
            // A str separator counts its characters, so one non-ASCII character is refused for being so.
            let units: Vec<u32> = match vm.heap().try_get(v) {
                Some(HeapObj::Str(t)) => t.chars().map(|c| c as u32).collect(),
                _ => match vm.heap().octets(v) { Some(b) => b.iter().map(|&c| c as u32).collect(), None => return Err(VmErr::TypeMsg(crate::s!("object of type '", str vm.type_name(v), "' has no len()"))) },
            };
            if units.len() != 1 { return Err(cold_value("sep must be length 1.")); }
            if units[0] > 0x7F { return Err(cold_value("sep must be ASCII.")); }
            Some(units[0] as u8 as char)
        }
    };
    let group = match pos.get(1) { Some(&n) => index_arg(vm, n)?, None => 1 };
    let buf = recv_bytes_ref(vm, recv)?;
    let width = group.unsigned_abs() as usize;
    let mut out = alloc::string::String::with_capacity(buf.len() * 3);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, &b) in buf.iter().enumerate() {
        // A group boundary falls where the bytes before it divide by the group size, from whichever end it counts.
        let before = if group < 0 { i } else { buf.len() - i };
        if let Some(c) = sep && i > 0 && width > 0 && before % width == 0 { out.push(c); }
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    vm.alloc_and_push_str(out)
}

// bytes-only, strings go through `string::startswith`.
pub fn startswith<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { affix_impl(vm, recv, pos, false) }
pub fn endswith<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { affix_impl(vm, recv, pos, true) }

/* `startswith` and `endswith`, of one affix or a tuple of them, within the optional start and end. */
fn affix_impl<H: Host>(vm: &mut H, recv: Val, pos: &[Val], end: bool) -> Result<(), VmErr> {
    let affixes = match vm.heap().try_get(pos[0]) { Some(HeapObj::Tuple(items)) => items.to_vec(), _ => alloc::vec![pos[0]] };
    let buf = recv_bytes_ref(vm, recv)?;
    let span = window(buf, pos, vm.heap())?;
    let mut hit = false;
    for affix in affixes {
        let Ok(p) = recv_bytes_ref(vm, affix) else {
            let which = if end { "endswith" } else { "startswith" };
            return Err(VmErr::TypeMsg(crate::s!(str which, " first arg must be bytes or a tuple of bytes, not ", str vm.type_name(affix))));
        };
        hit |= span.is_some_and(|(s, e)| if end { buf[s..e].ends_with(p) } else { buf[s..e].starts_with(p) });
    }
    vm.push(Val::bool(hit));
    Ok(())
}

/* Shared search for find/index and their right forms, `raise` turns a miss into ValueError. */
pub fn find<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, false, false) }
pub fn index<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, false, true) }
pub fn rfind<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, true, false) }
pub fn rindex<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, true, true) }

// `bytes.fromhex(s)` classmethod, parse pairs of hex digits, skipping ASCII whitespace between pairs.
pub fn fromhex<H: Host>(vm: &mut H, _recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let s = val_to_str(vm, pos[0], "fromhex() argument must be str or bytes-like")?;
    let mut out: Vec<u8> = Vec::new();
    let mut hi: Option<u8> = None;
    for (at, c) in s.chars().enumerate() {
        if c.is_ascii_whitespace() && hi.is_none() { continue; }
        let Some(d) = c.to_digit(16) else {
            return Err(VmErr::Raised(crate::s!("ValueError: non-hexadecimal number found in fromhex() arg at position ", int at as i64)));
        };
        match hi { None => hi = Some(d as u8), Some(h) => { out.push((h << 4) | d as u8); hi = None; } }
    }
    if hi.is_some() { return Err(cold_value("fromhex() arg must contain an even number of hexadecimal digits")); }
    give(vm, out)
}

pub fn lower<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let mut buf = recv_bytes(vm, recv)?;
    buf.make_ascii_lowercase();
    give(vm, buf)
}

pub fn upper<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let mut buf = recv_bytes(vm, recv)?;
    buf.make_ascii_uppercase();
    give(vm, buf)
}

// bytes strip, ASCII whitespace, or any byte in the optional argument.
fn bstrip<H: Host>(vm: &mut H, recv: Val, pos: &[Val], left: bool, right: bool) -> Result<(), VmErr> {
    let buf = recv_bytes_ref(vm, recv)?;
    let chars = match pos.first() { Some(&a) => Some(recv_bytes_ref(vm, a)?), None => None };
    let strip = |b: u8| -> bool { match &chars { Some(set) => set.contains(&b), None => is_ws(b) } };
    let mut s = 0usize;
    let mut e = buf.len();
    if left { while s < e && strip(buf[s]) { s += 1; } }
    if right { while e > s && strip(buf[e - 1]) { e -= 1; } }
    let out = buf[s..e].to_vec();
    give(vm, out)
}
pub fn strip<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { bstrip(vm, recv, pos, true, true) }
pub fn lstrip<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { bstrip(vm, recv, pos, true, false) }
pub fn rstrip<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { bstrip(vm, recv, pos, false, true) }

pub fn join<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let items = vm.extract_iter(pos[0]).map_err(|e| if matches!(e, VmErr::TypeMsg(_)) { cold_type("can only join an iterable of bytes") } else { e })?;
    let sep = recv_bytes_ref(vm, recv)?;
    let mut out: Vec<u8> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if i > 0 { out.extend_from_slice(sep); }
        out.extend_from_slice(recv_bytes_ref(vm, *it)?);
    }
    give(vm, out)
}

/* What a search looks for, bytes-like or one byte by its value. */
fn needle<H: Host>(vm: &H, v: Val) -> Result<Vec<u8>, VmErr> {
    if v.is_int() || v.is_bool() {
        return u8::try_from(index_arg(vm, v)?).map(|b| alloc::vec![b]).map_err(|_| cold_value("byte must be in range(0, 256)"));
    }
    bytes_like(vm, v).map(alloc::borrow::Cow::into_owned)
}

/* A bytes-like argument, refused the way Python names any other value. */
fn bytes_like<'v, H: Host>(vm: &'v H, v: Val) -> Result<alloc::borrow::Cow<'v, [u8]>, VmErr> {
    vm.heap().octets(v).ok_or_else(|| VmErr::TypeMsg(crate::s!("a bytes-like object is required, not '", str vm.type_name(v), "'")))
}

/* Start of the last `sub` in `buf`. */
fn rfind_in(buf: &[u8], sub: &[u8]) -> Option<usize> {
    if sub.is_empty() { return Some(buf.len()); }
    buf.windows(sub.len()).rposition(|w| w == sub)
}

/* Start of `sub` in `buf` at or after `from`. */
fn find_from(buf: &[u8], sub: &[u8], from: usize) -> Option<usize> {
    if sub.is_empty() { return (from <= buf.len()).then_some(from); }
    buf.get(from..)?.windows(sub.len()).position(|w| w == sub).map(|i| i + from)
}

/* `buf[start:end]` from the optional args after `sub`, clamped like a slice, None when start lies past the end or past stop. */
fn window(buf: &[u8], pos: &[Val], heap: &crate::value::HeapPool) -> Result<Option<(usize, usize)>, VmErr> {
    let arg = |i: usize| pos.get(i).map_or(Val::none(), |&v| crate::frontend::int::clip_index(heap, v));
    let (s, e, _) = slice_bounds(arg(1), arg(2), Val::none(), buf.len() as i64)?;
    let past = arg(1).is_int() && arg(1).as_int() > buf.len() as i64;
    Ok((!past && s <= e).then_some((s as usize, e as usize)))
}

fn find_impl<H: Host>(vm: &mut H, recv: Val, pos: &[Val], last: bool, raise: bool) -> Result<(), VmErr> {
    let sub = needle(vm, pos[0])?;
    let buf = recv_bytes_ref(vm, recv)?;
    let found = window(buf, pos, vm.heap())?.and_then(|(s, e)| if last { rfind_in(&buf[s..e], &sub).map(|i| i + s) } else { find_from(&buf[..e], &sub, s) });
    let idx = match found {
        Some(i) => i as i64,
        None if raise => return Err(cold_value("subsection not found")),
        None => -1,
    };
    vm.push(Val::int(idx));
    Ok(())
}

pub fn count<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let sub = needle(vm, pos[0])?;
    let (buf, sub) = (recv_bytes_ref(vm, recv)?, &sub[..]);
    let Some((s, e)) = window(buf, pos, vm.heap())? else { vm.push(Val::int(0)); return Ok(()) };
    let hay = &buf[s..e];
    let n = if sub.is_empty() { hay.len() + 1 } else {
        let (mut n, mut i) = (0, 0);
        while let Some(j) = find_from(hay, sub, i) { n += 1; i = j + sub.len(); }
        n
    };
    vm.push(Val::int(n as i64));
    Ok(())
}

// `bytes.replace(old, new[, count])`, a result past the memory limit fails before it allocates.
pub fn replace<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let (buf, old, new) = (recv_bytes_ref(vm, recv)?, recv_bytes_ref(vm, pos[0])?, recv_bytes_ref(vm, pos[1])?);
    let max = match pos.get(2) { Some(n) if n.is_int() && n.as_int() >= 0 => n.as_int() as usize, _ => usize::MAX };
    let mut hits = Vec::new();
    let mut i = 0;
    // An empty `old` matches before every byte and once at the end.
    while hits.len() < max && let Some(j) = find_from(buf, old, i) {
        hits.push(j);
        i = j + old.len().max(1);
    }
    if new.len() > old.len() {
        vm.heap().reserve(buf.len().saturating_add(hits.len().saturating_mul(new.len() - old.len())))?;
    }
    let mut out: Vec<u8> = Vec::with_capacity(buf.len() + hits.len() * new.len());
    let mut at = 0;
    for j in hits {
        out.extend_from_slice(&buf[at..j]);
        out.extend_from_slice(new);
        at = j + old.len();
    }
    out.extend_from_slice(&buf[at..]);
    give(vm, out)
}

// `bytes.split([sep[, maxsplit]])`, no separator splits on runs of ASCII whitespace.
pub fn split<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let buf = recv_bytes_ref(vm, recv)?;
    let max = match pos.get(1) { Some(n) if n.is_int() && n.as_int() >= 0 => n.as_int() as usize, _ => usize::MAX };
    let mut pieces: Vec<&[u8]> = Vec::new();
    match pos.first().filter(|v| !v.is_none()) {
        Some(&sep) => {
            let sep = recv_bytes_ref(vm, sep)?;
            if sep.is_empty() { return Err(cold_value("empty separator")); }
            let mut at = 0;
            while pieces.len() < max && let Some(j) = find_from(buf, sep, at) {
                pieces.push(&buf[at..j]);
                at = j + sep.len();
            }
            pieces.push(&buf[at..]);
        }
        None => {
            let skip = |b: &[u8]| -> usize { b.iter().position(|&c| !is_ws(c)).unwrap_or(b.len()) };
            let mut rest = &buf[skip(buf)..];
            while !rest.is_empty() {
                // Past `maxsplit` the rest stays whole, trailing whitespace included.
                if pieces.len() == max { pieces.push(rest); break; }
                let end = rest.iter().position(|&b| is_ws(b)).unwrap_or(rest.len());
                pieces.push(&rest[..end]);
                rest = &rest[end..];
                rest = &rest[skip(rest)..];
            }
        }
    }
    let owned: Vec<Vec<u8>> = pieces.into_iter().map(<[u8]>::to_vec).collect();
    push_parts(vm, owned)
}

/* What the bytes methods count as whitespace, the vertical tab included. */
fn is_ws(b: u8) -> bool { matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) }

/* Pushes `buf` as the bytes a method gives, which a bytearray receiver gets back as a bytearray. */
fn give<H: Host>(vm: &mut H, buf: Vec<u8>) -> Result<(), VmErr> {
    let v = vm.heap_mut().alloc(HeapObj::Bytes(buf))?;
    vm.push(v);
    Ok(())
}

// bytes predicates, class tests need a non-empty value and case tests a cased byte.
macro_rules! bytes_pred {
    ($($name:ident => $f:expr;)*) => { $(
        pub fn $name<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
            let hit = $f(recv_bytes_ref(vm, recv)?);
            vm.push(Val::bool(hit));
            Ok(())
        }
    )* };
}
fn all_bytes(b: &[u8], f: fn(&u8) -> bool) -> bool { !b.is_empty() && b.iter().all(f) }
bytes_pred! {
    isalnum => |b: &[u8]| all_bytes(b, u8::is_ascii_alphanumeric);
    isalpha => |b: &[u8]| all_bytes(b, u8::is_ascii_alphabetic);
    isdigit => |b: &[u8]| all_bytes(b, u8::is_ascii_digit);
    isspace => |b: &[u8]| all_bytes(b, |&c| is_ws(c));
    isascii => <[u8]>::is_ascii;
    islower => |b: &[u8]| b.iter().any(u8::is_ascii_lowercase) && !b.iter().any(u8::is_ascii_uppercase);
    isupper => |b: &[u8]| b.iter().any(u8::is_ascii_uppercase) && !b.iter().any(u8::is_ascii_lowercase);
    istitle => |b: &[u8]| { let t = titled(b); b.iter().any(u8::is_ascii_alphabetic) && t == b };
}

/* `b` with each run of letters started uppercase and continued lowercase. */
fn titled(b: &[u8]) -> Vec<u8> {
    let mut prev = false;
    b.iter().map(|&c| { let out = if prev { c.to_ascii_lowercase() } else { c.to_ascii_uppercase() }; prev = c.is_ascii_alphabetic(); out }).collect()
}

pub fn title<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> { let t = titled(recv_bytes_ref(vm, recv)?); give(vm, t) }

pub fn capitalize<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let mut buf = recv_bytes(vm, recv)?;
    buf.make_ascii_lowercase();
    if let Some(first) = buf.first_mut() { first.make_ascii_uppercase(); }
    give(vm, buf)
}

pub fn swapcase<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let buf = recv_bytes_ref(vm, recv)?.iter().map(|&c| if c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() }).collect();
    give(vm, buf)
}

// `ljust`/`rjust`/`center(width[, fill])`, the fill one byte, an odd centre pad leaning left on an odd width.
fn justify<H: Host>(vm: &mut H, recv: Val, pos: &[Val], name: &str, align: u8) -> Result<(), VmErr> {
    let width = width_arg(vm, pos[0])?;
    let fill = match pos.get(1) {
        None => b' ',
        Some(&f) => match vm.heap().octets(f) {
            Some(b) if b.len() == 1 => b[0],
            Some(b) => return Err(VmErr::TypeMsg(crate::s!(str name, "(): argument 2 must be a byte string of length 1, not a ", str vm.type_name(f), " object of length ", int b.len() as i64))),
            None => return Err(VmErr::TypeMsg(crate::s!(str name, "() argument 2 must be a byte string of length 1, not ", str vm.type_name(f)))),
        },
    };
    let buf = recv_bytes_ref(vm, recv)?;
    let pad = width.saturating_sub(buf.len());
    let left = match align { b'<' => 0, b'>' => pad, _ => pad / 2 + (pad & width & 1) };
    let out = core::iter::repeat_n(fill, left).chain(buf.iter().copied()).chain(core::iter::repeat_n(fill, pad - left)).collect();
    give(vm, out)
}
pub fn ljust<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { justify(vm, recv, pos, "ljust", b'<') }
pub fn rjust<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { justify(vm, recv, pos, "rjust", b'>') }
pub fn center<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { justify(vm, recv, pos, "center", b'^') }

// `bytes.zfill(width)`, zeros after any sign up to the width.
pub fn zfill<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let width = width_arg(vm, pos[0])?;
    let buf = recv_bytes_ref(vm, recv)?;
    let signed = matches!(buf.first(), Some(b'+' | b'-')) as usize;
    let zeros = width.saturating_sub(buf.len());
    let out = buf[..signed].iter().copied().chain(core::iter::repeat_n(b'0', zeros)).chain(buf[signed..].iter().copied()).collect();
    give(vm, out)
}

pub fn expandtabs<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let ts = tab_size(vm, pos)?;
    let out = expand_tabs(recv_bytes_ref(vm, recv)?.iter().copied(), *b"\t\n\r ", ts, vm.heap().room())?;
    vm.heap().spend(out.len());
    give(vm, out)
}

// `bytes.partition(sep)` and `rpartition`, the three parts around the first or last `sep`.
fn partition_impl<H: Host>(vm: &mut H, recv: Val, pos: &[Val], last: bool) -> Result<(), VmErr> {
    let sep = bytes_like(vm, pos[0])?.into_owned();
    if sep.is_empty() { return Err(cold_value("empty separator")); }
    let buf = recv_bytes_ref(vm, recv)?;
    let at = if last { rfind_in(buf, &sep) } else { find_from(buf, &sep, 0) };
    let parts: [Vec<u8>; 3] = match at {
        Some(i) => [buf[..i].to_vec(), sep.clone(), buf[i + sep.len()..].to_vec()],
        None if last => [Vec::new(), Vec::new(), buf.to_vec()],
        None => [buf.to_vec(), Vec::new(), Vec::new()],
    };
    let mut items = Vec::with_capacity(3);
    for p in parts { items.push(vm.heap_mut().alloc(HeapObj::Bytes(p))?); }
    vm.alloc_and_push_tuple(items)
}
pub fn partition<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { partition_impl(vm, recv, pos, false) }
pub fn rpartition<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> { partition_impl(vm, recv, pos, true) }

pub fn removeprefix<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let affix = bytes_like(vm, pos[0])?.into_owned();
    let buf = recv_bytes_ref(vm, recv)?;
    let out = buf.strip_prefix(&affix[..]).unwrap_or(buf).to_vec();
    give(vm, out)
}

pub fn removesuffix<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let affix = bytes_like(vm, pos[0])?.into_owned();
    let buf = recv_bytes_ref(vm, recv)?;
    let out = buf.strip_suffix(&affix[..]).unwrap_or(buf).to_vec();
    give(vm, out)
}

// `bytes.rsplit([sep[, maxsplit]])`, `split` counted from the right.
pub fn rsplit<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let max = match pos.get(1) { Some(n) if n.is_int() && n.as_int() >= 0 => n.as_int() as usize, _ => usize::MAX };
    let sep = match pos.first().filter(|v| !v.is_none()) { Some(&s) => Some(bytes_like(vm, s)?.into_owned()), None => None };
    let buf = recv_bytes_ref(vm, recv)?;
    let mut pieces: Vec<&[u8]> = Vec::new();
    match sep {
        Some(sep) => {
            if sep.is_empty() { return Err(cold_value("empty separator")); }
            let mut end = buf.len();
            while pieces.len() < max && let Some(j) = rfind_in(&buf[..end], &sep) {
                pieces.push(&buf[j + sep.len()..end]);
                end = j;
            }
            pieces.push(&buf[..end]);
        }
        None => {
            let mut rest = &buf[..buf.iter().rposition(|&b| !is_ws(b)).map_or(0, |i| i + 1)];
            while !rest.is_empty() {
                // Past `maxsplit` the rest stays whole, leading whitespace included.
                if pieces.len() == max { pieces.push(rest); break; }
                let start = rest.iter().rposition(|&b| is_ws(b)).map_or(0, |i| i + 1);
                pieces.push(&rest[start..]);
                rest = &rest[..rest[..start].iter().rposition(|&b| !is_ws(b)).map_or(0, |i| i + 1)];
            }
        }
    }
    let owned: Vec<Vec<u8>> = pieces.into_iter().rev().map(<[u8]>::to_vec).collect();
    push_parts(vm, owned)
}

// `bytes.splitlines([keepends])`, broken at `\n`, `\r` and `\r\n` only.
pub fn splitlines<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let keep = pos.first().is_some_and(|&v| vm.truthy(v));
    let buf = recv_bytes_ref(vm, recv)?;
    let (mut lines, mut at) = (Vec::new(), 0);
    while at < buf.len() {
        let end = buf[at..].iter().position(|&b| b == b'\n' || b == b'\r').map_or(buf.len(), |i| at + i);
        let next = if buf.get(end..end + 2) == Some(b"\r\n") { end + 2 } else { (end + 1).min(buf.len()) };
        lines.push(buf[at..if keep { next } else { end }].to_vec());
        at = next;
    }
    push_parts(vm, lines)
}

/* Pushes a list of bytes parts. */
fn push_parts<H: Host>(vm: &mut H, parts: Vec<Vec<u8>>) -> Result<(), VmErr> {
    let mut items = Vec::with_capacity(parts.len());
    for p in parts { items.push(vm.heap_mut().alloc(HeapObj::Bytes(p))?); }
    vm.alloc_and_push_list(items)
}

// `bytes.maketrans(from, to)`, the 256-byte table `translate` takes.
pub fn maketrans<H: Host>(vm: &mut H, _recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let (from, to) = (bytes_like(vm, pos[0])?.into_owned(), bytes_like(vm, pos[1])?.into_owned());
    if from.len() != to.len() { return Err(cold_value("maketrans arguments must have same length")); }
    let mut table: Vec<u8> = (0..=255).collect();
    for (a, b) in from.into_iter().zip(to) { table[a as usize] = b; }
    give(vm, table)
}

// `bytes.translate(table[, delete])`, every byte through the table, or kept with None, those in `delete` dropped.
pub fn translate<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let table = match pos.first().filter(|v| !v.is_none()) { Some(&t) => Some(bytes_like(vm, t)?.into_owned()), None => None };
    if table.as_ref().is_some_and(|t| t.len() != 256) { return Err(cold_value("translation table must be 256 characters long")); }
    let delete = match pos.get(1) { Some(&d) => bytes_like(vm, d)?.into_owned(), None => Vec::new() };
    let out = recv_bytes_ref(vm, recv)?.iter().filter(|b| !delete.contains(b)).map(|&b| table.as_ref().map_or(b, |t| t[b as usize])).collect();
    give(vm, out)
}
