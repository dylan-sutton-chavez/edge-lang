#![allow(unused_imports, dead_code, clippy::let_underscore_untyped)]
// every path railsbit names held at its 1.0 place so a rename fails to compile here
use interpreter::lexer::{index, operators, Binding, Blocks, Comments, Cond, Continuation, Conversion, Dot, Ends, Escapes, Interp, Keywords, Lambda, Numbers, Quote, Shapes, Spelling, Strings, Suffix, Syntax, TokenType, Unicode, Unknown, Zeros};
use interpreter::lexer::grammar::{Emit, Form, Grammar, Pattern, Reading, Row, Slot};
use interpreter::code::{constant_fold, Parsed, SSAChunk};
use interpreter::lang::{Args, Cmp, Cx, Descriptor, ErrKind, Falsy, Fields, Front, IntDiv, IntMod, KindId, Language, MapIter, Names, Native, Natives, NegPow, NumEq, Op, Overflow, Policy, Ret, Runtime, Show, Sig, StrIter, Tree, ZeroDiv};
use interpreter::value::{as_i128, HeapPool, IterFrame};
use interpreter::value::foreign::{BinOp, Kind, KindToken, Snapshot, Store, Transfer, UnOp};
use interpreter::{Diagnostic, Limits, Lines, NativeBinding, NativeClass, NoopResolver, Resolved, Resolver, SchedulerStatus, Val, VmErr, VM};
use interpreter::snapshot::{entry_of, limits_of, restore, save, SnapErr};
use interpreter::abi::WireValue;
use interpreter::lib::{Kernel, Lang, Site};
use interpreter::lib::fail::{english, raise, At, Class, Error, Fail};
use interpreter::lib::fmt::{format, show};
use interpreter::lib::types::{handles, is_a, type_of};
use interpreter::lib::num::int_kind;
use interpreter::lib::data::json::{parse, read, write, write_into};
use interpreter::lib::uni::{Decimal, Digit};
use interpreter::lib::seq::{extend, len, list, pop, push, sorted, tuple};
use interpreter::lib::iter::{backward, numbered, range, zipped};
use interpreter::lib::iter::fold::{every, most, some};
use interpreter::lib::text::text;
use interpreter::lib::text::find::{ends, starts};
use interpreter::lib::text::cut::{partition, rpartition, split};
use interpreter::lib::text::edit::{drop_prefix, drop_suffix, join, replace};
use interpreter::lib::text::pad::{trim, trim_start};
use interpreter::lib::text::class::is_digit;
use interpreter::lib::text::case::{lower, Full};
use interpreter::lib::num::int::new as int_new;
use interpreter::lib::map::{get, items, keys, new as map_new, set_default, update, values};
use interpreter::lib::set::{add, new as set_new};
use interpreter::lib::io::print;
use interpreter::{allocator, natives};
use interpreter::lib::{hooks, table};

pub(crate) fn limits() -> Limits { Limits::sandbox().ops(100_000).memory(usize::MAX) }

static TREE: Tree = Tree::of(&[("Exception", ""), ("TypeError", "Exception")]);
const KEYWORDS: Keywords = index(&[("if", TokenType::If)]);

// the names a runtime chains from its base with error and kind pairs
const NAMES: Names = Names::PLAIN
    .kinds(&[(KindId::Int, "int"), (KindId::Str, "str")])
    .errors(&[(ErrKind::Type, "TypeError"), (ErrKind::Value, "ValueError"), (ErrKind::Runtime, "RuntimeError")])
    .iters(&["iterator"])
    .fields(Fields::PLAIN.args("args"))
    .nil("nil")
    .anonymous("<fn>")
    .module_body("<module>")
    .tree(&TREE);

const POLICY: Policy = Policy::PLAIN
    .int_overflow(Overflow::Promote)
    .int_div(IntDiv::ToFloat)
    .int_mod(IntMod::Floored)
    .zero_div(ZeroDiv::Error)
    .neg_pow(NegPow::ToFloat)
    .numeric_eq(NumEq::Unified)
    .bool_is_int(true)
    .falsy(Falsy::NIL.or(Falsy::FALSE).or(Falsy::ZERO).or(Falsy::NAN).or(Falsy::EMPTY))
    .identity_first(true)
    .neg_index(true)
    .str_iter(StrIter::CodePoints)
    .map_iter(MapIter::Keys)
    .map_fields(false)
    .builtins_scope(true)
    .receiver_slot(true)
    .recursion_limit(1000)
    .max_int_digits(None)
    .open_fields(true)
    .shared_bound(false)
    .open_order(true);

