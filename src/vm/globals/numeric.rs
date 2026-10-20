use alloc::string::String;

use super::super::VM;
use super::super::types::*;

/* The int a float holds after `transform` truncates or rounds it, NaN and infinity raise. */
pub(crate) fn float_to_int(heap: &mut HeapPool, f: f64, transform: impl Fn(f64) -> f64) -> Result<Val, VmErr> {
    if f.is_nan() { return Err(cold_value("cannot convert float NaN to integer")); }
    if f.is_infinite() { return Err(VmErr::Raised(String::from("OverflowError: cannot convert float infinity to integer"))); }
    crate::frontend::int::from_f64(heap, transform(f))
}

/* A compensated sum, its compensation added only when nonzero and finite, so overflow stays infinite. */
pub(crate) fn settle((sum, comp): (f64, f64)) -> f64 { if comp != 0.0 && comp.is_finite() { sum + comp } else { sum } }

/* One Neumaier (improved Kahan) compensated-summation step returning (sum, comp). */
pub(crate) fn neumaier(sum: f64, comp: f64, x: f64) -> (f64, f64) {
    let t = sum + x;
    let c = if libm::fabs(sum) >= libm::fabs(x) { (sum - t) + x } else { (x - t) + sum };
    (t, comp + c)
}

/* `x` rounded half to even to a multiple of 10^k, reading every digit `{:.0}` prints. */
pub(crate) fn round_float_tens(x: f64, k: usize) -> f64 {
    let t = libm::trunc(x);
    let mut d = alloc::format!("{:0>w$.0}", libm::fabs(t), w = k + 1).into_bytes();
    let cut = d.len() - k;
    let up = match d[cut..].cmp(alloc::format!("5{:0<w$}", "", w = k - 1).as_bytes()) {
        core::cmp::Ordering::Greater => true,
        core::cmp::Ordering::Less => false,
        core::cmp::Ordering::Equal => x != t || d[cut - 1] % 2 == 1,
    };
    d.truncate(cut);
    if up {
        let carried = d.iter().rposition(|&c| c != b'9').map_or(0, |i| { d[i] += 1; i + 1 });
        d[carried..].fill(b'0');
        if carried == 0 { d.insert(0, b'1'); }
    }
    let r = core::str::from_utf8(&d).ok().and_then(|h| alloc::format!("{h}e{k}").parse().ok()).unwrap_or(0.0);
    libm::copysign(r, x)
}

/* `int(s, base)` parsing, optional sign, optional 0x/0o/0b prefix (matching the base, or inferred when base==0), `_` digit separators, radix 0 or 2..=36. */
pub(crate) fn parse_int_radix(heap: &mut HeapPool, s: &str, base: i64) -> Result<Val, VmErr> {
    if base != 0 && !(2..=36).contains(&base) {
        return Err(cold_value("int() base must be >= 2 and <= 36, or 0"));
    }
    let t = crate::util::uni::number_text(s);
    let t = t.as_ref();
    let (neg, rest) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let prefix = if rest.len() >= 2 && rest.as_bytes()[0] == b'0' {
        match rest.as_bytes()[1] | 0x20 {
            b'x' => Some(16u32), b'o' => Some(8), b'b' => Some(2), _ => None,
        }
    } else { None };
    let mut radix = base as u32;
    let mut body = rest;
    let mut had_prefix = false;
    if base == 0 {
        match prefix {
            Some(p) => { radix = p; body = &rest[2..]; had_prefix = true; }
            None => {
                radix = 10;
                // base 0 rejects superfluous leading zeros ("010"), "0"/"00" stay valid.
                if rest.len() > 1 && rest.starts_with('0') && !rest.bytes().all(|b| b == b'0') {
                    return Err(cold_value("int(): invalid literal with base 0"));
                }
            }
        }
    } else if prefix == Some(base as u32) {
        // Explicit base may carry its matching prefix, e.g. int("0x1f", 16).
        body = &rest[2..];
        had_prefix = true;
    }
    // One underscore may follow the base prefix, e.g. "0x_1f".
    if had_prefix && body.starts_with('_') { body = &body[1..]; }
    let cleaned: alloc::string::String = if body.contains('_') {
        if body.starts_with('_') || body.ends_with('_') || body.contains("__") {
            return Err(cold_value("int(): invalid literal"));
        }
        body.chars().filter(|&c| c != '_').collect()
    } else {
        alloc::string::String::from(body)
    };
    if cleaned.is_empty() { return Err(cold_value("int(): invalid literal")); }
    if let Ok(n) = i64::from_str_radix(&cleaned, radix) { return heap.int(if neg { -(n as i128) } else { n as i128 }); }
    let digits: Option<alloc::vec::Vec<u8>> = cleaned.chars().map(|c| c.to_digit(radix).map(|d| d as u8)).collect();
    crate::frontend::int::from_digits(heap, neg, &digits.ok_or_else(|| cold_value("int(): invalid literal"))?, radix)
}


impl<'a> VM<'a> {

















    // Number formatting.





}


/* The TypeError of `int(x)` for a value with no integer form. */
#[cold]
pub(crate) fn int_arg<H: Host>(vm: &H, o: Val) -> VmErr { VmErr::TypeMsg(crate::s!("int() argument must be a string, a bytes-like object or a real number, not '", str vm.type_name(o), "'")) }

/* The TypeError of `float(x)` for a value with no float form. */
#[cold]
pub(crate) fn float_arg<H: Host>(vm: &H, o: Val) -> VmErr { VmErr::TypeMsg(crate::s!("float() argument must be a string or a real number, not '", str vm.type_name(o), "'")) }


/* `complex(v)` of an instance as a job, calling the first of `__complex__`, `__float__` and `__index__` its class holds as a frame, `hook` next. */
pub(crate) struct ComplexOf { pub(crate) v: Val, pub(crate) hook: usize }

impl crate::vm::jobs::Calls for ComplexOf {
    fn step(&mut self, vm: &mut VM<'_>, got: Option<Result<Val, VmErr>>) -> Result<crate::vm::jobs::Call, VmErr> {
        use crate::frontend::complex::{parts, val};
        if let Some(r) = got && let Some(z) = parts(&vm.heap, r?) { return Ok(crate::vm::jobs::Call::Give(val(&mut vm.heap, z.re, z.im)?)); }
        let class = vm.instance_class(self.v).unwrap_or(Val::undef());
        while let Some(&hook) = ["__complex__", "__float__", "__index__"].get(self.hook) {
            self.hook += 1;
            if let Some((f, defining)) = vm.lookup_class_member(class, hook) && matches!(vm.heap.try_get(f), Some(HeapObj::Func(..))) {
                return Ok(crate::vm::jobs::Call::Frame(f, defining, [self.v, Val::undef(), Val::undef()]));
            }
        }
        Err(VmErr::TypeMsg(crate::s!("complex() argument must be a string or a number, not ", str vm.type_name(self.v))))
    }

    fn for_each_val(&self, f: &mut dyn FnMut(Val)) { f(self.v); }
}
