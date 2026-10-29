use edge::lang::{Args, Cx, Ret, Sig};
use edge::value::Val;

use super::recv_str;
use crate::uni::{self, Digits};
use crate::{Kernel, Site};

/* whether the text is nonempty and every character a digit of the class `D` names */
pub const fn digit<S: Site, D: Digits>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, |s| every(s, D::is))) }

/* whether the text is nonempty and every character a letter */
pub const fn alpha<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, |s| every(s, uni::is_alpha))) }

/* whether the text is nonempty and every character a letter or a number */
pub const fn alnum<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, |s| every(s, uni::is_alnum))) }

/* whether the text is nonempty and every character white space in the runtime's set */
pub const fn space<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, |s| every(s, crate::lang::spaces::<S::Lang>()))) }

/* whether the text has a cased character and no lowercase one */
pub const fn upper<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, |s| only_case(s, char::is_lowercase))) }

/* whether the text has a cased character and no uppercase one */
pub const fn lower<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, |s| only_case(s, char::is_uppercase))) }

/* whether each cased run starts uppercase then goes lowercase */
pub const fn title<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, title_cased)) }

/* whether every character is ascii */
pub const fn ascii<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, str::is_ascii)) }

/* whether every character prints as itself */
pub const fn printable<S: Site>() -> Kernel { Kernel::new(Sig::exact(0), |cx, a| is(cx, a, |s| s.chars().all(uni::is_printable))) }

fn is(cx: &mut Cx<'_, '_>, a: Args<'_>, f: impl Fn(&str) -> bool) -> Ret { Ok(Val::bool(f(recv_str(cx, a.recv)?)).into()) }

/* a class test that needs a character and every character in the class */
fn every(s: &str, f: fn(char) -> bool) -> bool { !s.is_empty() && s.chars().all(f) }

/* a case test that needs a cased character and none of the other case */
fn only_case(s: &str, other: fn(char) -> bool) -> bool { s.chars().any(|c| c.is_uppercase() || c.is_lowercase()) && !s.chars().any(other) }

/* whether each cased run starts uppercase then goes lowercase with one cased character */
fn title_cased(s: &str) -> bool {
    let (mut prev, mut any) = (false, false);
    for c in s.chars() {
        let (upper, lower) = (c.is_uppercase() || uni::is_titlecase(c), c.is_lowercase());
        if (upper && prev) || (lower && !prev) { return false; }
        prev = upper || lower;
        any |= prev;
    }
    any
}