// the descriptor keeps only what the front and the names do not give
const DESCRIPTOR: Descriptor = Descriptor::PLAIN
    .language("vfy")
    .entries(&["main.vfy"])
    .test_suffix("_test.vfy")
    .scaffold("")
    .example_fence("vfy")
    .manifest_fence("vfy-manifest")
    .failed_error("Failed")
    .statuses((65, 70))
    .diagnosed(None);

static ROWS: [Row; 6] = [
    Row::new(TokenType::Plus, 10, 11, Emit::Op(Op::Bin(BinOp::Add))),
    Row::new(TokenType::Slash, 20, 21, Emit::Op(Op::Bin(BinOp::TrueDiv))),
    Row::new(TokenType::EqualEqual, 5, 6, Emit::Op(Op::Cmp(Cmp::Eq))),
    Row::new(TokenType::NotIn, 5, 6, Emit::Op(Op::NotIn)),
    Row::new(TokenType::Not, 0, 30, Emit::Op(Op::Not)),
    Row::new(TokenType::Minus, 0, 30, Emit::Op(Op::Unary(UnOp::Neg))),
];
static READINGS: [Reading; 1] = [Reading::new(&ROWS, &[])];
static FORMS: [Form; 1] = [Form::new("if", &[Slot::Expr(0), Slot::Name], "")];
static GRAMMAR: Grammar = Grammar::PLAIN.readings(&READINGS).forms(&FORMS).patterns(&[]).modifiers(&[]).unmatched("").inner(0).tail(0).lazy(&[]);

// a token table with every setter railsbit spells
const SYNTAX: Syntax = Syntax::PLAIN
    .name("vfy")
    .extension("vfy")
    .words(&[])
    .symbols(&[])
    .keywords(&KEYWORDS)
    .precedence(&[])
    .chained(true)
    .blocks(Blocks::Indent)
    .ends(Ends::Newline)
    .comments(Comments::PLAIN.line(&["#"]).block(None).nested(false))
    .strings(Strings::PLAIN.quotes(&[Quote::PLAIN.byte(b'"').multiline(false).triple(false)]).prefixes(&[]).any_case(false).hashes(false).adjacent(false).newlines(false).escapes(Escapes::PLAIN.simple(&[]).hex(true).unicode(Unicode::Fixed).octal(false).line_join(false).unknown(Unknown::Refuse).refused(&[])))
    .interp(None)
    .numbers(Numbers::PLAIN.radixes(&[]).separator(None).zeros(Zeros::Refused).leading_dot(false).trailing_dot(false).exponent(true).suffix(Suffix::Refused))
    .continuation(Continuation::PLAIN.join(None).operator(false))
    .shapes(Shapes::PLAIN.cond(Cond::None).lambda(Lambda::None).dot(Dot::Key).binding(Binding::None).header_parens(false).comprehensions(false).decorators(false).named(false).arms(false).ranges(false).slices(true).splat(true).grammar(Some(&GRAMMAR)))
    .soft(&[])
    .bom(false);

// the blocks a table of words delimits as prep writes them
const WORDS: Blocks = Blocks::Words { implicit: true };

static KIND: Kind = Kind::PLAIN.name("handle").type_name("handle").family("handle").transfer(Transfer::Deny).snapshot(Snapshot::Reject);

