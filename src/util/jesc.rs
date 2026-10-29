pub fn escape(out: &mut alloc::string::String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                let b = c as u8;
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
                out.push(char::from_digit((b & 0xF) as u32, 16).unwrap());
            }
            c => out.push(c),
        }
    }
}
