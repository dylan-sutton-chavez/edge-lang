use crate::deps::walk::{self, Carry};
use crate::r#unsafe::web;

use super::resolver::WebHost;
use super::{with_runtime, with_slot, write_out, write_out_bytes};

/* starts the walk from the entry dir and each walk export returns its step's length */
pub(crate) fn walk_start(src: &str, system: &[u8]) -> u32 {
    let dir = with_slot(|s| s.entry_dir.clone());
    let carry = with_runtime(|rt| rt.carry.clone()).unwrap_or_else(carried);
    write_out(&walk::start(web::entry().front(), &mut WebHost, src, &dir, system, carry)) as u32
}

/* the field the next walks carry with its holders after it one per line */
pub(crate) fn walk_carry(spec: &[u8]) {
    with_runtime(|rt| rt.carry = Some(walk::carry(spec)));
}

// a host that never names a field gets the grant logic this artifact exports
#[cfg(feature = "perms")]
fn carried() -> Carry {
    use crate::deps::perms::{FIELD, HOLDERS};
    Carry { field: FIELD.into(), holders: HOLDERS.iter().map(|holder| (*holder).into()).collect() }
}

#[cfg(not(feature = "perms"))]
fn carried() -> Carry {
    Carry::default()
}

pub(crate) fn walk_fetched(bytes: &[u8], kind: u32) -> u32 {
    write_out(&walk::fetched(&mut WebHost, bytes, kind)) as u32
}

/* the plugin bytes raw since one inside a package never reached the host */
pub(crate) fn walk_plugin_bytes() -> u32 {
    write_out_bytes(walk::plugin_bytes(&mut WebHost)) as u32
}

pub(crate) fn walk_plugin(kind: u32, text: &str) -> u32 {
    write_out(&walk::loaded(&mut WebHost, kind, text.into())) as u32
}

pub(crate) fn walk_served(failures: &[u8]) -> u32 {
    write_out(&walk::served(&mut WebHost, failures)) as u32
}

pub(crate) fn walk_known(names: &[u8]) -> u32 {
    write_out(&walk::known(&mut WebHost, names)) as u32
}