// the host side of an embedding through the opaque values and the heap
fn host(vm: &mut VM<'_>, front: &Front, src: &str) -> Result<Val, VmErr> {
    let chunk: Result<SSAChunk, Vec<Diagnostic>> = front.compile_with(src, Box::new(NoopResolver));
    let _ = front.compile(src);
    let _ = Diagnostic::render_all(&[], src, None);
    let _ = Lines::new(src).line_col(0);
    let _: Option<(usize, usize)> = vm.error_line_col();
    let _ = (vm.error_pos(), vm.report_error(&VmErr::raised("x"), src, None));
    let heap = vm.heap_mut();
    let s = heap.new_str("text".into())?;
    let l = heap.new_list(vec![s])?;
    let t = heap.new_tuple(vec![l])?;
    let _ = heap.int(1 << 80)?;
    let _ = (heap.text(s), heap.items(t), as_i128(s, heap));
    let token: Option<KindToken> = heap.kind_token(&KIND);
    let token = match token { Some(t) => t, None => heap.register_kind(&KIND)? };
    let h = heap.foreign_new(token, Store::Words([1, 0, 0]))?;
    let _ = (heap.foreign_payload::<[u64; 3]>(token, h), heap.foreign_kind(h));
    heap.set_data(1u8);
    let _ = heap.data::<u8>().is_some();
    let _ = heap.take_data::<u8>();
    let _ = (Val::none(), Val::bool(true), Val::int(1), Val::float(1.0), Val::none().is_none(), Val::bool(true).as_bool(), s.is_heap(), s.as_heap(), h.is_foreign(), h.as_foreign());
    let _ = IterFrame::seq(vec![s].into());
    let e = VmErr::new(ErrKind::Type, "bad");
    let _ = (e.kind(), e.is_limit(), e.paused() == Some(SchedulerStatus::PendingHostCall), VmErr::deferred().is_deferred());
    let _ = chunk;
    vm.run()
}

// a module resolved by constructors and a binding by its constructor
fn modules() -> (Resolved, Resolved, NativeBinding, NativeClass) {
    let bind = NativeBinding::from_fn("f", |_, _, _| Ok(Val::none()));
    (Resolved::code("x = 1", "m.vfy"), Resolved::native("m", vec![bind.clone()]).classes(vec![]).consts(vec![]), bind.clone(), NativeClass::new("C", vec![bind]))
}

// the snapshot calls and their opaque error
fn snapshots(vm: &mut VM<'_>, blob: &[u8]) -> Result<(), SnapErr> {
    let _ = (entry_of(blob)?, limits_of(blob)?);
    let _ = save(vm, "main", "")?;
    restore(vm, blob)
}

// a native in a runtime's own kernels and a failure worded by the lib
fn native<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let _ = (cx.text(a.pos[0]), cx.heap(), Native::new("n", Sig::exact(1), |_, _| Ok(Val::none().into())), Kernel::new(Sig::exact(0), |_, _| Ok(Val::none().into())));
    let _ = (S::NAME, NAMES.error(ErrKind::Type), NAMES.kind(KindId::Int));
    Err(Error::Fail(Fail::Exit(Val::none())).into_vm::<S>(cx))
}

// the lib hook bodies a runtime names or writes through hooks
fn lib_hooks<L: Lang>(cx: &mut Cx<'_, '_>, v: Val, heap: &HeapPool) -> Result<String, VmErr> {
    let _ = handles::<L>(heap, v, v)?;
    let _ = format::<L>(cx, v, "")?;
    let _ = (raise::<L>(Class::Value, "x"), english::<L>(cx, At::NONE, Fail::Exit(v)));
    let _ = int_kind::<L>;
    show::<L>(cx, v, Show::Repr)
}

// the matches a runtime writes over failures and opaque errors
fn words(f: &Fail) -> usize {
    match f { Fail::BadJson { at, .. } => *at, _ => 0 }
}

fn fold(chunk: &mut SSAChunk) { constant_fold(chunk, &POLICY) }

// a runtime spelled the way railsbit writes one with its kernels named by path
mod vfy {
    use interpreter::lang::{Descriptor, Front, Language, Names, Natives, Policy, Runtime};
    use interpreter::lexer::{Spelling, Syntax};
    use interpreter::lib::{data, io, iter, map, num, seq, set, text, types, Lang};
    use interpreter::lib::text::case::Full;
    use interpreter::lib::uni::{Decimal, Digit};
    use interpreter::value::foreign::Kind;

    pub(crate) struct Vfy;

