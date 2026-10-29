use alloc::string::String;

use edge::lang::{Cx, KindId, Mode};
use edge::s;
use edge::value::{Val, VmErr, fabs, ftrunc};

use crate::Lang;
use crate::fail::{At, Fail, Spec as Why, bare};

// precision stays below the bound where the core float formatter panics
const PRECISION_MAX: usize = 65_000;

/* why a spec was refused or the spec left text past its type */
enum Refused { Why(Why), Invalid }

/* the error of a refused spec as the runtime words it */
#[cold]
fn bad<L: Lang>(why: Why) -> VmErr { bare::<L>(Fail::Spec(why)) }

/* `v` under a format spec as a field of interpolated text writes it */
pub fn format<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> {
    // a field without a spec renders as text and never meets the parser
    if spec.is_empty() { return L::show(cx, v, Mode::Str); }
    let kind = cx.kind(v);
    if !matches!(kind, KindId::Int | KindId::Bool | KindId::Float | KindId::Str) { return Err(L::words(cx, At::NONE, Fail::SpecRefused(v))); }
    let s = match parse::<L>(spec) {
        Ok(s) => s,
        Err(Refused::Invalid) => return Err(L::words(cx, At::NONE, Fail::SpecInvalid { spec: String::from(spec), v })),
        Err(Refused::Why(w)) => return Err(bad::<L>(w)),
    };
    // a separator groups decimal digits and an underscore groups a power of two base too
    let groups = matches!(s.ty, 0 | b'd' | b'e' | b'f' | b'g' | b'E' | b'G' | b'%' | b'F') || (s.sep == b'_' && matches!(s.ty, b'b' | b'o' | b'x' | b'X'));
    if s.sep != 0 && !groups { return Err(bad::<L>(Why::SepWith(s.sep, s.ty))); }
    let int = matches!(kind, KindId::Int | KindId::Bool);
    let fits = match s.ty {
        0 => true,
        b's' => kind == KindId::Str,
        b'd' | b'b' | b'o' | b'x' | b'X' | b'c' => int,
        b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => int || kind == KindId::Float,
        // these two types are parts a runtime takes or leaves
        b'n' if L::SPECS.n => int || kind == KindId::Float,
        b'%' if L::SPECS.percent => int || kind == KindId::Float,
        _ => false,
    };
    if !fits { return Err(L::words(cx, At::NONE, Fail::SpecUnknown { code: s.ty as char, v })); }
    let room = cx.room();
    // a fill past ascii takes several bytes for each unit of the width
    if s.width > room || !s.fill.is_ascii() && s.width.saturating_mul(s.fill.len_utf8()) > room { return Err(L::words(cx, At::NONE, Fail::SpecWidth)); }
    if s.precision.is_some_and(|p| p > room.min(PRECISION_MAX)) { return Err(L::words(cx, At::NONE, Fail::SpecPrecision)); }
    // a wide field counts what it writes as native work as a long text does
    if s.width > 64 { cx.spend(s.width); }
    apply::<L>(cx, v, kind, &s)
}

/* the format hook a runtime registers */
pub fn hook<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { format::<L>(cx, v, spec) }

/* a spec as `[[fill]align][sign][z][#][0][width][grouping][.precision][type]` reads */
#[derive(Clone, Copy)]
struct Spec {
    fill: char,
    // one of the four align marks and None for the kind's own default
    align: Option<u8>,
    // plus or minus or space and 0 when absent
    sign: u8,
    // turns a negative zero after rounding positive
    z: bool,
    alt: bool,
    width: usize,
    // the comma or the underscore and 0 when absent
    sep: u8,
    precision: Option<usize>,
    // 0 when the spec names no type
    ty: u8,
    // `=` written in the spec unlike the one a leading zero implies
    eq: bool,
}

/* the fill past ascii a spec opens with and the align mark after it */
#[cold]
#[inline(never)]
fn wide_fill(spec: &str) -> Option<(char, u8)> {
    let c = spec.chars().next()?;
    let &a @ (b'<' | b'>' | b'^' | b'=') = spec.as_bytes().get(c.len_utf8())? else { return None };
    Some((c, a))
}

