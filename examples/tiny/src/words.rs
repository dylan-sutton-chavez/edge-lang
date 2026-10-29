use alloc::format;
use alloc::string::{String, ToString};
use core::ops::Range;

use edge::lang::{Act, Cx, EngineError, Intrinsic, KindId, Misfit, Mode, Native, Op, VAR};
use edge::value::foreign::{BinOp, UnOp};
use edge::value::{Val, VmErr};
use lib::fail::{At, Fail, english};

use crate::Tiny;
use crate::natives::wrong;

/* tiny's words for the failures of its kernels with lib's neutral words for the rest */
pub fn words(cx: &mut Cx<'_, '_>, at: At, f: Fail) -> VmErr {
    match f {
        Fail::IntLiteral { base: 10, v } | Fail::FloatLiteral(v) => match cx.render(v, Mode::Repr) {
            Ok(r) => VmErr::Raised(format!("BadData: {r} spells no number")),
            Err(e) => e,
        },
        Fail::BadOperand(v) => wrong(cx, at.name, "a number", v),
        // a fraction is a number too so the words name what it lacks
        Fail::NotIndex(v) if cx.kind(v) == KindId::Float => VmErr::Raised(format!("WrongType: {} takes a whole number, not a fraction", at.name)),
        Fail::NotIndex(v) => wrong(cx, at.name, "a whole number", v),
        Fail::ExtremeEmpty => VmErr::Raised(format!("BadData: {} takes at least one value", at.name)),
        f => english::<Tiny>(cx, at, f),
    }
}