    const KERNELS: &[interpreter::lang::Native] = &interpreter::lib::table![Vfy;
        "print" => io::print{"sep", "end"},
        "len" => seq::len,
        "list" => seq::list,
        "tuple" => seq::tuple,
        "sorted" => seq::sorted{"key", "reverse"},
        "range" => iter::range,
        "reversed" => iter::backward,
        "enumerate" => iter::numbered("iterable", "start"),
        "zip" => iter::zipped,
        "max" => iter::fold::most{"key", "default"},
        "any" => iter::fold::some,
        "all" => iter::fold::every,
        "str" => text::text,
        "int" => num::int::new[Decimal],
        "set" => set::new,
        "dict" => map::new,
        "isinstance" => types::is_a,
        "type" => types::type_of,
        "json" => data::json::read
    ];
    const STRS: &[interpreter::lang::Native] = &interpreter::lib::table![Vfy;
        "startswith" => text::find::starts,
        "split" => text::cut::split,
        "isdigit" => text::class::is_digit[Digit],
        "lower" => text::case::lower[Full]
    ];
    const LISTS: &[interpreter::lang::Native] = &interpreter::lib::table![Vfy; "append" => seq::push, "extend" => seq::extend, "pop" => seq::pop];
    const DICTS: &[interpreter::lang::Native] = &interpreter::lib::table![Vfy; "get" => map::get, "keys" => map::keys];
    const SETS: &[interpreter::lang::Native] = &interpreter::lib::table![Vfy; "add" => set::add];

    pub(crate) static NATIVES: Natives = interpreter::natives! {
        language: Vfy,
        fns: [KERNELS],
        methods: [Str => [STRS], List => [LISTS], Dict => [DICTS], Set => [SETS]],
        types: &[],
    };
    static WIDE: Kind = interpreter::lib::num::int_kind::<Vfy>("int");
    pub(crate) static RUNTIME: Runtime = Runtime::of::<Vfy>();

    impl Spelling for Vfy {
        const SYNTAX: Syntax = super::SYNTAX;
        const NATIVES: &'static Natives = &NATIVES;
    }

    impl Language for Vfy {
        const VERSION: u32 = 1;
        const POLICY: Policy = super::POLICY;
        const NATIVES: &'static Natives = &NATIVES;
        const NAMES: &'static Names = &super::NAMES;
        const FRONT: Front = Front::spelled::<Vfy>();
        const DESCRIPTOR: &'static Descriptor = &super::DESCRIPTOR;
        const WIDE: Option<&'static Kind> = Some(&WIDE);
        type Ops = ();

        interpreter::lib::hooks!(Vfy);
    }

    // a runtime that writes its own lib setup holds no tree and no style
    impl Lang for Vfy {}
}

#[cfg(feature = "std")]
fn ready(rt: &'static Runtime) -> std::process::ExitCode { interpreter::native::main(rt, limits()) }

// the kit a command line is composed from with the grants of perms
#[cfg(all(feature = "cli-run", feature = "cli-perms"))]
mod cli {
    use std::sync::Arc;
    use interpreter::cli::{anyhow, clap, serde_json, Command, Engine, Field, Kit, Perms, DOCS_FIELD, MANIFEST_FIELDS};
    use interpreter::cli::cmd::Run;
    use interpreter::cli::host::{is_plain_path, Calls, Fault, Held, HostModule, Reply, Serve};
    use interpreter::abi::WireValue;

    fn compose(rt: &'static interpreter::lang::Runtime) -> Kit {
        Kit::new("vfy", "0.1.0", Engine::native(rt)).title("vfy").command(Run).limits(super::limits()).access(Arc::new(Perms))
    }

    fn engine(kit: &Kit) -> &Engine { kit.engine() }

    fn faults(held: &Held, module: &dyn HostModule) -> (Fault, Fault, bool) {
        let _ = (held.scopes(), held.need("read"), held.reach(module, "a/b"));
        (Fault::invalid("x"), Fault::failed("y"), is_plain_path("a/b"))
    }

    fn wire() -> [WireValue; 3] { [WireValue::None, WireValue::Bool(true), WireValue::Int(1)] }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_runtime_written_as_railsbit_writes_one_boots_with_its_names() {
        assert_eq!(vfy::RUNTIME.name(), "vfy");
        assert_eq!(<vfy::Vfy as Language>::FRONT.extension(), "vfy");
        assert_eq!(NAMES.error(ErrKind::Type), "TypeError");
        assert_eq!(NAMES.error(ErrKind::Attribute), Names::PLAIN.error(ErrKind::Attribute));
        assert_eq!(NAMES.kind(KindId::Int), "int");
    }
}