/* the fields of `spec` under the parts `L` takes or why the first is refused */
fn parse<L: Lang>(spec: &str) -> Result<Spec, Refused> {
    let (b, parts) = (spec.as_bytes(), L::SPECS);
    let mut s = Spec { fill: ' ', align: None, sign: 0, z: false, alt: false, width: 0, sep: 0, precision: None, ty: 0, eq: false };
    let mut i = 0;
    let aligns = |c: u8| matches!(c, b'<' | b'>' | b'^') || (c == b'=' && parts.eq);
    // a fill is the byte or the character past ascii before an align mark
    let wide = if b.first().is_some_and(|&c| c >= 0x80) { wide_fill(spec).filter(|&(_, a)| parts.eq || a != b'=') } else { None };
    let filled = if let Some((fill, align)) = wide {
        (s.fill, s.align, i) = (fill, Some(align), fill.len_utf8() + 1);
        true
    } else if b.len() >= 2 && aligns(b[1]) {
        s.fill = b[0] as char;
        s.align = Some(b[1]);
        i = 2;
        true
    } else { false };
    if !filled && let Some(&a) = b.first().filter(|&&a| aligns(a)) {
        s.align = Some(a);
        i = 1;
    }
    s.eq = s.align == Some(b'=');
    if let Some(&c @ (b'+' | b'-' | b' ')) = b.get(i) { s.sign = c; i += 1; }
    if parts.z && b.get(i) == Some(&b'z') { s.z = true; i += 1; }
    if b.get(i) == Some(&b'#') { s.alt = true; i += 1; }
    if !filled && b.get(i) == Some(&b'0') {
        // a leading zero fills with zeros after the sign unless an align was given
        s.fill = '0';
        if s.align.is_none() { s.align = Some(b'='); }
        i += 1;
    }
    let start = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) { i += 1; }
    if i > start { s.width = spec[start..i].parse().map_err(|_| Refused::Why(Why::BadWidth))?; }
    if let Some(&c @ (b',' | b'_')) = b.get(i).filter(|_| parts.groups) {
        s.sep = c;
        i += 1;
        // one separator only so a second one of either kind is refused
        match (c, b.get(i)) {
            (b',' | b'_', Some(&d)) if d == c => return Err(Refused::Why(Why::SepTwice(c))),
            (_, Some(b',' | b'_')) => return Err(Refused::Why(Why::SepBoth)),
            _ => {}
        }
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) { i += 1; }
        if i == start { return Err(Refused::Why(Why::MissingPrecision)); }
        s.precision = Some(spec[start..i].parse().map_err(|_| Refused::Why(Why::BadPrecision))?);
    }
    if let Some(&c) = b.get(i) {
        if !c.is_ascii() { return Err(Refused::Why(Why::BadType)); }
        s.ty = c;
        i += 1;
    }
    if i != b.len() { return Err(Refused::Invalid); }
    Ok(s)
}

/* the text of `v` under `s` once its type is known to fit the kind */
fn apply<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, kind: KindId, s: &Spec) -> Result<String, VmErr> {
    let int = matches!(kind, KindId::Int | KindId::Bool);
    // an integer type refuses a precision before the flags that mean nothing for it
    let whole = int && matches!(s.ty, 0 | b'd' | b'b' | b'o' | b'x' | b'X' | b'c' | b'n');
    if whole && s.precision.is_some() { return Err(bad::<L>(Why::PrecisionInInt)); }
    if s.z && kind == KindId::Str { return Err(bad::<L>(Why::ZeroInText)); }
    if s.z && whole { return Err(bad::<L>(Why::ZeroInInt)); }
    if s.ty == b'c' && s.sign != 0 { return Err(bad::<L>(Why::SignWithChar)); }
    if s.alt && s.ty == b'c' { return Err(bad::<L>(Why::AltWithChar)); }
    match s.ty {
        0 | b's' if kind == KindId::Str => text::<L>(cx, v, s),
        0 if kind == KindId::Float && s.precision.is_none() => float_str::<L>(cx, v.as_float(), s),
        0 if kind == KindId::Float => float::<L>(cx, v, s, 0),
        0 => int_text::<L>(cx, v, &Spec { ty: b'd', ..*s }),
        // the one locale there is reads `n` as `g` for floats
        b'n' if kind == KindId::Float => float::<L>(cx, v, s, b'g'),
        b'n' => int_text::<L>(cx, v, &Spec { ty: b'd', ..*s }),
        b'd' | b'b' | b'o' | b'x' | b'X' => int_text::<L>(cx, v, s),
        b'%' => percent::<L>(cx, v, s),
        b'c' => char_text::<L>(cx, v, s),
        _ => float::<L>(cx, v, s, s.ty),
    }
}

