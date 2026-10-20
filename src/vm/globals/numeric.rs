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

/* `int(s, base)` parsing, optional sign, optional 0x/0o/0b prefix (matching the base, or inferred when base==0), `_` digit separators, radix 0 or 2..=36. */
fn parse_int_radix(heap: &mut HeapPool, s: &str, base: i64) -> Result<Val, VmErr> {
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
    pub fn call_abs(&mut self, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        let o = self.pop()?;
        // An instance's `__abs__` runs as a frame of the loop.
        if ok && self.is_instance(o) && self.check_job(o, crate::vm::jobs::Check::Abs) { return Ok(()); }
        // Honor a user `__abs__` and pass any return type through.
        if let Some(r) = self.try_call_dunder(o, "__abs__", &[], chunk)? {
            self.push(r);
            return Ok(());
        }
        let v = if o.is_float() {
            Val::float(o.as_float().abs())
        } else if o.is_int() || o.is_bool() {
            Val::int(self.as_i128(o).unwrap_or(0).abs() as i64)
        } else if let Some(r) = self.heap.foreign_unop(UnOp::Abs, o)? {
            r
        } else {
            return Err(self.bad_operand("abs()", o));
        };
        self.push(v); Ok(())
    }

    #[inline(never)]
    pub fn call_int(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        // Two-arg form `int(string, base)` parses the string in the given radix.
        if argc == 2 {
            let base_v = self.pop()?;
            let o = self.pop()?;
            if !base_v.is_int() { return Err(cold_type("int() base must be an integer")); }
            let base = base_v.as_int();
            let s = match self.heap.try_get(o) {
                Some(HeapObj::Str(s)) => s.clone(),
                Some(HeapObj::Bytes(b) | HeapObj::ByteArray(b)) => core::str::from_utf8(b).map_err(|_| self.invalid_int(cold_value("invalid literal"), o, base))?.into(),
                _ => return Err(cold_type("int() can't convert non-string with explicit base")),
            };
            let v = parse_int_radix(&mut self.heap, &s, base).map_err(|e| self.invalid_int(e, o, base))?;
            self.push(v); return Ok(());
        }
        if argc == 0 { self.push(Val::int(0)); return Ok(()); }
        if argc > 2 { for _ in 0..argc { let _ = self.pop(); } return Err(cold_type("int() takes at most 2 arguments")); }
        let o = self.pop()?;
        let o = self.plain_number(o, "__int__", chunk)?;
        // An int passes through unchanged.
        if o.is_int() || crate::frontend::int::is_big(&self.heap, o) { self.push(o); return Ok(()); }
        let v = if o.is_float() { float_to_int(&mut self.heap, o.as_float(), ftrunc)? }
            else if o.is_bool() { Val::int(o.as_bool() as i64) }
            else if o.is_heap() && let HeapObj::Str(s) = self.heap.get(o) {
                let s = s.clone();
                parse_int_radix(&mut self.heap, &s, 10).map_err(|e| self.invalid_int(e, o, 10))?
            }
            else if let Some(b) = self.heap.bytes_of(o) {
                let text = core::str::from_utf8(b).map(String::from).map_err(|_| cold_value("invalid literal"));
                text.and_then(|t| parse_int_radix(&mut self.heap, &t, 10)).map_err(|e| self.invalid_int(e, o, 10))?
            }
            else if o.is_heap() && matches!(self.heap.get(o), HeapObj::Instance(..)) {
                // A user `__int__` runs as a frame of the loop, its int checked as it returns.
                if ok && self.check_job(o, crate::vm::jobs::Check::Int(o)) { return Ok(()); }
                // Honor a user `__int__` method.
                match self.try_call_dunder(o, "__int__", &[], chunk)? {
                    Some(r) if r.is_bool() => Val::int(r.as_bool() as i64),
                    Some(r) if r.is_int() || crate::frontend::int::is_big(&self.heap, r) => r,
                    Some(_) => return Err(cold_type("__int__ returned non-int")),
                    None => return Err(int_arg(self, o)),
                }
            }
            else { return Err(int_arg(self, o)); };
        self.push(v); Ok(())
    }

    /* The number in `o`, a value of a class over int or float, through the class `hook` when it defines one. */
    fn plain_number(&mut self, o: Val, hook: &str, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
        let Some(&HeapObj::Boxed(n)) = self.heap.try_get(o) else { return Ok(o) };
        Ok(self.try_call_dunder(o, hook, &[], chunk)?.map_or(n, |r| crate::value::unboxed(r, &self.heap)))
    }

    /* An invalid literal error that quotes `o` and names `base` like Python, any other error as it is. */
    fn invalid_int(&self, e: VmErr, o: Val, base: i64) -> VmErr {
        match e {
            VmErr::Value(m) if m.contains("invalid literal") => VmErr::Raised(crate::s!("ValueError: invalid literal for int() with base ", int base, ": ", str &self.repr(o))),
            e => e,
        }
    }

    /* Converts int or parseable string to floating point. */
    #[inline(never)]
    pub fn call_float(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        if argc == 0 { self.push(Val::float(0.0)); return Ok(()); } // `float()` is 0.0.
        let o = self.pop()?;
        let o = self.plain_number(o, "__float__", chunk)?;
        // A float comes back as the same object.
        if o.is_float() { self.push(o); return Ok(()); }
        let text = match self.heap.try_get(o) {
            Some(HeapObj::Str(s)) => Some(s.clone()),
            _ => self.heap.bytes_of(o).map(|b| String::from_utf8_lossy(b).into_owned()),
        };
        let f = if let Some(f) = num_f64(o, &self.heap) { f? }
            else if let Some(s) = text {
                let t = crate::util::uni::number_text(&s);
                let t = t.as_ref();
                // Accept `_` digit separators, strip then parse.
                let cleaned = if t.contains('_') {
                    if t.starts_with('_') || t.ends_with('_') || t.contains("__") { return Err(self.no_float(o)); }
                    t.chars().filter(|&c| c != '_').collect::<alloc::string::String>()
                } else { alloc::string::String::from(t) };
                cleaned.parse().map_err(|_| self.no_float(o))?
            }
            else if o.is_heap() && matches!(self.heap.get(o), HeapObj::Instance(..)) {
                // A user `__float__` runs as a frame of the loop, its float checked as it returns.
                if ok && self.check_job(o, crate::vm::jobs::Check::Float(o)) { return Ok(()); }
                // Honor a user `__float__` and fall back to `__index__` like CPython.
                match self.try_call_dunder(o, "__float__", &[], chunk)? {
                    Some(r) if r.is_float() => { self.push(r); return Ok(()); }
                    Some(r) => return Err(self.returned_wrong(o, "__float__", "a float", r)),
                    None => match self.try_call_dunder(o, "__index__", &[], chunk)? {
                        Some(r) if r.is_int() || r.is_bool() || crate::frontend::int::is_big(&self.heap, r) => num_f64(r, &self.heap).unwrap_or(Ok(0.0))?,
                        Some(r) => return Err(self.returned_wrong(o, "__index__", "an int", r)),
                        None => return Err(float_arg(self, o)),
                    },
                }
            }
            else { return Err(float_arg(self, o)); };
        self.push(Val::float(f)); Ok(())
    }

    /* `complex(real, imag)` from numbers or one string, a value whose class defines `__complex__` asked first. */
    pub fn call_complex(&mut self, argc: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        use crate::frontend::complex::{Complex, parse, parts, val};
        let ok = core::mem::take(&mut self.job_ok);
        let args = self.pop_n(argc as usize)?;
        // Called where a frame may open, the dunder that converts an instance runs as a frame of the loop.
        if ok && argc == 1 && let Some(&a) = args.first() && self.is_instance(a) {
            if let Some(v) = self.calls_run(alloc::boxed::Box::new(ComplexOf { v: a, hook: 0 }), true)? { self.push(v); }
            return Ok(());
        }
        let num = |vm: &mut Self, v: Val, which: &str| -> Result<Complex, VmErr> {
            if let Some(z) = parts(&vm.heap, crate::value::unboxed(v, &vm.heap)) { return Ok(z); }
            for hook in ["__complex__", "__float__", "__index__"] {
                if let Some(r) = vm.try_call_dunder(v, hook, &[], chunk)? && let Some(z) = parts(&vm.heap, r) { return Ok(z); }
            }
            Err(VmErr::TypeMsg(match which {
                "" => crate::s!("complex() argument must be a string or a number, not ", str vm.type_name(v)),
                part => crate::s!("complex() argument '", str part, "' must be a real number, not ", str vm.type_name(v)),
            }))
        };
        let text = |vm: &Self, v: Val| match vm.heap.try_get(v) { Some(HeapObj::Str(s)) => Some(s.clone()), _ => None };
        let z = match args.as_slice() {
            [] => Complex { re: 0.0, im: 0.0 },
            [s] if text(self, *s).is_some() => parse(&text(self, *s).unwrap_or_default()).ok_or_else(|| cold_value("complex() arg is a malformed string"))?,
            [a] => num(self, *a, "")?,
            [a, b] => {
                let (x, y) = (num(self, *a, "real")?, num(self, *b, "imag")?);
                Complex { re: x.re - y.im, im: x.im + y.re }
            }
            _ => return Err(cold_type("complex() takes at most 2 arguments")),
        };
        let v = val(&mut self.heap, z.re, z.im)?;
        self.push(v);
        Ok(())
    }

    fn no_float(&self, o: Val) -> VmErr {
        VmErr::Raised(crate::s!("ValueError: could not convert string to float: ", str &self.repr(o)))
    }

    pub fn call_chr(&mut self) -> Result<(), VmErr> {
        let o = self.pop()?;
        if !(o.is_int() || o.is_bool() || crate::frontend::int::is_big(&self.heap, o)) { return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(o), "' object cannot be interpreted as an integer"))); }
        let c = self.as_i128(o).and_then(|i| u32::try_from(i).ok()).and_then(char::from_u32).ok_or_else(|| cold_value("chr() arg not in range(0x110000)"))?;
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
        if let Some(b) = self.heap.bytes_of(o) && b.len() == 1 {
            let v = b[0] as i64;
            self.push(Val::int(v)); return Ok(());
        }
        let found = match self.heap.try_get(o) {
            Some(HeapObj::Str(s)) => s.chars().count(),
            _ => match self.heap.bytes_of(o) { Some(b) => b.len(), None => return Err(VmErr::TypeMsg(crate::s!("ord() expected string of length 1, but ", str self.type_name(o), " found"))) },
        };
        Err(VmErr::TypeMsg(crate::s!("ord() expected a character, but string of length ", int found as i64, " found")))
    }

    #[inline(never)]
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
                float_to_int(&mut self.heap, o.as_float(), fround)?
            }
            // Ints/bools round to themselves, negative ndigits round to tens/hundreds.
            (Some(o), n) if o.is_bool() || o.is_int() || crate::frontend::int::is_big(&self.heap, o) => {
                let o = if o.is_bool() { Val::int(o.as_bool() as i64) } else { o };
                match n {
                    // An inline int and at most 18 dropped digits stay in i64, half to even.
                    Some(n) if o.is_int() && (-18..0).contains(&n.as_int()) => {
                        let (x, f) = (o.as_int(), 10i64.pow(n.as_int().unsigned_abs() as u32));
                        let (q, r) = (x.div_euclid(f), x.rem_euclid(f));
                        self.heap.int(((q + (2 * r > f || (2 * r == f && q % 2 != 0)) as i64) * f) as i128)?
                    }
                    Some(n) if n.is_int() && n.as_int() < 0 => crate::frontend::int::round_tens(&mut self.heap, o, n.as_int().unsigned_abs()).unwrap_or(Ok(o))?,
                    _ => o,
                }
            }
            _ => return Err(cold_type("round() requires a number")),
        };
        self.push(v); Ok(())
    }

    pub fn call_min(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> { self.call_minmax(op, true, chunk) }
    pub fn call_max(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> { self.call_minmax(op, false, chunk) }

    fn call_minmax(&mut self, op: u16, is_min: bool, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
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
        let label = if is_min { "min() iterable argument is empty" } else { "max() iterable argument is empty" };
        // An `eval` or `exec` key runs each text as a floor of the loop.
        if ok && let Some(k) = key && self.runs_text(k) {
            // An iterable that may run Python has its items gathered by a job of the loop first, the call run again on them.
            if let [o] = positional[..] && matches!(self.heap.try_get(o), Some(HeapObj::Instance(..) | HeapObj::Coroutine(..) | HeapObj::Iter(..))) {
                let base = self.stack.len();
                self.push(o);
                for &v in &kw_flat { self.push(v); }
                self.job_ok = true;
                let id = if is_min { NativeFnId::Min } else { NativeFnId::Max };
                if self.gather_arg(id, op, base, chunk)? { return Ok(()); }
                let o = self.stack[base];
                self.stack.truncate(base);
                self.job_ok = false;
                let items = self.extract_iter(o)?;
                if items.is_empty() { return match default { Some(d) => { self.push(d); Ok(()) }, None => Err(cold_value(label)) }; }
                return self.keys_floor(k, items, is_min);
            }
            let items = match positional[..] { [o] => self.extract_iter(o)?, _ => positional };
            if items.is_empty() { return match default { Some(d) => { self.push(d); Ok(()) }, None => Err(cold_value(label)) }; }
            return self.keys_floor(k, items, is_min);
        }
        // A builtin key stepping a generator among the items in Rust runs through a function of the loop, as a job stepping it, any iterable's items unknown yet.
        if ok && let Some(k) = key && self.steps_whole(k) {
            let leaf = |vm: &Self, vs: &[Val]| vs.iter().any(|&v| vm.steps_py(v));
            let held = match positional[..] {
                [o] => match self.heap.try_get(o) { Some(HeapObj::List(rc)) => { let l = rc.borrow().clone(); leaf(self, &l) }, Some(HeapObj::Tuple(t)) => leaf(self, t), _ => true },
                _ => leaf(self, &positional),
            };
            if held { key = self.through_loop(k, 1)?.or(key); }
            // Values whose key now runs as frames fold as one iterable does.
            if held && positional.len() > 1 && let Some(k) = key {
                let all = self.heap.alloc(HeapObj::Tuple(positional))?;
                return self.with_roots([all], |vm| vm.fold_run(all, crate::vm::jobs::Takes::Keyed(is_min, k), &[], ok, chunk));
            }
        }
        // One iterable folds, an item at a time, so a range never becomes a list.
        if let [o] = positional[..] {
            let takes = match key { Some(k) => crate::vm::jobs::Takes::Keyed(is_min, k), None => crate::vm::jobs::Takes::Extreme(is_min) };
            return self.fold_run(o, takes, default.as_slice(), ok, chunk);
        }
        // Many args are values.
        let items = positional;
        if items.is_empty() {
            return match default { Some(d) => { self.push(d); Ok(()) }, None => Err(cold_value(label)) };
        }
        // Values whose comparison or key may run user code fold as one iterable does, its dunders frames of the loop.
        if self.heap.users && (items.iter().any(|v| v.is_heap()) || key.is_some_and(|k| self.heap.user(k))) {
            let takes = match key { Some(k) => crate::vm::jobs::Takes::Keyed(is_min, k), None => crate::vm::jobs::Takes::Extreme(is_min) };
            let all = self.heap.alloc(HeapObj::Tuple(items))?;
            return self.with_roots([all], |vm| vm.fold_run(all, takes, &[], ok, chunk));
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

    /* Whether `item < best` for min or `item > best` for max, raising with the operator that ran. */
    #[inline]
    pub(crate) fn beats(&mut self, item: Val, best: Val, is_min: bool, chunk: &crate::parser::SSAChunk) -> Result<bool, VmErr> {
        if item.is_int() && best.is_int() { return Ok(if is_min { item.as_int() < best.as_int() } else { item.as_int() > best.as_int() }); }
        if is_min { return self.sort_lt(item, best, chunk); }
        if let Some(r) = self.try_compare_dunder(crate::parser::OpCode::Gt, item, best, chunk)? { return Ok(self.truthy(r)); }
        match self.values_lt(best, item, chunk) {
            Err(VmErr::TypeMsg(_)) => Err(VmErr::TypeMsg(crate::s!("'>' not supported between instances of '", str self.type_name(item), "' and '", str self.type_name(best), "'"))),
            r => r,
        }
    }

    /* Index of the least key, or with `!is_min` the greatest, the first one on a tie. */
    pub(crate) fn extreme_index(&mut self, keys: &[Val], is_min: bool, chunk: &crate::parser::SSAChunk) -> Result<usize, VmErr> {
        let mut best = 0;
        for i in 1..keys.len() {
            if self.beats(keys[i], keys[best], is_min, chunk)? { best = i; }
        }
        Ok(best)
    }

    pub fn call_sum(&mut self, op: u16, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let ok = core::mem::take(&mut self.job_ok);
        let args = self.pop_n(op as usize)?;
        if args.is_empty() { return Err(cold_type("sum() requires at least 1 argument")); }
        // Text and bytes join, which sum points to.
        match args.get(1).and_then(|&start| self.heap.try_get(start)) {
            Some(HeapObj::Str(_)) => return Err(cold_type("sum() can't sum strings [use ''.join(seq) instead]")),
            Some(HeapObj::Bytes(_)) => return Err(cold_type("sum() can't sum bytes [use b''.join(seq) instead]")),
            Some(HeapObj::ByteArray(_)) => return Err(cold_type("sum() can't sum bytearray [use b''.join(seq) instead]")),
            _ => {}
        }
        // Each item adds as it comes, as CPython 3.15 adds it.
        self.fold_run(args[0], crate::vm::jobs::Takes::Sum, &args[1..], ok, chunk)
    }

    /* One step of `sum`, through a user `__add__` or `__radd__` when either side defines one. */
    pub(crate) fn sum_add(&mut self, a: Val, b: Val, chunk: &crate::parser::SSAChunk) -> Result<Val, VmErr> {
        match self.try_binary_dunder(crate::parser::types::OpCode::Add, a, b, false, chunk)? {
            Some(v) => Ok(v),
            None => self.add_vals(crate::value::unboxed(a, &self.heap), crate::value::unboxed(b, &self.heap)),
        }
    }

    #[inline(never)]
    pub fn call_range(&mut self, op: u16) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        // A bound past i64 makes a wide range.
        let (s, e, st) = match args[..] { [e] => (Val::int(0), e, Val::int(1)), [s, e] => (s, e, Val::int(1)), [s, e, st] => (s, e, st), _ => return Err(cold_type("range() takes 1 to 3 arguments")) };
        if let Some(r) = crate::frontend::int::new_range(&mut self.heap, s, e, st) { let r = r?; self.push(r); return Ok(()); }
        // Accept any integer, bools included, that fits the i64 range bounds.
        // The first bound that is no integer names its type.
        if let Some(&bad) = args.iter().find(|&&a| self.as_i128(a).is_none() || a.is_float()) {
            return Err(VmErr::TypeMsg(crate::s!("'", str self.type_name(bad), "' object cannot be interpreted as an integer")));
        }
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
        if st == 0 { return Err(cold_value("range() arg 3 must not be zero")); }
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
        let Some(d) = crate::frontend::int::radix_digits(&self.heap, o, radix) else { return Err(cold_type("integer required")); };
        let (neg, digits) = d?;
        self.alloc_and_push_str(crate::s!(str if neg { "-" } else { "" }, str prefix, str &digits))
    }

    pub fn call_divmod(&mut self) -> Result<(), VmErr> {
        let b = self.pop()?;
        let a = self.pop()?;
        let q = self.floor_op(BinOp::FloorDiv, a, b, "divmod()")?;
        let r = self.floor_op(BinOp::Mod, a, b, "divmod()")?;
        self.alloc_and_push_tuple(alloc::vec![q, r])
    }

    #[inline(never)]
    pub fn call_pow(&mut self, op: u16) -> Result<(), VmErr> {
        let args = self.pop_n(op as usize)?;
        match args.len() {
            2 => {
                let r = self.pow_vals(args[0], args[1], "pow() requires numeric operands")?;
                self.push(r);
                Ok(())
            }
            3 => {
                // A modulus under 2^63 keeps every product in i128, so small operands skip the boxed ints.
                if let (Some(b), Some(e), Some(m)) = (self.as_i128(args[0]), self.as_i128(args[1]), self.as_i128(args[2]))
                    && m != 0 && m.unsigned_abs() < 1 << 63 {
                    let (am, mut acc, mut b, mut e) = (m.abs(), 1i128, b, e);
                    b = b.rem_euclid(am);
                    // A negative exponent raises the inverse of the base, which needs it coprime to the modulus.
                    if e < 0 {
                        let (mut r, mut r1, mut t, mut t1) = (am, b, 0i128, 1i128);
                        while r1 != 0 { let q = r / r1; (r, r1, t, t1) = (r1, r - q * r1, t1, t - q * t1); }
                        if r != 1 { return Err(cold_value("base is not invertible for the given modulus")); }
                        (b, e) = (t.rem_euclid(am), -e);
                    }
                    while e > 0 { if e & 1 == 1 { acc = acc * b % am; } b = b * b % am; e >>= 1; }
                    let r = acc % am;
                    let r = self.heap.int(if m < 0 && r != 0 { r - am } else { r })?;
                    self.push(r);
                    return Ok(());
                }
                let r = crate::frontend::int::pow_mod(&mut self.heap, args[0], args[1], args[2])
                    .ok_or_else(|| cold_type("pow() 3rd argument not allowed unless all arguments are integers"))??;
                self.push(r);
                Ok(())
            }
            _ => Err(cold_type("pow() takes 2 or 3 arguments")),
        }
    }

    /* Two-arg power for `pow()` and `**`, exact for ints and a float for floats or negative exponents. */
    pub(crate) fn pow_vals(&mut self, a: Val, b: Val, err_msg: &'static str) -> Result<Val, VmErr> {
        if let (Some(ai), Some(exp)) = (self.as_i128(a), self.as_i128(b).and_then(|e| i64::try_from(e).ok())) {
            if exp >= 0 {
                if let Some(r) = checked_pow(ai, exp as u64) { return self.heap.int(r); }
                return self.heap.foreign_binop(BinOp::Pow, a, b)?.ok_or_else(cold_overflow);
            }
            if ai == 0 { return Err(VmErr::Raised(String::from("ZeroDivisionError: zero to a negative power"))); }
            // Clamp instead of wrapping so a huge negative exponent underflows to 0.0, base ±1 keeps its parity.
            let r = match ai {
                1 => 1.0,
                -1 => if exp % 2 == 0 { 1.0 } else { -1.0 },
                _ => libm::pow(ai as f64, exp as f64),
            };
            return Ok(Val::float(r));
        }
        if let Some(r) = self.wide_op(BinOp::Pow, a, b)? { return Ok(r); }
        let (Some(fa), Some(fb)) = (num_f64(a, &self.heap), num_f64(b, &self.heap)) else { return Err(cold_type(err_msg)); };
        let (fa, fb) = (fa?, fb?);
        if fa == 0.0 && fb < 0.0 { return Err(VmErr::Raised(String::from("ZeroDivisionError: zero to a negative power"))); }
        // A negative base under a fractional power has only a complex result.
        if crate::value::math::complex_power(fa, fb) { return crate::frontend::complex::power(&mut self.heap, fa, fb); }
        fpowf(fa, fb).map(Val::float).ok_or_else(|| VmErr::Raised(String::from("OverflowError: (34, 'Numerical result out of range')")))
    }
}

/* `base ** e` by squaring, None once a step leaves i128. */
fn checked_pow(mut base: i128, mut e: u64) -> Option<i128> {
    let mut acc: i128 = 1;
    while e > 0 {
        if e & 1 == 1 { acc = acc.checked_mul(base)?; }
        e >>= 1;
        if e > 0 { base = base.checked_mul(base)?; }
    }
    Some(acc)
}

/* The TypeError of `int(x)` for a value with no integer form. */
#[cold]
fn int_arg(vm: &VM, o: Val) -> VmErr { VmErr::TypeMsg(crate::s!("int() argument must be a string, a bytes-like object or a real number, not '", str vm.type_name(o), "'")) }

/* The TypeError of `float(x)` for a value with no float form. */
#[cold]
fn float_arg(vm: &VM, o: Val) -> VmErr { VmErr::TypeMsg(crate::s!("float() argument must be a string or a real number, not '", str vm.type_name(o), "'")) }

/* `complex(v)` of an instance as a job, calling the first of `__complex__`, `__float__` and `__index__` its class holds as a frame, `hook` next. */
struct ComplexOf { v: Val, hook: usize }

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
