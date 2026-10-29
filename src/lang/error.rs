use alloc::{string::String, vec::Vec};

use crate::syntax::lexer::TokenType;
use crate::value::foreign::{BinOp, UnOp};
use crate::value::{SchedulerStatus, Val, VmErr};
use super::{Cmp, Cx, ErrKind, KindId, Mode, NativeId, Op};

/* an error the engine raises as facts which the runtime words */
#[derive(Clone, Debug)]
pub enum EngineError {
    // an operator refused the kinds of its operands
    Unsupported { op: Op, a: Val, b: Option<Val> },
    // a value that cannot do what a site asked where none names no value
    Cannot { act: Act, recv: Option<Val> },
    // an index of the wrong kind
    BadIndex { recv: Val, index: Val },
    // a slice bound or with `step` its step of the wrong kind
    BadSlice { index: Val, step: bool },
    // an index past the end where `store` marks an assignment or a deletion
    OutOfRange { recv: Val, store: bool },
    // a wide int where an index sized one must fit raising under kind `kind`
    TooWide(ErrKind),
    // a map lacks the key
    MissingKey(Val),
    // a container of kind `kind` changed its size while a loop walked it
    Changed(KindId),
    // a slice or range step of zero
    ZeroStep { range: bool },
    // a call that misses what the callee takes
    Arity { callee: Callee, fit: Fit },
    // a call its callee refuses before it binds anything
    Call(CallFault),
    // a name no scope binds
    Undefined(String),
    // a local or a free cell read before anything binds it
    Unbound { name: Option<String>, free: bool },
    // an attribute the receiver lacks where `store` marks a store
    NoAttribute { recv: Val, name: String, store: bool },
    // an attribute no store may change for the reason `lock` gives
    ReadOnly { recv: Val, name: String, lock: Lock },
    ZeroDiv,
    // zero raised to a negative power
    ZeroPow,
    // an int raised to a negative int where the policy keeps powers of ints ints
    NegativePower,
    Overflow,
    // an int result past the inline range where the policy grows no int past it
    IntRange,
    // a float result past the float range
    FloatRange,
    NegativeShift,
    // an int that no byte holds
    ByteRange,
    // unpacking that met another count where `more` marks too many
    Unpack { want: u32, got: Option<u32>, more: bool, rest: bool },
    // an extended slice assigned a sequence of another size
    SliceSize { size: u32, span: u32 },
    // a strict walk whose argument `arg` ended after or before those ahead of it
    Uneven { arg: u32, longer: bool },
    // the protocol of generators and coroutines
    Generator(Gen),
    // a class pattern that does not fit its class
    Pattern(Pattern),
    // the end of an iteration with its value which ends the loop stepping it
    Stop(Val),
    // the signal a close throws into a generator
    Close,
    // a program asking its host to end with this value
    Exit(Val),
    // a failed assertion with its message or undef
    Assert(Val),
    Recursion,
    Memory,
    // words a native or a host already wrote under an error kind
    Of(ErrKind, String),
    // a value the program raised
    Raised(Val),
    // an engine invariant no program reaches
    Internal(&'static str),
    // the signals from here on end a run and no program catches them
    Budget,
    HostYield(SchedulerStatus),
    HostCallDeferred,
    Redo,
}

/* what a value was asked to do and cannot */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    Iterate,
    // step as an iterator steps
    Next,
    Call,
    Subscript,
    AssignItem,
    DeleteItem,
    AssignSlice,
    Slice,
    // a slice taken of a value that is no sequence
    Sequence,
    // report its size
    Size,
    // stand as a key in use `at` where an item of kind `inner` is unhashable
    Hash { at: Use, inner: KindId },
    Await,
    // read as a map where `spread` marks a double star argument
    Map { spread: bool },
    // hold items for a membership test
    Contain,
    // stand for an int as an index does
    Index,
    // be raised where `throw` marks a throw into a generator
    Raise { throw: bool },
    // read as bytes
    Bytes,
    // stand as a format spec
    Spec,
    // name a keyword argument
    Keyword,
    // lose an attribute when it is no object
    DeleteAttr,
    // lose the arguments an error holds
    DropArgs,
    // be resumed as a body of this kind
    Resume(Body),
    // stand as a complex number which the engine has none of
    Complex,
}

