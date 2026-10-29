use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::s;
use crate::value::VmErr;
use super::json::{list, quote};
use super::walk::Packages;
use super::{DepHost, HostResolver, system_spec, walk_up_dirs};

/* the packages the host serves system modules to with their grants as JSON */
pub(super) fn system(p: &Packages) -> String {
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
    out.push(']');
    // old hosts read the root section under the field the host named
    if !p.field.is_empty() {
        out.push(',');
        quote(&mut out, &p.field);
        out.push(':');
        section(&mut out, &p.section);
    }
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

/* a system module answers only code of the package that imported it whoever holds it */
pub fn answers<H: DepHost>(host: &mut H, spec: &str) -> Result<(), VmErr> {
    let dir = host.caller_dir();
    let name = spec.strip_prefix("system:").and_then(|s| s.split_once('@')).map_or("", |(n, _)| n);
    if package_of(host, &dir).is_some_and(|package| system_spec(name, &package) == spec) {
        return Ok(());
    }
    Err(VmErr::Of(crate::lang::ErrKind::Permission, s!("this ", str name, " module was imported by another package and answers only it, import ", str name, " here instead")))
}

/* the nearest edge.json dir at or above `dir` which is the package of its code */
pub fn package_of<H: DepHost>(host: &mut H, dir: &str) -> Option<String> {
    if let Some(found) = host.deps(|d| d.packages.iter().find(|(at, _)| at == dir).map(|(_, p)| p.clone())) {
        return found;
    }
    let mut resolver = HostResolver::new(dir.to_string(), host.clone());
    let found = walk_up_dirs(dir).find(|up| matches!(resolver.lookup_in_manifest(&s!(str up, "edge.json"), ""), Ok(Some(_))));
    host.deps(|d| d.packages.push((dir.to_string(), found.clone())));
    found
}
