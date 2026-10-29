use anyhow::Result;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Arc;

use super::{Fault, HostModule};
use crate::kit::Kit;
use crate::lock::Lock;

/* what a package holds of one host module */
#[derive(Clone)]
pub enum Held {
    // every call with no scope bounding it as a composition without grants gives
    All,
    Scopes(Scopes),
}

/* the scopes a grant gives a package and the words a refusal of it reads */
#[derive(Clone)]
pub struct Scopes {
    list: Vec<String>,
    before: String,
    after: String,
}

impl Scopes {
    /* `refusal` holds the words before and after the scope a refused call asked */
    pub fn new(list: Vec<String>, refusal: (String, String)) -> Scopes {
        Scopes { list, before: refusal.0, after: refusal.1 }
    }
}

impl Held {
    /* the scopes a grant bounds calls to or none when nothing bounds them */
    pub fn scopes(&self) -> Option<&[String]> {
        match self {
            Held::All => None,
            Held::Scopes(scopes) => Some(&scopes.list),
        }
    }

    /* refuses `scope` unless the package holds it naming what it holds instead */
    pub fn need(&self, scope: &str) -> Result<(), Fault> {
        match self {
            Held::Scopes(held) if !held.list.iter().any(|s| s == scope) => Err(Fault::denied(format!("{}{scope}{}", held.before, held.after))),
            _ => Ok(()),
        }
    }

    /* refuses what a call of `module` reaches unless a held scope covers it */
    pub fn reach(&self, module: &dyn HostModule, reached: &str) -> Result<(), Fault> {
        match self {
            Held::Scopes(held) if !held.list.iter().any(|scope| module.covers(scope, reached)) => {
                Err(Fault::denied(format!("{}{}{}", held.before, module.asked(&held.list, reached), held.after)))
            }
            _ => Ok(()),
        }
    }
}

/* the calls one package opened checked against its grant before the module sees them */
pub(super) struct Gate {
    pub(super) module: Arc<dyn HostModule>,
    pub(super) held: Held,
    pub(super) names: Vec<String>,
    pub(super) calls: Box<dyn super::Calls>,
}

impl super::Calls for Gate {
    fn call(&mut self, index: usize, args: Vec<super::WireValue>, reply: super::Reply) {
        let name = self.names.get(index).map_or("", String::as_str);
        let checked = self.module.reaches(name, &args).and_then(|reached| match reached {
            Some(reached) => self.held.reach(&*self.module, &reached),
            None => Ok(()),
        });
        match checked {
            Ok(()) => self.calls.call(index, args, reply),
            Err(fault) => reply.fail(fault),
        }
    }

    fn close(&mut self) {
        self.calls.close();
    }
}

/* every package one walk met as its host serves them host modules */
pub struct Packages {
    // the dir of the root manifest which a refusal of its grants names
    pub root: String,
    // whether an undeclared name or a plugin can reach a host module at all
    pub needed: bool,
    pub dirs: Vec<Package>,
    // what the root manifest holds under the field the access carries
    pub section: Value,
}

/* a package by its manifest dir and the name its importer gave it */
pub struct Package {
    pub dir: String,
    pub name: String,
    // each importer's section from the root down with the key the next arrives under
    pub chain: Value,
}

impl Packages {
    /* the system step of the engine walk read under the field the access carries */
    pub(crate) fn read(step: &Value, field: Option<&str>) -> Packages {
        let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
        let dirs = step["dirs"].as_array().into_iter().flatten().map(|dir| Package { dir: text(&dir[0]), name: text(&dir[1]), chain: dir[2].clone() });
        let section = field.and_then(|field| step.get(field)).cloned().unwrap_or_else(|| Value::Object(Map::new()));
        Packages { root: text(&step["root"]), needed: step["needed"].as_bool().unwrap_or(false), dirs: dirs.collect(), section }
    }
}

/* what each package of a run holds of each host module the composition wired */
pub trait Access: Send + Sync {
    /* the manifest field each walk carries per chain link or none */
    fn field(&self) -> Option<&'static str> {
        None
    }

    /* why the packages a walk met cannot be served or none when they can */
    fn check(&self, _packages: &Packages, _modules: &[Arc<dyn HostModule>]) -> Option<String> {
        None
    }

    /* what `package` holds of `module` or the text its import fails with */
    fn held(&self, packages: &Packages, package: &Package, module: &str) -> Result<Held, String>;

    /* what untrusted runs hold in a pool whose manifest sits at `pool` */
    fn untrusted(&self, _kit: &Kit, _pool: &str) -> Result<Untrusted> {
        Ok(Untrusted::closed())
    }

    /* why `manifest` cannot compile or lock with `lock` or the lock beside it */
    fn project(&self, _kit: &Kit, _manifest: &Path, _lock: Option<&Lock>) -> Result<()> {
        Ok(())
    }

    /* what each package `added` brings into the project asks its root to hold */
    fn asks(&self, _kit: &Kit, _manifest: &Path, _added: &Map<String, Value>, _lock: &Lock) -> Result<Vec<(String, Vec<String>)>> {
        Ok(Vec::new())
    }
}

