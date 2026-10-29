use alloc::{boxed::Box, string::{String, ToString}, sync::Arc, vec::Vec};

use crate::s;
use crate::util::hash::FxHashSet;
use crate::value::{HeapPool, Val, VmErr};
use super::{DepHost, ModuleEntry, NativeBinding, Resolved, Resolver, dir_of, joined, outside, parse_manifest, partition_bindings, served, system_spec, walk_up_dirs};

// caps the edge.json extends chain so a crafted loop ends far past real depth
const MAX_PACKAGES_HOPS: u32 = 32;

/* the import a manifest declares for a name and the dir it extends */
type Declared = (Option<String>, Option<String>);

/* `target` joined to `dir` or why code served from a URL there cannot reach it */
fn reach(dir: &str, target: &str) -> Result<String, String> {
    joined(dir, target).map_err(|home| s!("module ", str &outside(target, &home)))
}

/* the resolver the parser asks for imports from `dir` over the modules of `host` */
pub struct HostResolver<H> {
    dir: String,
    host: H,
}

impl<H: DepHost> Resolver for HostResolver<H> {
    fn resolve(&mut self, spec: &str) -> Result<Resolved, String> {
        if !spec.contains('/') {
            let dir = self.dir.clone();
            return self.resolve_bare(spec, &dir);
        }
        let canonical = if spec.contains("://") || spec.starts_with('/') {
            spec.to_string()
        } else if spec.starts_with("./") || spec.starts_with("../") {
            reach(&self.dir, spec)?
        } else {
            // a dotted import anchors at the nearest manifest dir
            let root = self.manifest_root(spec)?;
            reach(&root, spec)?
        };
        self.resolve_canonical(&canonical)
    }

    fn fetch_bytes(&mut self, spec: &str, expected_hash: Option<[u8; 32]>) -> Result<Vec<u8>, String> {
        self.host.fetch(spec, expected_hash)
    }

    fn child(&self, spec: &str) -> Box<dyn Resolver> {
        Box::new(HostResolver { dir: dir_of(spec), host: self.host.clone() })
    }
}

impl<H: DepHost> HostResolver<H> {
    pub fn new(dir: String, host: H) -> Self {
        HostResolver { dir, host }
    }

    fn resolve_bare(&mut self, name: &str, start_dir: &str) -> Result<Resolved, String> {
        let mut visited: FxHashSet<String> = FxHashSet::default();
        let mut search_dir = start_dir.to_string();
        let mut hops: u32 = 0;
        // the nearest manifest names the package the import comes from
        let mut package: Option<String> = None;
        loop {
            if hops > MAX_PACKAGES_HOPS {
                return Err(s!(
                    "edge.json walk-up exceeded ",
                    int MAX_PACKAGES_HOPS as i64,
                    " hops resolving '", str name, "'"));
            }
            hops += 1;

            let mut hit: Option<(String, Option<String>, Option<String>)> = None;
            for dir in walk_up_dirs(&search_dir) {
                let m_spec = s!(str &dir, "edge.json");
                if let Some((target, ext)) = self.lookup_in_manifest(&m_spec, name)? {
                    hit = Some((dir, target, ext));
                    break;
                }
            }
            let Some((dir, target, ext)) = hit else {
                return self.resolve_system(name, package.as_deref());
            };
            package.get_or_insert_with(|| dir.clone());
            if let Some(target) = target {
                let canonical = reach(&dir, &target)?;
                return self.resolve_canonical(&canonical);
            }
            let m_spec = s!(str &dir, "edge.json");
            if let Some(ext) = ext {
                if !visited.insert(m_spec) {
                    return Err(s!("circular extends chain in edge.json"));
                }
                let mut next = reach(&dir, &ext)?;
                if !next.ends_with('/') { next.push('/'); }
                search_dir = next;
                continue;
            }
            return self.resolve_system(name, package.as_deref());
        }
    }

