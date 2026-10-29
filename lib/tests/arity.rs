use edge::lang::{Misfit, Native, Sig};
use edge::value::VmErr;
use mini::words::arity;

fn native(name: &'static str, sig: Sig) -> Native { Native::new(name, sig, |_, _| Ok(edge::value::Val::none().into())) }

fn words(e: VmErr) -> String { e.to_string() }

#[test]
fn a_function_counts_as_the_engine_did() {
    assert!(words(arity(&native("len", Sig::exact(1)), None, Misfit::Count(0))).contains("len() takes exactly one argument (0 given)"));
    assert!(words(arity(&native("divmod", Sig::exact(2)), None, Misfit::Count(1))).contains("divmod expected 2 arguments, got 1"));
    assert!(words(arity(&native("min", Sig::var(1)), None, Misfit::Count(0))).contains("min expected at least 1 argument, got 0"));
    assert!(words(arity(&native("len", Sig::exact(1)), None, Misfit::Unexpected("x"))).contains("len() takes no keyword arguments"));
    let print = native("print", Sig::var(0).kw(&["sep", "end", "file", "flush"]));
    assert!(words(arity(&print, None, Misfit::Unexpected("bad"))).contains("print() got an unexpected keyword argument 'bad'"));
}

#[test]
fn a_method_counts_as_the_engine_did() {
    assert!(words(arity(&native("append", Sig::exact(1)), Some("list"), Misfit::Count(2))).contains("list.append() takes exactly one argument (2 given)"));
    assert!(words(arity(&native("insert", Sig::exact(2)), Some("list"), Misfit::Count(1))).contains("insert expected 2 arguments, got 1"));
    assert!(words(arity(&native("index", Sig::range(1, 3)), Some("list"), Misfit::Count(0))).contains("index expected at least 1 argument, got 0"));
    assert!(words(arity(&native("replace", Sig::range(2, 3)), Some("str"), Misfit::Count(1))).contains("replace() takes at least 2 positional arguments (1 given)"));
    assert!(words(arity(&native("upper", Sig::exact(0)), Some("str"), Misfit::Unexpected("x"))).contains("str.upper() takes no keyword arguments"));
    let split = native("split", Sig::range(0, 2).named(&["sep", "maxsplit"]));
    assert!(words(arity(&split, Some("str"), Misfit::Unexpected("sepp"))).contains("Did you mean 'sep'?"));
}
