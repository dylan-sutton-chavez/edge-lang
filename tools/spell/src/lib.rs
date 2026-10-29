#![forbid(unsafe_code)]

pub mod braces;
pub mod check;
pub mod corpus;
pub mod indent;
pub mod literals;
pub mod names;
pub mod rekey;
pub mod split;
pub mod translate;
pub mod words;
pub mod yml;

// the loader and key the bench runs so both hash a case alike
#[path = "../../../tests/cases.rs"]
pub mod cases;

pub use mini::{Mini, MINI};
pub use braces::{Braces, BRACES};
pub use indent::{Indent, INDENT};
pub use words::{Words, WORDS};
pub use names::Renames;
pub use translate::{translate, translate_renamed, Untranslatable};

use edge::code::SSAChunk;
use edge::lexer::{lex_as, Token};

// a token table a command names on its command line
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spell {
    Indent,
    Braces,
    Mini,
}

impl Spell {
    pub fn named(name: &str) -> Option<Spell> {
        match name {
            "indent" => Some(Spell::Indent),
            "braces" => Some(Spell::Braces),
            "mini" => Some(Spell::Mini),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Spell::Indent => "indent",
            Spell::Braces => "braces",
            Spell::Mini => "mini",
        }
    }

    // the table a program written for this one most likely came from
    pub fn other(self) -> Spell {
        match self {
            Spell::Indent | Spell::Braces => Spell::Mini,
            Spell::Mini => Spell::Braces,
        }
    }

    // the table the spelling reads
    pub fn syntax(self) -> edge::lexer::Syntax {
        match self {
            Spell::Indent => INDENT,
            Spell::Braces => BRACES,
            Spell::Mini => MINI,
        }
    }

    // the renames from this spelling to `to` and whether their table reads forward
    pub fn renames(self, to: Spell) -> (Renames, bool) {
        match (self, to) {
            (Spell::Indent, Spell::Mini) => (names::INDENT_MINI, true),
            (Spell::Mini, Spell::Indent) => (names::INDENT_MINI, false),
            _ => (Renames::NONE, true),
        }
    }

    // the tokens of `src` and how many lex errors it holds
    pub fn lex(self, src: &str) -> (Vec<Token>, usize) {
        let (tokens, errors) = match self {
            Spell::Indent => lex_as::<Indent>(src),
            Spell::Braces => lex_as::<Braces>(src),
            Spell::Mini => lex_as::<Mini>(src),
        };
        (tokens, errors.len())
    }

    pub fn compile(self, src: &str) -> (SSAChunk, Vec<String>) {
        match self {
            Spell::Indent => check::compiled::<Indent>(src),
            Spell::Braces => check::compiled::<Braces>(src),
            Spell::Mini => check::compiled::<Mini>(src),
        }
    }
}

// `src` written in `from` printed in `to`
pub fn respell(from: Spell, to: Spell, src: &str) -> Result<String, Untranslatable> {
    let (renames, forward) = from.renames(to);
    macro_rules! pairs {
        ($($a:ident => $b:ident),*) => {
            match (from, to) {
                $((Spell::$a, Spell::$b) => translate_renamed::<$a, $b>(src, renames, forward),)*
                _ => Ok(src.to_string()),
            }
        };
    }
    pairs!(Indent => Braces, Braces => Indent, Indent => Mini, Mini => Indent, Braces => Mini, Mini => Braces)
}

// the line of byte `at` in `src` counted from 1
pub fn line_of(src: &str, at: usize) -> usize {
    src.as_bytes()[..at.min(src.len())].iter().filter(|&&b| b == b'\n').count() + 1
}
