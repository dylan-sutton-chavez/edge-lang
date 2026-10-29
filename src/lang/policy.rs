use crate::value::Val;

/* the rules a runtime picks which the loop reads as constants of its language */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    // what an int result past the inline range becomes
    pub int_overflow: Overflow,
    // what `/` gives for two ints
    pub int_div: IntDiv,
    // how `//` rounds and whose sign `%` takes for ints and floats alike
    pub int_mod: IntMod,
    // what a float divided by zero gives since an int divided by zero always raises
    pub zero_div: ZeroDiv,
    // what an int raised to a negative int gives
    pub neg_pow: NegPow,
    // whether equal numbers of different kinds are one value and one key
    pub numeric_eq: NumEq,
    // whether a bool counts as the int zero or one in arithmetic indexes and keys
    pub bool_is_int: bool,
    // the values a test reads as false
    pub falsy: Falsy,
    // whether a container finds an item by identity before it compares
    pub identity_first: bool,
    // whether a negative index or slice bound counts from the end
    pub neg_index: bool,
    // what a loop over a text steps through
    pub str_iter: StrIter,
    // what a loop over a map steps through
    pub map_iter: MapIter,
    // whether an attribute a map lacks reads its key of that name
    pub map_fields: bool,
    // whether a name no module binds reads the runtime's builtins
    pub builtins_scope: bool,
    // whether a method takes its receiver as its first parameter
    pub receiver_slot: bool,
    // how deep calls nest before the recursion error
    pub recursion_limit: u32,
    // the decimal digits an int literal or text may hold where none sets no limit
    pub max_int_digits: Option<u32>,
}

/* what an int result past the inline range becomes */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Overflow {
    // the runtime's wide kind boxes it so ints grow without bound
    Promote,
    // the overflow error kind raises
    Error,
}

/* what `/` gives for two ints */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntDiv { ToFloat, Floor, Trunc }

/* how `//` rounds and whose sign `%` takes */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IntMod {
    // toward negative infinity so the remainder takes the divisor's sign
    Floored,
    // toward zero so the remainder takes the dividend's sign
    Truncated,
}

/* what a float divided by zero gives */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ZeroDiv {
    // the division by zero error kind raises
    Error,
    // an infinity or a NaN as the float standard gives
    Ieee,
}

/* what an int raised to a negative int gives */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NegPow { ToFloat, Error }

/* whether equal numbers of different kinds are one value */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumEq {
    // an int and a float of one value are equal with one hash
    Unified,
    // values of different kinds never compare equal
    ByKind,
}

/* what a loop over a text steps through */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StrIter { CodePoints, Bytes }

/* what a loop over a map steps through */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapIter {
    Keys,
    // each key with its value as a pair
    Pairs,
}

/* the values a test reads as false which the loop tests with one mask */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Falsy(u8);

impl Falsy {
    pub const NIL: Falsy = Falsy(1);
    pub const FALSE: Falsy = Falsy(2);
    // the int zero and either float zero
    pub const ZERO: Falsy = Falsy(4);
    pub const EMPTY_TEXT: Falsy = Falsy(8);
    pub const NAN: Falsy = Falsy(16);
    // an empty bytes buffer list tuple map set or range
    pub const EMPTY_CONTAINER: Falsy = Falsy(32);
    // an empty text and an empty container alike
    pub const EMPTY: Falsy = Falsy::EMPTY_TEXT.or(Falsy::EMPTY_CONTAINER);

    /* both sets of values */
    pub const fn or(self, other: Falsy) -> Falsy { Falsy(self.0 | other.0) }

    /* whether every value of `other` reads as false */
    pub const fn has(self, other: Falsy) -> bool { self.0 & other.0 == other.0 }

    /* the mask the loop tests */
    pub const fn bits(self) -> u8 { self.0 }

    /* the truth of a bool which every mask reads as itself */
    #[inline(always)]
    pub const fn bool_truth(self, b: bool) -> bool { b }

    /* the truth of an int under this mask */
    #[inline(always)]
    pub const fn int_truth(self, i: i64) -> bool { i != 0 || !self.has(Falsy::ZERO) }

    /* the truth of nil which every mask reads as false */
    #[inline(always)]
    pub const fn nil_truth(self) -> bool { false }

    /* the truth of a text that may be empty under this mask */
    #[inline(always)]
    pub const fn text_truth(self, empty: bool) -> bool { !empty || !self.has(Falsy::EMPTY_TEXT) }

