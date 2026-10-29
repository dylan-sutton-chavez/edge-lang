use alloc::string::String;

use lib::lang::{Codec, OnError, Order};

/* the codec an encoding name means once normalized */
pub fn codec(name: &str) -> Option<Codec> {
    // the spellings programs write skip the normalizing copy
    match name { "utf-8" | "utf8" => return Some(Codec::Utf8), "ascii" => return Some(Codec::Ascii), "latin-1" => return Some(Codec::Latin1), _ => {} }
    Some(match normalized(name).as_str() {
        "utf_8" | "utf8" | "u8" | "utf" | "utf8_ucs2" | "utf8_ucs4" | "cp65001" => Codec::Utf8,
        "ascii" | "646" | "ansi_x3.4_1968" | "ansi_x3_4_1968" | "ansi_x3.4_1986" | "cp367" | "csascii" | "ibm367" | "iso646_us" | "iso_646.irv_1991" | "iso_ir_6" | "us" | "us_ascii" => Codec::Ascii,
        "latin_1" | "latin1" | "latin" | "l1" | "8859" | "cp819" | "csisolatin1" | "ibm819" | "iso8859" | "iso8859_1" | "iso_8859_1" | "iso_8859_1_1987" | "iso_ir_100" => Codec::Latin1,
        "cp1252" | "windows_1252" | "1252" => Codec::Windows1252,
        "utf_16" | "utf16" | "u16" => Codec::Utf16(Order::Mark),
        "utf_16_le" | "utf_16le" | "unicodelittleunmarked" => Codec::Utf16(Order::Le),
        "utf_16_be" | "utf_16be" | "unicodebigunmarked" => Codec::Utf16(Order::Be),
        "utf_32" | "utf32" | "u32" => Codec::Utf32(Order::Mark),
        "utf_32_le" | "utf_32le" => Codec::Utf32(Order::Le),
        "utf_32_be" | "utf_32be" => Codec::Utf32(Order::Be),
        _ => return None,
    })
}

/* the name a codec failure gives each codec */
pub fn name(c: Codec) -> &'static str {
    match c {
        Codec::Utf8 => "utf-8", Codec::Ascii => "ascii", Codec::Latin1 => "latin-1", Codec::Windows1252 => "charmap",
        Codec::Utf16(Order::Be) => "utf-16-be", Codec::Utf16(_) => "utf-16-le",
        Codec::Utf32(Order::Be) => "utf-32-be", Codec::Utf32(_) => "utf-32-le",
    }
}

/* the handler a name means where surrogate handlers act strict as text holds no surrogates */
pub fn handler(name: &str) -> Option<OnError> {
    Some(match name {
        "strict" | "surrogateescape" | "surrogatepass" => OnError::Strict,
        "ignore" => OnError::Skip,
        "replace" => OnError::Replace,
        "backslashreplace" => OnError::Escape,
        "xmlcharrefreplace" => OnError::CharRef,
        "namereplace" => OnError::Named,
        _ => return None,
    })
}

/* `name` lowercased with each gap between letters digits and dots as one underscore */
fn normalized(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut gap = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            if gap && !out.is_empty() { out.push('_'); }
            out.push(c.to_ascii_lowercase());
            gap = false;
        } else {
            gap = true;
        }
    }
    out
}
