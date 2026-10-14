use super::prelude::*;
use crate::vm::opcodes::subscript::slice_bounds;

// `bytes.decode([encoding[, errors]])`. 'strict' raises on invalid UTF-8, 'ignore'/'replace' recover.
pub fn decode(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let buf = recv_bytes_ref(vm, recv)?;
    if let Some(arg) = pos.first() {
        let enc = val_to_str(vm, *arg)?;
        if !matches!(enc.as_str(), "utf-8" | "utf8" | "ascii") {
            return Err(cold_value("unsupported encoding (expected 'utf-8' or 'ascii')"));
        }
    }
    let errors = match pos.get(1) {
        Some(a) => val_to_str(vm, *a)?,
        None => alloc::string::String::from("strict"),
    };
    let text = match errors.as_str() {
        "strict" => core::str::from_utf8(buf)
            .map_err(|_| VmErr::Raised("UnicodeDecodeError: invalid UTF-8 in bytes.decode()".into()))?.to_string(),
        "ignore" => decode_recover(buf, false),
        "replace" => decode_recover(buf, true),
        _ => return Err(cold_value("unknown error handler (expected 'strict', 'ignore', or 'replace')")),
    };
    let v = vm.heap.alloc(HeapObj::Str(text))?;
    vm.push(v); Ok(())
}

// Decode dropping invalid bytes (replace=false) or substituting U+FFFD (replace=true).
fn decode_recover(buf: &[u8], replace: bool) -> alloc::string::String {
    let mut out = alloc::string::String::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        match core::str::from_utf8(&buf[i..]) {
            Ok(s) => { out.push_str(s); break; }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(core::str::from_utf8(&buf[i..i + valid]).unwrap());
                if replace { out.push('\u{FFFD}'); }
                match e.error_len() {
                    Some(n) => i += valid + n,
                    None => break, // incomplete sequence at the end
                }
            }
        }
    }
    out
}

