use alloc::string::{String, ToString};
use crate::vm::types::{Val, HeapObj, HeapPool, VmErr, cold_value, fabs, ftrunc, num_as_f64};

// `%c`/`{:c}` out-of-range raises OverflowError, other format errors are ValueError.
pub const C_RANGE_ERR: &str = "%c arg not in range(0x110000)";
const C_LONG_ERR: &str = "Python int too large to convert to C long";
const DIGITS_ERR: &str = "Exceeds the limit (4300 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit";

// A spec with characters past its type, which `format_typed` words with the spec and the type.
const INVALID_SPEC: &str = "Invalid format specifier";

pub fn fmt_err(m: &'static str) -> VmErr {
    if m == C_RANGE_ERR || m == C_LONG_ERR { VmErr::Raised(crate::s!("OverflowError: ", str m)) } else { cold_value(m) }
}

/* f-string format spec, PEP 3101 subset, `[[fill]align][sign][#][0][width][,][.precision][type]`. Types are d/b/o/x/X (ints), f/F/e/E/g/G (floats), % (percent), s (str), c (codepoint). Returns Err(msg), the caller raises ValueError. */
pub fn format_value(v: Val, spec: &str, heap: &HeapPool) -> Result<String, &'static str> {
    let parsed = parse_spec(spec)?;
    // Cap against the memory left, precision also below the u16 threshold where core::fmt panics.
    const PRECISION_MAX: usize = 65_000;
    if parsed.width > heap.room() { return Err("format width exceeds limit"); }
    if parsed.precision.is_some_and(|p| p > heap.room().min(PRECISION_MAX)) { return Err("format precision exceeds limit"); }
    apply(v, &parsed, heap)
}

#[derive(Default, Clone)]
struct Spec {
    fill: char,
    align: Option<u8>, // b'<' b'>' b'^' b'='
    sign: u8, // 0, b'+', b'-', b' '
    alt: bool, // '#' alternate form, emits base prefix for b/o/x/X
    zero_pad: bool,
    width: usize,
    sep: u8, // 0, b',' or b'_' digit-group separator
    precision: Option<usize>,
    ty: u8, // 0 means default
    eq: bool, // `=` written in the spec, unlike the one a leading `0` implies
    z: bool, // `z` turns a negative zero, after rounding, positive
}

/* `format(v, spec)` for `format`, f-strings and `str.format`, a code `v` has no meaning for named like Python. */
pub fn format_typed(v: Val, spec: &str, type_name: &str, heap: &HeapPool) -> Result<String, VmErr> {
    let ty = match parse_spec(spec) {
        Ok(s) => s.ty,
        Err(INVALID_SPEC) => return Err(VmErr::Raised(alloc::format!("ValueError: Invalid format specifier '{spec}' for object of type '{type_name}'"))),
        Err(m) => return Err(fmt_err(m)),
    };
    let int_like = v.is_int() || v.is_bool() || crate::frontend::int::is_big(heap, v);
    let fits = match ty {
        0 => true,
        b's' => matches!(heap.try_get(v), Some(HeapObj::Str(_))),
        b'd' | b'b' | b'o' | b'x' | b'X' | b'c' => int_like,
        b'n' | b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'%' => int_like || v.is_float(),
        _ => false,
    };
    if !fits { return Err(VmErr::Raised(alloc::format!("ValueError: Unknown format code '{}' for object of type '{}'", ty as char, type_name))); }
    format_value(v, spec, heap).map_err(fmt_err)
}

