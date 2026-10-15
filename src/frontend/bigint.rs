/* Arbitrary precision integers, sign and magnitude over 32-bit limbs, least significant first. */
use alloc::{string::String, vec, vec::Vec};
use core::cmp::Ordering;

// Below this many limbs on the shorter side the schoolbook product wins.
pub const KARATSUBA: usize = 40;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BigInt { neg: bool, mag: Vec<u32> }

/* Drops the high zero limbs, so every value has one magnitude. */
fn trim(mut m: Vec<u32>) -> Vec<u32> {
    while m.last() == Some(&0) { m.pop(); }
    m
}

fn mag_cmp(a: &[u32], b: &[u32]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn mag_add(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (a, b) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u64;
    for (i, &x) in a.iter().enumerate() {
        let s = x as u64 + *b.get(i).unwrap_or(&0) as u64 + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry != 0 { out.push(carry as u32); }
    out
}

/* `a - b` for `a >= b`. */
fn mag_sub(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let d = x as i64 - *b.get(i).unwrap_or(&0) as i64 - borrow;
        borrow = (d < 0) as i64;
        out.push(d.rem_euclid(1 << 32) as u32);
    }
    trim(out)
}

fn schoolbook(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        if x == 0 { continue; }
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let t = x as u64 * y as u64 + out[i + j] as u64 + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    out
}

/* Adds `b` into `acc` at limb `at`, the accumulator long enough for the sum. */
fn add_into(acc: &mut [u32], b: &[u32], at: usize) {
    let mut carry = 0u64;
    let mut i = 0;
    while i < b.len() || carry != 0 {
        let s = acc[at + i] as u64 + *b.get(i).unwrap_or(&0) as u64 + carry;
        acc[at + i] = s as u32;
        carry = s >> 32;
        i += 1;
    }
}

/* Splits each factor at `h` limbs and multiplies with three half products instead of four. */
fn karatsuba(a: &[u32], b: &[u32]) -> Vec<u32> {
    let h = a.len().max(b.len()).div_ceil(2);
    let (a0, a1) = a.split_at(h.min(a.len()));
    let (b0, b1) = b.split_at(h.min(b.len()));
    let (a0, b0) = (trim(a0.to_vec()), trim(b0.to_vec()));
    let z0 = mag_mul(&a0, &b0);
    let z2 = mag_mul(a1, b1);
    let z1 = mag_sub(&mag_sub(&mag_mul(&mag_add(&a0, a1), &mag_add(&b0, b1)), &z0), &z2);
    let mut out = vec![0u32; a.len() + b.len() + 1];
    add_into(&mut out, &z0, 0);
    add_into(&mut out, &z1, h);
    add_into(&mut out, &z2, 2 * h);
    trim(out)
}

fn mag_mul(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() { return Vec::new(); }
    if a.len().min(b.len()) < KARATSUBA { trim(schoolbook(a, b)) } else { karatsuba(a, b) }
}

/* `a / d` and `a % d` for one limb `d`. */
fn divmod_small(a: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut q = vec![0u32; a.len()];
    let mut r = 0u64;
    for i in (0..a.len()).rev() {
        let cur = (r << 32) | a[i] as u64;
        q[i] = (cur / d as u64) as u32;
        r = cur % d as u64;
    }
    (trim(q), r as u32)
}

fn shl_bits(a: &[u32], s: u32) -> Vec<u32> {
    if s == 0 { return a.to_vec(); }
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u32;
    for &x in a { out.push((x << s) | carry); carry = x >> (32 - s); }
    out.push(carry);
    out
}

/* Knuth algorithm D, the quotient and remainder of magnitudes with `b` at least two limbs. */
fn knuth_d(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let s = b[b.len() - 1].leading_zeros();
    let v = trim(shl_bits(b, s));
    let mut u = shl_bits(a, s);
    if u.len() == a.len() { u.push(0); }
    let (n, m) = (v.len(), u.len() - v.len());
    let mut q = vec![0u32; m];
    let (vt, vs) = (v[n - 1] as u64, v[n - 2] as u64);
    for j in (0..m).rev() {
        let num = ((u[j + n] as u64) << 32) | u[j + n - 1] as u64;
        let (mut qh, mut rh) = (num / vt, num % vt);
        while qh >> 32 != 0 || qh * vs > ((rh << 32) | u[j + n - 2] as u64) {
            qh -= 1;
            rh += vt;
            if rh >> 32 != 0 { break; }
        }
        // Subtract qh * v from the window, adding v back once when it overshot.
        let mut borrow = 0i64;
        let mut carry = 0u64;
        for i in 0..n {
            let p = qh * v[i] as u64 + carry;
            carry = p >> 32;
            let t = u[i + j] as i64 - borrow - (p & 0xFFFF_FFFF) as i64;
            u[i + j] = t as u32;
            borrow = -(t >> 32);
        }
        let t = u[j + n] as i64 - borrow - carry as i64;
        u[j + n] = t as u32;
        if t < 0 {
            qh -= 1;
            let mut c = 0u64;
            for i in 0..n {
                let s2 = u[i + j] as u64 + v[i] as u64 + c;
                u[i + j] = s2 as u32;
                c = s2 >> 32;
            }
            u[j + n] = u[j + n].wrapping_add(c as u32);
        }
        q[j] = qh as u32;
    }
    // The remainder is the low limbs of u shifted back.
    let mut r = vec![0u32; n];
    for i in 0..n {
        r[i] = if s == 0 { u[i] } else { (u[i] >> s) | (u[i + 1] << (32 - s)) };
    }
    (trim(q), trim(r))
}

fn mag_divmod(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if mag_cmp(a, b) == Ordering::Less { return (Vec::new(), a.to_vec()); }
    if b.len() == 1 { let (q, r) = divmod_small(a, b[0]); return (q, trim(vec![r])); }
    knuth_d(a, b)
}

/* Low `bits` bits shifted out, true when any is set. */
fn any_low(a: &[u32], bits: u64) -> bool {
    let (w, b) = ((bits / 32) as usize, (bits % 32) as u32);
    a.iter().take(w).any(|&x| x != 0) || (b != 0 && a.get(w).is_some_and(|&x| x & ((1 << b) - 1) != 0))
}

fn mag_shr(a: &[u32], bits: u64) -> Vec<u32> {
    let (w, b) = ((bits / 32) as usize, (bits % 32) as u32);
    if w >= a.len() { return Vec::new(); }
    let a = &a[w..];
    if b == 0 { return a.to_vec(); }
    trim((0..a.len()).map(|i| (a[i] >> b) | a.get(i + 1).map_or(0, |&n| n << (32 - b))).collect())
}

fn mag_shl(a: &[u32], bits: u64) -> Vec<u32> {
    if a.is_empty() { return Vec::new(); }
    let (w, b) = ((bits / 32) as usize, (bits % 32) as u32);
    let mut out = vec![0u32; w];
    out.extend(shl_bits(a, b));
    trim(out)
}

impl BigInt {
    pub fn zero() -> Self { Self::default() }
    fn from_mag(neg: bool, mag: Vec<u32>) -> Self {
        let mag = trim(mag);
        Self { neg: neg && !mag.is_empty(), mag }
    }
    pub fn is_zero(&self) -> bool { self.mag.is_empty() }
    pub fn is_neg(&self) -> bool { self.neg }
    /* Limbs the magnitude takes, what memory and work checks size by. */
    pub fn limbs(&self) -> usize { self.mag.len() }

    pub fn from_i128(i: i128) -> Self {
        let m = i.unsigned_abs();
        Self::from_mag(i < 0, vec![m as u32, (m >> 32) as u32, (m >> 64) as u32, (m >> 96) as u32])
    }

    pub fn to_i128(&self) -> Option<i128> {
        if self.mag.len() > 4 { return None; }
        let m = self.mag.iter().rev().fold(0u128, |acc, &x| (acc << 32) | x as u128);
        if self.neg { if m <= 1u128 << 127 { Some((m as i128).wrapping_neg()) } else { None } } else { i128::try_from(m).ok() }
    }

    pub fn neg(&self) -> Self { Self::from_mag(!self.neg, self.mag.clone()) }
    pub fn abs(&self) -> Self { Self::from_mag(false, self.mag.clone()) }

    pub fn add(&self, o: &Self) -> Self {
        if self.neg == o.neg { return Self::from_mag(self.neg, mag_add(&self.mag, &o.mag)); }
        match mag_cmp(&self.mag, &o.mag) {
            Ordering::Less => Self::from_mag(o.neg, mag_sub(&o.mag, &self.mag)),
            _ => Self::from_mag(self.neg, mag_sub(&self.mag, &o.mag)),
        }
    }
    pub fn sub(&self, o: &Self) -> Self { self.add(&o.neg()) }
    pub fn mul(&self, o: &Self) -> Self { Self::from_mag(self.neg != o.neg, mag_mul(&self.mag, &o.mag)) }

    /* Floor quotient and remainder, the remainder taking the sign of the divisor, None for a zero divisor. */
    pub fn divmod(&self, o: &Self) -> Option<(Self, Self)> {
        if o.is_zero() { return None; }
        let (q, r) = mag_divmod(&self.mag, &o.mag);
        let (mut q, mut r) = (Self::from_mag(self.neg != o.neg, q), Self::from_mag(self.neg, r));
        if !r.is_zero() && r.neg != o.neg { q = q.sub(&Self::from_i128(1)); r = r.add(o); }
        Some((q, r))
    }

    pub fn pow(&self, mut e: u64) -> Self {
        let (mut base, mut acc) = (self.clone(), Self::from_i128(1));
        while e > 0 {
            if e & 1 == 1 { acc = acc.mul(&base); }
            e >>= 1;
            if e > 0 { base = base.mul(&base); }
        }
        acc
    }

    pub fn shl(&self, bits: u64) -> Self { Self::from_mag(self.neg, mag_shl(&self.mag, bits)) }

    /* Floor shift, a negative value rounds toward minus infinity. */
    pub fn shr(&self, bits: u64) -> Self {
        let q = Self::from_mag(self.neg, mag_shr(&self.mag, bits));
        if self.neg && any_low(&self.mag, bits) { q.sub(&Self::from_i128(1)) } else { q }
    }

    pub fn bit_length(&self) -> u64 {
        self.mag.last().map_or(0, |&top| self.mag.len() as u64 * 32 - top.leading_zeros() as u64)
    }
    pub fn bit_count(&self) -> u64 { self.mag.iter().map(|x| x.count_ones() as u64).sum() }

    /* Two's complement limbs over `n` limbs, the sign extending past them. */
    fn twos(&self, n: usize) -> Vec<u32> {
        let mut m = self.mag.clone();
        m.resize(n, 0);
        if self.neg {
            let mut carry = 1u64;
            for x in m.iter_mut() { let s = (!*x) as u64 + carry; *x = s as u32; carry = s >> 32; }
        }
        m
    }
    fn from_twos(mut m: Vec<u32>, neg: bool) -> Self {
        if neg {
            let mut carry = 1u64;
            for x in m.iter_mut() { let s = (!*x) as u64 + carry; *x = s as u32; carry = s >> 32; }
        }
        Self::from_mag(neg, m)
    }
    /* `&`, `|` or `^` limb by limb in two's complement, the sign bits combined the same way. */
    pub fn bitwise(&self, o: &Self, f: impl Fn(u32, u32) -> u32) -> Self {
        // Two non-negative operands need no two's complement, their limbs combine as they are.
        if !self.neg && !o.neg {
            let at = |m: &[u32], i: usize| m.get(i).copied().unwrap_or(0);
            return Self::from_mag(false, (0..self.mag.len().max(o.mag.len())).map(|i| f(at(&self.mag, i), at(&o.mag, i))).collect());
        }
        let n = self.mag.len().max(o.mag.len()) + 1;
        let (a, b) = (self.twos(n), o.twos(n));
        let sign = |neg: bool| if neg { u32::MAX } else { 0 };
        let neg = f(sign(self.neg), sign(o.neg)) != 0;
        Self::from_twos(a.iter().zip(&b).map(|(&x, &y)| f(x, y)).collect(), neg)
    }
    pub fn invert(&self) -> Self { self.neg().sub(&Self::from_i128(1)) }

    /* The magnitude in base `radix`, lowercase, without a sign. */
    pub fn to_radix(&self, radix: u32) -> String {
        if self.is_zero() { return String::from("0"); }
        // A magnitude within u128 prints in one pass, the common case of an int just past the inline range.
        if radix == 10 && self.mag.len() <= 4 { return String::from(itoa::Buffer::new().format(self.mag.iter().rev().fold(0u128, |acc, &x| acc << 32 | x as u128))); }
        let digit = |d: u32| char::from_digit(d, radix).unwrap_or('0');
        let mut out: Vec<u8> = Vec::new();
        if radix.is_power_of_two() {
            let (bits, mask) = (radix.trailing_zeros() as u64, radix as u64 - 1);
            let mut i = 0;
            // A digit width that divides a limb reads every digit from its own limb.
            if 32 % bits == 0 {
                for &x in &self.mag { for k in 0..32 / bits { out.push(digit((x as u64 >> (k * bits) & mask) as u32) as u8); } }
            }
            while 32 % bits != 0 && i < self.bit_length() {
                let (at, off) = ((i / 32) as usize, i % 32);
                let pair = self.mag[at] as u64 | (self.mag.get(at + 1).copied().unwrap_or(0) as u64) << 32;
                out.push(digit((pair >> off & mask) as u32) as u8);
                i += bits;
            }
        } else {
            // Peel the largest power of the radix that fits a limb, then spell each chunk.
            let (chunk, width) = { let mut p = radix as u64; let mut w = 1; while p * radix as u64 <= u32::MAX as u64 { p *= radix as u64; w += 1; } (p as u32, w) };
            let mut m = self.mag.clone();
            while !m.is_empty() {
                // The quotient overwrites the magnitude, so no chunk allocates.
                let mut r = 0u64;
                for x in m.iter_mut().rev() { let cur = r << 32 | *x as u64; *x = (cur / chunk as u64) as u32; r = cur % chunk as u64; }
                while m.last() == Some(&0) { m.pop(); }
                let mut r = r as u32;
                for _ in 0..width {
                    out.push(digit(r % radix) as u8);
                    r /= radix;
                    if m.is_empty() && r == 0 { break; }
                }
            }
        }
        while out.len() > 1 && out.last() == Some(&b'0') { out.pop(); }
        out.reverse();
        String::from_utf8(out).unwrap_or_default()
    }

    /* Bit `i` of the magnitude. */
    pub fn bit(&self, i: u64) -> bool { self.mag.get((i / 32) as usize).is_some_and(|&x| x >> (i % 32) & 1 == 1) }
    pub fn is_odd(&self) -> bool { self.bit(0) }

    /* Digits already checked for `radix`, most significant first, packed for powers of two and chunked otherwise. */
    pub fn from_digits(neg: bool, digits: &[u8], radix: u32) -> Self {
        if radix.is_power_of_two() {
            let w = radix.trailing_zeros() as usize;
            let mut mag = vec![0u32; (digits.len() * w).div_ceil(32)];
            for (i, &d) in digits.iter().rev().enumerate() {
                let (at, off) = (i * w / 32, i * w % 32);
                mag[at] |= (d as u32) << off;
                if off + w > 32 { mag[at + 1] |= (d as u32) >> (32 - off); }
            }
            return Self::from_mag(neg, mag);
        }
        let mut k = 1;
        while (radix as u64).pow(k + 1) <= u32::MAX as u64 { k += 1; }
        let mut mag: Vec<u32> = Vec::new();
        let head = digits.len() % k as usize;
        for chunk in core::iter::once(&digits[..head]).chain(digits[head..].chunks(k as usize)) {
            let (scale, mut carry) = ((radix as u64).pow(chunk.len() as u32), chunk.iter().fold(0u64, |a, &d| a * radix as u64 + d as u64));
            for x in mag.iter_mut() { let t = *x as u64 * scale + carry; *x = t as u32; carry = t >> 32; }
            if carry != 0 { mag.push(carry as u32); }
        }
        Self::from_mag(neg, mag)
    }

    /* The top 64 bits of the magnitude, its exponent past them, and whether any lower bit is set. */
    fn top64(&self) -> (u64, i64, bool) {
        let len = self.bit_length();
        if len <= 64 { return (self.mag.iter().rev().fold(0u64, |acc, &x| (acc << 32) | x as u64), 0, false); }
        let shift = len - 64;
        let top = mag_shr(&self.mag, shift).iter().rev().fold(0u64, |acc, &x| (acc << 32) | x as u64);
        (top, shift as i64, any_low(&self.mag, shift))
    }

    /* The nearest float, ties to even, None when it overflows. */
    pub fn to_f64(&self) -> Option<f64> {
        let (top, exp, sticky) = self.top64();
        let f = round_to_f64(top, exp, sticky)?;
        Some(if self.neg { -f } else { f })
    }

    /* The integer a finite integral float holds. */
    pub fn from_f64(f: f64) -> Self {
        let bits = f.to_bits();
        let exp = ((bits >> 52) & 0x7ff) as i64;
        let mant = if exp == 0 { (bits & ((1 << 52) - 1)) << 1 } else { (bits & ((1 << 52) - 1)) | (1 << 52) };
        let m = Self::from_i128(mant as i128);
        let e = exp - 1075;
        let v = if e >= 0 { m.shl(e as u64) } else { m.shr((-e) as u64) };
        if f < 0.0 { v.neg() } else { v }
    }

    /* Exact order against a float, None for NaN. */
    pub fn cmp_f64(&self, f: f64) -> Option<Ordering> {
        if f.is_nan() { return None; }
        if f.is_infinite() { return Some(if f > 0.0 { Ordering::Less } else { Ordering::Greater }); }
        let t = libm::trunc(f);
        match self.cmp(&Self::from_f64(t)) {
            Ordering::Equal if f != t => Some(if f > t { Ordering::Less } else { Ordering::Greater }),
            o => Some(o),
        }
    }

    /* `a / b` correctly rounded, None when the quotient overflows. */
    pub fn true_div(&self, o: &Self) -> Option<f64> {
        let (a, b) = (self.abs(), o.abs());
        // Scale so the quotient keeps at least 66 bits, then round once with the remainder as sticky.
        let shift = 66 - (a.bit_length() as i64 - b.bit_length() as i64);
        let (n, d) = if shift >= 0 { (a.shl(shift as u64), b) } else { (a, b.shl((-shift) as u64)) };
        let (q, r) = mag_divmod(&n.mag, &d.mag);
        let q = Self::from_mag(false, q);
        let (top, exp, sticky) = q.top64();
        let f = round_to_f64(top, exp - shift, sticky || !r.is_empty())?;
        Some(if self.neg != o.neg { -f } else { f })
    }

    /* `a / b` for operands that fit i128, correctly rounded, the quotient carried to 66 bits by long division. */
    pub fn div_i128(a: i128, b: i128) -> f64 {
        let (n, d) = (a.unsigned_abs(), b.unsigned_abs());
        if n == 0 { return if (a < 0) != (b < 0) { -0.0 } else { 0.0 }; }
        // Shift the numerator once so a single division leaves 66 quotient bits, the loop only covers what does not fit.
        let bits = |x: u128| 128 - x.leading_zeros() as i64;
        let k = (66 - bits(n) + bits(d)).clamp(0, n.leading_zeros() as i64);
        let n = n << k;
        let (mut q, mut r, mut exp) = (n / d, n % d, -k);
        while bits(q) < 66 { r <<= 1; q = q << 1 | (r >= d) as u128; if r >= d { r -= d; } exp -= 1; }
        let shift = (bits(q) as u32).saturating_sub(64);
        let sticky = r != 0 || q & ((1u128 << shift) - 1) != 0;
        let f = round_to_f64((q >> shift) as u64, exp + shift as i64, sticky).unwrap_or(f64::INFINITY);
        if (a < 0) != (b < 0) { -f } else { f }
    }

    /* The value modulo 2^61 - 1 with its sign, the hash every equal number shares. */
    pub fn hash_mod(&self) -> i64 {
        // 2^61 is 1 modulo the prime, so shifting a limb in rotates the bits that pass bit 61 back to the bottom.
        const P: u64 = (1 << 61) - 1;
        let m = self.mag.iter().rev().fold(0u64, |acc, &x| { let r = (((acc << 32) & P) | (acc >> 29)) + x as u64; if r >= P { r - P } else { r } }) as i64;
        let h = if self.neg { -m } else { m };
        if h == -1 { -2 } else { h }
    }

    /* Little-endian two's complement bytes of `len`, None when the value does not fit. */
    pub fn to_bytes(&self, len: usize, signed: bool) -> Option<Vec<u8>> {
        if self.neg && !signed { return None; }
        // A signed value needs one more bit for its sign, -2^k fitting in k + 1.
        let bits = match (signed, self.neg) {
            (false, _) => self.bit_length(),
            (true, false) => self.bit_length() + 1,
            (true, true) => self.abs().sub(&Self::from_i128(1)).bit_length() + 1,
        };
        if bits > len as u64 * 8 { return None; }
        Some(self.twos(len.div_ceil(4) + 1).iter().flat_map(|x| x.to_le_bytes()).take(len).collect())
    }

    pub fn from_bytes_le(bytes: &[u8], signed: bool) -> Self {
        let mut mag: Vec<u32> = bytes.chunks(4).map(|c| { let mut b = [0u8; 4]; b[..c.len()].copy_from_slice(c); u32::from_le_bytes(b) }).collect();
        let neg = signed && bytes.last().is_some_and(|&b| b & 0x80 != 0);
        if neg {
            // Sign-extend the last partial limb before negating back to a magnitude.
            let pad = bytes.len() % 4;
            if pad != 0 && let Some(last) = mag.last_mut() { *last |= u32::MAX << (pad * 8); }
            mag.push(u32::MAX);
        }
        Self::from_twos(mag, neg)
    }

    /* Serialized as a sign byte and little-endian limbs, the snapshot and copy codec. */
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.neg as u8);
        for x in &self.mag { out.extend_from_slice(&x.to_le_bytes()); }
    }
    pub fn decode(b: &[u8]) -> Option<Self> {
        let (&neg, rest) = b.split_first()?;
        if neg > 1 || rest.len() % 4 != 0 { return None; }
        let mag: Vec<u32> = rest.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        // A canonical encoding never carries a high zero limb.
        if mag.last() == Some(&0) || (neg == 1 && mag.is_empty()) { return None; }
        Some(Self { neg: neg == 1, mag })
    }
}

