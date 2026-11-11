use crate::deps::package;

use super::write_out;

pub(crate) fn bundle_index(bundle: &[u8]) -> u32 {
    write_out(&package::index(bundle)) as u32
}
