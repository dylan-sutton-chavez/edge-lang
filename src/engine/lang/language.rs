use alloc::{string::String, vec, vec::Vec};

use crate::engine::abi::ErrorKind;
use crate::engine::syntax::chunk::SSAChunk;
use crate::engine::value::foreign::Kind;
use crate::engine::value::{Cause, HeapPool, Val, VmErr, cold_runtime};
use crate::engine::vm::VM;
use super::{Access, ArityFn, CallRule, CallShape, Cx, Descriptor, EngineError, FormatFn, Front, GetRule, Key, MatchFn, Misfit, Mode, Name, Names, Native, Natives, Op, OpRule, OpSet, OpTable, OwnTable, Overflow, Policy, RenderFn, WordsFn};

/* everything a runtime gives the engine as one type the loop is generic over */
pub trait Language: Sized + 'static {
    const VERSION: u32;
    // the rules the loop folds as constants
    const POLICY: Policy;
    // the static tables `natives!` builds with no work at boot
    const NATIVES: &'static Natives;
    // what the runtime calls the engine's kinds errors and conventions
    const NAMES: &'static Names;
    // how sources become chunks through a token table or a front end of its own
    const FRONT: Front;
    // how the tools around the engine name the runtime's files and errors
    const DESCRIPTOR: &'static Descriptor;
    // the kind that boxes an int past the inline range
    const WIDE: Option<&'static Kind> = None;
    // the kinds natives box on first use which a restore names before any value
    const KINDS: &'static [&'static Kind] = &[];
    // the kinds of the slices and the ellipsis the engine builds itself
    const BUILT: crate::engine::value::foreign::Built = crate::engine::value::foreign::Built::PLAIN;
    // how every host shows an uncaught error where none keeps the engine's report
    const REPORT: Option<WordsFn> = None;
    // the runtime's own opcodes or the unit type for none
    type Ops: OpTable;

    /* the rule for `recv.name` the engine asks where its own lookup misses */
    #[cfg(feature = "unstable-internals")]
    fn bind_get(_cx: &mut Cx<'_, '_>, _recv: Key, _name: Name, _how: Access) -> GetRule { GetRule::Ask }

    /* the rule for calling `callee` the engine asks where it cannot call it */
    #[cfg(feature = "unstable-internals")]
    fn bind_call(_cx: &mut Cx<'_, '_>, _callee: Val, _shape: CallShape) -> CallRule { CallRule::Ask }

    /* the rule for `op` the engine asks where its kernels refuse both keys */
    #[cfg(feature = "unstable-internals")]
    fn bind_op(_cx: &mut Cx<'_, '_>, _op: Op, _a: Key, _b: Key) -> OpRule { OpRule::Kernel }

    /* `v` as text in `mode` wherever the engine shows a value */
    fn show(_cx: &mut Cx<'_, '_>, _v: Val, _mode: Mode) -> Result<String, VmErr> { Err(VmErr(Cause::Runtime("this runtime renders no values"))) }

    /* `v` under format spec `spec` where a runtime with no specs writes only plain fields */
    fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> {
        if spec.is_empty() { cx.render(v, Mode::Str) } else { Err(VmErr(Cause::Runtime("this runtime formats no values"))) }
    }

    /* whether exception `exc` is of the class or tuple of classes a handler names */
    fn handles(_heap: &HeapPool, _exc: Val, _class: Val) -> Result<bool, VmErr> { Err(cold_runtime("the runtime gives no exception matcher")) }

    /* the error of a call missing native `n` until misfits reach `message` */
    fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { super::sig::plain(n, owner, m) }

    /* the message of `e` in the runtime's words with its class from `NAMES` */
    fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { super::plain(cx, e) }

    /* the value a handler binds for `e` which a reraise throws again */
    fn raise(cx: &mut Cx<'_, '_>, e: EngineError) -> Result<Val, VmErr> { raised::<Self>(cx, e) }

    /* the name of the type of `v` wherever the engine names one and `cx.type_name` asks */
    fn type_name<'h>(cx: &'h Cx<'_, '_>, v: Val) -> &'h str { cx.plain_type(v) }

    /* the error kind a plugin sees for exception `exc` */
    fn error_kind(cx: &Cx<'_, '_>, exc: Val) -> ErrorKind { super::error::abi_of(Self::NAMES.error_named(cx.heap().instance_type(exc).unwrap_or(""))) }
}