/* `format(z, spec)` of a complex number, each part formatted alone, the imaginary one signed, then padded whole. */
pub fn format_complex(re: f64, im: f64, spec: &str, heap: &HeapPool) -> Result<String, VmErr> {
    let s = parse_spec(spec).map_err(fmt_err)?;
    if s.zero_pad { return Err(fmt_err("Zero padding is not allowed in complex format specifier")); }
    if s.eq { return Err(fmt_err("'=' alignment flag is not allowed in complex format specifier")); }
    if !matches!(s.ty, 0 | b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'n') {
        return Err(VmErr::Raised(alloc::format!("ValueError: Unknown format code '{}' for object of type 'complex'", s.ty as char)));
    }
    // A spec without a type keeps the parentheses and the repr of each part, its precision read as `g`.
    let bare = s.ty == 0;
    let part = Spec { fill: ' ', align: None, width: 0, ty: if bare && s.precision.is_some() { b'g' } else { s.ty }, ..s.clone() };
    let text = |f: f64, sign: u8| -> Result<String, VmErr> {
        let p = Spec { sign: if sign == 0 { part.sign } else { sign }, ..part.clone() };
        if bare && p.precision.is_none() {
            let t = crate::util::fstr::format_f64(f);
            let t = match t.strip_suffix(".0") { Some(x) => String::from(x), None => t };
            return Ok(if p.sign == b'+' && !t.starts_with('-') { alloc::format!("+{t}") } else { t });
        }
        apply(Val::float(f), &p, heap).map_err(fmt_err)
    };
    let (r, i) = (text(re, 0)?, text(im, b'+')?);
    let body = if bare && re == 0.0 && re.is_sign_positive() { alloc::format!("{}j", i.trim_start_matches('+')) }
        else if bare { alloc::format!("({r}{i}j)") } else { alloc::format!("{r}{i}j") };
    Ok(pad_aligned(&Spec { sign: 0, ..s }, &body, 0))
}

fn parse_spec(spec: &str) -> Result<Spec, &'static str> {
    let bytes = spec.as_bytes();
    let mut s = Spec { fill: ' ', ..Spec::default() };
    let mut i = 0;

    /* fill+align is "<char><align>" only when char #2 is one of `<>^=`. */
    if bytes.len() >= 2 && matches!(bytes[1], b'<' | b'>' | b'^' | b'=') {
        s.fill = bytes[0] as char;
        s.align = Some(bytes[1]);
        i = 2;
    } else if !bytes.is_empty() && matches!(bytes[0], b'<' | b'>' | b'^' | b'=') {
        s.align = Some(bytes[0]);
        i = 1;
    }
    s.eq = s.align == Some(b'=');

    if i < bytes.len() && matches!(bytes[i], b'+' | b'-' | b' ') {
        s.sign = bytes[i];
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'z' { s.z = true; i += 1; }

    // '#' alternate form, opt in to base prefix on b/o/x/X.
    if i < bytes.len() && bytes[i] == b'#' { s.alt = true; i += 1; }

    if i < bytes.len() && bytes[i] == b'0' {
        s.zero_pad = true;
        if s.align.is_none() {
            s.align = Some(b'=');
            s.fill = '0';
        }
        i += 1;
    }

    let w_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
    if i > w_start {
        s.width = spec[w_start..i].parse().map_err(|_| "invalid width in format spec")?;
    }

    // ',' or '_' digit-group separator, '_' groups hex/oct/bin by four.
    if i < bytes.len() && matches!(bytes[i], b',' | b'_') {
        s.sep = bytes[i];
        i += 1;
        // One separator only, the same twice or the two together refused as Python refuses them.
        match (s.sep, bytes.get(i)) {
            (b',', Some(b',')) => return Err("Cannot specify ',' with ','."),
            (b'_', Some(b'_')) => return Err("Cannot specify '_' with '_'."),
            (_, Some(b',' | b'_')) => return Err("Cannot specify both ',' and '_'."),
            _ => {}
        }
    }

    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let p_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
        if i == p_start { return Err("missing precision in format spec"); }
        s.precision = Some(spec[p_start..i].parse().map_err(|_| "invalid precision in format spec")?);
    }

    if i < bytes.len() {
        if !bytes[i].is_ascii() { return Err("invalid type in format spec"); }
        s.ty = bytes[i];
        i += 1;
    }

    if i != bytes.len() { return Err(INVALID_SPEC); }
    Ok(s)
}

