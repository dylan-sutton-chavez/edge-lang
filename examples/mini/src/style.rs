use alloc::string::String;

use edge::lang::ErrKind;
use lib::fmt::render::{Escape, Floats, Style, hex};

use crate::{DESCRIPTOR, Mini, NAMES};

/* the ` at 0x` a text places an object by */
fn at(out: &mut String, a: u64) { out.push_str(" at "); hex(out, a); }

impl Style for Mini {
    const TRUE: &'static str = "True";
    const FALSE: &'static str = "False";
    const QUOTE: char = '\'';
    const SWITCH: Option<char> = Some('"');
    const ESCAPE: Escape = Escape::Fixed;
    const BYTES: &'static str = "b";
    const BUFFER: (&'static str, &'static str) = ("bytearray(", ")");
    const EMPTY_SET: &'static str = "set()";
    const ONE: &'static str = ",)";
    const FLOATS: Floats = Floats { nan: "nan", inf: "inf", neg_inf: "-inf", low: -3, high: 16, plus: true, width: 2, point: true };
    const AGAIN: &'static str = "...";
    const BOUND: (&'static str, &'static str, &'static str) = ("<bound method ", " of ", ">");
    const SLICE: (&'static str, &'static str, &'static str) = ("slice(", ", ", ")");
    const NUMBERED: (&'static str, &'static str, &'static str) = ("[Errno ", "] ", ": ");

    fn type_form(out: &mut String, name: &str) { out.push_str("<class '"); out.push_str(name); out.push_str("'>"); }

    fn native_form(out: &mut String, name: &str) { out.push_str("<built-in function "); out.push_str(name); out.push('>'); }

    fn method_form(out: &mut String, name: &str, kind: &str) {
        out.push_str("<method '");
        out.push_str(name);
        out.push_str("' of '");
        out.push_str(kind);
        out.push_str("' objects>");
    }

    fn bound_native_form(out: &mut String, name: &str, ty: &str, a: u64) {
        out.push_str("<built-in method ");
        out.push_str(name);
        out.push_str(" of ");
        out.push_str(ty);
        out.push_str(" object");
        at(out, a);
        out.push('>');
    }

    fn object_form(out: &mut String, ty: &str, a: u64) { out.push('<'); out.push_str(ty); out.push_str(" object"); at(out, a); out.push('>'); }

    fn module_form(out: &mut String, name: &str) { out.push_str("<module '"); out.push_str(name); out.push_str("'>"); }

    fn extern_form(out: &mut String, name: &str) { out.push_str("<extern function "); out.push_str(name); out.push('>'); }

    fn function_form(out: &mut String, name: &str, a: u64) { out.push_str("<function "); out.push_str(name); at(out, a); out.push('>'); }

    fn class_form(out: &mut String, name: &str) { out.push_str("<class '__main__."); out.push_str(name); out.push_str("'>"); }

    fn instance_form(out: &mut String, name: &str, a: u64) { out.push_str("<__main__."); out.push_str(name); out.push_str(" object"); at(out, a); out.push('>'); }

    fn coroutine_form(out: &mut String, ty: &str, name: &str, a: u64) {
        out.push('<');
        out.push_str(ty);
        out.push_str(" object ");
        out.push_str(name);
        at(out, a);
        out.push('>');
    }

    fn cell_form(out: &mut String, a: u64, held: Option<(&str, u64)>) {
        out.push_str("<cell");
        at(out, a);
        match held {
            None => out.push_str(": empty>"),
            Some((ty, h)) => { out.push_str(": "); out.push_str(ty); out.push_str(" object"); at(out, h); out.push('>'); }
        }
    }

    fn range_form(out: &mut String, start: &str, stop: &str, step: Option<&str>) {
        out.push_str("range(");
        if start == Self::AGAIN { return out.push_str("...)"); }
        out.push_str(start);
        out.push_str(", ");
        out.push_str(stop);
        if let Some(st) = step { out.push_str(", "); out.push_str(st); }
        out.push(')');
    }

    fn keyed(name: &str) -> bool { name == NAMES.error(ErrKind::Key) }

    // the class a failed host call raises and its children carry an error number
    fn numbered(name: &str) -> bool { crate::types::TREE.derives(name, DESCRIPTOR.failed_error) }
}