/* the engine's exception object for `e` of the class `L` names holding `L`'s words */
pub fn raised<L: Language>(cx: &mut Cx<'_, '_>, e: EngineError) -> Result<Val, VmErr> {
    let Some(kind) = e.kind() else {
        return match e { EngineError::Raised(v) => Ok(v), _ => Err(cold_runtime("a signal reached the language")) };
    };
    let args: Vec<Val> = match e {
        // the key a map lacked rides whole as the one argument
        EngineError::MissingKey(k) => vec![k],
        // these carry their value as the one argument when they have one
        EngineError::Exit(v) | EngineError::Stop(v) | EngineError::Assert(v) => if v.is_undef() || v.is_none() { Vec::new() } else { vec![v] },
        // words a native or the runtime already wrote ride as they are
        EngineError::Of(_, text) => if text.is_empty() { Vec::new() } else { vec![cx.new_str(text)?] },
        _ => match L::message(cx, &e) { text if text.is_empty() => Vec::new(), text => vec![cx.new_str(text)?] },
    };
    cx.heap_mut().engine_error(kind, args)
}

// the loop of one language the vm enters once per nested run
pub(crate) type DriveFn = for<'v, 'm, 'c> fn(&'m mut VM<'v>, &'c SSAChunk, usize) -> Result<Val, VmErr>;
pub(crate) type MessageFn = fn(&mut Cx<'_, '_>, &EngineError) -> String;
pub(crate) type RaiseFn = fn(&mut Cx<'_, '_>, EngineError) -> Result<Val, VmErr>;
pub(crate) type TypeNameFn = for<'h> fn(&'h Cx<'_, '_>, Val) -> &'h str;
pub(crate) type ErrorKindFn = fn(&Cx<'_, '_>, Val) -> ErrorKind;
pub(crate) type BindGetFn = fn(&mut Cx<'_, '_>, Key, Name, Access) -> GetRule;
pub(crate) type BindCallFn = fn(&mut Cx<'_, '_>, Val, CallShape) -> CallRule;
pub(crate) type BindOpFn = fn(&mut Cx<'_, '_>, Op, Key, Key) -> OpRule;

/* one language erased to the tables and calls the engine reads outside the loop */
pub struct Runtime {
    pub(crate) natives: &'static Natives,
    pub(crate) descriptor: &'static Descriptor,
    // the front end code compiled while a program runs goes through where none compiles nothing
    pub(crate) front: Option<Front>,
    pub(crate) wide: Option<&'static Kind>,
    pub(crate) kinds: &'static [&'static Kind],
    pub(crate) report: Option<WordsFn>,
    pub(crate) name: &'static str,
    // each operator as its front end's table spells it which its words name
    pub(crate) spellings: [&'static str; super::SPELLED],
    // what a blob it saves records of its name version natives opcodes and rules
    pub(crate) identity: u64,
    // its own opcodes as the slow paths and the words read them
    pub(crate) ops: &'static OpSet,
    // whether it runs the engine's own loop which a nested run enters directly
    pub(crate) engine_loop: bool,
    pub(crate) drive: DriveFn,
    pub(crate) show: RenderFn,
    pub(crate) format: FormatFn,
    pub(crate) handles: MatchFn,
    pub(crate) misfit: ArityFn,
    pub(crate) message: MessageFn,
    pub(crate) raise: RaiseFn,
    pub(crate) type_name: TypeNameFn,
    pub(crate) error_kind: ErrorKindFn,
    pub(crate) bind_get: BindGetFn,
    pub(crate) bind_call: BindCallFn,
    pub(crate) bind_op: BindOpFn,
}

// what a host reads of a runtime it boots
impl Runtime {
    pub const fn natives(&self) -> &'static Natives { self.natives }
    pub const fn descriptor(&self) -> &'static Descriptor { self.descriptor }
    pub const fn name(&self) -> &'static str { self.name }
    internal! { tests
        pub const fn identity(&self) -> u64 { self.identity }
    }
    internal! { tests
        pub const fn ops(&self) -> &'static OpSet { self.ops }
    }
}