fn apply(v: Val, s: &Spec, heap: &HeapPool) -> Result<String, &'static str> {
    let is_int_like = v.is_int() || v.is_bool() || crate::frontend::int::is_big(heap, v);
    // The flags that mean nothing for a string or an integer code.
    if s.z && matches!(heap.try_get(v), Some(HeapObj::Str(_))) { return Err("Negative zero coercion (z) not allowed in string format specifier"); }
    if s.z && is_int_like && matches!(s.ty, 0 | b'd' | b'b' | b'o' | b'x' | b'X' | b'c' | b'n') { return Err("Negative zero coercion (z) not allowed in integer format specifier"); }
    if s.alt && s.ty == b'c' { return Err("Alternate form (#) not allowed with integer format specifier 'c'"); }
    match (s.sep, s.ty) {
        (b',', b'b') => return Err("Cannot specify ',' with 'b'."),
        (b',', b'o') => return Err("Cannot specify ',' with 'o'."),
        (b',', b'x') => return Err("Cannot specify ',' with 'x'."),
        (b',', b'X') => return Err("Cannot specify ',' with 'X'."),
        (b',', b'c') => return Err("Cannot specify ',' with 'c'."),
        (b'_', b'c') => return Err("Cannot specify '_' with 'c'."),
        _ => {}
    }
    // Dispatch by type char, int types -> int formatter, float types coerce ints up, `s` only strings.
    match s.ty {
        0 | b's' => {
            if let Some(HeapObj::Str(raw)) = heap.try_get(v) {
                // A separator, a sign, `#` and `=` belong to numbers only.
                match s.sep { b',' => return Err("Cannot specify ',' with 's'."), b'_' => return Err("Cannot specify '_' with 's'."), _ => {} }
                match s.sign { b' ' => return Err("Space not allowed in string format specifier"), 0 => {} _ => return Err("Sign not allowed in string format specifier") }
                if s.alt { return Err("Alternate form (#) not allowed in string format specifier"); }
                if s.eq { return Err("'=' alignment not allowed in string format specifier"); }
                let text: String = match s.precision { Some(p) => raw.chars().take(p).collect(), None => raw.clone() };
                return Ok(pad_string(s, &text));
            }
            if !is_int_like && !v.is_float() { return Err("unsupported format string"); }
            if s.ty == b's' { return Err("'s' format spec requires a string"); }
            if v.is_float() {
                return if s.precision.is_some() { format_float(v, s, 0, heap) } else { Ok(format_float_str(v.as_float(), s)) };
            }
            if s.precision.is_some() { return Err("Precision not allowed in integer format specifier"); }
            format_int(v, &Spec { ty: b'd', ..s.clone() }, heap)
        }
        // `n` is `g` for a float and `d` for an int in the C locale, the only one there is, and takes no separator.
        b'n' if s.sep != 0 => Err(if s.sep == b',' { "Cannot specify ',' with 'n'." } else { "Cannot specify '_' with 'n'." }),
        b'n' if v.is_float() => format_float(v, s, b'g', heap),
        b'd' | b'b' | b'o' | b'x' | b'X' | b'n' => {
            if s.precision.is_some() { return Err("Precision not allowed in integer format specifier"); }
            let mut s2 = s.clone();
            if s2.ty == b'n' { s2.ty = b'd'; }
            format_int(v, &s2, heap)
        }
        b'f' | b'F' => format_float(v, s, s.ty, heap),
        b'e' | b'E' | b'g' | b'G' => format_float(v, s, s.ty, heap),
        b'%' => format_percent(v, s, heap),
        b'c' => format_char(v, s, heap),
        _ => Err("unknown format type"),
    }
}