impl PartialOrd for BigInt { fn partial_cmp(&self, o: &Self) -> Option<Ordering> { Some(self.cmp(o)) } }
impl Ord for BigInt {
    fn cmp(&self, o: &Self) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => mag_cmp(&self.mag, &o.mag),
            (true, true) => mag_cmp(&o.mag, &self.mag),
        }
    }
}

/* `top * 2^exp` with `sticky` below it, rounded to 53 bits ties to even, None past the float range. */
fn round_to_f64(top: u64, exp: i64, sticky: bool) -> Option<f64> {
    if top == 0 { return Some(0.0); }
    let len = 64 - top.leading_zeros() as i64;
    // The weight of the leading bit decides the precision, 53 bits until the subnormal range.
    let lead = len - 1 + exp;
    if lead > 1023 { return None; }
    if lead < -1076 { return Some(0.0); }
    let keep = if lead < -1022 { 53 - (-1022 - lead) } else { 53 };
    if keep <= 0 {
        // Only a value above half the least subnormal rounds up to it.
        let above_half = keep == 0 && (top != 1 << (len - 1) || sticky);
        return Some(if above_half { libm::ldexp(1.0, -1074) } else { 0.0 });
    }
    let drop = len - keep;
    let (m, x) = if drop > 0 {
        let (half, rest, m) = (1u64 << (drop - 1), top & ((1u64 << drop) - 1), top >> drop);
        let up = rest > half || (rest == half && (sticky || m & 1 == 1));
        (m + up as u64, exp + drop)
    } else {
        (top << -drop, exp + drop)
    };
    let f = libm::ldexp(m as f64, x as i32);
    (!f.is_infinite()).then_some(f)
}
