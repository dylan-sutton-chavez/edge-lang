use alloc::string::String;

use super::super::VM;
use super::super::types::*;

/* Convert a float to i128 for int()/round(), NaN and infinity raise, and the value is rejected before the saturating cast overflows. `transform` truncates or rounds. */
pub(crate) fn finite_f64_to_i128(f: f64, transform: impl Fn(f64) -> f64) -> Result<i128, VmErr> {
    if f.is_nan() { return Err(cold_value("cannot convert float NaN to integer")); }
    if f.is_infinite() { return Err(VmErr::Raised(String::from("OverflowError: cannot convert float infinity to integer"))); }
    let v = transform(f);
    // i128::MAX as f64 rounds up past the true max, so reject before the saturating cast.
    if !(-1.7014118346046923e38..=1.7014118346046921e38).contains(&v) { return Err(cold_overflow()); }
    Ok(v as i128)
}

fn i128_to_radix(n: i128, radix: u32, prefix: &str) -> String {
    if n == 0 {
        let mut s = String::with_capacity(prefix.len() + 1);
        s.push_str(prefix); s.push('0'); return s;
    }
    let neg = n < 0;
    // unsigned_abs handles i128::MIN safely, returning 2^127 in u128.
    let mut abs = n.unsigned_abs();
    // Max length is i128 in base 2, 128 digits + sign + prefix. 144 fits all.
    let mut buf = [0u8; 144];
    let mut i = buf.len();
    while abs > 0 {
        i -= 1;
        let d = (abs % radix as u128) as u8;
        buf[i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        abs /= radix as u128;
    }
    let mut s = String::with_capacity(prefix.len() + buf.len() - i + 1);
    if neg { s.push('-'); }
    s.push_str(prefix);
    s.push_str(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
    s
}

/* A compensated sum, its compensation added only when nonzero and finite, so overflow stays infinite. */
fn settle((sum, comp): (f64, f64)) -> f64 { if comp != 0.0 && comp.is_finite() { sum + comp } else { sum } }

/* One Neumaier (improved Kahan) compensated-summation step returning (sum, comp). */
fn neumaier(sum: f64, comp: f64, x: f64) -> (f64, f64) {
    let t = sum + x;
    let c = if libm::fabs(sum) >= libm::fabs(x) { (sum - t) + x } else { (x - t) + sum };
    (t, comp + c)
}

/* `x` rounded half to even to a multiple of 10^k, reading every digit `{:.0}` prints. */
fn round_float_tens(x: f64, k: usize) -> f64 {
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

// Rounds an integer to -k decimal digits using banker's rounding, matching Python.
fn round_int_banker(n: i128, k: u32) -> i128 {
    let factor = 10i128.checked_pow(k.min(38)).unwrap_or(i128::MAX);
    let q = n.div_euclid(factor);
    let r = n.rem_euclid(factor);
    let half = factor / 2;
    let up = r > half || (r == half && q % 2 != 0);
    if up { q + 1 } else { q }.saturating_mul(factor)
}

/* `int(s, base)` parsing, optional sign, optional 0x/0o/0b prefix (matching the base, or inferred when base==0), `_` digit separators, radix 0 or 2..=36. */
fn parse_int_radix(s: &str, base: i64) -> Result<i128, VmErr> {
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
    // Parse the magnitude unsigned so i128::MIN fits, then apply the sign.
    let mag = u128::from_str_radix(&cleaned, radix).map_err(|_| cold_value("int(): invalid literal"))?;
    if neg {
        if mag > (1u128 << 127) { return Err(cold_value("int(): invalid literal")); }
        Ok((mag as i128).wrapping_neg())
    } else {
        i128::try_from(mag).map_err(|_| cold_value("int(): invalid literal"))
    }
}

impl<'a> VM<'a> {

    pub fn call_abs(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let o = self.pop()?;
        // Honor a user `__abs__` and pass any return type through.
        if let Some(r) = self.try_call_dunder(o, "__abs__", &[], chunk)? {
            self.push(r);
            return Ok(());
        }
        let v = if o.is_float() {
            Val::float(o.as_float().abs())
        } else if let Some(i) = self.as_i128(o) {
            // i128::MIN.checked_abs() is None, trap as OverflowError.
            self.int_to_val(i.checked_abs())?
        } else {
            return Err(cold_type("abs() requires a number"));
        };
        self.push(v); Ok(())
    }

    pub fn call_int(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        // Two-arg form `int(string, base)` parses the string in the given radix.
        if argc == 2 {
            let base_v = self.pop()?;
            let o = self.pop()?;
            if !base_v.is_int() { return Err(cold_type("int() base must be an integer")); }
            let base = base_v.as_int();
            let s = match self.heap.try_get(o) {
                Some(HeapObj::Str(s)) => s.clone(),
                Some(HeapObj::Bytes(b)) => core::str::from_utf8(b).map_err(|_| cold_value("int() bytes not valid utf-8"))?.into(),
                _ => return Err(cold_type("int() can't convert non-string with explicit base")),
            };
            let i = parse_int_radix(&s, base)?;
            let v = self.int_to_val(Some(i))?;
            self.push(v); return Ok(());
        }
        if argc == 0 { self.push(Val::int(0)); return Ok(()); }
        if argc > 2 { for _ in 0..argc { let _ = self.pop(); } return Err(cold_type("int() takes at most 2 arguments")); }
        let o = self.pop()?;
        // Already an int (inline or LongInt), pass through unchanged.
        if o.is_int() { self.push(o); return Ok(()); }
        if o.is_heap() && matches!(self.heap.get(o), HeapObj::LongInt(_)) {
            self.push(o); return Ok(());
        }
        let i: i128 = if o.is_float() {
            finite_f64_to_i128(o.as_float(), ftrunc)?
        }
            else if o.is_bool() { o.as_bool() as i128 }
            else if o.is_heap() && let HeapObj::Str(s) = self.heap.get(o) {
                let s = s.clone();
                parse_int_radix(&s, 10)?
            }
            else if o.is_heap() && let HeapObj::Bytes(b) = self.heap.get(o) {
                parse_int_radix(core::str::from_utf8(b).map_err(|_| cold_value("int() bytes not valid utf-8"))?, 10)?
            }
            else if o.is_heap() && matches!(self.heap.get(o), HeapObj::Instance(..)) {
                // Honor a user `__int__` method.
                match self.try_call_dunder(o, "__int__", &[], chunk)? {
                    Some(r) if r.is_int() || r.is_bool() => self.as_i128(r).unwrap_or(r.as_bool() as i128),
                    Some(_) => return Err(cold_type("__int__ returned non-int")),
                    None => return Err(cold_type("int() requires a number or string")),
                }
            }
            else { return Err(cold_type("int() requires a number or string")); };
        let v = self.int_to_val(Some(i))?;
        self.push(v); Ok(())
    }

    /* Converts int or parseable string to floating point. */
    pub fn call_float(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        if argc == 0 { self.push(Val::float(0.0)); return Ok(()); } // `float()` is 0.0.
        let o = self.pop()?;
        // A float comes back as the same object.
        if o.is_float() { self.push(o); return Ok(()); }
        let f = if let Some(f) = num_as_f64(o, &self.heap) { f }
            else if o.is_heap() && let HeapObj::Str(s) = self.heap.get(o) {
                let t = crate::util::uni::number_text(s);
                let t = t.as_ref();
                // Accept `_` digit separators, strip then parse.
                let cleaned = if t.contains('_') {
                    if t.starts_with('_') || t.ends_with('_') || t.contains("__") { return Err(cold_value("float(): invalid literal")); }
                    t.chars().filter(|&c| c != '_').collect::<alloc::string::String>()
                } else { alloc::string::String::from(t) };
                cleaned.parse().map_err(|_| cold_value("float(): invalid literal"))?
            }
            else if o.is_heap() && matches!(self.heap.get(o), HeapObj::Instance(..)) {
                // Honor a user `__float__` and fall back to `__index__` like CPython.
                match self.try_call_dunder(o, "__float__", &[], chunk)? {
                    Some(r) if r.is_float() => { self.push(r); return Ok(()); }
                    Some(_) => return Err(cold_type("__float__ returned non-float")),
                    None => match self.try_call_dunder(o, "__index__", &[], chunk)? {
                        Some(r) if r.is_int() || r.is_bool() => self.as_i128(r).unwrap_or(r.as_bool() as i128) as f64,
                        Some(_) => return Err(cold_type("__index__ returned non-int")),
                        None => return Err(cold_type("float() requires a number or string")),
                    },
                }
            }
            else { return Err(cold_type("float() requires a number or string")); };
        self.push(Val::float(f)); Ok(())
    }

    pub fn call_chr(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        let i = self.as_i128(o).ok_or_else(|| cold_type("chr() requires an integer"))?;
        let c = u32::try_from(i).ok().and_then(char::from_u32).ok_or_else(|| cold_value("chr() arg out of range"))?;
        let mut s = String::with_capacity(4);
        s.push(c);
        self.alloc_and_push_str(s)
    }

    pub fn call_ord(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        if o.is_heap()
            && let HeapObj::Str(s) = self.heap.get(o) {
                let mut cs = s.chars();
                if let (Some(c), None) = (cs.next(), cs.next()) {
                    self.push(Val::int(c as i64)); return Ok(());
                }
        }
        // ord() of a one-byte bytes returns the byte value.
        if o.is_heap() && let HeapObj::Bytes(b) = self.heap.get(o) && b.len() == 1 {
            let v = b[0] as i64;
            self.push(Val::int(v)); return Ok(());
        }
        Err(cold_type("ord() requires string of length 1"))
    }

    pub fn call_round(&mut self, op: u16) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        let v = match (args.first().copied(), args.get(1).copied()) {
            (Some(o), Some(n)) if o.is_float() && n.is_int() => {
                let x = o.as_float();
                let nd = n.as_int();
                if !x.is_finite() {
                    Val::float(x)
                } else if nd >= 0 {
                    // Correctly-rounded decimal (round-half-even on the true value), avoids the double-rounding `x*10^n` would introduce (e.g. round(2.675, 2) -> 2.67).
                    let s = alloc::format!("{:.*}", (nd as usize).min(323), x);
                    Val::float(s.parse().unwrap_or(x))
                } else {
                    let r = round_float_tens(x, nd.unsigned_abs().min(309) as usize);
                    if r.is_infinite() { return Err(VmErr::Raised(String::from("OverflowError: rounded value too large to represent"))); }
                    Val::float(r)
                }
            }
            (Some(o), None) if o.is_float() => {
                // 1-arg round returns an int, promote via int_to_val so large results don't wrap.
                let i = finite_f64_to_i128(o.as_float(), fround)?;
                self.int_to_val(Some(i))?
            }
            // Ints/bools round to themselves, negative ndigits round to tens/hundreds.
            (Some(o), n) if o.is_bool() || o.is_int() || (o.is_heap() && matches!(self.heap.get(o), HeapObj::LongInt(_))) => {
                let i = if o.is_bool() { o.as_bool() as i128 } else { self.as_i128(o).ok_or_else(|| cold_type("round() requires a number"))? };
                let nd = match n { Some(n) if n.is_int() => n.as_int(), _ => 0 };
                let r = if nd < 0 { round_int_banker(i, (-nd) as u32) } else { i };
                self.int_to_val(Some(r))?
            }
            _ => return Err(cold_type("round() requires a number")),
        };
        self.push(v); Ok(())
    }

    pub fn call_min(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> { self.call_minmax(op, true, chunk) }
    pub fn call_max(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> { self.call_minmax(op, false, chunk) }

    fn call_minmax(&mut self, op: u16, is_min: bool, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let (positional, kw_flat) = self.parse_call_args(op)?;
        // Optional `default=` (returned when a single iterable is empty) and `key=` (compare by key(x)).
        let mut default: Option<Val> = None;
        let mut key: Option<Val> = None;
        for pair in kw_flat.as_chunks::<2>().0 {
            match self.kw_name(pair[0]) {
                Some("default") => default = Some(pair[1]),
                Some("key") => { if !pair[1].is_none() { key = Some(pair[1]); } }
                _ => return Err(cold_type("min()/max() got an unexpected keyword argument")),
            }
        }
        // One arg iterable, many args are values.
        let items = if positional.len() == 1 { self.extract_iter(positional[0])? } else { positional };
        let label = if is_min { "min() arg is an empty sequence" } else { "max() arg is an empty sequence" };
        if items.is_empty() {
            return match default { Some(d) => { self.push(d); Ok(()) }, None => Err(cold_value(label)) };
        }
        // Without a key, compare elements directly, with one, compare key(x) but return the winning element.
        let keys = match key {
            Some(k) => Some(self.call_rows(k, core::slice::from_ref(&items), items.len(), chunk)?),
            None => None,
        };
        let keys = keys.as_deref().unwrap_or(&items);
        // An instance key runs `__lt__` like `sorted`, so then the operands stay rooted.
        let roots = if self.any_instance(keys) { items.len() + keys.len() } else { 0 };
        let best = self.with_roots(items.iter().chain(keys).copied().take(roots), |vm| vm.extreme_index(keys, is_min, chunk))?;
        self.push(items[best]); Ok(())
    }

    /* Index of the least key, or with `!is_min` the greatest, the first one on a tie. */
    fn extreme_index(&mut self, keys: &[Val], is_min: bool, chunk: &crate::parser::SSAChunk) -> Result<usize, VmErr> {
        let mut best = 0;
        for i in 1..keys.len() {
            let (l, r) = if is_min { (keys[i], keys[best]) } else { (keys[best], keys[i]) };
            if self.sort_lt(l, r, chunk)? { best = i; }
        }
        Ok(best)
    }

    pub fn call_sum(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        if args.is_empty() { return Err(cold_type("sum() requires at least 1 argument")); }
        let start = if args.len() > 1 { args[1] } else { Val::int(0) };
        let mut cur = self.iter_cursor(args[0])?;
        let base = self.temp_roots.len();
        let result = self.sum_cursor(&args, &mut cur, start, chunk);
        self.temp_roots.truncate(base);
        let result = result?;
        self.push(result); Ok(())
    }

    fn sum_cursor(&mut self, args: &[Val], cur: &mut IterFrame, start: Val, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
        let base = self.temp_roots.len();
        let mut acc = start;
        // Once a float enters, switch to Neumaier compensated summation (Python 3.12+).
        let mut fstate: Option<(f64, f64)> = if start.is_float() { Some((start.as_float(), 0.0)) } else { None };
        while let Some(item) = cur.next_item(&mut self.heap)? {
            self.charge_step()?;
            let user = self.instance_class(acc).is_some() || self.instance_class(item).is_some();
            // A user `__add__` can collect, so from the first instance on the iterable and its items stay rooted.
            if user && self.temp_roots.len() == base {
                self.temp_roots.extend_from_slice(args);
                if let IterFrame::Seq { items, .. } = &*cur { self.temp_roots.extend(items.iter().copied()); }
            }
            match fstate {
                Some((s, c)) => match self.to_f64_coerce(item) {
                    Ok(x) => fstate = Some(neumaier(s, c, x)),
                    // Non-numeric after floats, the plain addition raises the proper TypeError.
                    Err(_) => { acc = self.sum_add(Val::float(settle((s, c))), item, chunk)?; fstate = None; }
                },
                None if (item.is_float() || acc.is_float())
                    && self.to_f64_coerce(acc).is_ok() && self.to_f64_coerce(item).is_ok() => {
                    let base = self.to_f64_coerce(acc).unwrap();
                    let x = self.to_f64_coerce(item).unwrap();
                    fstate = Some(neumaier(base, 0.0, x));
                }
                None if user => { acc = self.sum_add(acc, item, chunk)?; }
                None => { acc = self.add_vals(acc, item)?; }
            }
        }
        Ok(match fstate { Some(state) => Val::float(settle(state)), None => acc })
    }

    /* One step of `sum`, through a user `__add__` or `__radd__` when either side defines one. */
    fn sum_add(&mut self, a: Val, b: Val, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
        match self.try_binary_dunder(crate::parser::types::OpCode::Add, a, b, false, chunk)? {
            Some(v) => Ok(v),
            None => self.add_vals(a, b),
        }
    }

    pub fn call_range(&mut self, op: u16) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        // Accept any integer (incl. LongInt/bool) that fits the i64 range bounds.
        let gi = |i: Option<i128>| -> Result<i64, VmErr> {
            match i {
                Some(n) if (i64::MIN as i128..=i64::MAX as i128).contains(&n) => Ok(n as i64),
                Some(_) => Err(cold_overflow()),
                None => Err(cold_type("range() arguments must be integers")),
            }
        };
        let (s, e, st) = match args.len() {
            1 => (0, gi(self.as_i128(args[0]))?, 1),
            2 => (gi(self.as_i128(args[0]))?, gi(self.as_i128(args[1]))?, 1),
            3 => (gi(self.as_i128(args[0]))?, gi(self.as_i128(args[1]))?, gi(self.as_i128(args[2]))?),
            _ => return Err(cold_type("range() takes 1 to 3 arguments")),
        };
        if st == 0 { return Err(cold_value("range() step cannot be zero")); }
        let val = self.heap.alloc(HeapObj::Range(s, e, st))?;
        self.push(val); Ok(())
    }

    // Number formatting.

    pub fn call_bin(&mut self) -> Result<(), VmErr> { self.call_radix(2,  "0b") }
    pub fn call_oct(&mut self) -> Result<(), VmErr> { self.call_radix(8,  "0o") }
    pub fn call_hex(&mut self) -> Result<(), VmErr> { self.call_radix(16, "0x") }

    /* Push int as "<prefix><digits>" with optional sign. */
    fn call_radix(&mut self, radix: u32, prefix: &str) -> Result<(), VmErr> {
        let o = self.pop()?;
        let Some(i) = self.as_i128(o) else { return Err(cold_type("integer required")); };
        self.alloc_and_push_str(i128_to_radix(i, radix, prefix))
    }

    pub fn call_divmod(&mut self) -> Result<(), VmErr> {
        let b = self.pop()?;
        let a = self.pop()?;
        let (q, r) = self.divmod_vals(a, b, "divmod() requires numeric operands")?;
        self.alloc_and_push_tuple(alloc::vec![q, r])
    }

    pub fn call_pow(&mut self, op: u16) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        match args.len() {
            2 => {
                let r = self.pow_vals(args[0], args[1], "pow() requires numeric operands")?;
                self.push(r);
                Ok(())
            }
            3 => {
                // Modular exponentiation (a ** b) % c on i128.
                let (Some(base), Some(modulus)) = (self.as_i128(args[0]), self.as_i128(args[2]))
                else { return Err(cold_type("pow() with 3 args requires integers")); };
                let exp = self.as_i128(args[1]).ok_or_else(|| cold_type("pow() with 3 args requires integer exponent"))?;
                if modulus == 0 { return Err(cold_value("pow() 3rd argument cannot be 0")); }

                let m = modulus.unsigned_abs();
                // Cap |m| < 2^63 so m*m fits in i128, larger moduli would overflow silently.
                if m > (1u128 << 63) { return Err(cold_value("pow() modulus too large; must be < 2^63 (no arbitrary precision)")); }
                let m = m as i128;
                // Seed with 1 % m so pow(x, 0, 1) yields 0, not 1.
                let mut result = 1i128.rem_euclid(m);
                let mut b = base.rem_euclid(m);
                // A negative exponent raises the inverse of the base, which needs it coprime to the modulus.
                if exp < 0 {
                    let (mut r, mut r1, mut t, mut t1) = (m, b, 0i128, 1i128);
                    while r1 != 0 { let q = r / r1; (r, r1, t, t1) = (r1, r - q * r1, t1, t - q * t1); }
                    if r != 1 { return Err(cold_value("base is not invertible for the given modulus")); }
                    b = t.rem_euclid(m);
                }
                let mut e = exp.unsigned_abs();
                while e > 0 {
                    if e & 1 == 1 {
                        result = (result * b).rem_euclid(m);
                    }
                    e >>= 1;
                    if e > 0 {
                        b = (b * b).rem_euclid(m);
                    }
                }
                // Result takes the sign of the modulus, matching Python.
                if modulus < 0 && result != 0 { result -= m; }
                let r = self.int_to_val(Some(result))?;
                self.push(r);
                Ok(())
            }
            _ => Err(cold_type("pow() takes 2 or 3 arguments")),
        }
    }


    /* Two-arg power for `pow()` and `**`. int**non-neg int stays i128 (overflow trap), floats or negative exponents promote to f64. */
    pub(crate) fn pow_vals(&mut self, a: Val, b: Val, err_msg: &'static str) -> Result<Val, VmErr> {
        if let (Some(ai), Some(exp)) = (self.as_i128(a), self.as_i128(b).and_then(|e| i64::try_from(e).ok())) {
            if exp >= 0 {
                // i128 exp-by-squaring, overflow at any step -> OverflowError. Bases +/- 1/0 never overflow regardless of exp size.
                let mut result: i128 = 1;
                let mut base = ai;
                let mut e = exp;
                while e > 0 {
                    if e & 1 == 1 {
                        result = result.checked_mul(base).ok_or_else(cold_overflow)?;
                    }
                    e >>= 1;
                    if e > 0 {
                        base = base.checked_mul(base).ok_or_else(cold_overflow)?;
                    }
                }
                return self.int_to_val(Some(result));
            }
            if ai == 0 { return Err(VmErr::Raised(String::from("ZeroDivisionError: 0.0 cannot be raised to a negative power"))); }
            // Clamp instead of wrapping so a huge negative exponent underflows to 0.0, base ±1 keeps its parity.
            let r = match ai {
                1 => 1.0,
                -1 => if exp % 2 == 0 { 1.0 } else { -1.0 },
                _ => libm::pow(ai as f64, exp as f64),
            };
            return Ok(Val::float(r));
        }
        let (Some(fa), Some(fb)) = (num_as_f64(a, &self.heap), num_as_f64(b, &self.heap)) else { return Err(cold_type(err_msg)); };
        if fa == 0.0 && fb < 0.0 { return Err(VmErr::Raised(String::from("ZeroDivisionError: 0.0 cannot be raised to a negative power"))); }
        // Without a complex type, a negative base under a fractional power has no real result.
        if crate::value::math::complex_power(fa, fb) { return Err(cold_value("negative number cannot be raised to a fractional power")); }
        fpowf(fa, fb).map(Val::float).ok_or_else(|| VmErr::Raised(String::from("OverflowError: (34, 'Numerical result out of range')")))
    }
}