/* why a store cannot change an attribute */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lock {
    // the receiver is a builtin type
    Type,
    // the attribute is a native method of the receiver's kind
    Native,
    // the receiver keeps only its fixed fields
    Fixed,
    // the attribute exists and takes no store
    Plain,
}

/* where an unhashable value was put */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Use { Plain, Key, Element }

/* the body a generator protocol error names */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Body { Generator, Coroutine }

/* what a call that misses its signature called */
#[derive(Clone, Debug)]
pub enum Callee {
    // a function of the program by its qualified name with its normal and default parameters
    Function { name: String, normal: u32, defaults: u32 },
    // a native and the kind whose method it is
    Native { id: NativeId, owner: Option<KindId> },
}

/* how a call misses a signature */
#[derive(Clone, Debug)]
pub enum Fit {
    // the positionals given past what the callee takes
    Count(u32),
    // a keyword no parameter takes with the parameter it may mean
    Unexpected { key: String, near: Option<String> },
    // a parameter given twice
    Twice(String),
    // required parameters left unfilled from place `at` where `keyword` marks keyword only ones
    Missing { at: u32, names: Vec<String>, keyword: bool },
    // a positional only parameter given by keyword
    ByKeyword(String),
}

/* a call its callee refuses before binding */
#[derive(Clone, Debug)]
pub enum CallFault {
    // a callee taking no arguments where `keywords` marks keywords and `called` the parentheses
    NoArgs { name: String, keywords: bool, called: bool },
    // the default init given arguments past its object
    InitArgs,
    // an exception class given keywords
    ExcKeywords,
    // a class with no init given arguments
    CtorArgs,
    // a method called off its kind's type with no receiver
    NoReceiver { kind: KindId, name: &'static str },
    // a method called off its kind's type on a value of another kind
    WrongReceiver { kind: KindId, name: &'static str, got: Val },
    // a call past the arguments one call passes
    TooMany { name: String, got: u32 },
    // a builtin type whose instances no call makes
    Instantiate(String),
    // a base no class may derive from where none marks no class at all
    Base(Option<String>),
}

/* the generator and coroutine protocol */
#[derive(Clone, Debug)]
pub enum Gen {
    // resumed while it runs
    Running(Body),
    // a value other than nil sent before it started
    FreshSend(Body),
    // a coroutine awaited after it finished
    Reused,
    // a body that yielded when a close asked it to end
    IgnoredClose(Body),
    // a host wait inside a body that cannot park where `inside` names the body
    Wait { inside: &'static str },
    // a class body that tried to suspend
    ClassSuspend,
    // a value sent into a builtin iterator a body waits inside which takes none
    Sent,
}

/* a class pattern that does not fit its class */
#[derive(Clone, Debug)]
pub enum Pattern {
    // more than one positional pattern for a builtin subject
    OnePositional(Val),
    // positional patterns for a class with no matched fields
    NoFields,
    // more positional patterns than matched fields
    TooMany,
}

/* the errors raised most built in calls of their own so sites keep no room */
impl EngineError {
    #[cold]
    #[inline(never)]
    pub fn cannot(act: Act, recv: Option<Val>) -> VmErr { EngineError::Cannot { act, recv }.into() }

    #[cold]
    #[inline(never)]
    pub fn bad_index(recv: Val, index: Val) -> VmErr { EngineError::BadIndex { recv, index }.into() }

    #[cold]
    #[inline(never)]
    pub fn out_of_range(recv: Val, store: bool) -> VmErr { EngineError::OutOfRange { recv, store }.into() }

    #[cold]
    #[inline(never)]
    pub fn unsupported(op: Op, a: Val, b: Option<Val>) -> VmErr { EngineError::Unsupported { op, a, b }.into() }

    #[cold]
    #[inline(never)]
    pub fn bad_slice(index: Val, step: bool) -> VmErr { EngineError::BadSlice { index, step }.into() }

    #[cold]
    #[inline(never)]
    pub fn zero_step(range: bool) -> VmErr { EngineError::ZeroStep { range }.into() }
}

impl EngineError {
    /* the kind whose class the runtime names and none for a raised value or signal */
    #[inline(never)]
    pub fn kind(&self) -> Option<ErrKind> {
        use EngineError as E;
        Some(match self {
            E::Unsupported { .. } | E::BadIndex { .. } | E::BadSlice { .. } | E::Arity { .. } | E::Call(_) | E::Pattern(_) => ErrKind::Type,
            E::Cannot { act: Act::Complex, .. } => ErrKind::Value,
            E::Cannot { .. } => ErrKind::Type,
            E::OutOfRange { .. } => ErrKind::Index,
            &E::TooWide(k) => k,
            E::MissingKey(_) => ErrKind::Key,
            E::Changed(_) => ErrKind::Runtime,
            E::ZeroStep { .. } | E::NegativeShift | E::ByteRange | E::Unpack { .. } | E::SliceSize { .. } | E::Uneven { .. } | E::NegativePower => ErrKind::Value,
            E::Undefined(_) | E::Unbound { free: true, .. } => ErrKind::Name,
            E::Unbound { .. } => ErrKind::Unbound,
            E::ReadOnly { lock: Lock::Type, .. } => ErrKind::Type,
            E::NoAttribute { .. } | E::ReadOnly { .. } => ErrKind::Attribute,
            E::ZeroDiv | E::ZeroPow => ErrKind::ZeroDiv,
            E::Overflow | E::IntRange | E::FloatRange => ErrKind::Overflow,
            E::Generator(Gen::Running(_)) => ErrKind::Value,
            E::Generator(Gen::Sent) => ErrKind::Attribute,
            E::Generator(Gen::FreshSend(_)) => ErrKind::Type,
            E::Generator(_) => ErrKind::Runtime,
            E::Stop(_) => ErrKind::Stop,
            E::Close => ErrKind::Close,
            E::Exit(_) => ErrKind::Exit,
            E::Assert(_) => ErrKind::Assert,
            E::Recursion => ErrKind::Recursion,
            E::Memory => ErrKind::Memory,
            &E::Of(k, _) => k,
            E::Internal(_) => ErrKind::Runtime,
            E::Raised(_) | E::Budget | E::HostYield(_) | E::HostCallDeferred | E::Redo => return None,
        })
    }
}

/* the token a table spells binary operator `b` with or its augmented form with `place` */
const fn bin_token(b: BinOp, place: bool) -> TokenType {
    use TokenType as T;
    match (b, place) {
        (BinOp::Add, false) => T::Plus, (BinOp::Add, true) => T::PlusEqual,
        (BinOp::Sub, false) => T::Minus, (BinOp::Sub, true) => T::MinusEqual,
        (BinOp::Mul, false) => T::Star, (BinOp::Mul, true) => T::StarEqual,
        (BinOp::TrueDiv, false) => T::Slash, (BinOp::TrueDiv, true) => T::SlashEqual,
        (BinOp::FloorDiv, false) => T::FloorDiv, (BinOp::FloorDiv, true) => T::FloorDivEqual,
        (BinOp::Mod, false) => T::Percent, (BinOp::Mod, true) => T::PercentEqual,
        (BinOp::Pow, false) => T::Pow, (BinOp::Pow, true) => T::PowEqual,
        (BinOp::And, false) => T::BitAnd, (BinOp::And, true) => T::BitAndEqual,
        (BinOp::Or, false) => T::BitOr, (BinOp::Or, true) => T::BitOrEqual,
        (BinOp::Xor, false) => T::BitXor, (BinOp::Xor, true) => T::BitXorEqual,
        (BinOp::Shl, false) => T::LeftShift, (BinOp::Shl, true) => T::LeftShiftEqual,
        (BinOp::Shr, false) => T::RightShift, (BinOp::Shr, true) => T::RightShiftEqual,
        (BinOp::MatMul, false) => T::At, (BinOp::MatMul, true) => T::AtEqual,
    }
}

/* the token a table spells operator `op` with */
pub const fn token_of(op: Op) -> Option<TokenType> {
    use TokenType as T;
    Some(match op {
        Op::Bin(b) => bin_token(b, false),
        Op::InPlace(b) => bin_token(b, true),
        Op::Cmp(c) => match c { Cmp::Eq => T::EqualEqual, Cmp::Ne => T::NotEqual, Cmp::Lt => T::Less, Cmp::Le => T::LessEqual, Cmp::Gt => T::Greater, Cmp::Ge => T::GreaterEqual },
        Op::Unary(UnOp::Neg) => T::Minus,
        Op::Unary(UnOp::Pos) => T::Plus,
        Op::Unary(UnOp::Invert) => T::Tilde,
        Op::In => T::In,
        _ => return None,
    })
}

/* how many operators a runtime keeps the spelling of */
pub const SPELLED: usize = 36;

// the binary operators in their order as the spellings keep them
const BINS: [BinOp; 13] = [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::FloorDiv, BinOp::Mod, BinOp::TrueDiv, BinOp::Pow, BinOp::And, BinOp::Or, BinOp::Xor, BinOp::Shl, BinOp::Shr, BinOp::MatMul];

/* where the spellings keep `op` and none for an operator no token spells */
pub const fn slot(op: Op) -> Option<usize> {
    Some(match op {
        Op::Bin(b) => b as usize,
        Op::InPlace(b) => BINS.len() + b as usize,
        Op::Cmp(c) => 2 * BINS.len() + c as usize,
        Op::Unary(UnOp::Neg) => 32,
        Op::Unary(UnOp::Pos) => 33,
        Op::Unary(UnOp::Invert) => 34,
        Op::In => 35,
        _ => return None,
    })
}

/* each operator as `syntax` spells it at its slot or empty where it lacks one */
pub const fn spellings(syntax: &crate::syntax::lexer::Syntax) -> [&'static str; SPELLED] {
    let mut out = [""; SPELLED];
    let cmps = [Cmp::Eq, Cmp::Ne, Cmp::Lt, Cmp::Le, Cmp::Gt, Cmp::Ge];
    let mut k = 0;
    while k < BINS.len() {
        out[k] = spelling_in(syntax, bin_token(BINS[k], false));
        out[BINS.len() + k] = spelling_in(syntax, bin_token(BINS[k], true));
        k += 1;
    }
    k = 0;
    while k < cmps.len() {
        if let Some(t) = token_of(Op::Cmp(cmps[k])) { out[2 * BINS.len() + k] = spelling_in(syntax, t); }
        k += 1;
    }
    (out[32], out[33], out[34], out[35]) = (spelling_in(syntax, TokenType::Minus), spelling_in(syntax, TokenType::Plus), spelling_in(syntax, TokenType::Tilde), spelling_in(syntax, TokenType::In));
    out
}

/* the first spelling of token `t` in the words and then the symbols of `syntax` */
const fn spelling_in(syntax: &crate::syntax::lexer::Syntax, t: TokenType) -> &'static str {
    let mut k = 0;
    while k < syntax.words.len() {
        if syntax.words[k].1 as usize == t as usize { return syntax.words[k].0; }
        k += 1;
    }
    k = 0;
    while k < syntax.symbols.len() {
        if syntax.symbols[k].1 as usize == t as usize { return syntax.symbols[k].0; }
        k += 1;
    }
    ""
}

/* how the runtime's own table spells `op` or its role where the table lacks it */
pub fn spelled(cx: &Cx<'_, '_>, op: Op) -> &'static str {
    match slot(op).map(|k| cx.runtime().spellings[k]) {
        Some(s) if !s.is_empty() => s,
        _ => match op {
            Op::Bin(_) | Op::InPlace(_) => "the operator",
            Op::Cmp(_) => "the comparison",
            // a runtime opcode reads as the native it fuses or its own name
            Op::Runtime(n) => cx.runtime().ops.ops.get(n as usize).map_or("the operation", |o| o.fuse.unwrap_or(o.name)),
            _ => "the operation",
        },
    }
}

/* the engine's plain words for `e` which a runtime naming its own replaces */
pub fn plain(cx: &mut Cx<'_, '_>, e: &EngineError) -> String {
    use EngineError as E;
    let ty = |cx: &Cx<'_, '_>, v: Val| String::from(cx.type_name(v));
    match e {
        &E::Unsupported { op, a, b: Some(b) } => crate::s!(str spelled(cx, op), " does not apply to ", str &ty(cx, a), " and ", str &ty(cx, b)),
        &E::Unsupported { op, a, b: None } => crate::s!(str spelled(cx, op), " does not apply to ", str &ty(cx, a)),
        &E::Cannot { act, recv } => {
            let what = recv.map_or(String::from("the value"), |v| crate::s!("a value of kind ", str &ty(cx, v)));
            crate::s!(str &what, " cannot ", str verb(act))
        }
        &E::BadIndex { recv, index } => crate::s!(str &ty(cx, recv), " takes no index of kind ", str &ty(cx, index)),
        &E::BadSlice { index, .. } => crate::s!("a slice takes no bound of kind ", str &ty(cx, index)),
        &E::OutOfRange { recv, .. } => crate::s!(str &ty(cx, recv), " index out of range"),
        E::TooWide(_) => String::from("the int is too wide for an index"),
        &E::MissingKey(k) => crate::s!("no key ", str &cx.render(k, Mode::Repr).unwrap_or_default()),
        &E::Changed(k) => crate::s!("the ", str cx.names().kind(k), " changed its size while a loop walked it"),
        E::ZeroStep { .. } => String::from("a step of zero"),
        E::Arity { callee, fit } => arity(cx, callee, fit),
        E::Call(f) => call_fault(cx, f),
        E::Undefined(n) => crate::s!(str n, " is not bound"),
        E::Unbound { name: Some(n), .. } => crate::s!(str n, " is read before it is bound"),
        E::Unbound { name: None, .. } => String::from("a local is read before it is bound"),
        E::NoAttribute { recv, name, .. } => crate::s!(str &ty(cx, *recv), " has no field ", str name),
        E::ReadOnly { recv, name, .. } => crate::s!("the field ", str name, " of ", str &ty(cx, *recv), " is read only"),
        E::ZeroDiv => String::from("division by zero"),
        E::ZeroPow => String::from("zero to a negative power"),
        E::NegativePower => String::from("an int to a negative power"),
        E::Overflow => String::from(cx.names().overflow),
        E::IntRange => String::from("the int result is past the range ints hold"),
        E::FloatRange => String::from("the float result is out of range"),
        E::NegativeShift => String::from("a negative shift count"),
        E::ByteRange => String::from("a byte lies from 0 to 255"),
        &E::Unpack { want, got, .. } => match got {
            Some(got) => crate::s!("unpacking takes ", int want as i64, " values and got ", int got as i64),
            None => crate::s!("unpacking takes ", int want as i64, " values"),
        },
        &E::SliceSize { size, span } => crate::s!("a sequence of ", int size as i64, " cannot fill a slice of ", int span as i64),
        &E::Uneven { arg, longer } => crate::s!("iterable ", int i64::from(arg) + 1, " of a strict walk ends ", str if longer { "after" } else { "before" }, " the ones ahead of it"),
        E::Generator(g) => match g {
            Gen::Running(_) => String::from("the body is already running"),
            Gen::FreshSend(_) => crate::s!("a body that has not started takes only ", str cx.names().nil),
            Gen::Reused => String::from("the body already ran to its end"),
            Gen::IgnoredClose(_) => String::from("the body went on after a close"),
            Gen::Wait { inside } => crate::s!("a host wait cannot park inside ", str inside),
            Gen::ClassSuspend => String::from("a class body cannot suspend"),
            Gen::Sent => String::from("a builtin iterator takes no sent value"),
        },
        E::Pattern(_) => String::from("the class pattern does not fit its class"),
        &E::Stop(v) | &E::Exit(v) | &E::Assert(v) => if v.is_undef() || v.is_none() { String::new() } else { cx.render(v, Mode::Str).unwrap_or_default() },
        E::Close => String::new(),
        E::Recursion => String::from("calls nested past the recursion limit"),
        E::Memory => String::from("memory limit"),
        E::Of(_, text) => text.clone(),
        &E::Raised(v) => cx.render(v, Mode::Str).unwrap_or_default(),
        E::Internal(text) => String::from(*text),
        E::Budget => String::from("budget exceeded"),
        E::HostYield(_) | E::HostCallDeferred | E::Redo => String::from("a signal left the run"),
    }
}

/* what `act` asks in plain words */
fn verb(act: Act) -> &'static str {
    match act {
        Act::Iterate => "be iterated", Act::Next => "step as an iterator", Act::Call => "be called", Act::Subscript => "be indexed",
        Act::AssignItem => "take an item", Act::DeleteItem => "drop an item", Act::AssignSlice => "take a slice", Act::Slice => "be sliced",
        Act::Sequence => "be sliced as a sequence", Act::Size => "report a size", Act::Hash { .. } => "stand as a key", Act::Await => "be awaited",
        Act::Map { .. } => "be read as a map", Act::Contain => "hold items", Act::Index => "stand for an index", Act::Raise { .. } => "be raised",
        Act::Bytes => "be read as bytes", Act::Spec => "stand as a format spec", Act::Keyword => "name a keyword", Act::DeleteAttr => "lose a field", Act::DropArgs => "lose the arguments it holds",
        Act::Resume(_) => "be resumed", Act::Complex => "be a complex number",
    }
}