    /* the truth of a container that may be empty under this mask */
    #[inline(always)]
    pub const fn container_truth(self, empty: bool) -> bool { !empty || !self.has(Falsy::EMPTY_CONTAINER) }

    /* the truth of a float under this mask */
    #[inline(always)]
    pub fn float_truth(self, x: f64) -> bool { !(x == 0.0 && self.has(Falsy::ZERO) || x.is_nan() && self.has(Falsy::NAN)) }
}

impl Policy {
    /* the rules the engine keeps where a runtime states none of its own */
    pub const ENGINE: Policy = Policy {
        int_overflow: Overflow::Promote,
        int_div: IntDiv::ToFloat,
        int_mod: IntMod::Truncated,
        zero_div: ZeroDiv::Error,
        neg_pow: NegPow::ToFloat,
        numeric_eq: NumEq::Unified,
        bool_is_int: true,
        // an empty text reads as false and an empty container as true
        falsy: Falsy::NIL.or(Falsy::FALSE).or(Falsy::ZERO).or(Falsy::NAN).or(Falsy::EMPTY_TEXT),
        identity_first: true,
        neg_index: true,
        str_iter: StrIter::CodePoints,
        map_iter: MapIter::Keys,
        map_fields: false,
        builtins_scope: true,
        receiver_slot: true,
        recursion_limit: 1000,
        max_int_digits: Some(4300),
    };

    /* whether the loop compiles alike under this policy and `other` */
    pub const fn loops_as(&self, other: &Policy) -> bool {
        self.int_div as u8 == other.int_div as u8 && self.int_mod as u8 == other.int_mod as u8 && self.zero_div as u8 == other.zero_div as u8
            && self.neg_pow as u8 == other.neg_pow as u8 && self.numeric_eq as u8 == other.numeric_eq as u8 && self.bool_is_int == other.bool_is_int
            && self.falsy.bits() == other.falsy.bits() && self.identity_first == other.identity_first && self.neg_index == other.neg_index
            && self.str_iter as u8 == other.str_iter as u8 && self.map_iter as u8 == other.map_iter as u8
            && self.builtins_scope == other.builtins_scope && self.receiver_slot == other.receiver_slot
    }

    /* every rule as bytes so a blob records the rules it ran under */
    pub const fn bytes(&self) -> [u8; 21] {
        let (r, d) = (self.recursion_limit.to_le_bytes(), self.digits().to_le_bytes());
        [
            self.int_overflow as u8, self.int_div as u8, self.int_mod as u8, self.zero_div as u8, self.neg_pow as u8, self.numeric_eq as u8,
            self.bool_is_int as u8, self.falsy.bits(), self.identity_first as u8, self.neg_index as u8, self.str_iter as u8, self.map_iter as u8,
            self.map_fields as u8, self.builtins_scope as u8, self.receiver_slot as u8, r[0], r[1], r[2], r[3], d[0], d[1],
        ]
    }

    /* whether this policy states every rule `other` states */
    pub const fn same(&self, other: &Policy) -> bool {
        let (a, b) = (self.bytes(), other.bytes());
        let mut i = 0;
        while i < a.len() {
            if a[i] != b[i] { return false; }
            i += 1;
        }
        self.digits() == other.digits()
    }

    /* the digit limit where none reads as no limit */
    pub const fn digits(&self) -> u32 {
        match self.max_int_digits { Some(d) => d, None => u32::MAX }
    }

    /* refuses a rule the loop does not read yet unless it is the engine's own */
    pub const fn refuse_unread(&self) {
        assert!(self.falsy.has(Falsy::NIL.or(Falsy::FALSE)), "a policy reads nil and false as false");
        assert!(matches!(self.numeric_eq, NumEq::Unified), "the loop reads no numeric_eq yet so a policy states the engine's Unified");
        assert!(self.identity_first, "the loop reads no identity_first yet so a policy states the engine's true");
        assert!(matches!(self.str_iter, StrIter::CodePoints), "the loop reads no str_iter yet so a policy states the engine's CodePoints");
        assert!(matches!(self.map_iter, MapIter::Keys), "the loop reads no map_iter yet so a policy states the engine's Keys");
        assert!(self.builtins_scope, "the loop reads no builtins_scope yet so a policy states the engine's true");
        assert!(self.receiver_slot, "the loop reads no receiver_slot yet so a policy states the engine's true");
    }
}

