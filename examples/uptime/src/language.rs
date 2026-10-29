use alloc::string::String;

use edge::lang::{Cx, Descriptor, Falsy, Front, IntDiv, IntMod, Language, MapIter, Mode, Names, Natives, NegPow, NumEq, Overflow, Policy, StrIter, ZeroDiv};
use edge::value::foreign::Kind;
use edge::value::{HeapPool, Val, VmErr};

use crate::Uptime;

impl Language for Uptime {
    const VERSION: u32 = 1;
    // the engine's own rules but for map fields and the digit limit
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
        // an answer's fields read as `status.indicator`
        map_fields: true,
        builtins_scope: true,
        receiver_slot: true,
        recursion_limit: 1000,
        max_int_digits: Some(10_000),
    };
    const NATIVES: &'static Natives = &crate::NATIVES;
    const NAMES: &'static Names = &crate::NAMES;
    const FRONT: Front = crate::FRONT;
    const DESCRIPTOR: &'static Descriptor = &crate::DESCRIPTOR;
    const WIDE: Option<&'static Kind> = Some(&crate::natives::WIDE);
    type Ops = ();

    fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { lib::fmt::render::hook::<Uptime>(cx, v, mode) }

    fn format(cx: &mut Cx<'_, '_>, v: Val, spec: &str) -> Result<String, VmErr> { lib::fmt::spec::hook::<Uptime>(cx, v, spec) }

    fn handles(heap: &HeapPool, exc: Val, class: Val) -> Result<bool, VmErr> { lib::types::match_exc::<Uptime>(heap, exc, class) }
}
