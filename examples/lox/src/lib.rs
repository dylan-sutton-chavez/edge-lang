#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

use alloc::format;
use alloc::string::{String, ToString};

use interpreter::Diagnostic;
use interpreter::code::{Parsed, Unit};
use interpreter::lang::{Act, Args, CallFault, Cx, Descriptor, EngineError, ErrKind, Falsy, Fit, Front, KindId, Language, Names, Native, Natives, Op, Overflow, Policy, Ret, Runtime, Show, Sig, ZeroDiv};
use interpreter::lexer::TokenType as T;
use interpreter::lexer::{Comments, Cond, Keywords, Lambda, Quote, Shapes, Spelling, Strings, Syntax, TokenType, index, operators};
use interpreter::{Val, VmErr};

mod lower;
pub mod parse;

pub struct Lox;

const WORDS: &[(&str, TokenType)] = &[
    ("and", T::And), ("class", T::Class), ("else", T::Else), ("false", T::False), ("for", T::For), ("fun", T::Fn), ("if", T::If),
    ("nil", T::Nil), ("or", T::Or), ("return", T::Return), ("true", T::True), ("while", T::While),
];

const SYMBOLS: &[(&str, TokenType)] = &[
    ("(", T::ParenOpen), (")", T::ParenClose), ("{", T::BraceOpen), ("}", T::BraceClose), ("{", T::BlockOpen), ("}", T::BlockClose),
    (",", T::Comma), (".", T::Dot), ("-", T::Minus), ("+", T::Plus), (";", T::Semi), ("/", T::Slash), ("*", T::Star),
    ("!=", T::NotEqual), ("!", T::Not), ("==", T::EqualEqual), ("=", T::Equal), (">=", T::GreaterEqual), (">", T::Greater),
    ("<=", T::LessEqual), ("<", T::Less),
];

static OPERATORS: [[TokenType; 4]; 128] = operators(SYMBOLS);

static KEYWORDS: Keywords = index(WORDS);

// the scanner of the book where a string spans lines and keeps its backslashes
pub const LOX: Syntax = Syntax::PLAIN
    .name("lox")
    .extension("lox")
    .words(WORDS)
    .symbols(SYMBOLS)
    .operators(&OPERATORS)
    .keywords(&KEYWORDS)
    .comments(Comments::PLAIN.line(&["//"]))
    .strings(Strings::PLAIN
        .quotes(&[Quote::PLAIN.multiline(true)]))
    .shapes(Shapes::PLAIN
        .cond(Cond::None)
        .lambda(Lambda::None)
        .splat(false)
        .slices(false)
        .header_parens(true)
        .comprehensions(false)
        .decorators(false)
        .named(false));

impl Spelling for Lox {
    const SYNTAX: Syntax = LOX;
    const NATIVES: &'static Natives = &NATIVES;
}

/* writes the value as its own line */
fn print(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let mut line = cx.render(a.pos[0], Show::Str)?;
    line.push('\n');
    cx.write_line(line);
    Ok(Val::none().into())
}

/* the seconds since the epoch on a host with a clock and zero elsewhere */
#[allow(clippy::disallowed_methods)]
fn clock(cx: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
    #[cfg(feature = "std")]
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    #[cfg(not(feature = "std"))]
    let secs = 0.0;
    let _ = cx;
    Ok(Val::float(secs).into())
}

/* method `name` of superclass `sup` bound to receiver `this` */
fn superclass(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (sup, this) = (a.pos[0], a.pos[1]);
    let name = cx.text(a.pos[2]).unwrap_or("").to_string();
    let Some(f) = cx.class_member(sup, &name) else { return Err(EngineError::Of(ErrKind::Attribute, format!("Undefined property '{name}'.")).into()) };
    Ok(cx.heap_mut().bind(this, f)?.into())
}

/* any value as print shows it */
fn text(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let s = cx.render(a.pos[0], Show::Str)?;
    Ok(cx.new_str(s)?.into())
}

