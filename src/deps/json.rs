use alloc::string::String;
use alloc::vec::Vec;

use crate::s;
use crate::util::hash::FxHashMap;

// values nest this deep at most far past what a manifest or lock holds
const MAX_DEPTH: usize = 32;

/* edge.json and edge.lock values with numbers and booleans refused so typos never pass */
pub enum Value {
    Str(String),
    List(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/* one whole document named `what` in its errors with anything past the value refused */
pub fn parse(bytes: &[u8], what: &str) -> Result<Value, String> {
    let src = core::str::from_utf8(bytes).map_err(|_| s!(str what, " is not valid UTF-8"))?;
    let mut r = Reader { src, pos: 0, what, depth: 0 };
    r.skip_ws();
    let value = r.value()?;
    r.skip_ws();
    if r.pos != src.len() {
        return Err(s!("unexpected text after the value in ", str what));
    }
    Ok(value)
}

/* writes `s` as a JSON string literal with its quotes */
pub fn quote(out: &mut String, s: &str) {
    out.push('"');
    crate::util::jesc::escape(out, s);
    out.push('"');
}

/* writes `items` as a JSON list of strings */
pub fn list(out: &mut String, items: &[String]) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        quote(out, item);
    }
    out.push(']');
}

struct Reader<'a> {
    src: &'a str,
    pos: usize,
    what: &'a str,
    // the lists and objects open around the cursor
    depth: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, c: u8, msg: &str) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            return Ok(());
        }
        Err(s!(str msg, " in ", str self.what))
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'[' | b'{') if self.depth >= MAX_DEPTH => Err(s!(str self.what, " nests deeper than ", int MAX_DEPTH, " levels")),
            Some(b'[') => self.nested(Self::list),
            Some(b'{') => self.nested(Self::object),
            _ => Err(s!("unsupported value in ", str self.what, " (only strings, lists of strings and string-objects)")),
        }
    }

    // a list or object read one level deeper
    fn nested(&mut self, read: fn(&mut Self) -> Result<Value, String>) -> Result<Value, String> {
        self.depth += 1;
        let value = read(self);
        self.depth -= 1;
        value
    }

    // plain runs copy as the UTF-8 they are and only escapes decode byte by byte
    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"', "expected '\"' starting a string")?;
        let mut out = String::new();
        let mut run = self.pos;
        loop {
            match self.peek() {
                None => return Err(s!("unterminated string in ", str self.what)),
                Some(b'"') => {
                    out.push_str(&self.src[run..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(&self.src[run..self.pos]);
                    self.pos += 1;
                    let esc = self.peek().ok_or_else(|| s!("dangling '\\' in ", str self.what))?;
                    out.push(match esc {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        _ => return Err(s!("unsupported escape '\\", char esc as char, "' in ", str self.what)),
                    });
                    self.pos += 1;
                    run = self.pos;
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn list(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::List(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::List(items));
                }
                _ => return Err(s!("expected ',' or ']' in a list in ", str self.what)),
            }
        }
    }

    fn object(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut fields: Vec<Option<(String, Value)>> = Vec::new();
        // where each key sits so a repeat finds its earlier field at once
        let mut at: FxHashMap<String, usize> = FxHashMap::default();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Obj(Vec::new()));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':', "expected ':' after a key")?;
            self.skip_ws();
            let value = self.value()?;
            // a repeated key keeps its last value at its last place as JSON readers do
            if let Some(earlier) = at.insert(key.clone(), fields.len()) { fields[earlier] = None; }
            fields.push(Some((key, value)));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Obj(fields.into_iter().flatten().collect()));
                }
                _ => return Err(s!("expected ',' or '}' in ", str self.what)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_survives_and_escapes_decode() {
        let v = parse("{ \"d\": \"Café \\\"fuerte\\\"\" }".as_bytes(), "edge.json").unwrap();
        assert_eq!(v.get("d").and_then(Value::as_str), Some("Café \"fuerte\""));
    }

    // nesting stops at a cap so a deep document fails before the stack does
    #[test]
    fn deep_nesting_is_refused_and_flat_depth_reads() {
        let deep = "[".repeat(100_000);
        assert_eq!(parse(deep.as_bytes(), "edge.json").err().as_deref(), Some("edge.json nests deeper than 32 levels"));
        let fits = format!("{}\"x\"{}", "[".repeat(32), "]".repeat(32));
        assert!(parse(fits.as_bytes(), "edge.json").is_ok());
    }

    // repeated keys keep the last value at its place and many keys read linearly
    #[test]
    fn repeated_keys_keep_their_last_value_and_place() {
        let v = parse(br#"{ "a": "1", "b": "2", "a": "3" }"#, "edge.json").unwrap();
        let Value::Obj(fields) = &v else { panic!("an object") };
        assert_eq!(fields.iter().map(|(k, v)| (k.as_str(), v.as_str().unwrap())).collect::<Vec<_>>(), [("b", "2"), ("a", "3")]);
        let many = format!("{{ {} }}", (0..200_000).map(|i| alloc::format!("\"k{i}\": \"v\"")).collect::<Vec<_>>().join(", "));
        let Value::Obj(fields) = parse(many.as_bytes(), "edge.json").unwrap() else { panic!("an object") };
        assert_eq!(fields.len(), 200_000);
    }

    #[test]
    fn numbers_booleans_and_trailing_text_are_refused() {
        for bad in ["{ \"a\": 1 }", "{ \"a\": true }", "{ \"a\": null }", "{} {}", "{ \"a\": \"\\u0041\" }"] {
            assert!(parse(bad.as_bytes(), "edge.json").is_err(), "{bad}");
        }
    }
}