/* a text under `s` where fill align width and precision apply */
fn text<L: Lang>(cx: &Cx<'_, '_>, v: Val, s: &Spec) -> Result<String, VmErr> {
    if matches!(s.sep, b',' | b'_') { return Err(bad::<L>(Why::SepWith(s.sep, b's'))); }
    match s.sign { b' ' => return Err(bad::<L>(Why::SpaceInText)), 0 => {} _ => return Err(bad::<L>(Why::SignInText)) }
    if s.alt { return Err(bad::<L>(Why::AltInText)); }
    if s.eq { return Err(bad::<L>(Why::EqInText)); }
    let raw = cx.text(v).unwrap_or("");
    // a long text counts what the field copies of it as native work
    if raw.len() > 64 { cx.spend(raw.len()); }
    let body = match s.precision { Some(p) => raw.char_indices().nth(p).map_or(raw, |(at, _)| &raw[..at]), None => raw };
    // text pads left by default and a zero fill pads it left too
    let align = match s.align { None | Some(b'=') => b'<', Some(a) => a };
    Ok(pad(s, align, "", "", body))
}

/* an int in its type's radix with the prefix grouping and sign of `s` */
fn int_text<L: Lang>(cx: &Cx<'_, '_>, v: Val, s: &Spec) -> Result<String, VmErr> {
    let radix = match s.ty { b'b' => 2, b'o' => 8, b'x' | b'X' => 16, _ => 10 };
    let (neg, mut digits) = int_digits::<L>(cx, v, radix)?;
    let prefix = if !s.alt { "" } else { match s.ty { b'b' => "0b", b'o' => "0o", b'x' => "0x", b'X' => "0X", _ => "" } };
    if s.ty == b'X' { digits.make_ascii_uppercase(); }
    // an underscore groups the power of two radixes by four
    let group = if s.sep == b'_' && radix != 10 { 4 } else { 3 };
    let body = if s.sep != 0 { grouped(&digits, group, s.sep, zero_places(s, sign_len(s, neg) + prefix.len())) } else { digits };
    Ok(signed(s, neg, prefix, &body))
}

/* the sign and the magnitude digits of an int in `radix` */
fn int_digits<L: Lang>(cx: &Cx<'_, '_>, v: Val, radix: u32) -> Result<(bool, String), VmErr> {
    if v.is_int() || v.is_bool() && L::POLICY.bool_is_int {
        let i = if v.is_int() { v.as_int() } else { v.as_bool() as i64 };
        let mut n = i.unsigned_abs();
        if radix == 10 { return Ok((i < 0, String::from(itoa::Buffer::new().format(n)))); }
        let (mut buf, mut at) = ([0u8; 64], 64);
        loop {
            at -= 1;
            buf[at] = b"0123456789abcdef"[(n % radix as u64) as usize];
            n /= radix as u64;
            if n == 0 { break; }
        }
        return Ok((i < 0, String::from(core::str::from_utf8(&buf[at..]).unwrap_or_default())));
    }
    match crate::num::big::radix_digits::<L>(cx, v, radix) {
        Some(d) => d,
        None => Err(bad::<L>(Why::NeedsInt)),
    }
}

/* an int as the one character it codes */
fn char_text<L: Lang>(cx: &Cx<'_, '_>, v: Val, s: &Spec) -> Result<String, VmErr> {
    // a bool codes the character its int names where bools are ints
    let i = if v.is_bool() && L::POLICY.bool_is_int { i64::from(v.as_bool()) } else if v.is_int() { v.as_int() } else {
        return Err(bad::<L>(if cx.kind(v) == KindId::Int { Why::CharWide } else { Why::CharNeedsInt }));
    };
    if !(0..=0x10FFFF).contains(&i) { return Err(bad::<L>(Why::CharRange)); }
    let c = char::from_u32(i as u32).ok_or_else(|| bad::<L>(Why::CharInvalid))?;
    let mut buf = [0u8; 4];
    // a character aligns right as numbers do
    Ok(pad(s, s.align.unwrap_or(b'>'), "", "", c.encode_utf8(&mut buf)))
}

/* `v` as a float which ints and bools become */
fn number<L: Lang>(cx: &Cx<'_, '_>, v: Val) -> Result<f64, VmErr> {
    cx.float(v).and_then(Result::ok).ok_or_else(|| bad::<L>(Why::NeedsNumber))
}