/* the words of a call that misses a native's signature */
pub fn arity(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr {
    let name = match owner { Some(_) => format!(".{}", n.name), None => n.name.to_string() };
    let (lo, hi) = (n.sig.min as usize, n.sig.max);
    let want = match (lo, hi) {
        (l, h) if h != VAR && l == h as usize => format!("{l}"),
        (l, VAR) => format!("at least {l}"),
        (l, h) => format!("{l} to {h}"),
    };
    // one argument however many more are allowed takes no plural
    let plural = if lo == 1 && (hi == 1 || hi == VAR) { "" } else { "s" };
    VmErr::Raised(match m {
        Misfit::Count(got) => format!("WrongType: {name} takes {want} argument{plural}, not {got}"),
        Misfit::Unexpected(k) | Misfit::Twice(k) => format!("WrongType: {name} takes its arguments in order, not as {k}"),
        Misfit::Missing(i) => format!("WrongType: {name} misses argument {}", i + 1),
    })
}

/* the place in a flow a run's error rose at and the line it reads */
pub fn report(src: &str, err: &str, pos: Option<usize>) -> String {
    let (class, msg) = err.split_once(": ").unwrap_or((err, ""));
    let (class, msg) = named(class, msg);
    // a run out of steps or memory fails as a whole and names no report
    let Some(pos) = pos.filter(|_| class != "OutOfBudget") else { return format!("{class}, {msg}") };
    let line = src.as_bytes()[..pos.min(src.len())].iter().filter(|&&c| c == b'\n').count() + 1;
    let place = place(src, pos);
    format!("{place} at line {line}: {class}, {msg}")
}

/* tiny's class and words for an error the engine or a kernel raised */
fn named(class: &str, msg: &str) -> (String, String) {
    // a run out of steps fails as a runtime error whose words tell it apart
    if msg == "budget exceeded" { return ("OutOfBudget".into(), "the run spent its budget of steps".into()); }
    if class == "OutOfBudget" && msg == "memory limit" { return (class.into(), "the run passed its memory limit".into()); }
    (class.to_string(), keyed(msg))
}

/* a missing key's text as the field it names */
fn keyed(msg: &str) -> String {
    if msg.len() > 1 && msg.starts_with('"') && msg.ends_with('"') { return format!("no field {msg}"); }
    // a record's key that is no text reads as the value it was
    if msg.parse::<f64>().is_ok() { return format!("no field {msg}"); }
    if msg.starts_with('(') && msg.ends_with(')') && msg.matches("; ").count() == 2 { return "a record is read by field, not by a slice".into(); }
    msg.to_string()
}

/* every engine error in tiny's words with the engine's plain words for the rest */
pub fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String {
    use EngineError as E;
    let ty = |cx: &Cx<'_, '_>, v: Val| String::from(cx.type_name(v));
    match e {
        &E::Unsupported { op, a, b: Some(b) } => operands(cx, op, a, b).unwrap_or_else(|| edge::lang::plain(cx, e)),
        &E::Unsupported { op: Op::Unary(UnOp::Neg), a, b: None } => format!("cannot negate {}", ty(cx, a)),
        &E::Unsupported { op: Op::Unary(UnOp::Pos), a, b: None } => format!("cannot apply + to {}", ty(cx, a)),
        &E::Cannot { act, recv: Some(v) } => match act {
            Act::Subscript => format!("{} holds no items", ty(cx, v)),
            Act::Iterate => format!("{} holds no values", ty(cx, v)),
            Act::Contain => format!("cannot look for a value in {}", ty(cx, v)),
            Act::Hash { .. } => format!("a {} cannot be a key", ty(cx, v)),
            // the length native as tiny names it
            Act::Size => format!("{} takes text, a list or a record, not {}", cx.natives().intrinsic(Intrinsic::Len).map_or("len", |n| n.name), ty(cx, v)),
            _ => edge::lang::plain(cx, e),
        },
        &E::BadIndex { recv, index } if cx.kind(index) == KindId::Float => format!("the {} takes a whole number as its index, not a fraction", ty(cx, recv)),
        &E::BadIndex { recv, index } => format!("the {} takes a number as its index, not {}", ty(cx, recv), ty(cx, index)),
        &E::OutOfRange { recv, .. } => format!("the {} holds no item at that index", ty(cx, recv)),
        E::NoAttribute { recv, name, store: false } if cx.kind(*recv) == KindId::Dict => format!("no field \"{name}\""),
        E::NoAttribute { recv, name, store: false } => format!("{} has no field or method {name}", ty(cx, *recv)),
        E::Undefined(name) => format!("no field or function \"{name}\""),
        E::FloatRange => "the result is too large for a number".into(),
        E::IntRange => "the result is too large for a whole number".into(),
        e => edge::lang::plain(cx, e),
    }
}

/* a binary operator's refusal of `a` and `b` in tiny's words */
fn operands(cx: &mut Cx<'_, '_>, op: Op, a: Val, b: Val) -> Option<String> {
    let (ta, tb) = (String::from(cx.type_name(a)), String::from(cx.type_name(b)));
    let repeats = |k: KindId| matches!(k, KindId::Str | KindId::List | KindId::Tuple | KindId::Bytes | KindId::ByteArray);
    let bin = match op {
        Op::Cmp(_) => return Some(format!("cannot compare {ta} and {tb}")),
        // only text holds text which the left side must then be
        Op::In => return Some(format!("in looks for text in text, not {ta}")),
        Op::Bin(b) | Op::InPlace(b) => b,
        _ => return None,
    };
    let verb = match bin {
        BinOp::Pow => return Some("** takes a number on each side".into()),
        BinOp::Mul if repeats(cx.kind(a)) || repeats(cx.kind(b)) => {
            let count = if repeats(cx.kind(a)) { b } else { a };
            let kind = if cx.kind(count) == KindId::Float { "a fraction" } else { cx.type_name(count) };
            return Some(format!("text or a list repeats a whole number of times, not {kind}"));
        }
        BinOp::Add => "add",
        BinOp::Sub => "subtract",
        BinOp::Mul => "multiply",
        BinOp::TrueDiv | BinOp::FloorDiv => "divide",
        BinOp::Mod => "take the remainder of",
        _ => return None,
    };
    Some(format!("cannot {verb} {ta} and {tb}"))
}

/* the report and key whose bytes hold `pos` as an error names them */
fn place(src: &str, pos: usize) -> String {
    let Ok(docs) = crate::yaml::documents(src) else { return "the flow".into() };
    let Ok(prog) = crate::flow::check(src, &docs) else { return "the flow".into() };
    let mut best: Option<(Range<usize>, String)> = None;
    let mut keep = |span: &Range<usize>, what: String| {
        let holds = span.start <= pos && pos < span.end.max(span.start + 1);
        if holds && best.as_ref().is_none_or(|(b, _)| span.len() <= b.len()) { best = Some((span.clone(), what)); }
    };
    keep(&prog.from.span, "from".into());
    if let Some(w) = &prog.filter { keep(&w.span, "where".into()); }
    for r in &prog.reports {
        let here = format!("report {}", r.name);
        keep(&r.span, here.clone());
        for (span, key) in r.entries() { keep(&span, format!("{here}, {key}")); }
    }
    best.map_or_else(|| "the flow".into(), |(_, w)| w)
}
