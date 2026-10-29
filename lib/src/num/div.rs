use edge::lang::{Args, Cx, IntMod, Ret, Sig};
use edge::value::Val;

use super::numeric;
use crate::fail::{Fail, fail};
use crate::{Kernel, Site};

/* the quotient and the remainder as a pair rounded as the runtime's policy says */
pub const fn divmod<S: Site>() -> Kernel { Kernel::new(Sig::exact(2), divmod_run::<S>) }

fn divmod_run<S: Site>(cx: &mut Cx<'_, '_>, a: Args<'_>) -> Ret {
    let (x, y) = (a.pos[0], a.pos[1]);
    // two inline ints divide here in machine words floored only where the runtime floors
    if x.is_int() && y.is_int() && y.as_int() != 0 {
        let (p, q) = (x.as_int(), y.as_int());
        let (mut d, mut m) = (p / q, p % q);
        if m != 0 && (m < 0) != (q < 0) && matches!(cx.policy().int_mod, IntMod::Floored) { (d, m) = (d - 1, m + q); }
        let d = cx.new_int(d as i128)?;
        return Ok(cx.new_tuple(alloc::vec![d, Val::int(m)])?.into());
    }
    if !numeric(cx.heap(), x) || !numeric(cx.heap(), y) { return Err(fail::<S>(cx, Fail::BadOperands(x, y))); }
    let q = cx.floor_div(x, y)?;
    let r = cx.modulo(x, y)?;
    Ok(cx.new_tuple(alloc::vec![q, r])?.into())
}
