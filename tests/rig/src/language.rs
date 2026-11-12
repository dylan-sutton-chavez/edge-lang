use alloc::string::String;

use edge::lang::{Cx, Descriptor, Falsy, Front, IntDiv, IntMod, Language, MapIter, Mode, Names, Natives, NegPow, NumEq, Overflow, Policy, StrIter, ZeroDiv};
use edge::value::foreign::{Built, Kind};
use edge::value::{HeapPool, Val, VmErr};

use crate::{Rig, NAMES};

impl Language for Rig {
    const VERSION: u32 = 1;
    // a record's fields read and write with a dot
    const POLICY: Policy = Policy {
        int_overflow: Overflow::Promote,
        int_div: IntDiv::ToFloat,
        int_mod: IntMod::Truncated,
        zero_div: ZeroDiv::Error,
        neg_pow: NegPow::ToFloat,
        numeric_eq: NumEq::Unified,
        bool_is_int: true,
        falsy: Falsy::NIL.or(Falsy::FALSE).or(Falsy::ZERO).or(Falsy::NAN).or(Falsy::EMPTY_TEXT),
        identity_first: true,
        neg_index: true,
        str_iter: StrIter::CodePoints,
        map_iter: MapIter::Keys,
        map_fields: true,
        open_fields: true,
        shared_bound: true,
        open_order: true,
        builtins_scope: true,
        receiver_slot: true,
        recursion_limit: 1000,
        max_int_digits: Some(10_000),
    };
    const NATIVES: &'static Natives = &crate::NATIVES;
    const NAMES: &'static Names = &NAMES;
    const FRONT: Front = crate::FRONT;
    const DESCRIPTOR: &'static Descriptor = &crate::DESCRIPTOR;
    const WIDE: Option<&'static Kind> = Some(&crate::lang::WIDE);
    const KINDS: &'static [&'static Kind] = &[&crate::lang::TIME, &crate::lang::DURATION];
    const BUILT: Built = Built { ellipsis: None, slice: Some(&crate::lang::SLICE) };
    type Ops = ();

    fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { lib::fmt::render::hook::<Rig>(cx, v, mode) }

    fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { lib::fmt::spec::hook::<Rig>(cx, v, spec) }

    fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { lib::types::match_exc::<Rig>(heap, exc, class) }
}

/* implements `Language` for `$lang` as the rig's over the natives `$natives` and names `$names` */
#[doc(hidden)]
#[macro_export]
macro_rules! over {
    ($lang:ident, $natives:expr) => { $crate::over!($lang, $natives, &$crate::NAMES); };
    ($lang:ident, $natives:expr, $names:expr) => {
        impl ::edge::lang::Language for $lang {
            const VERSION: u32 = 1;
            const POLICY: ::edge::lang::Policy = <$crate::Rig as ::edge::lang::Language>::POLICY;
            const NATIVES: &'static ::edge::lang::Natives = $natives;
            const NAMES: &'static ::edge::lang::Names = $names;
            const FRONT: ::edge::lang::Front = $crate::FRONT;
            const DESCRIPTOR: &'static ::edge::lang::Descriptor = &$crate::DESCRIPTOR;
            const WIDE: Option<&'static ::edge::value::foreign::Kind> = <$crate::Rig as ::edge::lang::Language>::WIDE;
            const KINDS: &'static [&'static ::edge::value::foreign::Kind] = <$crate::Rig as ::edge::lang::Language>::KINDS;
            const BUILT: ::edge::value::foreign::Built = <$crate::Rig as ::edge::lang::Language>::BUILT;
            type Ops = ();

            fn show(cx: &mut ::edge::lang::Cx<'_, '_>, v: ::edge::value::Val, mode: ::edge::lang::Mode) -> Result<$crate::__String, ::edge::value::VmErr> {
                <$crate::Rig as ::edge::lang::Language>::show(cx, v, mode)
            }

            fn format(cx: &mut ::edge::lang::Cx<'_, '_>, v: ::edge::value::Val, spec: &str) -> Result<$crate::__String, ::edge::value::VmErr> {
                <$crate::Rig as ::edge::lang::Language>::format(cx, v, spec)
            }

            fn handles(heap: &::edge::value::HeapPool, exc: ::edge::value::Val, class: ::edge::value::Val) -> Result<bool, ::edge::value::VmErr> {
                <$crate::Rig as ::edge::lang::Language>::handles(heap, exc, class)
            }
        }
    };
}
