use edge::lang::Cx;
use edge::value::VmErr;
use edge::value::foreign::Kind;
use lib::fail::{At, Class, Fail};
use lib::lang::{Centre, Codec, Endian, Machine, Numerals, OnError, Specs, Tree};
use lib::num::hash::Scheme;
use lib::text::cut::Breaks;
use lib::Lang;

use crate::Mini;

// the kind that boxes every int too wide for an inline value
pub static WIDE: Kind = lib::num::big::kind::<Mini>(crate::names::KIND_NAMES[edge::lang::KindId::Int as usize]);

// the kind of the one value the ellipsis names
pub static DOTS: Kind = lib::types::dots::<Mini>();

// the kind of the slice a key of bounds makes
pub static SLICE: Kind = lib::seq::slice::<Mini>("slice");

// the kinds mini boxes its times and durations in
pub static TIME: Kind = lib::date::time_kind::<Mini>("time");
pub static DURATION: Kind = lib::date::duration_kind::<Mini>("duration");

impl lib::date::Dates for Mini {
    const TIME: &'static Kind = &TIME;
    const DURATION: &'static Kind = &DURATION;
}

/* numbers modulo the mersenne prime of 61 bits and tuples mixed as xxhash mixes lanes */
pub struct Modular;

impl Scheme for Modular {
    const BITS: u32 = 61;
    const NIL: i64 = 0xFCA8_6420;
    const INF: i64 = 314_159;
    const RESERVED: Option<(i64, i64)> = Some((-1, -2));
    const RESERVED_TUPLE: i64 = 1_546_275_796;
    const SEED: u64 = 2_870_177_450_012_600_261;
    const LANE: (u64, u64) = (14_029_467_366_897_019_727, 11_400_714_785_074_694_791);
    const ROTATE: u32 = 31;
    // the length mixes with a constant that keeps the empty tuple's hash of the corpus
    const LENGTH: u64 = Self::SEED ^ 3_527_539;
}

/* the line breaks of unicode with the three information separators the corpus breaks at */
pub struct Lines;

impl Breaks for Lines { const BREAKS: &'static [char] = &['\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}']; }

impl Lang for Mini {
    const TREE: &'static Tree = &crate::types::TREE;
    // the unit an odd pad leaves goes before the text on an odd width
    const CENTRE: Centre = Centre::ByWidth;
    // number text takes the separators and prefixes mini's literals take
    const NUMERALS: Numerals = Numerals { separator: Some(b'_'), doubled: false, after_prefix: true, leading_zero: false, prefixed_base: true };
    const SEPARATORS: bool = true;
    // a tab size fits 32 bits and a buffer count 64 bits
    const MACHINE: Option<Machine> = Some(Machine { int: 32, index: 64 });
    const SPECS: Specs = Specs::ALL;
    const THROW_VALUE: bool = true;
    const SENTINEL: bool = true;

    // the classes past the engine's error kinds with the rest by kind
    fn class(c: Class) -> &'static str {
        match c {
            Class::Lookup => "LookupError", Class::Decode => "UnicodeDecodeError", Class::Encode => "UnicodeEncodeError", Class::End => "EOFError",
            c => lib::lang::class::<Mini>(c),
        }
    }

    fn words(cx: &mut Cx<'_, '_>, at: At, f: Fail) -> VmErr { crate::words::words(cx, at, f) }

    fn words_bare(f: Fail) -> VmErr { crate::words::words_bare(f) }

    fn codec(name: &str) -> Option<Codec> { crate::codecs::codec(name) }

    fn handler(name: &str) -> Option<OnError> { crate::codecs::handler(name) }

    fn codec_name(c: Codec) -> &'static str { crate::codecs::name(c) }

    fn endian(name: &str) -> Option<Endian> {
        match name { "big" => Some(Endian::Big), "little" => Some(Endian::Little), _ => None }
    }
}