/* a misfit call in plain words */
fn arity(cx: &mut Cx<'_, '_>, callee: &Callee, fit: &Fit) -> String {
    let name = match callee {
        Callee::Function { name, .. } => name.as_str(),
        &Callee::Native { id, .. } => cx.vm().natives.name(id),
    };
    match fit {
        Fit::Count(got) => crate::s!(str name, " takes another count of arguments than ", int *got as i64),
        Fit::Unexpected { key, .. } => crate::s!(str name, " takes no keyword ", str key),
        Fit::Twice(key) => crate::s!(str name, " got ", str key, " twice"),
        Fit::Missing { names, .. } if !names.is_empty() => crate::s!(str name, " misses ", str &names.join(" and ")),
        Fit::Missing { at, .. } => crate::s!(str name, " misses argument ", int *at as i64 + 1),
        Fit::ByKeyword(key) => crate::s!(str name, " takes ", str key, " only by position"),
    }
}

/* a refused call in plain words */
fn call_fault(cx: &Cx<'_, '_>, f: &CallFault) -> String {
    match f {
        CallFault::NoArgs { name, .. } => crate::s!(str name, " takes no arguments"),
        CallFault::InitArgs | CallFault::CtorArgs => String::from("the class takes no arguments"),
        CallFault::ExcKeywords => String::from("an error class takes no keywords"),
        &CallFault::NoReceiver { kind, name } => crate::s!(str name, " of ", str cx.names().kind(kind), " needs a receiver"),
        &CallFault::WrongReceiver { kind, name, got } => crate::s!(str name, " of ", str cx.names().kind(kind), " takes no receiver of kind ", str cx.type_name(got)),
        CallFault::TooMany { name, got } => crate::s!(str name, " got ", int *got as i64, " arguments past the most a call passes"),
        CallFault::Instantiate(name) => crate::s!(str name, " makes no instances"),
        CallFault::Base(Some(name)) => crate::s!(str name, " cannot be a base"),
        CallFault::Base(None) => String::from("a base must be a class"),
    }
}

