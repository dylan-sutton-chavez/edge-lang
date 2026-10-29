#[derive(Clone, Copy, Debug)]
// the names and texts a respell renames beside the words its token tables spell
pub struct Renames {
    // builtin and class names a program spells as plain names first spelling then second
    pub names: &'static [(&'static str, &'static str)],
    // whole words the expected output or error of a case carries first spelling then second
    pub texts: &'static [(&'static str, &'static str)],
}

// indent and mini name every builtin and error alike so the flip renames none
pub const INDENT_MINI: Renames = Renames::NONE;

impl Renames {
    pub const NONE: Renames = Renames { names: &[], texts: &[] };

    pub fn is_empty(&self) -> bool { self.names.is_empty() && self.texts.is_empty() }

    // the name `name` takes going to the second spelling or back when not `forward`
    pub fn name(&self, name: &str, forward: bool) -> Option<&'static str> { pick(self.names, name, forward) }

    // a chunk name renamed whole or as the base of its versioned form
    pub fn chunk_name(&self, name: &str, forward: bool) -> String {
        if let Some(new) = self.name(name, forward) { return new.to_string(); }
        match name.rsplit_once('_') {
            Some((base, ver)) if !ver.is_empty() && ver.bytes().all(|b| b.is_ascii_digit()) => match self.name(base, forward) {
                Some(new) => format!("{new}_{ver}"),
                None => name.to_string(),
            },
            _ => name.to_string(),
        }
    }

    // `text` with each whole word the text table holds renamed
    pub fn text(&self, text: &str, forward: bool) -> String {
        if self.texts.is_empty() { return text.to_string(); }
        let word = |c: char| c.is_alphanumeric() || c == '_';
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find(word) {
            out.push_str(&rest[..at]);
            let end = rest[at..].find(|c: char| !word(c)).map_or(rest.len(), |n| at + n);
            let w = &rest[at..end];
            out.push_str(pick(self.texts, w, forward).unwrap_or(w));
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }
}

fn pick(table: &'static [(&'static str, &'static str)], w: &str, forward: bool) -> Option<&'static str> {
    table.iter().find(|(a, b)| if forward { *a == w } else { *b == w }).map(|&(a, b)| if forward { b } else { a })
}
