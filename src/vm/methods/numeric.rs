use super::prelude::*;

pub fn bit_length<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let (bits, _) = crate::frontend::int::bits(vm.heap(), recv).ok_or_else(|| cold_type("bit_length() requires an int"))?;
    vm.push(Val::int(bits as i64)); Ok(())
}

pub fn bit_count<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let (_, ones) = crate::frontend::int::bits(vm.heap(), recv).ok_or_else(|| cold_type("bit_count() requires an int"))?;
    vm.push(Val::int(ones as i64)); Ok(())
}

// `int.to_bytes(length=1, byteorder='big')`, unsigned (signed=False), errors if it doesn't fit.
pub fn to_bytes<H: Host>(vm: &mut H, recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let length = match pos.first() { Some(v) if v.is_int() => v.as_int().max(0) as usize, None => 1, _ => return Err(cold_type("length must be an integer")) };
    // Length is user-controlled, cap it against the memory left so a huge count errors instead of aborting.
    vm.heap().reserve(length)?;
    let big = byteorder_is_big(vm, pos.get(1))?;
    let signed = pos.get(2).is_some_and(|&s| vm.truthy(s));
    let out = crate::frontend::int::to_bytes(vm.heap(), recv, length, big, signed).ok_or_else(|| cold_type("to_bytes() requires an int"))??;
    let val = vm.heap_mut().alloc(HeapObj::Bytes(out))?;
    vm.push(val); Ok(())
}

// `int.from_bytes(bytes, byteorder='big')` classmethod, unsigned.
pub fn from_bytes<H: Host>(vm: &mut H, _recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let buf = recv_bytes_ref(vm, pos[0])?.to_vec();
    let big = byteorder_is_big(vm, pos.get(1))?;
    let signed = pos.get(2).is_some_and(|&s| vm.truthy(s));
    let v = crate::frontend::int::from_bytes(vm.heap_mut(), &buf, big, signed)?;
    vm.push(v); Ok(())
}

pub fn is_integer<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let f = if recv.is_float() { recv.as_float() } else { return Err(cold_type("is_integer() requires a float")); };
    vm.push(Val::bool(f.is_finite() && f == libm::trunc(f))); Ok(())
}

fn byteorder_is_big<H: Host>(vm: &H, arg: Option<&Val>) -> Result<bool, VmErr> {
    match arg {
        None => Ok(true), // default is 'big'
        Some(&v) if v.is_none() => Ok(true),
        Some(&v) => match vm.heap().try_get(v) {
            Some(HeapObj::Str(s)) if s == "big" => Ok(true),
            Some(HeapObj::Str(s)) if s == "little" => Ok(false),
            _ => Err(cold_value("byteorder must be 'little' or 'big'")),
        },
    }
}

/* `n.as_integer_ratio()` of an int, the int itself over one. */
pub fn int_ratio<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let n = crate::value::unboxed(recv, vm.heap());
    let n = if n.is_bool() { Val::int(n.as_bool() as i64) } else { n };
    vm.alloc_and_push_tuple(alloc::vec![n, Val::int(1)])
}

/* `n.is_integer()` of an int, always true. */
pub fn int_is_integer<H: Host>(vm: &mut H, _recv: Val, _pos: &[Val]) -> Result<(), VmErr> { vm.push(Val::bool(true)); Ok(()) }

/* `x.as_integer_ratio()` of a float, the exact fraction in lowest terms with a power of two below. */
pub fn float_ratio<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let f = crate::value::unboxed(recv, vm.heap()).as_float();
    if f.is_nan() { return Err(cold_value("cannot convert NaN to integer ratio")); }
    if f.is_infinite() { return Err(VmErr::Raised(alloc::string::String::from("OverflowError: cannot convert Infinity to integer ratio"))); }
    let (mantissa, exp) = parts(f);
    let pair = if exp >= 0 || mantissa == 0 {
        [crate::frontend::int::from_f64(vm.heap_mut(), f)?, Val::int(1)]
    } else {
        // The twos the mantissa ends in cancel against the denominator.
        let shed = (mantissa.trailing_zeros() as i32).min(-exp);
        let num = mantissa >> shed;
        let k = (-exp - shed) as usize;
        let mut den = alloc::vec![0u8; k / 8 + 1];
        den[0] = 1 << (k % 8);
        let num = if f < 0.0 { -num } else { num };
        [vm.heap_mut().int(num as i128)?, crate::frontend::int::from_bytes(vm.heap_mut(), &den, true, false)?]
    };
    vm.alloc_and_push_tuple(pair.to_vec())
}

/* The integer mantissa and the binary exponent of a finite `f`, `f == mantissa * 2**exp` with the mantissa positive. */
fn parts(f: f64) -> (i64, i32) {
    let bits = f.to_bits();
    let (raw_exp, frac) = (((bits >> 52) & 0x7FF) as i32, (bits & ((1 << 52) - 1)) as i64);
    if raw_exp == 0 { (frac, -1074) } else { (frac | 1 << 52, raw_exp - 1075) }
}

