use super::TokenType;
use crate::code::OpCode;

/* what an operator row compiles to in its mode */
#[derive(Clone, Copy)]
pub enum Emit {
    Op(OpCode),
    // the right side runs only when the left one does not decide
    Lazy(bool),
    // a call of the named function with the operands as its arguments
    Call(&'static str),
}

/* an operator with its left and right power where a zero left marks a prefix */
#[derive(Clone, Copy)]
pub struct Row { pub tok: TokenType, pub left: u8, pub right: u8, pub emit: Emit }

/* the operators of one reading of an expression and the words it reads as texts */
#[derive(Clone, Copy)]
pub struct Mode { pub rows: &'static [Row], pub words: &'static [&'static str] }

/* one part of a form after its head word */
#[derive(Clone, Copy)]
pub enum Slot {
    Word(&'static str),
    // an expression read in mode `k`
    Expr(u8),
    // a word then an expression in mode `k` or none without the word
    Opt(&'static str, u8),
    // a bracketed expression handed to the call its leading word names or to the default
    Tagged(&'static [(&'static str, &'static str)], &'static str),
    // after the word a list of name and count pairs each led by its words
    Limits(&'static str, &'static [(&'static str, &'static str)]),
}

/* a construct led by a word that compiles to a call of `call` with its slots */
#[derive(Clone, Copy)]
pub struct Form { pub head: &'static str, pub slots: &'static [Slot], pub call: &'static str }

/* what a pattern word tests of its subject before matching its part */
#[derive(Clone, Copy)]
pub enum Test { Nil, NotNil, Item(u16, bool), Field(&'static str, i64) }

/* a pattern word with its test and the index of the part its own pattern matches */
#[derive(Clone, Copy)]
pub struct Pattern { pub word: &'static str, pub test: Test, pub part: Option<u16> }

/* the grammar of a table whose expressions read by modes and forms */
#[derive(Clone, Copy)]
pub struct Grammar {
    // mode zero reads plain values and the others what their forms ask
    pub modes: &'static [Mode],
    pub forms: &'static [Form],
    pub patterns: &'static [Pattern],
    // words before an item that a call of the named function wraps
    pub modifiers: &'static [(&'static str, Option<&'static str>)],
    // the function a match calls when no arm takes its value
    pub unmatched: &'static str,
    // bytes a name may hold inside and the byte it may end with
    pub inner: u8,
    pub tail: u8,
}

impl Grammar {
    pub const NONE: Grammar = Grammar { modes: &[], forms: &[], patterns: &[], modifiers: &[], unmatched: "", inner: 0, tail: 0 };
}
