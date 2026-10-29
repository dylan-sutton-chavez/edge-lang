use alloc::vec::Vec;

use crate::value::{Val, VmErr};
use super::{Native, Sig, VAR};

/* how a call misses a signature which the runtime's arity hook words */
#[derive(Clone, Copy, Debug)]
pub enum Misfit<'a> {
    // the positionals the call came to past the signature's range
    Count(usize),
    // a keyword the signature has no parameter for
    Unexpected(&'a str),
    // a parameter given twice by position and keyword or by two keywords
    Twice(&'a str),
    // a required positional left unfilled at this index
    Missing(usize),
}

/* a call's arguments laid out in signature order */
pub(crate) struct Bound { pub pos: Vec<Val>, pub kw: Vec<Val>, pub named: Vec<Val> }

/* binds positionals and flat keyword pairs to `sig` mapping keywords before counts are checked */
pub(crate) fn bind<'a>(sig: &Sig, pos: &[Val], kw: &[Val], name_of: impl Fn(Val) -> Option<&'a str>) -> Result<Bound, Misfit<'a>> {
    let mut out = Bound { pos: pos.to_vec(), kw: alloc::vec![Val::undef(); sig.kw.len()], named: Vec::new() };
    for pair in kw.as_chunks::<2>().0 {
        let name = name_of(pair[0]).unwrap_or("");
        if let Some(i) = sig.names.iter().position(|&n| !n.is_empty() && n == name) {
            if out.pos.get(i).is_some_and(|v| !v.is_undef()) { return Err(Misfit::Twice(name)); }
            if out.pos.len() <= i { out.pos.resize(i + 1, Val::undef()); }
            out.pos[i] = pair[1];
        } else if let Some(j) = sig.kw.iter().position(|&n| n == name) {
            if !out.kw[j].is_undef() { return Err(Misfit::Twice(name)); }
            out.kw[j] = pair[1];
        } else if sig.extra {
            out.named.extend_from_slice(pair);
        } else {
            return Err(Misfit::Unexpected(name));
        }
    }
    let n = out.pos.len();
    if sig.max != VAR && n > sig.max as usize { return Err(Misfit::Count(n)); }
    if let Some(i) = out.pos.iter().take(sig.min as usize).position(|v| v.is_undef()) { return Err(Misfit::Missing(i)); }
    if n < sig.min as usize { return Err(Misfit::Count(n)); }
    Ok(out)
}

/* the plain words of a misfit when the runtime words none */
#[cold]
pub(crate) fn plain(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr {
    let name = match owner { Some(o) => crate::s!(str o, ".", str n.name, "()"), None => crate::s!(str n.name, "()") };
    let (min, max) = (n.sig.min as i64, n.sig.max as i64);
    // one argument reads singular where every other count reads plural
    let noun = if min == 1 { " argument, not " } else { " arguments, not " };
    VmErr::TypeMsg(match m {
        Misfit::Count(got) if n.sig.max == VAR => crate::s!(str &name, " takes at least ", int min, str noun, int got as i64),
        Misfit::Count(got) if min == max => crate::s!(str &name, " takes ", int min, str noun, int got as i64),
        Misfit::Count(got) => crate::s!(str &name, " takes ", int min, " to ", int max, " arguments, not ", int got as i64),
        Misfit::Unexpected(k) => crate::s!(str &name, " takes no keyword ", str k),
        Misfit::Twice(k) => crate::s!(str &name, " takes ", str k, " only once"),
        Misfit::Missing(i) => match n.sig.names.get(i).filter(|p| !p.is_empty()) {
            Some(p) => crate::s!(str &name, " needs ", str p),
            None => crate::s!(str &name, " needs argument ", int i as i64 + 1),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIG: Sig = Sig::range(1, 3).named(&["", "base", "end"]).kw(&["key"]);

    fn bound(pos: &[Val], kw: &[(&'static str, Val)]) -> Result<Bound, Misfit<'static>> {
        let names: Vec<&'static str> = kw.iter().map(|p| p.0).collect();
        let flat: Vec<Val> = kw.iter().enumerate().flat_map(|(i, p)| [Val::int(i as i64), p.1]).collect();
        bind(&SIG, pos, &flat, |k| names.get(k.as_int() as usize).copied())
    }

    #[test]
    fn keywords_fill_their_positions_and_slots() {
        let b = bound(&[Val::int(1)], &[("end", Val::int(3)), ("key", Val::int(9))]).unwrap_or_else(|m| panic!("{m:?}"));
        assert_eq!(b.pos.len(), 3);
        assert!(b.pos[1].is_undef() && b.pos[2].as_int() == 3 && b.kw[0].as_int() == 9);
    }

    #[test]
    fn the_engine_words_a_misfit_in_plain_terms() {
        let body: crate::lang::NativeFn = |_, _| Ok(Val::none().into());
        let (one, span, many) = (Native::new("size", Sig::exact(1), body), Native::new("between", Sig::range(1, 3).named(&["low", "", "high"]), body), Native::new("log", Sig::var(1), body));
        let said = |e: VmErr| match e { VmErr::TypeMsg(t) => t, e => panic!("{e:?}") };
        assert_eq!(said(plain(&one, None, Misfit::Count(0))), "size() takes 1 argument, not 0");
        assert_eq!(said(plain(&span, Some("Batch"), Misfit::Count(4))), "Batch.between() takes 1 to 3 arguments, not 4");
        assert_eq!(said(plain(&many, None, Misfit::Count(0))), "log() takes at least 1 argument, not 0");
        assert_eq!(said(plain(&one, None, Misfit::Unexpected("unit"))), "size() takes no keyword unit");
        assert_eq!(said(plain(&span, None, Misfit::Twice("low"))), "between() takes low only once");
        assert_eq!(said(plain(&span, None, Misfit::Missing(0))), "between() needs low");
        assert_eq!(said(plain(&span, None, Misfit::Missing(1))), "between() needs argument 2");
    }

    #[test]
    fn a_misfit_names_what_it_missed() {
        assert!(matches!(bound(&[Val::int(1)], &[("x", Val::int(0))]), Err(Misfit::Unexpected("x"))));
        assert!(matches!(bound(&[Val::int(1), Val::int(2)], &[("base", Val::int(0))]), Err(Misfit::Twice("base"))));
        assert!(matches!(bound(&[], &[("base", Val::int(0))]), Err(Misfit::Missing(0))));
        assert!(matches!(bound(&[Val::int(1); 4], &[]), Err(Misfit::Count(4))));
    }
}
