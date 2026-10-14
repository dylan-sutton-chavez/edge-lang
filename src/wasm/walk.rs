use alloc::string::String;
use alloc::vec::Vec;

use crate::bridge::{safe_bytes, safe_str_owned};
use crate::modules::json::quote;
use crate::modules::walk::{Fetched, Loaded, Packages, Step, Walk};
use crate::s;

use super::{ModuleEntry, with_runtime, with_slot, write_out, write_out_bytes};

/* Starts the resolution walk over `src` from the entry `set_entry` named, `system` the newline-joined modules this host serves. Every walk export returns the next step's JSON length in the out buffer. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn walk_start(src_ptr: *const u8, src_len: u32, system_ptr: *const u8, system_len: u32) -> u32 {
    let src = unsafe { safe_str_owned(src_ptr, src_len) };
    let system = split(unsafe { safe_bytes(system_ptr, system_len) }, '\n');
    let dir = with_slot(|s| s.entry_dir.clone());
    with_runtime(|rt| rt.walk = Some(Walk::new(&src, &dir, system)));
    drive()
}

/* Answers the last fetch, `kind` 0 with the bytes, 1 missing with an optional hint, 2 failed with why. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn walk_fetched(ptr: *const u8, len: u32, kind: u32) -> u32 {
    let bytes = unsafe { safe_bytes(ptr, len) };
    let text = || String::from_utf8_lossy(bytes).into_owned();
    let answer = match kind {
        0 => Fetched::Bytes(bytes.to_vec()),
        1 => Fetched::Missing((!bytes.is_empty()).then(text)),
        _ => Fetched::Failed(text()),
    };
    with_runtime(|rt| rt.walk.as_mut().map(|walk| walk.fetched(answer)));
    drive()
}

/* The bytes of the plugin the last step named, written raw to the out buffer, since one inside a package never reached the host. */
#[unsafe(no_mangle)]
pub extern "C" fn walk_plugin_bytes() -> u32 {
    let bytes = with_runtime(|rt| core::mem::take(&mut rt.walk_plugin));
    write_out_bytes(bytes) as u32
}

/* Answers the last plugin, `kind` 0 registered, 1 failed with why, 2 refused by this host with why. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn walk_plugin(kind: u32, ptr: *const u8, len: u32) -> u32 {
    let text = unsafe { safe_str_owned(ptr, len) };
    let answer = match kind {
        0 => Loaded::Ok,
        1 => Loaded::Failed(text),
        _ => Loaded::Refused(text),
    };
    with_runtime(|rt| rt.walk.as_mut().map(|walk| walk.loaded(answer)));
    drive()
}

/* The host served the system modules, `ptr` its failures joined by NUL, since one may span lines. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn walk_served(ptr: *const u8, len: u32) -> u32 {
    let failures = split(unsafe { safe_bytes(ptr, len) }, '\0');
    with_runtime(|rt| rt.walk.as_mut().map(|walk| walk.served(failures)));
    drive()
}

/* Answers the undeclared names with the ones a registry has, joined by NUL. */
#[unsafe(no_mangle)]
pub unsafe extern "C" fn walk_known(ptr: *const u8, len: u32) -> u32 {
    let names = split(unsafe { safe_bytes(ptr, len) }, '\0');
    with_runtime(|rt| rt.walk.as_mut().map(|walk| walk.known(names)));
    drive()
}

pub(super) fn split(bytes: &[u8], at: char) -> Vec<String> {
    core::str::from_utf8(bytes).unwrap_or("").split(at).filter(|l| !l.is_empty()).map(String::from).collect()
}

/* Applies what the walk registers itself, and stops at the first step only the host can answer. */
fn drive() -> u32 {
    loop {
        let Some(step) = with_runtime(|rt| rt.walk.as_mut().map(Walk::step)) else {
            return write_out("{\"done\":[\"no resolution walk is running\"]}") as u32;
        };
        let mut out = String::new();
        match step {
            Step::Code { spec, src } => with_runtime(|rt| rt.register(spec, ModuleEntry::Code(src))),
            Step::Refuse { spec, msg } => with_runtime(|rt| rt.refuse(spec, msg)),
            Step::Manifest { spec, manifest } => with_runtime(|rt| {
                rt.manifests.retain(|(s, _)| *s != spec);
                rt.manifests.push((spec, manifest));
                // A manifest found anew can move the package a file belongs to.
                rt.packages.clear();
            }),
            Step::Fetch(spec) => {
                out.push_str("{\"fetch\":");
                quote(&mut out, &spec);
                out.push('}');
                return write_out(&out) as u32;
            }
            Step::Plugin { spec, name, bytes } => {
                with_runtime(|rt| rt.walk_plugin = bytes);
                out.push_str("{\"plugin\":");
                quote(&mut out, &spec);
                out.push_str(",\"name\":");
                quote(&mut out, &name);
                out.push('}');
                return write_out(&out) as u32;
            }
            Step::System(packages) => return write_out(&system(&packages)) as u32,
            Step::Undeclared(names) => {
                out.push_str("{\"undeclared\":");
                list(&mut out, &names);
                out.push('}');
                return write_out(&out) as u32;
            }
            Step::Done(failures) => {
                with_runtime(|rt| rt.walk = None);
                out.push_str("{\"done\":");
                list(&mut out, &failures);
                out.push('}');
                return write_out(&out) as u32;
            }
        }
    }
}

// Each package to serve system modules to with its grants, and the root section as written.
fn system(p: &Packages) -> String {
    let mut out = s!("{\"system\":{\"root\":");
    quote(&mut out, &p.root);
    out.push_str(",\"needed\":");
    out.push_str(if p.needed { "true" } else { "false" });
    out.push_str(",\"dirs\":[");
    for (i, (dir, pkg, chain)) in p.dirs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        quote(&mut out, dir);
        out.push(',');
        quote(&mut out, pkg);
        out.push_str(",[");
        for (j, (granting, key)) in chain.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push('[');
            section(&mut out, granting);
            out.push(',');
            quote(&mut out, key);
            out.push(']');
        }
        out.push_str("]]");
    }
    out.push_str("],\"permissions\":");
    section(&mut out, &p.permissions);
    out.push_str("}}");
    out
}

fn section(out: &mut String, holders: &[(String, Vec<String>)]) {
    out.push('{');
    for (i, (holder, entries)) in holders.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        quote(out, holder);
        out.push(':');
        list(out, entries);
    }
    out.push('}');
}

fn list(out: &mut String, items: &[String]) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        quote(out, item);
    }
    out.push(']');
}
