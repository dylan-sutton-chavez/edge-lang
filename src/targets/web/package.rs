use crate::deps::package;

use super::write_out;

pub(crate) fn manifest_check(manifest: &[u8], lock: &[u8], system: &[u8]) -> u32 {
    write_out(&package::check(manifest, lock, system)) as u32
}

pub(crate) fn manifest_fields(manifest: &[u8]) -> u32 {
    write_out(&package::fields(manifest)) as u32
}

pub(crate) fn bundle_index(bundle: &[u8]) -> u32 {
    write_out(&package::index(bundle)) as u32
}