fn format_percent(v: Val, s: &Spec, heap: &HeapPool) -> Result<String, &'static str> {
    let f = require_float(v, heap)? * 100.0;
    let prec = s.precision.unwrap_or(6);
    let body = if f.is_nan() { "nan".to_string() }
    else if f.is_infinite() { if f.is_sign_negative() { "-inf".into() } else { "inf".into() } }
    else { fixed(f.abs(), prec) };
    let body = alloc::format!("{body}%");
    Ok(signed_pad(s, f.is_sign_negative() && !f.is_nan() && !(s.z && zero_text(&body)), "", &body))
}

fn format_char(v: Val, s: &Spec, heap: &HeapPool) -> Result<String, &'static str> {
    if crate::frontend::int::is_big(heap, v) { return Err(C_LONG_ERR); }
    if !v.is_int() { return Err("'c' format spec requires an integer"); }
    let i = v.as_int();
    if !(0..=0x10FFFF).contains(&i) { return Err(C_RANGE_ERR); }
    let ch = char::from_u32(i as u32).ok_or("'c' format spec arg not a valid char")?;
    // 'c' is numeric for alignment, it defaults to right-align like ints.
    Ok(pad_aligned(s, &ch.to_string(), 0))
}

fn format_int(v: Val, s: &Spec, heap: &HeapPool) -> Result<String, &'static str> {
    let radix = match s.ty { b'b' => 2, b'o' => 8, b'x' | b'X' => 16, _ => 10 };
    let (neg, digits) = int_digits(v, radix, heap)?;
    let prefix = if !s.alt { "" } else { match s.ty { b'b' => "0b", b'o' => "0o", b'x' => "0x", b'X' => "0X", _ => "" } };
    let digits = if s.ty == b'X' { digits.to_uppercase() } else { digits };
    let body = if s.sep != 0 { add_grouped(&digits, group_size(s.ty, s.sep), s.sep) } else { digits };
    Ok(signed_pad(s, neg, prefix, &body))
}

fn format_float(v: Val, s: &Spec, ty: u8, heap: &HeapPool) -> Result<String, &'static str> {
    let f = require_float(v, heap)?;
    let prec = s.precision.unwrap_or(6);

    /* NaN/inf go through unchanged (emits "nan"/"inf" before padding). */
    if f.is_nan() {
        let body = if ty == b'F' { "NAN" } else { "nan" };
        return Ok(signed_pad(s, false, "", body));
    }
    if f.is_infinite() {
        return Ok(signed_pad(s, f.is_sign_negative(), "", if ty == b'F' { "INF" } else { "inf" }));
    }

    let mag = f.abs();
    let body = match ty {
        b'f' | b'F' => fixed(mag, prec),
        // e/g delegate to Rust's f64 formatter, round-half-to-even applies only to `f`.
        b'e' => format_with_e(mag, prec, false),
        b'E' => format_with_e(mag, prec, true),
        // `g/G` pick fixed when the rounded exponent is in -4..precision, a typeless precision one below, else scientific.
        b'g' | b'G' | 0 => {
            let p = prec.max(1);
            let sci = format_with_e(mag, p - 1, ty == b'G');
            let exp: i32 = sci.rsplit_once(['e', 'E']).and_then(|(_, e)| e.parse().ok()).unwrap_or(0);
            let fixed_form = (-4..p as i32 - (ty == 0) as i32).contains(&exp);
            let out = if fixed_form { fixed(mag, (p as i32 - 1 - exp) as usize) } else { sci };
            let mut out = if s.alt { out } else { trim_zeros(out) };
            // Typeless keeps one digit past the point, `5.0` and not `5`.
            if ty == 0 && fixed_form && !out.contains('.') { out.push_str(".0"); }
            out
        }
        _ => unreachable!(),
    };
    let body = if s.sep != 0 { add_thousands_float(&body, s.sep) } else { body };
    Ok(signed_pad(s, f.is_sign_negative() && !(s.z && zero_text(&body)), "", &body))
}

/* Whether formatted digits read as zero, the mantissa only, which `z` keeps from a minus sign. */
fn zero_text(body: &str) -> bool {
    body.split(['e', 'E']).next().unwrap_or("").bytes().all(|b| matches!(b, b'0' | b'.' | b',' | b'_' | b'%'))
}