/* the words of `e` with no machine to name its values */
pub fn bare(e: &EngineError) -> String {
    use EngineError as E;
    match e {
        E::Unsupported { .. } => String::from("an operator does not apply to its operands"),
        &E::Cannot { act, .. } => crate::s!("the value cannot ", str verb(act)),
        E::OutOfRange { .. } => String::from("index out of range"),
        E::Undefined(n) => crate::s!(str n, " is not bound"),
        E::NoAttribute { name, .. } => crate::s!("no field ", str name),
        E::Of(_, text) => text.clone(),
        E::Internal(text) => String::from(*text),
        E::ZeroDiv => String::from("division by zero"),
        E::Recursion => String::from("calls nested past the recursion limit"),
        E::Memory => String::from("memory limit"),
        E::Budget => String::from("budget exceeded"),
        _ => String::from("the engine refused the operation"),
    }
}

/* the ABI kind whose class the error names and Custom for any other class */
pub fn abi_kind(e: &VmErr, names: &super::Names) -> crate::abi::ErrorKind {
    abi_of(match e { VmErr::Raised(_) => names.error_named(e.class(names)), e => e.kind() })
}

/* the ABI kind of engine error kind `kind` and Custom for any other */
pub(crate) fn abi_of(kind: Option<ErrKind>) -> crate::abi::ErrorKind {
    use crate::abi::ErrorKind;
    match kind {
        Some(ErrKind::Type) => ErrorKind::Type,
        Some(ErrKind::Value) => ErrorKind::Value,
        Some(ErrKind::Runtime) => ErrorKind::Runtime,
        Some(ErrKind::Attribute) => ErrorKind::Attribute,
        Some(ErrKind::Index) => ErrorKind::Index,
        Some(ErrKind::Key) => ErrorKind::Key,
        _ => ErrorKind::Custom,
    }
}
