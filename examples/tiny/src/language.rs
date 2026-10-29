use alloc::string::String;

use edge::lang::{Cx, Descriptor, EngineError, Falsy, Front, IntDiv, IntMod, Language, MapIter, Misfit, Mode, Names, Native, Natives, NegPow, NumEq, Overflow, Policy, StrIter, WordsFn, ZeroDiv};
use edge::value::foreign::{Built, Kind};
use edge::value::{Val, VmErr};

use crate::Tiny;

impl Language for Tiny {
    const VERSION: u32 = 1;
    // the rules of a language for records of data
    const POLICY: Policy = Policy {
        // a record that overflows fails rather than growing
        int_overflow: Overflow::Error,
        int_div: IntDiv::ToFloat,
        int_mod: IntMod::Truncated,
        zero_div: ZeroDiv::Error,
        neg_pow: NegPow::ToFloat,
        // data mixes 1 and 1.0 as one number
        numeric_eq: NumEq::Unified,
        // a bool is no number so true plus 1 fails
        bool_is_int: false,
        // a zero or an empty text is a value of a record
        falsy: Falsy::NIL.or(Falsy::FALSE),
        identity_first: true,
        // an index counts from either end as the expressions read
        neg_index: true,
        str_iter: StrIter::CodePoints,
        map_iter: MapIter::Keys,
        // a dotted name reads the record's key
        map_fields: true,
        builtins_scope: true,
        // tiny defines no methods so a receiver never binds
        receiver_slot: true,
        // flows nest no deep calls
        recursion_limit: 64,
        // the limit a number's text keeps in the limits flows
        max_int_digits: Some(10_000),
    };
    const NATIVES: &'static Natives = &crate::NATIVES;
    const NAMES: &'static Names = &crate::lang::NAMES;
    const FRONT: Front = crate::FRONT;
    const DESCRIPTOR: &'static Descriptor = &crate::DESCRIPTOR;
    const KINDS: &'static [&'static Kind] = &[&crate::lang::TIME, &crate::lang::DURATION];
    const BUILT: Built = Built { ellipsis: Some(&crate::lang::DOTS), slice: Some(&crate::lang::SLICE) };
    const REPORT: Option<WordsFn> = Some(crate::words::report);
    type Ops = ();

    fn show(cx: &mut Cx<'_, '_>, v: Val, mode: Mode) -> Result<String, VmErr> { lib::fmt::render::hook::<Tiny>(cx, v, mode) }

    fn misfit(n: &Native, owner: Option<&str>, m: Misfit<'_>) -> VmErr { crate::words::arity(n, owner, m) }

    fn message(cx: &mut Cx<'_, '_>, e: &EngineError) -> String { crate::words::message(cx, e) }
}