// `bytes.hex()`, lowercase hex of every byte. No separator.
pub fn hex(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let buf = recv_bytes_ref(vm, recv)?;
    let mut out = alloc::string::String::with_capacity(buf.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in buf {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    let v = vm.heap.alloc(HeapObj::Str(out))?;
    vm.push(v); Ok(())
}

// bytes-only, strings go through `string::startswith`.
pub fn startswith(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { affix_impl(vm, recv, pos, false) }
pub fn endswith(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { affix_impl(vm, recv, pos, true) }

/* `startswith` and `endswith`, of one affix or a tuple of them, within the optional start and end. */
fn affix_impl(vm: &mut VM, recv: Val, pos: &[Val], end: bool) -> Result<(), VmErr> {
    let affixes = match vm.heap.try_get(pos[0]) { Some(HeapObj::Tuple(items)) => items.to_vec(), _ => alloc::vec![pos[0]] };
    let buf = recv_bytes_ref(vm, recv)?;
    // A start past the end matches nothing, not even an empty affix.
    let past = pos.get(1).is_some_and(|v| v.is_int() && v.as_int() > buf.len() as i64);
    let (s, e) = window(buf, pos)?;
    let mut hit = false;
    for affix in affixes {
        let Ok(p) = recv_bytes_ref(vm, affix) else {
            let which = if end { "endswith" } else { "startswith" };
            return Err(VmErr::TypeMsg(crate::s!(str which, " first arg must be bytes or a tuple of bytes, not ", str vm.type_name(affix))));
        };
        hit |= !past && if end { buf[s..e].ends_with(p) } else { buf[s..e].starts_with(p) };
    }
    vm.push(Val::bool(hit));
    Ok(())
}

/* Shared search for find/index, `raise` turns a miss into ValueError. Empty needle matches at 0 (Python), windows(0) would panic. */
pub fn find(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, false) }
pub fn index(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { find_impl(vm, recv, pos, true) }




// `bytes.fromhex(s)` classmethod, parse pairs of hex digits, skipping ASCII whitespace.
pub fn fromhex(vm: &mut VM, _recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let s = val_to_str(vm, pos[0])?;
    let mut out: Vec<u8> = Vec::new();
    let mut hi: Option<u8> = None;
    for c in s.chars() {
        if c.is_ascii_whitespace() { continue; }
        let d = c.to_digit(16).ok_or_else(|| cold_value("non-hexadecimal number found in fromhex() arg"))? as u8;
        match hi { None => hi = Some(d), Some(h) => { out.push((h << 4) | d); hi = None; } }
    }
    if hi.is_some() { return Err(cold_value("non-hexadecimal number found in fromhex() arg")); }
    let v = vm.heap.alloc(HeapObj::Bytes(out))?;
    vm.push(v); Ok(())
}

pub fn lower(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let mut buf = recv_bytes(vm, recv)?;
    buf.make_ascii_lowercase();
    let v = vm.heap.alloc(HeapObj::Bytes(buf))?; vm.push(v); Ok(())
}

pub fn upper(vm: &mut VM, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let mut buf = recv_bytes(vm, recv)?;
    buf.make_ascii_uppercase();
    let v = vm.heap.alloc(HeapObj::Bytes(buf))?; vm.push(v); Ok(())
}

// bytes strip, ASCII whitespace, or any byte in the optional argument.
fn bstrip(vm: &mut VM, recv: Val, pos: &[Val], left: bool, right: bool) -> Result<(), VmErr> {
    let buf = recv_bytes_ref(vm, recv)?;
    let chars = match pos.first() { Some(&a) => Some(recv_bytes_ref(vm, a)?), None => None };
    // Python's bytes whitespace set includes the vertical tab (0x0b), which Rust omits.
    let strip = |b: u8| -> bool { match &chars { Some(set) => set.contains(&b), None => b.is_ascii_whitespace() || b == 0x0b } };
    let mut s = 0usize;
    let mut e = buf.len();
    if left { while s < e && strip(buf[s]) { s += 1; } }
    if right { while e > s && strip(buf[e - 1]) { e -= 1; } }
    let out = buf[s..e].to_vec();
    let v = vm.heap.alloc(HeapObj::Bytes(out))?; vm.push(v); Ok(())
}
pub fn strip(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { bstrip(vm, recv, pos, true, true) }
pub fn lstrip(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { bstrip(vm, recv, pos, true, false) }
pub fn rstrip(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> { bstrip(vm, recv, pos, false, true) }

pub fn join(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let items = vm.extract_iter(pos[0]).map_err(|e| if matches!(e, VmErr::TypeMsg(_)) { cold_type("can only join an iterable of bytes") } else { e })?;
    let sep = recv_bytes_ref(vm, recv)?;
    let mut out: Vec<u8> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if i > 0 { out.extend_from_slice(sep); }
        out.extend_from_slice(recv_bytes_ref(vm, *it)?);
    }
    let v = vm.heap.alloc(HeapObj::Bytes(out))?; vm.push(v); Ok(())
}

/* Start of `sub` in `buf` at or after `from`. */
fn find_from(buf: &[u8], sub: &[u8], from: usize) -> Option<usize> {
    if sub.is_empty() { return (from <= buf.len()).then_some(from); }
    buf.get(from..)?.windows(sub.len()).position(|w| w == sub).map(|i| i + from)
}

/* `buf[start:end]` from the optional args after `sub`, clamped like a slice, as (start, end). */
fn window(buf: &[u8], pos: &[Val]) -> Result<(usize, usize), VmErr> {
    let arg = |i: usize| pos.get(i).copied().unwrap_or(Val::none());
    let (s, e, _) = slice_bounds(arg(1), arg(2), Val::none(), buf.len() as i64)?;
    Ok((s as usize, e.max(s) as usize))
}

fn find_impl(vm: &mut VM, recv: Val, pos: &[Val], raise: bool) -> Result<(), VmErr> {
    let (buf, sub) = (recv_bytes_ref(vm, recv)?, recv_bytes_ref(vm, pos[0])?);
    let (s, e) = window(buf, pos)?;
    let idx = match find_from(&buf[..e], sub, s) {
        Some(i) => i as i64,
        None if raise => return Err(cold_value("subsection not found")),
        None => -1,
    };
    vm.push(Val::int(idx));
    Ok(())
}

pub fn count(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let (buf, sub) = (recv_bytes_ref(vm, recv)?, recv_bytes_ref(vm, pos[0])?);
    let (s, e) = window(buf, pos)?;
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
pub fn replace(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
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
        vm.heap.reserve(buf.len().saturating_add(hits.len().saturating_mul(new.len() - old.len())))?;
    }
    let mut out: Vec<u8> = Vec::with_capacity(buf.len() + hits.len() * new.len());
    let mut at = 0;
    for j in hits {
        out.extend_from_slice(&buf[at..j]);
        out.extend_from_slice(new);
        at = j + old.len();
    }
    out.extend_from_slice(&buf[at..]);
    let v = vm.heap.alloc(HeapObj::Bytes(out))?;
    vm.push(v); Ok(())
}

// `bytes.split([sep[, maxsplit]])`, no separator splits on runs of ASCII whitespace.
pub fn split(vm: &mut VM, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
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
            let mut rest = buf.trim_ascii_start();
            while !rest.is_empty() {
                // Past `maxsplit` the rest stays whole, trailing whitespace included.
                if pieces.len() == max { pieces.push(rest); break; }
                let end = rest.iter().position(|b| b.is_ascii_whitespace()).unwrap_or(rest.len());
                pieces.push(&rest[..end]);
                rest = rest[end..].trim_ascii_start();
            }
        }
    }
    let owned: Vec<Vec<u8>> = pieces.into_iter().map(<[u8]>::to_vec).collect();
    let mut parts = Vec::with_capacity(owned.len());
    for p in owned { parts.push(vm.heap.alloc(HeapObj::Bytes(p))?); }
    vm.alloc_and_push_list(parts)
}
