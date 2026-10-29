use crate::markdown::Block;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Output,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mini,
    MiniSkip,
    Actor,
    Untrusted,
}

pub struct Cell {
    pub kind: Kind,
    pub verdict: Verdict,
    pub body: String,
    pub expect: String,
    pub line: usize,
}

fn parse_text_meta(meta: &str, line: usize) -> Result<Verdict, String> {
    let mut verdict = Verdict::Output;
    for tok in meta.split_whitespace() {
        match tok {
            "Output" => verdict = Verdict::Output,
            "Error" => verdict = Verdict::Error,
            other => return Err(format!("line {line}: unknown text meta '{other}'")),
        }
    }
    Ok(verdict)
}

pub fn collect(blocks: &[Block]) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        let b = &blocks[i];
        let kind = match (b.lang.as_str(), b.meta.as_str()) {
            ("mini", "") => Kind::Mini,
            ("mini", "skip") => Kind::MiniSkip,
            ("yml", "actor") => Kind::Actor,
            ("yml", "untrusted") => Kind::Untrusted,
            ("mini" | "yml", meta) => {
                return Err(format!("line {}: unknown {} meta '{meta}'", b.line, b.lang));
            }
            _ => {
                i += 1;
                continue;
            }
        };

        if kind == Kind::MiniSkip {
            if !b.body.contains("// skip") {
                return Err(format!(
                    "line {}: skip cell needs a '// skip' comment at the nondeterministic construct",
                    b.line
                ));
            }
            if blocks.get(i + 1).is_some_and(|n| n.lang == "text") {
                return Err(format!(
                    "line {}: skip cell never pairs with a text block",
                    b.line
                ));
            }
            i += 1;
            continue;
        }

        let next = blocks.get(i + 1);
        let Some(text) = next.filter(|n| n.lang == "text") else {
            if kind == Kind::Mini {
                // A bare mini block is illustrative, only a text pair makes it runnable.
                i += 1;
                continue;
            }
            return Err(format!(
                "line {}: runnable cell is not followed by a text block",
                b.line
            ));
        };
        let verdict = parse_text_meta(&text.meta, text.line)?;

        if kind == Kind::Actor && b.body.contains("eval: true") {
            return Err(format!(
                "line {}: yml actor group uses eval, label it yml untrusted",
                b.line
            ));
        }
        if kind == Kind::Untrusted && !b.body.contains("eval: true") {
            return Err(format!(
                "line {}: yml untrusted needs every group with eval: true",
                b.line
            ));
        }

        cells.push(Cell {
            kind,
            verdict,
            body: b.body.clone(),
            expect: text.body.clone(),
            line: b.line,
        });
        i += 2;
    }
    Ok(cells)
}
