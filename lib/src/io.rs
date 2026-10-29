use alloc::borrow::Cow;
use alloc::string::String;

use edge::lang::{Args, Cx, Language, Mode, Ret, STEPS, Sig, Step};
use edge::value::{HeapObj, HeapPool, Val, VmErr};

use crate::fail::{Fail, fail};
use crate::fmt::render::{Style, push_f64};
use crate::{Kernel, Lang, Site};

/* writes each value as the runtime renders it between a separator and before an end */
pub const fn print<S: Site>() -> Kernel {
    let print = Kernel::new(Sig::var(0), print_run::<S>).reads(2);
    if <S::Lang as Lang>::PLAIN { print.fast_out(print_plain::<S>) } else { print }
}

/* the next line the host fed */
pub const fn read_line<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), read_line_run::<S>) }

/* the oldest event the host pushed or a wait for the next */
pub const fn receive<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), receive_run).flags(STEPS) }

/* hands one message to the host's scheduler */
pub const fn send<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), send_run::<S>) }

/* ends the run with the status it is given where none means zero */
pub const fn exit<S: Site>() -> Kernel { Kernel::new(Sig::range(0, 1), exit_run::<S>) }

fn print_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (sep, end) = if a.kw.is_empty() { (Cow::Borrowed(" "), Cow::Borrowed("\n")) } else { (text_kw::<S>(cx, a.kw(0), " ", 0)?, text_kw::<S>(cx, a.kw(1), "\n", 1)?) };
    let mut body = cx.line();
    for (i, &v) in a.pos.iter().enumerate() {
        if i > 0 { body.push_str(&sep); }
        <S::Lang as Lang>::show_into(cx, v, Mode::Str, &mut body)?;
    }
    body.push_str(&end);
    cx.write_line(body);
    Ok(Val::none().into())
}

/* the line a print of scalars and texts writes with the units their rendering charges */
fn print_plain<S: Site>(heap: &HeapPool, pos: &[Val], out: &mut String) -> Option<usize> {
    let mut units = 0;
    for (i, &v) in pos.iter().enumerate() {
        if i > 0 { out.push(' '); }
        let start = out.len();
        if v.is_int() { out.push_str(itoa::Buffer::new().format(v.as_int())); }
        else if let Some(HeapObj::Str(s)) = heap.try_get(v) { out.push_str(s); }
        else if v.is_bool() { out.push_str(if v.as_bool() { <S::Lang as Style>::TRUE } else { <S::Lang as Style>::FALSE }); }
        else if v.is_none() { out.push_str(<S::Lang as Language>::NAMES.nil); }
        else if v.is_float() { push_f64::<S::Lang>(out, v.as_float()); }
        else { return None; }
        units += out.len() - start;
    }
    out.push('\n');
    Some(units)
}

/* keyword slot `at` as given where nil keeps the default */
fn text_kw<S: Site>(cx: &mut Cx<'_, '_>, v: Option<Val>, default: &'static str, at: u8) -> Result<Cow<'static, str>, VmErr> {
    let Some(v) = v.filter(|v| !v.is_none()) else { return Ok(Cow::Borrowed(default)) };
    match cx.text(v) {
        Some(t) => Ok(Cow::Owned(t.into())),
        None => Err(fail::<S>(cx, Fail::KeywordText { at, v })),
    }
}

fn read_line_run<S: Site>(cx: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
    match cx.read_line() {
        Some(line) => Ok(cx.new_str(line)?.into()),
        None => Err(fail::<S>(cx, Fail::EndOfInput)),
    }
}

fn receive_run(cx: &mut Cx<'_, '_>, _: Args<'_>) -> Ret {
    match cx.take_event() {
        Some(v) => Ok(v.into()),
        None => Ok(Step::Wait.into()),
    }
}

fn exit_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let code = a.pos.first().copied().unwrap_or(Val::int(0));
    if !code.is_int() { return Err(fail::<S>(cx, Fail::ExitCode(code))); }
    // an uncaught exit failure ends the run with the status it holds
    Err(fail::<S>(cx, Fail::Exit(code)))
}

fn send_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let Some(group) = cx.text(a.pos[0]).map(String::from) else { return Err(fail::<S>(cx, Fail::SendText(1))) };
    let Some(body) = cx.text(a.pos[1]).map(String::from) else { return Err(fail::<S>(cx, Fail::SendText(2))) };
    if !cx.sent(&group, &body)? { return Err(fail::<S>(cx, Fail::NoScheduler)); }
    Ok(Val::none().into())
}