/* the arithmetic of each rule as one kernel the loop the folder and lib share */
impl Policy {
    /* the quotient and remainder of `a` by `b` rounded by `int_mod` or none for zero */
    #[inline(always)]
    pub const fn int_divmod(&self, a: i128, b: i128) -> Option<(i128, i128)> {
        // one division since a wide one is a call on 32-bit targets
        let Some(q) = a.checked_div(b) else { return None };
        let r = a - q * b;
        Some(match self.int_mod {
            IntMod::Floored if r != 0 && (r < 0) != (b < 0) => (q - 1, r + b),
            _ => (q, r),
        })
    }

    /* the quotient `a // b` of two inline ints and none for a zero divisor */
    #[inline(always)]
    pub const fn int_floordiv(&self, a: i64, b: i64) -> Option<i64> {
        if b == 0 { return None; }
        let (q, r) = (a / b, a % b);
        Some(if matches!(self.int_mod, IntMod::Floored) && r != 0 && (r < 0) != (b < 0) { q - 1 } else { q })
    }

    /* the remainder `a % b` of two inline ints and none for a zero divisor */
    #[inline(always)]
    pub const fn int_rem(&self, a: i64, b: i64) -> Option<i64> {
        if b == 0 { return None; }
        let r = a % b;
        Some(if matches!(self.int_mod, IntMod::Floored) && r != 0 && (r < 0) != (b < 0) { r + b } else { r })
    }

    /* `a / b` of two inline ints by `int_div` or none for a zero divisor */
    #[inline(always)]
    pub fn int_quotient(&self, a: i64, b: i64) -> Option<Val> {
        if b == 0 { return None; }
        match self.int_div {
            IntDiv::ToFloat => Some(Val::float(a as f64 / b as f64)),
            // a quotient past the inline range leaves for the slow path which boxes it
            IntDiv::Floor => Val::int_checked(self.int_floordiv_floored(a, b)),
            IntDiv::Trunc => Val::int_checked(a / b),
        }
    }

    /* the floored quotient whatever `int_mod` says as `int_div` Floor asks */
    #[inline(always)]
    const fn int_floordiv_floored(&self, a: i64, b: i64) -> i64 {
        let (q, r) = (a / b, a % b);
        if r != 0 && (r < 0) != (b < 0) { q - 1 } else { q }
    }

    /* `a / b` of two floats or none where `zero_div` raises for a zero divisor */
    #[inline(always)]
    pub fn float_quotient(&self, a: f64, b: f64) -> Option<Val> {
        if b == 0.0 && matches!(self.zero_div, ZeroDiv::Error) { return None; }
        Some(Val::float(a / b))
    }

    /* the quotient and remainder of floats as `int_mod` rounds them and none where `zero_div` raises */
    pub fn float_divmod(&self, a: f64, b: f64) -> Option<(f64, f64)> {
        if b == 0.0 {
            return match self.zero_div { ZeroDiv::Error => None, ZeroDiv::Ieee => Some((libm::floor(a / b), f64::NAN)) };
        }
        Some(match self.int_mod {
            IntMod::Floored => crate::value::float_divmod(a, b),
            IntMod::Truncated => {
                let r = libm::fmod(a, b);
                let d = (a - r) / b;
                // a zero quotient keeps the sign the two operands give it
                if d == 0.0 { return Some((f64::from_bits((a.to_bits() ^ b.to_bits()) & (1 << 63)), r)); }
                // d is whole but for rounding so the nearest whole is the quotient
                let f = libm::floor(d);
                (if d - f > 0.5 { f + 1.0 } else { f }, r)
            }
        })
    }

    /* whether an int to a negative int power gives a float rather than raising */
    #[inline(always)]
    pub const fn negative_powers(&self) -> bool { matches!(self.neg_pow, NegPow::ToFloat) }

    /* whether `v` reads as false where its tag decides or none for a heap value */
    #[inline(always)]
    pub fn tag_falsy(&self, v: Val) -> Option<bool> {
        let f = self.falsy;
        if v.is_bool() { return Some(!f.bool_truth(v.as_bool())); }
        if v.is_int() { return Some(!f.int_truth(v.as_int())); }
        if v.is_none() { return Some(!f.nil_truth()); }
        if v.is_float() { return Some(!f.float_truth(v.as_float())); }
        None
    }
}
