use interpreter::lang::{Descriptor, Front, Language, Names, Natives, Policy};
use interpreter::value::foreign::{Built, Kind};

use crate::{Rig, NAMES};

impl Language for Rig {
    const VERSION: u32 = 1;
    // a record's fields read and write with a dot
    const POLICY: Policy = Policy::PLAIN
        .map_fields(true)
        .recursion_limit(1000)
        .max_int_digits(Some(10_000));
    const NATIVES: &'static Natives = &crate::NATIVES;
    const NAMES: &'static Names = &NAMES;
    const FRONT: Front = crate::FRONT;
    const DESCRIPTOR: &'static Descriptor = &crate::DESCRIPTOR;
    const WIDE: Option<&'static Kind> = Some(&crate::lang::WIDE);
    const KINDS: &'static [&'static Kind] = &[&crate::lang::TIME, &crate::lang::DURATION];
    const BUILT: Built = Built::PLAIN.slice(Some(&crate::lang::SLICE));
    type Ops = ();

    interpreter::lib::hooks!(Rig);
}

/* implements `Language` for `$lang` as the rig's over the natives `$natives` */
#[doc(hidden)]
#[macro_export]
macro_rules! over {
    ($lang:ident, $natives:expr) => {
        impl ::interpreter::lang::Language for $lang {
            const VERSION: u32 = 1;
            const POLICY: ::interpreter::lang::Policy = <$crate::Rig as ::interpreter::lang::Language>::POLICY;
            const NATIVES: &'static ::interpreter::lang::Natives = $natives;
            const NAMES: &'static ::interpreter::lang::Names = &$crate::NAMES;
            const FRONT: ::interpreter::lang::Front = $crate::FRONT;
            const DESCRIPTOR: &'static ::interpreter::lang::Descriptor = &$crate::DESCRIPTOR;
            const WIDE: Option<&'static ::interpreter::value::foreign::Kind> = <$crate::Rig as ::interpreter::lang::Language>::WIDE;
            const KINDS: &'static [&'static ::interpreter::value::foreign::Kind] = <$crate::Rig as ::interpreter::lang::Language>::KINDS;
            const BUILT: ::interpreter::value::foreign::Built = <$crate::Rig as ::interpreter::lang::Language>::BUILT;
            type Ops = ();

            fn show(cx: &mut ::interpreter::lang::Cx<'_, '_>, v: ::interpreter::Val, mode: ::interpreter::lang::Show) -> Result<$crate::__String, ::interpreter::VmErr> {
                <$crate::Rig as ::interpreter::lang::Language>::show(cx, v, mode)
            }

            fn format(cx: &mut ::interpreter::lang::Cx<'_, '_>, v: ::interpreter::Val, spec: &str) -> Result<$crate::__String, ::interpreter::VmErr> {
                <$crate::Rig as ::interpreter::lang::Language>::format(cx, v, spec)
            }

            fn handles(heap: &::interpreter::value::HeapPool, exc: ::interpreter::Val, class: ::interpreter::Val) -> Result<bool, ::interpreter::VmErr> {
                <$crate::Rig as ::interpreter::lang::Language>::handles(heap, exc, class)
            }
        }
    };
}