impl Runtime {
    /* the runtime of `L` with every rule it states checked as the build evaluates it */
    pub const fn of<L: Language>() -> Runtime {
        // an int past the inline range goes to the wide kind so it decides overflow
        assert!(matches!(L::POLICY.int_overflow, Overflow::Promote) == L::WIDE.is_some(), "a policy promotes ints exactly when its language names a wide kind");
        L::POLICY.refuse_unread();
        // the slow paths and folder read the natives' rules and the loop the language's
        assert!(L::NATIVES.policy.same(&L::POLICY) && L::FRONT.policy.same(&L::POLICY), "a language builds its natives and folds its constants under the rules it states");
        assert!(<L::Ops as OwnTable>::SET.ops.len() == <L::Ops as OwnTable>::COUNT as usize && <L::Ops as OwnTable>::SET.hash == <L::Ops as OwnTable>::HASH, "a runtime's own opcodes are the table its type names");
        // the value a spelled ellipsis makes takes the kind the language names for it
        assert!(L::BUILT.ellipsis.is_some() || !matches!(L::FRONT.syntax, Some(s) if s.spells(crate::engine::syntax::lexer::TokenType::Ellipsis)), "a language whose syntax spells the ellipsis names its kind in BUILT");
        // a language keeping the engine's loop rules runs the one loop the engine builds
        let engine_loop = L::POLICY.loops_as(&Policy::PLAIN) && L::Ops::COUNT == 0;
        Runtime {
            natives: L::NATIVES,
            descriptor: &Described::<L>::OF,
            front: Some(L::FRONT),
            wide: L::WIDE,
            kinds: L::KINDS,
            report: L::REPORT,
            name: L::DESCRIPTOR.language,
            spellings: match L::FRONT.syntax { Some(s) => super::spellings(&s), None => [""; super::SPELLED] },
            identity: identity::<L>(),
            ops: <L::Ops as OwnTable>::SET,
            engine_loop,
            drive: if engine_loop { crate::engine::vm::driven::<crate::engine::lang::Bare> } else { crate::engine::vm::driven::<L> },
            show: L::show,
            format: L::format,
            handles: L::handles,
            misfit: L::misfit,
            message: L::message,
            raise: L::raise,
            type_name: L::type_name,
            error_kind: L::error_kind,
            #[cfg(feature = "unstable-internals")]
            bind_get: L::bind_get,
            #[cfg(feature = "unstable-internals")]
            bind_call: L::bind_call,
            #[cfg(feature = "unstable-internals")]
            bind_op: L::bind_op,
            #[cfg(not(feature = "unstable-internals"))]
            bind_get: ask_get,
            #[cfg(not(feature = "unstable-internals"))]
            bind_call: ask_call,
            #[cfg(not(feature = "unstable-internals"))]
            bind_op: kernel_op,
        }
    }

    /* a machine with no runtime which names nothing and compiles nothing while it runs */
    #[cfg(feature = "unstable-internals")]
    pub(crate) const EMPTY: Runtime = Runtime { front: None, ..Runtime::of::<Bare>() };

    /* the rules this runtime states */
    internal! { tests
        #[inline(always)]
        pub fn policy(&self) -> &'static Policy { &self.natives.policy }
    }

    /* what this runtime calls the engine's kinds errors and conventions */
    internal! { tests
        #[inline(always)]
        pub fn names(&self) -> &'static Names { self.natives.names() }
    }
}

/* the descriptor of `L` with the extension its front reads and the error classes its names give */
struct Described<L>(core::marker::PhantomData<L>);

impl<L: Language> Described<L> {
    const OF: Descriptor = L::DESCRIPTOR.derived(L::FRONT.extension, L::NAMES);
}

/* the rules a runtime gives where the binding hooks stay the engine's own */
#[cfg(not(feature = "unstable-internals"))]
fn ask_get(_cx: &mut Cx<'_, '_>, _recv: Key, _name: Name, _how: Access) -> GetRule { GetRule::Ask }