/* a number under float type `ty` where 0 is the typeless form with a precision */
fn float<L: Lang>(cx: &Cx<'_, '_>, v: Val, s: &Spec, ty: u8) -> Result<String, VmErr> {
    let f = number::<L>(cx, v)?;
    let prec = s.precision.unwrap_or(6);
    // a long precision counts the digits it works out as native work
    if prec > 64 { cx.spend(prec); }
    // an upper case type writes nan and inf upper case
    if f.is_nan() { return Ok(signed(s, false, "", if ty.is_ascii_uppercase() { "NAN" } else { "nan" })); }
    if f.is_infinite() { return Ok(signed(s, f.is_sign_negative(), "", if ty.is_ascii_uppercase() { "INF" } else { "inf" })); }
    let mag = fabs(f);
    let body = match ty {
        b'f' | b'F' => fixed(mag, prec),
        b'e' | b'E' => exponent(mag, prec, ty == b'E'),
        _ => general(mag, prec, ty, s.alt),
    };
    Ok(number_text(s, f.is_sign_negative(), body))
}

/* the typeless form without a precision as the shortest digits that read back */
fn float_str<L: Lang>(cx: &mut Cx<'_, '_>, f: f64, s: &Spec) -> Result<String, VmErr> {
    // nan pads like a number and takes a plus sign when asked
    if f.is_nan() { return Ok(signed(s, false, "", "nan")); }
    if f.is_infinite() { return Ok(signed(s, f.is_sign_negative(), "", "inf")); }
    let body = L::show(cx, Val::float(fabs(f)), Mode::Str)?;
    Ok(number_text(s, f.is_sign_negative(), body))
}

/* a number times a hundred in fixed form with a percent sign */
fn percent<L: Lang>(cx: &Cx<'_, '_>, v: Val, s: &Spec) -> Result<String, VmErr> {
    let f = number::<L>(cx, v)? * 100.0;
    if s.precision.is_some_and(|p| p > 64) { cx.spend(s.precision.unwrap_or(0)); }
    if f.is_nan() { return Ok(signed(s, false, "", "nan%")); }
    if f.is_infinite() { return Ok(signed(s, f.is_sign_negative(), "", "inf%")); }
    let mut body = fixed(fabs(f), s.precision.unwrap_or(6));
    body.push('%');
    Ok(number_text(s, f.is_sign_negative(), body))
}

/* a finite float's digits with the point grouping zero fill and sign `s` asks */
fn number_text(s: &Spec, negative: bool, body: String) -> String {
    let body = if s.alt { pointed(body) } else { body };
    let neg = negative && !(s.z && zero_text(&body));
    let body = if s.sep == 0 { body } else {
        // the whole digits group and the zero fill counts the point and exponent after them
        let (whole, rest) = body.split_at(body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len()));
        let mut out = grouped(whole, 3, s.sep, zero_places(s, sign_len(s, neg) + rest.len()));
        out.push_str(rest);
        out
    };
    signed(s, neg, "", &body)
}

/* `body` with a point after its leading digits as the alternate form asks */
fn pointed(mut body: String) -> String {
    let at = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
    if body.as_bytes().get(at) != Some(&b'.') { body.insert(at, '.'); }
    body
}

/* how many places the sign of `s` takes before the digits */
fn sign_len(s: &Spec, neg: bool) -> usize { usize::from(neg || matches!(s.sign, b'+' | b' ')) }

/* the places a zero fill groups after `taken` places and none for any other fill */
fn zero_places(s: &Spec, taken: usize) -> usize { if s.fill == '0' && s.align == Some(b'=') { s.width.saturating_sub(taken) } else { 0 } }

/* `g` `G` and the typeless form pick fixed when the rounded exponent is in range */
fn general(mag: f64, prec: usize, ty: u8, alt: bool) -> String {
    let p = prec.max(1);
    let sci = exponent(mag, p - 1, ty == b'G');
    let exp: i32 = sci.rsplit_once(['e', 'E']).and_then(|(_, e)| e.parse().ok()).unwrap_or(0);
    // the typeless form switches to scientific one exponent sooner
    let fixed_form = (-4..p as i32 - (ty == 0) as i32).contains(&exp);
    let out = if fixed_form { fixed(mag, (p as i32 - 1 - exp) as usize) } else { sci };
    let mut out = if alt { out } else { trim_zeros(out) };
    // the typeless form keeps one digit past the point
    if ty == 0 && fixed_form && !out.contains('.') && !alt { out.push_str(".0"); }
    out
}

/* whether formatted digits read as zero in the mantissa which `z` keeps unsigned */
fn zero_text(body: &str) -> bool {
    body.split(['e', 'E']).next().unwrap_or("").bytes().all(|b| matches!(b, b'0' | b'.' | b',' | b'_' | b'%'))
}

