use edge::value::foreign::Kind;
use lib::lang::{Centre, Codec, Endian, Machine, Numerals, OnError, Specs, Tree};
use lib::text::cut::Breaks;
use lib::Lang;

use crate::Mini;

// the kind that boxes every int too wide for an inline value
pub static WIDE: Kind = lib::num::big::kind::<Mini>(crate::names::KIND_NAMES[edge::lang::KindId::Int as usize]);

// the kind of the slice a key of bounds makes
pub static SLICE: Kind = lib::seq::slice::<Mini>("slice");

// the kinds mini boxes its times and durations in
pub static TIME: Kind = lib::date::time_kind::<Mini>("time");
pub static DURATION: Kind = lib::date::duration_kind::<Mini>("duration");

impl lib::date::Dates for Mini {
    const TIME: &'static Kind = &TIME;
    const DURATION: &'static Kind = &DURATION;
}

/* the line breaks of unicode with the three information separators */
pub struct Lines;

impl Breaks for Lines { const BREAKS: &'static [char] = &['\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}']; }

// a record shows its pairs the way its literal writes them
impl lib::fmt::render::Style for Mini {
    const PAIR: &'static str = " = ";

    fn bare_key(key: &str) -> bool {
        let mut chars = key.chars();
        let start = chars.next().is_some_and(|c| c == '_' || c.is_alphabetic());
        start && chars.all(|c| c == '_' || c.is_alphanumeric()) && !crate::MINI.words.iter().any(|w| w.0 == key)
    }
}

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

    fn codec(name: &str) -> Option<Codec> { crate::codecs::codec(name) }

    fn handler(name: &str) -> Option<OnError> { crate::codecs::handler(name) }

    fn codec_name(c: Codec) -> &'static str { crate::codecs::name(c) }

    fn endian(name: &str) -> Option<Endian> {
        match name { "big" => Some(Endian::Big), "little" => Some(Endian::Little), _ => None }
    }
}
