fn unsafe_char(c: char) -> bool {
    // a character a block scalar cannot hold as itself
    (c.is_control() && c != '\n' && c != '\t') || matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}' | '\u{fffe}' | '\u{ffff}')
}

// `s` in double quotes with every character a parser could change escaped
pub fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x100 && unsafe_char(c) => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if unsafe_char(c) => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// `text` as a literal block at `indent` or None where a block cannot hold it
fn literal(text: &str, indent: usize) -> Option<String> {
    let body = text.trim_end_matches('\n');
    if body.is_empty() || body.starts_with([' ', '\t', '\n']) || text.chars().any(unsafe_char) { return None; }
    // a kept block takes the blank line after it so such texts stay quoted
    let chomp = match text.len() - body.len() { 0 => "|-", 1 => "|", _ => return None };
    let pad = " ".repeat(indent);
    let mut out = String::from(chomp);
    for line in text.strip_suffix('\n').unwrap_or(text).split('\n') {
        out.push('\n');
        if !line.is_empty() { out.push_str(&pad); out.push_str(line); }
    }
    Some(out)
}

// `text` as a literal block at `indent` or quoted where a block reads back otherwise
pub fn text(text: &str, indent: usize) -> String {
    let back = |yml: &str| serde_yaml_ng::from_str::<std::collections::BTreeMap<String, String>>(&format!("k: {yml}\n")).ok().and_then(|mut m| m.remove("k"));
    match literal(text, indent) {
        Some(block) if back(&block).as_deref() == Some(text) => block,
        _ => quoted(text),
    }
}

// a name as a plain key where yaml reads it as that text
pub fn key(name: &str) -> String {
    let plain = name.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b == b'_') && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if plain && !matches!(name, "null" | "true" | "false" | "yes" | "no" | "on" | "off" | "y" | "n") { name.to_string() } else { quoted(name) }
}
