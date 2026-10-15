use super::prelude::*;
use crate::s;
use crate::parser::{SSAChunk, types::OpCode};
use crate::vm::cache::OpcodeCache;
use crate::vm::opcodes::dunder::{binary_dunder_names, compare_dunder_names};

const BINARY: [OpCode; 13] = [OpCode::Add, OpCode::Sub, OpCode::Mul, OpCode::Div, OpCode::FloorDiv, OpCode::MatMul, OpCode::Mod, OpCode::Pow, OpCode::BitAnd, OpCode::BitOr, OpCode::BitXor, OpCode::Shl, OpCode::Shr];
const COMPARE: [OpCode; 6] = [OpCode::Eq, OpCode::NotEq, OpCode::Lt, OpCode::LtEq, OpCode::Gt, OpCode::GtEq];

/* The dunders a builtin value of type `ty` answers through its operator, by the family of the type. */
pub(crate) fn applies(ty: &str, name: &str) -> bool {
    const ANY: &[&str] = &["__eq__", "__ne__", "__repr__", "__str__", "__format__"];
    const ORDER: &[&str] = &["__lt__", "__le__", "__gt__", "__ge__"];
    const ARITH: &[&str] = &["__add__", "__radd__", "__sub__", "__rsub__", "__mul__", "__rmul__", "__truediv__", "__rtruediv__", "__floordiv__", "__rfloordiv__",
        "__mod__", "__rmod__", "__pow__", "__rpow__", "__divmod__", "__rdivmod__", "__neg__", "__pos__", "__abs__", "__bool__", "__int__", "__float__",
        "__trunc__", "__floor__", "__ceil__", "__round__", "conjugate", "__hash__"];
    const BITS: &[&str] = &["__and__", "__rand__", "__or__", "__ror__", "__xor__", "__rxor__", "__lshift__", "__rlshift__", "__rshift__", "__rrshift__", "__invert__", "__index__"];
    const SEQ: &[&str] = &["__add__", "__mul__", "__rmul__", "__contains__", "__getitem__", "__len__", "__iter__"];
    const SETS: &[&str] = &["__contains__", "__len__", "__iter__", "__or__", "__ror__", "__and__", "__rand__", "__xor__", "__rxor__", "__sub__", "__rsub__"];
    let has = |list: &[&str]| list.contains(&name);
    has(ANY) || match ty {
        "int" | "bool" => has(ARITH) || has(BITS) || has(ORDER),
        "float" => has(ARITH) || has(ORDER),
        "str" | "bytes" => has(SEQ) || has(ORDER) || matches!(name, "__mod__" | "__rmod__" | "__hash__"),
        "tuple" => has(SEQ) || has(ORDER) || name == "__hash__",
        "list" => has(SEQ) || has(ORDER),
        "dict" => matches!(name, "__contains__" | "__getitem__" | "__len__" | "__iter__" | "__or__" | "__ror__"),
        "set" => has(SETS) || has(ORDER),
        "frozenset" => has(SETS) || has(ORDER) || name == "__hash__",
        _ => name == "__hash__",
    }
}

/* Whether `b` is an operand the dunder of `a` takes, any other answers NotImplemented. */
fn takes(vm: &VM, a: Val, b: Val, name: &str) -> bool {
    let (ta, tb) = (vm.type_name(a), vm.type_name(b));
    let int = |t: &str| matches!(t, "int" | "bool");
    let set = |t: &str| matches!(t, "set" | "frozenset");
    // An int takes ints, a float takes ints and floats, and a sequence joins or repeats by its own rules.
    ta == tb || (int(ta) && int(tb)) || (ta == "float" && int(tb)) || (set(ta) && set(tb)) || concat(ta, name)
        || (matches!(ta, "str" | "bytes") && name == "__mod__")
        || matches!(name, "__contains__" | "__getitem__" | "__format__")
}

/* Whether `name` joins or repeats a sequence of type `ty`, which raises on a wrong operand instead of NotImplemented. */
fn concat(ty: &str, name: &str) -> bool {
    matches!(ty, "str" | "bytes" | "list" | "tuple") && matches!(name, "__add__" | "__mul__" | "__rmul__")
}

/* The number attributes `real`, `imag`, `numerator` and `denominator` of an int, a bool or a float. */
pub(crate) fn number_attr(vm: &VM, v: Val, name: &str) -> Option<Val> {
    if v.is_float() { return match name { "real" => Some(v), "imag" => Some(Val::float(0.0)), _ => None }; }
    let int = if v.is_bool() { Val::int(v.as_bool() as i64) } else if vm.as_i128(v).is_some() { v } else { return None };
    match name { "real" | "numerator" => Some(int), "imag" => Some(Val::int(0)), "denominator" => Some(Val::int(1)), _ => None }
}