/* `g` drops trailing mantissa zeros, and the point once nothing follows it. */
fn trim_zeros(text: String) -> String {
    let (mant, exp) = text.split_at(text.find(['e', 'E']).unwrap_or(text.len()));
    if !mant.contains('.') { return text; }
    crate::s!(str mant.trim_end_matches('0').trim_end_matches('.'), str exp)
}

/* Typeless float spec, str() digits with only align/width/sign and optional grouping applied. */
fn format_float_str(f: f64, s: &Spec) -> String {
    // NaN pads like a number, right-aligned and with its `+` sign when asked.
    if f.is_nan() { return signed_pad(s, false, "", "nan"); }
    if f.is_infinite() { return signed_pad(s, f.is_sign_negative(), "", "inf"); }
    let body = crate::util::fstr::format_f64(f.abs());
    let body = if s.sep != 0 { add_thousands_float(&body, s.sep) } else { body };
    signed_pad(s, f.is_sign_negative() && !(s.z && zero_text(&body)), "", &body)
}

fn format_with_e(mag: f64, prec: usize, upper: bool) -> String {
    // Rust emits "3.14e0" but we expect "e+00", inject the sign and pad exponent to >=2 digits.
    let raw = alloc::format!("{:.*e}", prec, mag);
    let (mant, exp_str) = raw.split_once('e').unwrap_or((raw.as_str(), "0"));
    let (esign, edigs) = if let Some(rest) = exp_str.strip_prefix('-') { ('-', rest) }
    else { ('+', exp_str) };
    let mut out = String::new();
    out.push_str(mant);
    out.push(if upper { 'E' } else { 'e' });
    out.push(esign);
    if edigs.len() < 2 { out.push('0'); }
    out.push_str(edigs);
    out
}

fn require_float(v: Val, heap: &HeapPool) -> Result<f64, &'static str> {
    num_as_f64(v, heap).ok_or("format spec requires a number")
}

/* The sign and magnitude digits of an int in `radix`. */
fn int_digits(v: Val, radix: u32, heap: &HeapPool) -> Result<(bool, String), &'static str> {
    if v.is_int() || v.is_bool() {
        let i = if v.is_int() { v.as_int() } else { v.as_bool() as i64 };
        let (mut n, mut out) = (i.unsigned_abs(), alloc::vec::Vec::new());
        loop { out.push(core::char::from_digit((n % radix as u64) as u32, radix).unwrap_or('0') as u8); n /= radix as u64; if n == 0 { break; } }
        out.reverse();
        return Ok((i < 0, String::from_utf8(out).unwrap_or_default()));
    }
    match crate::frontend::int::radix_digits(heap, v, radix) {
        Some(Ok(d)) => Ok(d),
        Some(Err(_)) => Err(DIGITS_ERR),
        None => Err("format spec requires an integer"),
    }
}

// '_' groups binary/octal/hex every four digits, decimal and ',' every three.
fn group_size(ty: u8, sep: u8) -> usize {
    if sep == b'_' && matches!(ty, b'b' | b'o' | b'x' | b'X') { 4 } else { 3 }
}

fn add_grouped(digits: &str, group: usize, sep: u8) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / group);
    let chars: alloc::vec::Vec<char> = digits.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i).is_multiple_of(group) { out.push(sep as char); }
        out.push(c);
    }
    out
}