/* a list of the values given which a host module takes where it wants pairs */
fn list(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret { Ok(cx.new_list(a.pos.to_vec())?.into()) }

/* the item at `key` of what a host gave with a whole number as an index */
fn item(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let key = match cx.float(a.pos[1]).and_then(Result::ok) {
        Some(f) if f as i64 as f64 == f && f.abs() < 9e15 => cx.new_int(f as i128)?,
        _ => a.pos[1],
    };
    Ok(cx.get_item(a.pos[0], key)?.into())
}

// what a program reaches past the book through its host each as a lib kernel
const HOST: &[Native] = &interpreter::lib::table![Lox;
    "input" => interpreter::lib::io::read_line,
    "exit" => interpreter::lib::io::exit,
    "send" => interpreter::lib::io::send,
    "receive" => interpreter::lib::io::receive,
    "len" => interpreter::lib::seq::len,
];

const OWN: &[Native] = &[
    Native::new("clock", Sig::exact(0), clock),
    Native::new("text", Sig::exact(1), text),
    Native::new("list", Sig::var(0), list),
    Native::new("item", Sig::exact(2), item),
    // no program spells these names so a variable never hides them
    Native::new("@print", Sig::exact(1), print),
    Native::new("@super", Sig::exact(3), superclass),
];

// the natives a crash test calls to fail on purpose which no release build holds
#[cfg(feature = "crash")]
const CRASH: &[Native] = &[Native::new("crash_panic", Sig::exact(0), crash_panic), Native::new("crash_abort", Sig::exact(0), crash_abort)];
#[cfg(not(feature = "crash"))]
const CRASH: &[Native] = &[];

#[cfg(feature = "crash")]
fn crash_panic(_: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
    panic!("a crash test runtime panics on purpose")
}

#[cfg(feature = "crash")]
fn crash_abort(_: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
    std::process::abort()
}

pub static NATIVES: Natives = interpreter::natives! { language: Lox, fns: [OWN, HOST, CRASH], methods: [], types: &[] };

pub static RUNTIME: Runtime = Runtime::of::<Lox>();

fn parse(src: &str, unit: Unit) -> Parsed {
    let prog = parse::parse(src)?;
    interpreter::code::build::<Lox>(src, unit, |b| lower::lower(b, &prog))
}

pub const FRONT: Front = Front::new(parse, parse::imports, LOX, &<Lox as Language>::POLICY);

/* `src` compiled as `FRONT.compile` does keeping each instruction's span and the outline of its forms */
pub fn read(src: &str) -> (interpreter::code::SSAChunk, alloc::vec::Vec<Diagnostic>) {
    let unit = FRONT.unit(alloc::boxed::Box::new(interpreter::NoopResolver));
    match parse::read(src).and_then(|prog| interpreter::code::build::<interpreter::code::Recorded<Lox>>(src, unit, |b| lower::lower(b, &prog))) {
        Ok(chunk) => {
            let mut chunk = alloc::rc::Rc::unwrap_or_clone(chunk);
            interpreter::code::constant_fold(&mut chunk, FRONT.policy());
            (chunk, alloc::vec::Vec::new())
        }
        Err(errors) => (interpreter::code::SSAChunk::default(), errors),
    }
}

// the names the CLI gives this runtime's files and errors
pub const DESCRIPTOR: Descriptor = Descriptor::PLAIN
    .language("lox")
    .entries(&["main.lox", "app.lox", "index.lox"])
    .test_suffix("_test.lox")
    .scaffold("print \"hello from interpreter\";\n")
    .example_fence("lox")
    .manifest_fence("lox-manifest")
    .statuses((65, 70))
    .diagnosed(Some(diagnosed));

/* compile errors as the book reports them each on its line */
fn diagnosed(src: &str, errors: &[Diagnostic]) -> String {
    errors.iter().map(|e| format!("[line {}] {}\n", line(src, e.start), e.msg)).collect()
}

/* the line a byte offset sits on counting from one */
fn line(src: &str, at: usize) -> usize { 1 + src[..at.min(src.len())].bytes().filter(|&c| c == b'\n').count() }

/* a number as the book prints one with no fraction for a whole value */
pub fn number(n: f64) -> String {
    if n.is_nan() { return "nan".into(); }
    format!("{n}")
}

/* a raised error as the book reports one with its message then its line */
fn report(src: &str, head: &str, pos: Option<usize>) -> String {
    let message = head.split_once(": ").map_or(head, |(_, m)| m);
    format!("{message}\n[line {}] in script", line(src, pos.unwrap_or(0)))
}

// lib words its failures and renders what the book has no words for
impl interpreter::lib::Lang for Lox {}

impl Language for Lox {
    const VERSION: u32 = 1;
    // every number is a double and only nil and false are false
    const POLICY: Policy = Policy::PLAIN
        .int_overflow(Overflow::Error)
        .zero_div(ZeroDiv::Ieee)
        .bool_is_int(false)
        .falsy(Falsy::NIL.or(Falsy::FALSE))
        .neg_index(false)
        .open_fields(false)
        .shared_bound(false)
        .open_order(false)
        .recursion_limit(1000);
    const NATIVES: &'static Natives = &NATIVES;
    const NAMES: &'static Names = &Names::PLAIN;
    const FRONT: Front = FRONT;
    const DESCRIPTOR: &'static Descriptor = &DESCRIPTOR;
    const REPORT: Option<interpreter::lang::WordsFn> = Some(report);
    type Ops = ();

    fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Show) -> Result<String, VmErr> {
        Ok(match cx.kind(v) {
            KindId::None => "nil".into(),
            KindId::Bool => if v.as_bool() { "true" } else { "false" }.into(),
            KindId::Float | KindId::Int => number(cx.float(v).and_then(Result::ok).unwrap_or(0.0)),
            KindId::Str => cx.text(v).unwrap_or("").into(),
            KindId::Class => cx.fn_name(v).unwrap_or("").rsplit('.').next().unwrap_or("").into(),
            KindId::Instance => {
                let class = cx.heap().class_of(v);
                format!("{} instance", class.and_then(|c| cx.fn_name(c)).unwrap_or("").rsplit('.').next().unwrap_or(""))
            }
            _ if cx.callable(v) => match cx.fn_name(v) {
                Some(name) => format!("<fn {}>", name.rsplit('.').next().unwrap_or(name)),
                None => "<native fn>".into(),
            },
            _ => interpreter::lib::fmt::show::<Lox>(cx, v, mode)?,
        })
    }

    fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String {
        match e {
            EngineError::Unsupported { op: Op::Bin(interpreter::value::foreign::BinOp::Add), .. } => "Operands must be two numbers or two strings.".into(),
            EngineError::Unsupported { op: Op::Unary(_), .. } => "Operand must be a number.".into(),
            EngineError::Unsupported { .. } => "Operands must be numbers.".into(),
            EngineError::Cannot { act: Act::Call, .. } => "Can only call functions and classes.".into(),
            EngineError::Arity { callee: interpreter::lang::Callee::Function { name, normal, .. }, fit, .. } => {
                let normal = *normal as usize;
                let got = match fit { Fit::Count(n) => *n as usize, Fit::Missing { names, .. } => normal - names.len(), _ => 0 };
                // a method counts its receiver which its qualified name shows as a class before it
                let mut parts = name.rsplit('.');
                let method = parts.next().is_some() && parts.next().is_some_and(|p| p != "<scope>");
                format!("Expected {} arguments but got {}.", normal - usize::from(method), got - usize::from(method))
            }
            EngineError::Undefined(name) => format!("Undefined variable '{name}'."),
            EngineError::NoAttribute { recv, name, store, .. } => {
                if cx.kind(*recv) == KindId::Instance { format!("Undefined property '{name}'.") }
                else if *store { "Only instances have fields.".into() }
                else { "Only instances have properties.".into() }
            }
            EngineError::ReadOnly { .. } => "Only instances have fields.".into(),
            EngineError::Recursion => "Stack overflow.".into(),
            EngineError::Call(CallFault::Base(_)) => "Superclass must be a class.".into(),
            EngineError::Call(CallFault::NoArgs { got, .. }) => format!("Expected 0 arguments but got {got}."),
            _ => interpreter::lang::plain(cx, e),
        }
    }
}

#[cfg(feature = "web")]
interpreter::exports!(RUNTIME);
