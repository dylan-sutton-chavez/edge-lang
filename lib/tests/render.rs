use lib::fmt::render::{ascii_escape, format_f64, push_bytes, push_repr_str};
use rig::Rig;

fn quoted(s: &str) -> String { let mut out = String::new(); push_repr_str::<Rig>(&mut out, s); out }

fn bytes(b: &[u8], array: bool) -> String { let mut out = String::new(); push_bytes::<Rig>(&mut out, b, array); out }

#[test]
fn floats_read_back_in_their_shortest_digits() {
    let cases: &[(f64, &str)] = &[
        (0.0, "0.0"), (-0.0, "-0.0"), (f64::NAN, "NaN"), (-f64::NAN, "NaN"), (f64::INFINITY, "inf"), (f64::NEG_INFINITY, "-inf"),
        (1.0, "1.0"), (-2.5, "-2.5"), (0.1 + 0.2, "0.30000000000000004"), (100.0, "100.0"), (123.456, "123.456"),
        (1e15, "1000000000000000.0"), (1234567890123456.0, "1234567890123456.0"), (1e16, "1e16"), (12345678901234567.0, "1.2345678901234568e16"),
        (0.0001, "0.0001"), (0.00001, "0.00001"), (1.5e-7, "1.5e-7"), (1e22, "1e22"), (1e100, "1e100"), (-1e-100, "-1e-100"),
        (5e-324, "5e-324"), (f64::MAX, "1.7976931348623157e308"), (f64::MIN_POSITIVE, "2.2250738585072014e-308"), (1.0 / 3.0, "0.3333333333333333"),
    ];
    for &(f, want) in cases { assert_eq!(format_f64::<Rig>(f), want, "{f:e}"); }
}

#[test]
fn a_str_repr_picks_its_quote_and_escapes_what_does_not_print() {
    assert_eq!(quoted("abc"), "\"abc\"");
    assert_eq!(quoted("it's"), "\"it's\"");
    assert_eq!(quoted("'\""), "\"'\\\"\"");
    assert_eq!(quoted("a\\b\n\r\t"), "\"a\\\\b\\n\\r\\t\"");
    assert_eq!(quoted("\x00\x7f\u{85}é"), "\"\\u{0}\\u{7f}\\u{85}é\"");
    assert_eq!(quoted("\u{2028}\u{e000}\u{10ffff}日本"), "\"\\u{2028}\\u{e000}\\u{10ffff}日本\"");
}

#[test]
fn bytes_quote_as_their_kind_shows_them() {
    assert_eq!(bytes(b"abc", false), "b\"abc\"");
    assert_eq!(bytes(b"it's", false), "b\"it's\"");
    assert_eq!(bytes(b"it's", true), "b\"it's\"");
    assert_eq!(bytes(b"'\"", false), "b\"'\\\"\"");
    assert_eq!(bytes(b"\x00\n\\\xff", false), "b\"\\x00\\n\\\\\\xff\"");
}

#[test]
fn ascii_escapes_past_the_ascii_range() {
    assert_eq!(ascii_escape::<Rig>("\"é日😀\""), "\"\\u{e9}\\u{65e5}\\u{1f600}\"");
    assert_eq!(ascii_escape::<Rig>("plain"), "plain");
}
