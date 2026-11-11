use alloc::string::String;

use super::bundle::Bundle;
use super::json::quote;

/* where each file of a bundle sits as JSON or why the bytes are none */
pub fn index(bytes: &[u8]) -> String {
    let mut out = String::new();
    match Bundle::index(bytes) {
        Err(e) => {
            out.push_str("{\"error\":");
            quote(&mut out, &e);
        }
        Ok(index) => {
            out.push_str("{\"entry\":");
            quote(&mut out, &index.entry);
            out.push_str(",\"files\":[");
            for (i, (path, at, size)) in index.files.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                quote(&mut out, path);
                out.push(',');
                out.push_str(itoa::Buffer::new().format(*at));
                out.push(',');
                out.push_str(itoa::Buffer::new().format(*size));
                out.push(']');
            }
            out.push(']');
        }
    }
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::bundle::Entry;
    use crate::s;

    #[test]
    fn a_bundle_index_points_at_each_file_or_says_why_not() {
        let bytes = Bundle { entry: s!("main.mini"), files: alloc::vec![Entry { path: s!("main.mini"), bytes: b"x = 1\n".to_vec() }] }.encode();
        let at = bytes.len() - 6;
        assert_eq!(index(&bytes), s!("{\"entry\":\"main.mini\",\"files\":[[\"main.mini\",", int at, ",6]]}"));
        assert_eq!(index(b"junk"), r#"{"error":"bundle truncated"}"#);
    }
}