/* `x.hex()`, the float in hexadecimal, `0x1.<13 digits>p<exp>`, a subnormal led by `0x0.`. */
pub fn float_hex<H: Host>(vm: &mut H, recv: Val, _pos: &[Val]) -> Result<(), VmErr> {
    let f = crate::value::unboxed(recv, vm.heap()).as_float();
    let text = if f.is_nan() { "nan".into() } else if f.is_infinite() { if f > 0.0 { "inf".into() } else { "-inf".into() } } else {
        let sign = if f.is_sign_negative() { "-" } else { "" };
        let bits = f.to_bits();
        let (raw_exp, frac) = ((bits >> 52) & 0x7FF, bits & ((1 << 52) - 1));
        if raw_exp == 0 && frac == 0 { crate::s!(str sign, "0x0.0p+0") } else {
            let (lead, exp) = if raw_exp == 0 { (0, -1022) } else { (1, raw_exp as i64 - 1023) };
            crate::s!(str sign, "0x", int lead, ".", str &alloc::format!("{frac:013x}"), "p", str if exp < 0 { "" } else { "+" }, int exp)
        }
    };
    vm.alloc_and_push_str(text)
}

/* `float.fromhex(s)`, the float a hexadecimal string spells, rounded half to even like any literal. */
pub fn float_fromhex<H: Host>(vm: &mut H, _recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let text = val_to_str(vm, pos[0], "fromhex() argument must be str")?;
    let f = parse_hex_float(text.trim()).ok_or_else(|| cold_value("invalid hexadecimal floating-point string"))?;
    let f = f.map_err(|()| VmErr::Raised(alloc::string::String::from("OverflowError: hexadecimal value too large to represent as a float")))?;
    vm.push(Val::float(f));
    Ok(())
}

/* The value of a hexadecimal float, None when it is no such text, Err when it is too large for a float. */
fn parse_hex_float(s: &str) -> Option<Result<f64, ()>> {
    let (neg, body) = match s.as_bytes().first() { Some(b'-') => (true, &s[1..]), Some(b'+') => (false, &s[1..]), _ => (false, s) };
    let signed = |f: f64| if neg { -f } else { f };
    match body.to_ascii_lowercase().as_str() {
        "inf" | "infinity" => return Some(Ok(signed(f64::INFINITY))),
        "nan" => return Some(Ok(f64::NAN)),
        _ => {}
    }
    let body = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")).unwrap_or(body);
    let (digits, exp) = match body.find(['p', 'P']) { Some(i) => (&body[..i], body[i + 1..].parse::<i64>().ok()?), None => (body, 0) };
    let (int_part, frac_part) = digits.split_once('.').unwrap_or((digits, ""));
    if int_part.is_empty() && frac_part.is_empty() { return None; }
    // The digits gathered into a mantissa of up to 120 bits, the rest only noted as nonzero.
    let (mut m, mut e, mut sticky) = (0u128, exp.clamp(-100_000, 100_000) as i32, false);
    for (i, c) in int_part.chars().chain(frac_part.chars()).enumerate() {
        let d = c.to_digit(16)? as u128;
        let fraction = i >= int_part.len();
        if m >> 116 == 0 { m = m << 4 | d; if fraction { e -= 4; } }
        else { sticky |= d != 0; if !fraction { e += 4; } }
    }
    if m == 0 { return Some(Ok(signed(0.0))); }
    let bits = 128 - m.leading_zeros() as i32;
    let top = bits - 1 + e;
    if top > 1023 { return Some(Err(())); }
    // A normal float keeps 53 bits, a subnormal fewer, down to none.
    let precision = if top >= -1022 { 53 } else { top + 1075 };
    if precision < 0 { return Some(Ok(signed(0.0))); }
    let shift = bits - precision;
    let mut kept = if shift > 0 { m >> shift } else { m };
    if shift > 0 {
        let rest = m & ((1 << shift) - 1);
        let half = 1u128 << (shift - 1);
        if rest > half || (rest == half && (sticky || kept & 1 == 1)) { kept += 1; }
    }
    let f = libm::scalbn(kept as f64, e + shift.max(0));
    if f.is_infinite() { return Some(Err(())); }
    Some(Ok(signed(f)))
}

/* `float.from_number(x)`, a number made a float, any other value refused. */
pub fn float_from_number<H: Host>(vm: &mut H, _recv: Val, pos: &[Val]) -> Result<(), VmErr> {
    let x = crate::value::unboxed(pos[0], vm.heap());
    let Some(f) = crate::value::num_f64(x, vm.heap()).filter(|_| !x.is_heap() || crate::frontend::int::is_big(vm.heap(), x)) else {
        return Err(VmErr::TypeMsg(crate::s!("must be real number, not ", str vm.type_name(pos[0]))));
    };
    vm.push(Val::float(f?));
    Ok(())
}
