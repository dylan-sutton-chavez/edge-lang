use crate::deps::perms;

use super::write_out;

pub(crate) fn grants_check(section: &[u8], modules: &[u8], taken: &[u8]) -> u32 {
    write_out(&perms::check_json(section, modules, taken)) as u32
}

pub(crate) fn grants_scopes(section: &[u8]) -> u32 {
    write_out(&perms::scopes_json(section)) as u32
}

pub(crate) fn grants_held(chain: &[u8], pkg: &str, module: &str) -> u32 {
    write_out(&perms::held_json(chain, pkg, module)) as u32
}
