use edge::lang::{Args, Cx, Mode, Native, Ret, Sig};
use edge::value::{Val, VmErr};
use lib::date::{Days, Duration, Field, Hours, Minutes, Part, Time};
use lib::num::round::HalfAway;
use lib::text::case::Full;
use lib::uni::Decimal;
use lib::{date, iter, num, seq, text};

use crate::Tiny;

pub const FNS: &[Native] = &lib::table![Tiny;
    "time" => date::parse[Tiny],
    "len" => seq::len,
    "str" => text::text,
    "int" => num::int::new[Decimal],
    "float" => num::float::new[Decimal],
    "round" => num::round::round[HalfAway],
    "abs" => num::abs,
    "min" => iter::fold::least,
    "max" => iter::fold::most,
];

// the spans between two times which lib holds no kernel for
pub const OWN: &[Native] = &[
    Native::new("hours_between", Sig::exact(2), between::<Hours>),
    Native::new("minutes_between", Sig::exact(2), between::<Minutes>),
    Native::new("days_between", Sig::exact(2), between::<Days>),
];

pub const STR: &[Native] = &lib::table![Tiny;
    "endswith" => text::find::ends,
    "startswith" => text::find::starts,
    "removeprefix" => text::edit::drop_prefix,
    "removesuffix" => text::edit::drop_suffix,
    "lower" => text::case::lower[Full],
    "upper" => text::case::upper[Full],
    "strip" => text::pad::trim,
    "split" => text::cut::split,
    "replace" => text::edit::replace,
    "find" => text::find::find,
];

/* the kind of `v` as tiny's names table words it */
pub fn kind_name<'a>(cx: &'a Cx<'_, '_>, v: Val) -> &'a str { cx.type_name(v) }

/* the error a native raises when its receiver or argument is of another kind */
pub fn wrong(cx: &Cx<'_, '_>, what: &str, want: &str, v: Val) -> VmErr {
    VmErr::Raised(alloc::format!("WrongType: {what} takes {want}, not {}", kind_name(cx, v)))
}

/* the span from the first time to the second counted in the unit `P` names */
fn between<P: Part>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let what = match P::FIELD { Field::Days => "days_between", Field::Minutes => "minutes_between", _ => "hours_between" };
    let (start, end) = (instant(cx, a.pos[0], what)?, instant(cx, a.pos[1], what)?);
    Ok(Val::float(P::FIELD.of_duration(&Duration { nanos: end.nanos - start.nanos })).into())
}

/* the time a value holds or its RFC 3339 text spells */
fn instant(cx: &mut Cx<'_, '_>, v: Val, what: &str) -> Result<Time, VmErr> {
    if let Some(s) = cx.str_of(v) {
        if let Some(t) = Time::parse(s) { return Ok(t); }
        let shown = cx.render(v, Mode::Repr)?;
        return Err(VmErr::Raised(alloc::format!("BadData: invalid RFC 3339 time: {shown}")));
    }
    let heap = cx.heap();
    if let Some(t) = Cx::kind_token(heap, &crate::lang::TIME).and_then(|token| heap.foreign_payload::<Time>(token, v)) { return Ok(*t); }
    Err(wrong(cx, what, "times or RFC 3339 texts", v))
}
