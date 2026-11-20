use super::TokenType;
use crate::engine::lang::Op;

/* what an operator row compiles to in its mode */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum Emit {
    Op(Op),
    // the right side runs only when the left one does not decide
    Lazy(bool),
    // a call of the named function with the operands as its arguments
    Call(&'static str),
}

/* an operator with its left and right power where a zero left marks a prefix */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Row { pub tok: TokenType, pub left: u8, pub right: u8, pub emit: Emit }

impl Row {
    pub const fn new(tok: TokenType, left: u8, right: u8, emit: Emit) -> Row { Row { tok, left, right, emit } }
}

/* the operators of one reading of an expression and the words it reads as texts */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Mode { pub rows: &'static [Row], pub words: &'static [&'static str] }

impl Mode {
    pub const fn new(rows: &'static [Row], words: &'static [&'static str]) -> Mode { Mode { rows, words } }
}

/* one part of a form after its head word */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum Slot {
    Word(&'static str),
    // a bare name or dotted path read as its text
    Name,
    // an expression read in mode `k`
    Expr(u8),
    // a word then an expression in mode `k` or none without the word
    Opt(&'static str, u8),
    // a bracketed expression handed to the call its leading word names or to the default
    Tagged(&'static [(&'static str, &'static str)], &'static str),
    // after the word a list of name and count pairs each led by its words where a `_` word puts unit words after the count
    Limits(&'static str, &'static [(&'static str, &'static str)]),
    // names split by commas the body slot after them binds as the parameters of a function
    Bind,
    // a braced expression in mode `k` made the function of the bound name
    Body(u8),
    // the byte offset of the form's head word
    At,
    // braced items in mode `k` as a list where an item led by `for` and its generators repeats its own braced items
    Items(u8),
    // generators `p in xs` split by commas in parentheses then braced items in mode `k` as a list of the items of each pass
    Each(u8),
}

/* a construct led by a word that compiles to a call of `call` with its slots */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Form { pub head: &'static str, pub slots: &'static [Slot], pub call: &'static str }

impl Form {
    pub const fn new(head: &'static str, slots: &'static [Slot], call: &'static str) -> Form { Form { head, slots, call } }
}

/* what a pattern word tests of its subject before matching its part */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum Test { Nil, NotNil, Item(u16, bool), Field(&'static str, i64) }

/* a pattern word with its test and the index of the part its own pattern matches */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Pattern { pub word: &'static str, pub test: Test, pub part: Option<u16> }

impl Pattern {
    pub const fn new(word: &'static str, test: Test, part: Option<u16>) -> Pattern { Pattern { word, test, part } }
}

/* the grammar of a table whose expressions read by modes and forms */
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct Grammar {
    // mode zero reads plain values and the others what their forms ask
    pub(crate) readings: &'static [Mode],
    pub(crate) forms: &'static [Form],
    pub(crate) patterns: &'static [Pattern],
    // words before an item that a call of the named function wraps
    pub(crate) modifiers: &'static [(&'static str, Option<&'static str>)],
    // the function a match calls when no arm takes its value
    pub(crate) unmatched: &'static str,
    // bytes a name may hold inside and the byte it may end with
    pub(crate) inner: u8,
    pub(crate) tail: u8,
    // calls whose lone comprehension argument runs item by item as a generator
    pub(crate) lazy: &'static [&'static str],
}

impl Grammar {
    setters! {
        readings: &'static [Mode],
        forms: &'static [Form],
        patterns: &'static [Pattern],
        modifiers: &'static [(&'static str, Option<&'static str>)],
        unmatched: &'static str,
        inner: u8,
        tail: u8,
        lazy: &'static [&'static str],
    }
}

impl Grammar {
    pub const PLAIN: Grammar = Grammar { readings: &[], forms: &[], patterns: &[], modifiers: &[], unmatched: "", inner: 0, tail: 0, lazy: &[] };
}