fn add_thousands_float(s: &str, sep: u8) -> String {
    /* Insert separators only in the integer portion (before `.` / `e`). */
    let split = s.find(['.', 'e', 'E']);
    let (int_part, rest) = match split {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let mut out = add_grouped(int_part, 3, sep);
    out.push_str(rest);
    out
}

fn sign_char(neg: bool, sign_flag: u8) -> Option<char> {
    if neg { Some('-') }
    else if sign_flag == b'+' { Some('+') }
    else if sign_flag == b' ' { Some(' ') }
    else { None }
}

/* Sign char + optional radix prefix + body, then numeric-aligned padding. */
fn signed_pad(s: &Spec, neg: bool, prefix: &str, body: &str) -> String {
    let sign_ch = sign_char(neg, s.sign);
    let mut left = String::new();
    if let Some(c) = sign_ch { left.push(c); }
    left.push_str(prefix);
    left.push_str(body);
    pad_aligned(s, &left, sign_ch.map(|_| 1).unwrap_or(0) + prefix.len())
}

fn pad_string(s: &Spec, body: &str) -> String {
    let len = body.chars().count();
    if len >= s.width { return body.to_string(); }
    let pad = s.width - len;
    // Strings can't use '=' alignment, a zero-fill default becomes left-align.
    let align = match s.align.unwrap_or(b'<') { b'=' => b'<', a => a };
    pad_with(body, pad, align, s.fill, 0)
}

fn pad_aligned(s: &Spec, body: &str, sign_prefix_len: usize) -> String {
    let len = body.chars().count();
    if len >= s.width { return body.to_string(); }
    let pad = s.width - len;
    let align = s.align.unwrap_or(b'>');
    pad_with(body, pad, align, s.fill, sign_prefix_len)
}

fn pad_with(body: &str, pad: usize, align: u8, fill: char, sign_prefix_len: usize) -> String {
    let mut out = String::new();
    match align {
        b'<' => {
            out.push_str(body);
            for _ in 0..pad { out.push(fill); }
        }
        b'>' => {
            for _ in 0..pad { out.push(fill); }
            out.push_str(body);
        }
        b'^' => {
            let l = pad / 2;
            let r = pad - l;
            for _ in 0..l { out.push(fill); }
            out.push_str(body);
            for _ in 0..r { out.push(fill); }
        }
        b'=' => {
            /* Sign-aware padding goes between the sign/prefix and the digits. */
            let mut chars = body.chars();
            for _ in 0..sign_prefix_len {
                if let Some(c) = chars.next() { out.push(c); }
            }
            for _ in 0..pad { out.push(fill); }
            out.push_str(chars.as_str());
        }
        _ => unreachable!(),
    }
    out
}

// Every power of ten up to 1e22 is exact in an f64.
const POW10: [f64; 23] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22];

fn fixed(mag: f64, prec: usize) -> String {
    // A product under 2^53 whose fraction sits more than an ulp from one half rounds as the exact value does.
    if let Some(&scale) = POW10.get(prec) {
        let scaled = mag * scale;
        let whole = ftrunc(scaled);
        let ulp = f64::from_bits(scaled.to_bits() + 1) - scaled;
        if scaled < 9_007_199_254_740_992.0 && fabs(scaled - whole - 0.5) > ulp {
            let digits = itoa_str(whole as i64 + (scaled - whole > 0.5) as i64);
            if prec == 0 { return digits; }
            let mut out = String::with_capacity(digits.len() + prec + 2);
            for _ in digits.len()..=prec { out.push('0'); }
            out.push_str(&digits);
            out.insert(out.len() - prec, '.');
            return out;
        }
    }
    // Ties and the rest go to core::fmt, which expands the exact binary value and rounds half to even.
    alloc::format!("{:.*}", prec, mag)
}

fn itoa_str(i: i64) -> String {
    let mut b = itoa::Buffer::new(); b.format(i).to_string()
}

/* `ascii()` of a repr, every non-ASCII char escaped as `\\x`, `\\u` or `\\U`. */
pub fn ascii_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c as u32 {
            n if n < 0x80 => out.push(c),
            n if n < 0x100 => out.push_str(&alloc::format!("\\x{n:02x}")),
            n if n < 0x10000 => out.push_str(&alloc::format!("\\u{n:04x}")),
            n => out.push_str(&alloc::format!("\\U{n:08x}")),
        }
    }
    out
}