/* untrusted access in one pool for bundles with their own grants and for snippets */
#[derive(Clone)]
pub struct Untrusted {
    pub bundle: Arc<dyn Access>,
    pub snippet: Arc<dyn Access>,
}

impl Untrusted {
    /* untrusted runs reaching no host module whatever they carry */
    pub fn closed() -> Untrusted {
        Untrusted { bundle: Arc::new(Closed), snippet: Arc::new(Closed) }
    }
}

/* a trusted run of a composition without grants reaching every module it wired unbounded */
pub struct Open;

impl Access for Open {
    fn held(&self, _packages: &Packages, _package: &Package, _module: &str) -> Result<Held, String> {
        Ok(Held::All)
    }
}

/* an untrusted run of a composition without grants reaching none */
pub struct Closed;

impl Access for Closed {
    fn held(&self, _packages: &Packages, _package: &Package, module: &str) -> Result<Held, String> {
        Err(format!("module '{module}' is not available to untrusted eval runs"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Calls, Host, Instance, Project, Reply, Status, WireValue};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /* a module whose one call answers whether its grant bounds it */
    struct Probe;

    struct Bound(bool);

    impl Calls for Bound {
        fn call(&mut self, _index: usize, _args: Vec<WireValue>, reply: Reply) {
            reply.ok(WireValue::Bytes(if self.0 { b"bounded".to_vec() } else { b"unbounded".to_vec() }));
        }
    }

    impl HostModule for Probe {
        fn name(&self) -> &str {
            "probe"
        }

        fn scope(&self, _scope: &str) -> bool {
            true
        }

        fn reaches(&self, _call: &str, _args: &[WireValue]) -> Result<Option<String>, Fault> {
            Ok(None)
        }

        fn open(&self, inst: &mut Instance, spec: &str, _pkg: &str, held: Held) -> Result<(), String> {
            inst.serve(spec, "probe", &["reach"], Box::new(Bound(held.scopes().is_some())))
        }
    }

    /* what `src` printed and how it ended under `project` on the kit */
    fn run(kit: &Kit, project: Project, src: &str) -> (String, Status) {
        let printed = Arc::new(Mutex::new(String::new()));
        let into = printed.clone();
        let host = Host::new(kit.load().unwrap()).unwrap();
        let mut vm = host.vm(Box::new(move |text: &str| into.lock().unwrap().push_str(text)), project, None, None).unwrap();
        let status = vm.start(src, None).unwrap();
        let out = printed.lock().unwrap().clone();
        (out, status)
    }

    #[test]
    fn a_kit_without_grants_serves_trusted_runs_every_module_and_untrusted_ones_none() {
        let kit = crate::kit::tests::bare().module(Arc::new(Probe));
        let (out, status) = run(&kit, Project::disk("", None), "import probe\nprint(probe.reach())\n");
        assert!(matches!(status, Status::Done) && out == "unbounded\n", "{out:?}");
        let (_, status) = run(&kit, Project::bundle(HashMap::new(), "", true), "import probe\n");
        let Status::Error(refused) = status else { panic!("an untrusted run imported probe") };
        assert!(refused.contains("module 'probe' is not available to untrusted eval runs"), "{refused}");
    }

    #[test]
    fn a_composition_without_grants_opens_trusted_runs_and_closes_untrusted_ones() {
        let step = json!({ "root": "", "needed": true, "dirs": [["", "main", [[{ "main": ["net:a.test"] }, "main"]]]], "permissions": { "main": ["net:a.test"] } });
        let packages = Packages::read(&step, Open.field());
        assert_eq!(packages.section, json!({}), "a field nobody names is never read");
        let open = Open.held(&packages, &packages.dirs[0], "net").unwrap();
        assert!(open.scopes().is_none() && open.need("evil.example").is_ok());
        let closed = Untrusted::closed();
        for access in [closed.bundle, closed.snippet] {
            let Err(refused) = access.held(&packages, &packages.dirs[0], "net") else { panic!("an untrusted run reached net") };
            assert_eq!(refused, "module 'net' is not available to untrusted eval runs");
        }
    }

    #[test]
    fn a_scoped_grant_refuses_what_it_does_not_name_in_the_words_it_was_given() {
        let held = Held::Scopes(Scopes::new(vec!["a.test".to_string()], ("'main' has no net:".to_string(), ", edge.json grants it net:a.test".to_string())));
        assert_eq!(held.scopes(), Some(&["a.test".to_string()][..]));
        assert!(held.need("a.test").is_ok());
        let refused = held.need("b.test").unwrap_err();
        assert_eq!((refused.class, refused.text.as_str()), (Some(crate::host::Class::Denied), "'main' has no net:b.test, edge.json grants it net:a.test"));
        let read = Packages::read(&json!({ "root": "app/", "needed": false, "dirs": [], "grants": { "main": [] } }), Some("grants"));
        assert_eq!((read.root.as_str(), read.needed, read.section), ("app/", false, json!({ "main": [] })));
    }
}