    /* an undeclared name may be a system module the host serves to its package */
    fn resolve_system(&mut self, name: &str, package: Option<&str>) -> Result<Resolved, String> {
        let Some(dir) = package else { return Err(self.undeclared(name)) };
        let spec = system_spec(name, dir);
        let served = self.refusal(&spec).is_some() || self.host.deps(|d| d.registry.iter().any(|(s, _)| *s == spec));
        if !served {
            return Err(self.undeclared(name));
        }
        self.resolve_canonical(&spec)
    }

    /* the nearest dir above holding an edge.json probed live like the bare name walk */
    fn manifest_root(&mut self, spec: &str) -> Result<String, String> {
        let start = self.dir.clone();
        for dir in walk_up_dirs(&start) {
            let m_spec = s!(str &dir, "edge.json");
            let cached = self.host.deps(|d| d.manifests.iter().any(|(s, _)| s == &m_spec));
            if cached || self.fetch_bytes(&m_spec, None).is_ok() {
                return Ok(dir);
            }
        }
        Err(s!("no edge.json above '", str &self.dir, "' to resolve '", str spec, "'"))
    }

    /* what the manifest at `m_spec` declares for `name` read once or none when absent */
    pub(super) fn lookup_in_manifest(&mut self, m_spec: &str, name: &str) -> Result<Option<Declared>, String> {
        if let Some(hit) = self.host.deps(|d| {
            d.manifests.iter()
                .find(|(s, _)| s == m_spec)
                .map(|(_, m)| (m.imports.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()), m.extends.clone()))
        }) {
            return Ok(Some(hit));
        }
        // a walk up fetch takes no hash since no url fragment pins a manifest
        let bytes = match self.fetch_bytes(m_spec, None) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let parsed = parse_manifest(&bytes).map_err(|e| s!("edge.json at '", str m_spec, "': ", str &e))?;
        let target = parsed.imports.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let ext = parsed.extends.clone();
        self.host.deps(|d| d.manifests.push((m_spec.to_string(), parsed)));
        Ok(Some((target, ext)))
    }

    fn resolve_canonical(&mut self, spec: &str) -> Result<Resolved, String> {
        if let Some(msg) = self.refusal(spec) {
            return Err(msg);
        }
        let entry = self.host.deps(|d| d.registry.iter().find(|(s, _)| s == spec).map(|(_, e)| e.clone()))
            .ok_or_else(|| s!("module '", str spec, "' not registered (host did not pre-fetch / register before run())"))?;
        match entry {
            ModuleEntry::Code(src) => Ok(Resolved::Code {
                src,
                canonical: spec.to_string(),
            }),
            ModuleEntry::Native(funcs) => {
                let module: Arc<str> = Arc::from(spec);
                let all: Vec<NativeBinding> = funcs.iter().map(|(n, id)| self.binding(n.clone(), *id, module.clone())).collect();
                let (bindings, classes, consts) = partition_bindings(all);
                Ok(Resolved::Native { bindings, classes, consts, canonical: spec.to_string() })
            }
        }
    }

    /* the host's binding where a system module answers only code of the package importing it */
    fn binding(&mut self, name: String, id: u32, module: Arc<str>) -> NativeBinding {
        if !module.starts_with("system:") {
            return self.host.native(name, id, module);
        }
        let binding = self.host.native(name, id, module.clone());
        let (host, call) = (self.host.clone(), binding.func);
        let checked = move |heap: &mut HeapPool, args: &[Val], kwargs: Option<Val>| -> Result<Val, VmErr> {
            served::answers(&mut host.clone(), &module)?;
            call(heap, args, kwargs)
        };
        NativeBinding { name: binding.name, func: Arc::new(checked) }
    }

    /* why the host cannot load `spec` as it refused it */
    fn refusal(&mut self, spec: &str) -> Option<String> {
        self.host.deps(|d| d.refusals.iter().find(|(s, _)| s == spec).map(|(_, m)| m.clone()))
    }

    /* an undeclared bare name failing in the host's own words when it refused one */
    fn undeclared(&mut self, name: &str) -> String {
        self.refusal(name).unwrap_or_else(|| s!("module '", str name, "' is not provided by this host and no edge.json declares it"))
    }

    /* where `module.function` is served to the package holding `dir` never an import sharing its name */
    pub fn system_fn(&mut self, module: &str, function: &str) -> Result<(Arc<str>, u32), VmErr> {
        let start = self.dir.clone();
        let mut package = None;
        for dir in walk_up_dirs(&start) {
            if self.lookup_in_manifest(&s!(str &dir, "edge.json"), module).map_err(VmErr::Raised)?.is_some() {
                package = Some(dir);
                break;
            }
        }
        let denied = |msg: String| VmErr::Of(crate::lang::ErrKind::Permission, msg);
        let spec = system_spec(module, &package.ok_or_else(|| denied(self.undeclared(module)))?);
        if let Some(msg) = self.refusal(&spec) {
            return Err(denied(msg));
        }
        let funcs = self.host.deps(|d| d.registry.iter().find(|(s, _)| *s == spec).and_then(|(_, e)| match e {
            ModuleEntry::Native(funcs) => Some(funcs.clone()),
            ModuleEntry::Code(_) => None,
        }));
        let funcs = funcs.ok_or_else(|| denied(self.undeclared(module)))?;
        let id = funcs.iter().find(|(n, _)| n == function).map(|(_, id)| *id);
        id.map(|id| (Arc::from(spec.as_str()), id)).ok_or_else(|| VmErr::Attribute(s!("module '", str module, "' has no call '", str function, "'")))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::deps::Deps;
    use std::sync::{Mutex, MutexGuard};

    #[derive(Default)]
    pub(crate) struct State {
        pub(crate) deps: Deps,
        // each file a fetch answers by its exact spec
        pub(crate) files: Vec<(String, Vec<u8>)>,
        pub(crate) caller: String,
        pub(crate) changed: usize,
    }

    /* a host over files in memory whose natives answer their own id */
    #[derive(Clone, Default)]
    pub(crate) struct Host(Arc<Mutex<State>>);

    impl Host {
        pub(crate) fn with(files: &[(&str, &str)]) -> Host {
            let host = Host::default();
            host.state().files = files.iter().map(|(path, text)| (path.to_string(), text.as_bytes().to_vec())).collect();
            host
        }

        pub(crate) fn state(&self) -> MutexGuard<'_, State> {
            self.0.lock().unwrap()
        }

        fn with_code(self, spec: &str, src: &str) -> Host {
            self.state().deps.register(spec.to_string(), ModuleEntry::Code(src.to_string()));
            self
        }

        fn with_native(self, spec: &str, funcs: &[(&str, u32)]) -> Host {
            let funcs = funcs.iter().map(|(name, id)| (name.to_string(), *id)).collect();
            self.state().deps.register(spec.to_string(), ModuleEntry::Native(funcs));
            self
        }
    }

    impl DepHost for Host {
        fn deps<R>(&mut self, f: impl FnOnce(&mut Deps) -> R) -> R {
            f(&mut self.state().deps)
        }

        fn fetch(&mut self, spec: &str, _pin: Option<[u8; 32]>) -> Result<Vec<u8>, String> {
            self.state().files.iter().find(|(path, _)| path == spec).map(|(_, bytes)| bytes.clone()).ok_or_else(|| s!("no file '", str spec, "'"))
        }

        fn native(&mut self, name: String, id: u32, _module: Arc<str>) -> NativeBinding {
            NativeBinding { name, func: Arc::new(move |_: &mut HeapPool, _: &[Val], _: Option<Val>| Ok(Val::int(id as i64))) }
        }

        fn caller_dir(&mut self) -> String {
            self.state().caller.clone()
        }

        fn changed(&mut self) {
            self.state().changed += 1;
        }
    }

    fn resolve(host: &Host, dir: &str, spec: &str) -> Result<Resolved, String> {
        HostResolver::new(dir.to_string(), host.clone()).resolve(spec)
    }

    fn code_of(resolved: Result<Resolved, String>) -> (String, String) {
        match resolved {
            Ok(Resolved::Code { src, canonical }) => (canonical, src),
            Ok(Resolved::Native { canonical, .. }) => panic!("{canonical} resolved native"),
            Err(e) => panic!("{e}"),
        }
    }

    fn error_of(resolved: Result<Resolved, String>) -> String {
        resolved.err().expect("the import should fail")
    }

    fn call(binding: &NativeBinding) -> Result<Val, VmErr> {
        (binding.func)(&mut HeapPool::new(1 << 20), &[], None)
    }

    fn system(host: &Host, dir: &str, module: &str, function: &str) -> Result<(Arc<str>, u32), VmErr> {
        HostResolver::new(dir.to_string(), host.clone()).system_fn(module, function)
    }

    #[test]
    fn a_bare_name_resolves_through_the_nearest_manifest_and_what_it_extends() {
        let host = Host::with(&[
            ("edge.json", r#"{ "imports": { "util": "./lib/util.mini" } }"#),
            ("app/edge.json", r#"{ "extends": "../shared" }"#),
            ("shared/edge.json", r#"{ "imports": { "helper": "./helper.mini" } }"#),
        ]).with_code("./lib/util.mini", "u = 1\n").with_code("shared/helper.mini", "h = 1\n");
        assert_eq!(code_of(resolve(&host, "app/", "helper")), (s!("shared/helper.mini"), s!("h = 1\n")));
        assert_eq!(code_of(resolve(&host, "", "util")).0, "./lib/util.mini");
        // the nearest manifest of app holds no util and neither does the one it extends
        let missing = error_of(resolve(&host, "app/", "util"));
        assert_eq!(missing, "module 'util' is not provided by this host and no edge.json declares it");
        assert_eq!(host.state().deps.manifests.len(), 3);
    }

    #[test]
    fn an_extends_loop_and_a_chain_past_the_cap_fail() {
        let looped = Host::with(&[("a/edge.json", r#"{ "extends": "../b" }"#), ("b/edge.json", r#"{ "extends": "../a" }"#)]);
        assert_eq!(error_of(resolve(&looped, "a/", "x")), "circular extends chain in edge.json");
        let files: Vec<(String, String)> = (0..40).map(|i| (s!("d", int i, "/edge.json"), s!("{ \"extends\": \"../d", int i + 1, "\" }"))).collect();
        let files: Vec<(&str, &str)> = files.iter().map(|(path, text)| (path.as_str(), text.as_str())).collect();
        assert_eq!(error_of(resolve(&Host::with(&files), "d0/", "x")), "edge.json walk-up exceeded 32 hops resolving 'x'");
    }

    #[test]
    fn paths_resolve_from_the_importer_or_the_nearest_manifest() {
        let host = Host::with(&[("app/edge.json", "{}")]).with_code("app/x.mini", "").with_code("app/pkg/mod.mini", "").with_code("https://cdn.test/y.mini", "");
        assert_eq!(code_of(resolve(&host, "app/", "./x.mini")).0, "app/x.mini");
        assert_eq!(code_of(resolve(&host, "app/sub/", "pkg/mod.mini")).0, "app/pkg/mod.mini");
        assert_eq!(code_of(resolve(&host, "", "https://cdn.test/y.mini")).0, "https://cdn.test/y.mini");
        assert_eq!(error_of(resolve(&host, "lib/", "pkg/mod.mini")), "no edge.json above 'lib/' to resolve 'pkg/mod.mini'");
        assert_eq!(error_of(resolve(&host, "", "./z.mini")), "module './z.mini' not registered (host did not pre-fetch / register before run())");
        let mut child = HostResolver::new(s!("app/"), host.clone()).child("app/pkg/mod.mini");
        assert!(matches!(child.resolve("../x.mini"), Ok(Resolved::Code { canonical, .. }) if canonical == "app/x.mini"));
    }

    #[test]
    fn a_refusal_wins_over_what_is_registered() {
        let host = Host::with(&[]).with_code("lib/a.mini", "");
        host.state().deps.refuse(s!("lib/a.mini"), s!("module 'a' is JavaScript, ship a .mini or a .wasm"));
        host.state().deps.refuse(s!("net"), s!("no network here"));
        assert_eq!(error_of(resolve(&host, "lib/", "./a.mini")), "module 'a' is JavaScript, ship a .mini or a .wasm");
        assert_eq!(error_of(resolve(&host, "", "net")), "no network here");
    }

    #[test]
    fn a_system_module_answers_only_the_package_that_imported_it() {
        let host = Host::with(&[("edge.json", "{}"), ("other/edge.json", "{}")]).with_native("system:net@", &[("get", 7)]);
        let Ok(Resolved::Native { bindings, canonical, .. }) = resolve(&host, "", "net") else { panic!("net is a system module of main") };
        assert_eq!((canonical.as_str(), bindings[0].name.as_str()), ("system:net@", "get"));
        assert_eq!(call(&bindings[0]).unwrap().as_int(), 7);
        host.state().caller = s!("other/");
        let denied = call(&bindings[0]).unwrap_err().render(&crate::lang::Names::PLAIN);
        assert_eq!(denied, "Permission: this net module was imported by another package and answers only it, import net here instead");
        assert_eq!(error_of(resolve(&host, "other/", "time")), "module 'time' is not provided by this host and no edge.json declares it");
        assert_eq!(served::package_of(&mut host.clone(), "other/deep/"), Some(s!("other/")));
        assert!(host.state().deps.packages.iter().any(|(dir, package)| dir == "other/deep/" && package.as_deref() == Some("other/")));
    }

    #[test]
    fn a_plain_native_binding_is_never_held_to_a_package() {
        let host = Host::with(&[]).with_native("https://cdn.test/math.wasm", &[("__fn_sqrt", 3), ("__const_pi", 4)]);
        let Ok(Resolved::Native { bindings, consts, .. }) = resolve(&host, "", "https://cdn.test/math.wasm") else { panic!("math should be native") };
        host.state().caller = s!("anywhere/");
        assert_eq!((bindings[0].name.as_str(), consts[0].name.as_str()), ("sqrt", "pi"));
        assert_eq!(call(&bindings[0]).unwrap().as_int(), 3);
    }

    #[test]
    fn a_system_call_is_served_to_the_package_of_its_caller() {
        let host = Host::with(&[("pkg/edge.json", "{}")]).with_native("system:time@pkg/", &[("now", 9)]).with_code("system:net@pkg/", "");
        let (spec, id) = system(&host, "pkg/sub/", "time", "now").unwrap();
        assert_eq!((&*spec, id), ("system:time@pkg/", 9));
        assert_eq!(system(&host, "pkg/", "time", "later").unwrap_err().render(&crate::lang::Names::PLAIN), "Attribute: module 'time' has no call 'later'");
        let undeclared = "Permission: module 'net' is not provided by this host and no edge.json declares it";
        assert_eq!(system(&host, "pkg/", "net", "get").unwrap_err().render(&crate::lang::Names::PLAIN), undeclared);
        assert_eq!(system(&host, "elsewhere/", "time", "now").unwrap_err().render(&crate::lang::Names::PLAIN), "Permission: module 'time' is not provided by this host and no edge.json declares it");
        host.state().deps.refuse(s!("system:time@pkg/"), s!("time is off"));
        assert_eq!(system(&host, "pkg/", "time", "now").unwrap_err().render(&crate::lang::Names::PLAIN), "Permission: time is off");
        let broken = Host::with(&[("edge.json", "{ nope")]);
        assert!(system(&broken, "", "time", "now").unwrap_err().render(&crate::lang::Names::PLAIN).starts_with("edge.json at 'edge.json': "));
    }
}