/* drops trailing mantissa zeros and the point once nothing follows it */
fn trim_zeros(text: String) -> String {
    let (mant, exp) = text.split_at(text.find(['e', 'E']).unwrap_or(text.len()));
    if !mant.contains('.') { return text; }
    s!(str mant.trim_end_matches('0').trim_end_matches('.'), str exp)
}

/* scientific digits whose exponent has a sign and at least two digits */
fn exponent(mag: f64, prec: usize, upper: bool) -> String {
    let raw = alloc::format!("{:.*e}", prec, mag);
    let (mant, exp) = raw.split_once('e').unwrap_or((raw.as_str(), "0"));
    let (sign, digits) = match exp.strip_prefix('-') { Some(d) => ('-', d), None => ('+', exp) };
    let mut out = String::with_capacity(mant.len() + 5);
    out.push_str(mant);
    out.push(if upper { 'E' } else { 'e' });
    out.push(sign);
    if digits.len() < 2 { out.push('0'); }
    out.push_str(digits);
    out
}

// every power of ten up to 1e22 is exact in an f64
const POW10: [f64; 23] = [1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22];

/* `mag` rounded to `prec` places as its exact binary value rounds */
fn fixed(mag: f64, prec: usize) -> String {
    // a product under 2^53 more than an ulp from a half rounds exactly
    if let Some(&scale) = POW10.get(prec) {
        let scaled = mag * scale;
        let whole = ftrunc(scaled);
        let ulp = f64::from_bits(scaled.to_bits() + 1) - scaled;
        if scaled < 9_007_199_254_740_992.0 && fabs(scaled - whole - 0.5) > ulp {
            let mut buf = itoa::Buffer::new();
            let digits = buf.format(whole as i64 + (scaled - whole > 0.5) as i64);
            if prec == 0 { return String::from(digits); }
            let mut out = String::with_capacity(digits.len() + prec + 2);
            for _ in digits.len()..=prec { out.push('0'); }
            out.push_str(digits);
            out.insert(out.len() - prec, '.');
            return out;
        }
    }
    // ties and the rest go to core which rounds the exact value half to even
    alloc::format!("{:.*}", prec, mag)
}

/* `digits` grouped by `group` from the right with zeros filling them out to `min` places */
fn grouped(digits: &str, group: usize, sep: u8, min: usize) -> String {
    // no digits such as those of inf take a plain fill
    if digits.is_empty() { return String::new(); }
    let d = digits.as_bytes();
    let (g, mut left, mut need) = (group as isize, d.len() as isize, min as isize);
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(d.len().max(min) + d.len().max(min) / group + 1);
    loop {
        // a group whose digits run out takes zeros while places are still needed
        let take = g.min(left.max(need).max(1));
        let digits = left.min(take);
        if !out.is_empty() { out.push(sep); }
        out.extend(d[(left - digits) as usize..left as usize].iter().rev());
        out.extend(core::iter::repeat_n(b'0', (take - digits) as usize));
        left -= digits;
        need -= g;
        if left <= 0 && need <= 0 { break; }
        need -= 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/* the sign `s` asks for then `prefix` and `body` padded as numbers pad */
fn signed(s: &Spec, neg: bool, prefix: &str, body: &str) -> String {
    let sign = if neg { "-" } else { match s.sign { b'+' => "+", b' ' => " ", _ => "" } };
    pad(s, s.align.unwrap_or(b'>'), sign, prefix, body)
}

/* `sign` and `prefix` then `body` filled to the width where `=` fills after the prefix */
fn pad(s: &Spec, align: u8, sign: &str, prefix: &str, body: &str) -> String {
    let n = s.width.saturating_sub(sign.len() + prefix.len() + body.chars().count());
    let mut out = String::with_capacity(sign.len() + prefix.len() + body.len() + n * s.fill.len_utf8());
    let fill = |out: &mut String, k: usize| out.extend(core::iter::repeat_n(s.fill, k));
    match align {
        b'<' => { out.push_str(sign); out.push_str(prefix); out.push_str(body); fill(&mut out, n); }
        b'^' => { fill(&mut out, n / 2); out.push_str(sign); out.push_str(prefix); out.push_str(body); fill(&mut out, n - n / 2); }
        b'=' => { out.push_str(sign); out.push_str(prefix); fill(&mut out, n); out.push_str(body); }
        _ => { fill(&mut out, n); out.push_str(sign); out.push_str(prefix); out.push_str(body); }
    }
    out
}