#[cfg(not(feature = "unstable-internals"))]
fn ask_call(_cx: &mut Cx<'_, '_>, _callee: Val, _shape: CallShape) -> CallRule { CallRule::Ask }

#[cfg(not(feature = "unstable-internals"))]
fn kernel_op(_cx: &mut Cx<'_, '_>, _op: Op, _a: Key, _b: Key) -> OpRule { OpRule::Kernel }

/* the identity of `L` from its name version natives digest both opcode tables and rules */
const fn identity<L: Language>() -> u64 {
    use crate::engine::code::ops::{FNV, fnv};
    let mut h = fnv(FNV, L::DESCRIPTOR.language.as_bytes());
    h = fnv(h, &[0xFF]);
    h = fnv(h, &L::VERSION.to_le_bytes());
    h = fnv(h, &L::NATIVES.digest.to_le_bytes());
    h = fnv(h, &[<L::Ops as OwnTable>::COUNT]);
    h = fnv(h, &<L::Ops as OwnTable>::HASH.to_le_bytes());
    // the engine's table too so a host refuses a blob of other opcodes before parsing
    h = fnv(h, &crate::engine::code::OpCode::HASH.to_le_bytes());
    fnv(h, &L::POLICY.bytes())
}

/* the language of a machine booted with no runtime whose loop every engine policy shares */
pub(crate) struct Bare;

// no natives built under the rules the bare machine states
static BARE_NATIVES: Natives = Natives::EMPTY.under(<Bare as Language>::POLICY);

impl Language for Bare {
    const VERSION: u32 = 0;
    // no wide kind so an int past the inline range raises
    const POLICY: Policy = Policy { int_overflow: Overflow::Error, ..Policy::PLAIN };
    const NATIVES: &'static Natives = &BARE_NATIVES;
    const NAMES: &'static Names = &Names::PLAIN;
    const FRONT: Front = Front { policy: &<Bare as Language>::POLICY, ..Front::NONE };
    const DESCRIPTOR: &'static Descriptor = &Descriptor::PLAIN;
    type Ops = ();
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::engine::lang::{ErrKind, KindId};

    // a runtime with error classes of its own and a front reading files of its own extension
    struct Named;

    static NAMED_NAMES: Names = Names::PLAIN.errors(&[(ErrKind::Type, "WrongType"), (ErrKind::Permission, "Refused")]);
    static NAMED_NATIVES: Natives = Natives::EMPTY.under(<Named as Language>::POLICY);
    static NAMED: Runtime = Runtime::of::<Named>();

    impl Language for Named {
        const VERSION: u32 = 1;
        const POLICY: Policy = Policy { int_overflow: Overflow::Error, ..Policy::PLAIN };
        const NATIVES: &'static Natives = &NAMED_NATIVES;
        const NAMES: &'static Names = &NAMED_NAMES;
        const FRONT: Front = Front { extension: "nm", policy: &<Named as Language>::POLICY, ..Front::NONE };
        const DESCRIPTOR: &'static Descriptor = &Descriptor::PLAIN.language("named").failed_error("Broken");
        type Ops = ();
    }

    #[test]
    fn a_descriptor_takes_its_extension_from_the_front_and_its_classes_from_the_names() {
        let d = NAMED.descriptor();
        assert_eq!((d.language, d.extension), ("named", "nm"));
        assert_eq!((d.type_error, d.value_error, d.runtime_error, d.denied_error, d.failed_error), ("WrongType", "Value", "Runtime", "Refused", "Broken"));
    }

    #[test]
    fn names_a_runtime_leaves_out_keep_the_plain_ones() {
        let names = Names::PLAIN.kinds(&[(KindId::Str, "text")]).errors(&[(ErrKind::Value, "BadValue")]).iters(&["walk"]);
        assert_eq!((names.kind(KindId::Str), names.kind(KindId::List)), ("text", "List"));
        assert_eq!((names.error(ErrKind::Value), names.error(ErrKind::Type)), ("BadValue", "Type"));
        assert_eq!((names.iters[0], names.iters[1]), ("walk", "RangeIter"));
    }
}
