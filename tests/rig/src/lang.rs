use interpreter::value::foreign::Kind;
use interpreter::lib::text::cut::Breaks;
use interpreter::lib::{Centre, Codec, Endian, Lang, Machine, Numerals, OnError, Specs};

use crate::Rig;

// the kind that boxes every int too wide for an inline value
pub static WIDE: Kind = interpreter::lib::num::int_kind::<Rig>(crate::names::KIND_NAMES[interpreter::lang::KindId::Int as usize].1);

// the kind of the slice a key of bounds makes
pub static SLICE: Kind = interpreter::lib::seq::slice_kind::<Rig>("slice");

// the kinds rig boxes its times and durations in
pub static TIME: Kind = interpreter::lib::date::time_kind::<Rig>("time");
pub static DURATION: Kind = interpreter::lib::date::duration_kind::<Rig>("duration");

impl interpreter::lib::date::Dates for Rig {
    const TIME: &'static Kind = &TIME;
    const DURATION: &'static Kind = &DURATION;
}

/* the line breaks of unicode with the three information separators */
pub struct Lines;

impl Breaks for Lines { const BREAKS: &'static [char] = &['\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}']; }

impl Lang for Rig {
    // the unit an odd pad leaves goes before the text on an odd width
    const CENTRE: Centre = Centre::ByWidth;
    // number text takes the separators and prefixes the rig's literals take
    const NUMERALS: Numerals = Numerals::PLAIN.separator(Some(b'_')).after_prefix(true).leading_zero(false).prefixed_base(true);
    const SEPARATORS: bool = true;
    // a tab size fits 32 bits and a buffer count 64 bits
    const MACHINE: Option<Machine> = Some(Machine::new(32, 64));
    const SPECS: Specs = Specs::ALL;
    const THROW_VALUE: bool = true;
    const SENTINEL: bool = true;
    // a record shows its pairs the way its literal writes them
    const PAIR: &'static str = " = ";

    fn codec(name: &str) -> Option<Codec> { crate::codecs::codec(name) }

    fn handler(name: &str) -> Option<OnError> { crate::codecs::handler(name) }

    fn codec_name(c: Codec) -> &'static str { crate::codecs::name(c) }

    fn endian(name: &str) -> Option<Endian> {
        match name { "big" => Some(Endian::Big), "little" => Some(Endian::Little), _ => None }
    }

    fn bare_key(key: &str) -> bool {
        let mut chars = key.chars();
        let start = chars.next().is_some_and(|c| c == '_' || c.is_alphabetic());
        start && chars.all(|c| c == '_' || c.is_alphanumeric()) && !crate::RIG.words.iter().any(|w| w.0 == key)
    }
}
