use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use edge::code::{Builder, OpCode as O, Value};

use crate::parse::{Expr, Fun, Kind, Place, Program, Stmt};
use crate::Lox;

type B<'b, 's> = Builder<'b, 's, Lox>;

/* builds the script as a function the module calls once so its blocks keep locals */
pub fn lower(b: &mut B<'_, '_>, prog: &Program) {
    function(b, prog, &prog.script);
    b.call(0, 0).pop();
}

fn name(prog: &Program, place: &Place) -> String {
    match place { Place::Local(id) => prog.name(*id), Place::Global(n) => n.clone() }
}

fn function(b: &mut B<'_, '_>, prog: &Program, f: &Fun) {
    let params: Vec<String> = f.params.iter().map(|&id| prog.name(id)).collect();
    let params: Vec<&str> = params.iter().map(String::as_str).collect();
    b.closure(&f.name, &params, |b| {
        for n in &f.outer { b.nonlocal(n); }
        for s in &f.body { stmt(b, prog, f, s); }
        ret(b, prog, f, None);
    });
}

fn ret(b: &mut B<'_, '_>, prog: &Program, f: &Fun, value: Option<&Expr>) {
    match value {
        Some(e) => expr(b, prog, e),
        // an initializer hands back its receiver however it returns
        None if f.kind == Kind::Init => { b.load(&prog.name(f.params[0])); }
        None => { b.constant(Value::None); }
    }
    b.ret();
}

/* binds what `value` pushes to a declared name in a cell no earlier closure holds */
fn define(b: &mut B<'_, '_>, prog: &Program, place: &Place, value: impl FnOnce(&mut B<'_, '_>)) {
    if let Place::Local(id) = place && prog.locals[*id as usize].captured { b.fresh(&prog.name(*id)); }
    value(b);
    b.store(&name(prog, place));
}

fn stmt(b: &mut B<'_, '_>, prog: &Program, f: &Fun, s: &Stmt) {
    match s {
        Stmt::Expr(e) => { expr(b, prog, e); b.pop(); }
        Stmt::Print(e, at) => {
            b.at(*at).load("@print");
            expr(b, prog, e);
            b.at(*at).call(1, 0).pop();
        }
        Stmt::Define(place, e) => define(b, prog, place, |b| expr(b, prog, e)),
        Stmt::Fun(place, g) => define(b, prog, place, |b| function(b, prog, g)),
        Stmt::Block(body) => for s in body { stmt(b, prog, f, s) },
        Stmt::If(c, then, other) => {
            b.if_else(|b| expr(b, prog, c), |b| stmt(b, prog, f, then), |b| if let Some(o) = other { stmt(b, prog, f, o) });
        }
        Stmt::While(c, body) => { b.while_loop(|b| expr(b, prog, c), |b| stmt(b, prog, f, body)); }
        Stmt::Return(e) => ret(b, prog, f, e.as_ref()),
        Stmt::Class { place, name: cname, sup, methods } => {
            if let Some((value, id, _)) = sup { define(b, prog, &Place::Local(*id), |b| expr(b, prog, value)); }
            define(b, prog, place, |b| {
                if let Some((_, id, at)) = sup { b.at(*at).load(&prog.name(*id)); }
                b.class(cname, u16::from(sup.is_some()), |b| for m in methods {
                    function(b, prog, m);
                    b.store(&m.name);
                });
            });
        }
    }
}

fn expr(b: &mut B<'_, '_>, prog: &Program, e: &Expr) {
    match e {
        Expr::Num(n) => { b.constant(Value::Float(*n)); }
        Expr::Str(s) => { b.constant(Value::Str(s.to_string())); }
        Expr::Bool(v) => { b.constant(Value::Bool(*v)); }
        Expr::Nil => { b.constant(Value::None); }
        Expr::Get(place, at) => { b.at(*at).load(&name(prog, place)); }
        Expr::Set(place, v, at) => {
            expr(b, prog, v);
            // a global must exist before an assignment so the read raises first
            if let Place::Global(n) = place { b.at(*at).load(n).pop(); }
            b.dup().store(&name(prog, place));
        }
        Expr::Neg(v, at) => { expr(b, prog, v); b.at(*at).unary(O::Minus); }
        Expr::Not(v) => { expr(b, prog, v); b.unary(O::Not); }
        Expr::Binary(op, l, r, at) => { expr(b, prog, l); expr(b, prog, r); b.at(*at).binary(*op); }
        Expr::Logic(and, l, r) => { expr(b, prog, l); b.lazy(*and, |b| expr(b, prog, r)); }
        Expr::Call(callee, args, at) => {
            expr(b, prog, callee);
            for a in args { expr(b, prog, a); }
            b.at(*at).call(args.len() as u16, 0);
        }
        Expr::Field(obj, n, at) => { expr(b, prog, obj); b.at(*at).field(n); }
        Expr::SetField(obj, n, v, at) => {
            expr(b, prog, obj);
            expr(b, prog, v);
            b.dup().rot3().swap().at(*at).set_field(n);
        }
        Expr::Import(spec, at) => {
            let alias = format!("import@{at}");
            b.at(*at).import(spec, &alias).load(&alias);
        }
        Expr::Super { sup, this, name: n, at } => {
            b.at(*at).load("@super").load(&name(prog, sup)).load(&name(prog, this)).constant(Value::Str(n.to_string())).call(3, 0);
        }
    }
}