/* Runs dunder `name` of the builtin value `recv` with `args`, through the operator or builtin it stands for. */
pub(crate) fn call(vm: &mut VM, recv: Val, name: &str, args: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
    let arg = args.first().copied();
    if let Some(b) = arg && !takes(vm, recv, b, name) {
        let ni = vm.builtin_value("NotImplemented")?;
        vm.push(ni);
        return Ok(());
    }
    // A sequence repeats only by an int.
    if let Some(b) = arg && matches!(name, "__mul__" | "__rmul__") && concat(vm.type_name(recv), name) && !matches!(vm.type_name(b), "int" | "bool") {
        return Err(VmErr::TypeMsg(s!("'", str vm.type_name(b), "' object cannot be interpreted as an integer")));
    }
    let cache = OpcodeCache::new(0);
    let operator = |op: OpCode| {
        let (f, r) = binary_dunder_names(op).or_else(|| compare_dunder_names(op)).unwrap_or(("", ""));
        (f == name).then_some((op, false)).or_else(|| (r == name && binary_dunder_names(op).is_some()).then_some((op, true)))
    };
    let run = |vm: &mut VM, op: OpCode, a: Val, b: Option<Val>| -> Result<(), VmErr> {
        vm.push(a);
        vm.stack.extend(b);
        match op {
            _ if COMPARE.contains(&op) => vm.handle_compare(op, 0, &cache, chunk),
            OpCode::BitAnd | OpCode::BitOr | OpCode::BitXor | OpCode::Shl | OpCode::Shr | OpCode::BitNot => vm.handle_bitwise(op, 0, chunk),
            OpCode::MatMul => vm.handle_matmul(0, chunk),
            _ => vm.handle_arith(op, 0, 0, &cache, chunk),
        }
    };
    let found = BINARY.iter().chain(COMPARE.iter()).find_map(|&op| operator(op));
    let result = match (found, arg) {
        (Some((OpCode::Pow, false)), Some(b)) if args.len() == 2 => builtin(vm, "pow", &[recv, b, args[1]], chunk),
        (Some((op, false)), Some(b)) => run(vm, op, recv, Some(b)),
        (Some((op, true)), Some(b)) => run(vm, op, b, Some(recv)),
        _ => match name {
            "__neg__" => run(vm, OpCode::Minus, recv, None),
            "__pos__" => run(vm, OpCode::Pos, recv, None),
            "__invert__" => run(vm, OpCode::BitNot, recv, None),
            "__divmod__" => builtin(vm, "divmod", &[recv, args[0]], chunk),
            "__rdivmod__" => builtin(vm, "divmod", &[args[0], recv], chunk),
            "__contains__" => { let found = vm.contains_op(recv, args[0], chunk)?; vm.push(Val::bool(found)); Ok(()) }
            "__getitem__" => { vm.push(recv); vm.push(args[0]); vm.get_item(0, chunk, &cache) }
            "__floor__" | "__ceil__" if recv.is_float() => {
                let f = recv.as_float();
                let r = if name == "__floor__" { libm::floor(f) } else { libm::ceil(f) };
                builtin(vm, "int", &[Val::float(r)], chunk)
            }
            "conjugate" if recv.is_float() => { vm.push(recv); Ok(()) }
            "__trunc__" | "__floor__" | "__ceil__" | "__index__" | "conjugate" | "__int__" => builtin(vm, "int", &[recv], chunk),
            _ => {
                let callee = match name {
                    "__abs__" => "abs", "__bool__" => "bool", "__float__" => "float", "__round__" => "round", "__hash__" => "hash",
                    "__repr__" => "repr", "__str__" => "str", "__format__" => "format", "__len__" => "len", "__iter__" => "iter",
                    _ => return Err(cold_type("unsupported dunder")),
                };
                let mut all = alloc::vec![recv];
                all.extend_from_slice(args);
                builtin(vm, callee, &all, chunk)
            }
        },
    };
    match result {
        // An operand type the operator refuses answers NotImplemented, as the dunder itself would.
        Err(VmErr::Type(_) | VmErr::TypeMsg(_)) if arg.is_some() && !concat(vm.type_name(recv), name) && !matches!(name, "__getitem__" | "__contains__" | "__format__") => {
            let ni = vm.builtin_value("NotImplemented")?;
            vm.push(ni);
            Ok(())
        }
        r => r,
    }
}

fn builtin(vm: &mut VM, name: &'static str, args: &[Val], chunk: &SSAChunk) -> Result<(), VmErr> {
    let f = vm.builtin_value(name)?;
    vm.call_with(f, None, args, &[], chunk)
}
