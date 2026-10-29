#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use edge::value::{SchedulerStatus, VmErr};
    use edge::deps::NativeBinding;

    use crate::common::{answer, fail, lex, parse, parser, TestResolver, test_native};

    /* `native` resolves to extern bindings and `code` splices in a source module */
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[serde(untagged)]
    enum ModuleDef {
        Native {
            native: Vec<String>,
        },
        Code {
            code: String,
        },
    }

    /* a per-directory `edge.json` for walk-up cases where flat fixtures use `aliases` */
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ManifestDef {
        #[serde(default)]
        imports: HashMap<String, String>,
        #[serde(default)]
        extends: Option<String>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Case {
        src: String,
        #[serde(default)]
        output: Vec<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        input: Vec<String>,
        #[serde(default)]
        modules: HashMap<String, ModuleDef>,
        /* the synthetic root `edge.json` which nested `manifests` entries shadow */
        #[serde(default)]
        aliases: HashMap<String, String>,
        /* nested manifests by directory for walk-up and `extends` paths with cycles */
        #[serde(default)]
        manifests: HashMap<String, ManifestDef>,
        #[serde(default)]
        expect_externs: Option<usize>,
        #[serde(default)]
        expect_functions: Option<usize>,
        #[serde(default)]
        error_span_covers: Option<String>,
        /* deliveries by call id that simulate the host resolving calls out of order */
        #[serde(default)]
        host_deliveries: Vec<Delivery>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Delivery {
        id: u32,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }

    fn build_resolver(modules: &HashMap<String, ModuleDef>, aliases: &HashMap<String, String>, manifests: &HashMap<String, ManifestDef>) -> TestResolver {
        let mut r = TestResolver::new();
        for (spec, def) in modules {
            match def {
                ModuleDef::Native { native } => {
                    let bindings: Vec<NativeBinding> = native.iter()
                        .map(|n| test_native(n)
                        .unwrap_or_else(|| panic!("unknown test native: {}", n)))
                        .collect();
                    r = r.with_native(spec, bindings);
                }
                ModuleDef::Code { code } => {
                    r = r.with_code(spec, code);
                }
            }
            /* bare-name modules alias themselves so flat fixtures need no `aliases` */
            if !spec.contains('/') {
                r = r.with_alias(spec, spec);
            }
        }
        for (name, target) in aliases {
            r = r.with_alias(name, target);
        }
        for (dir, m) in manifests {
            let pairs: Vec<(&str, &str)> = m.imports.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            r = r.with_manifest(dir, &pairs, m.extends.as_deref());
        }
        r
    }

    /* with NoopResolver an import gives a clean diagnostic which the JSON runner cannot reach */
    #[test]
    fn noop_resolver_default() {
        let src = "from json import dumps";
        let (_, errs) = parse(src);
        assert!(errs.iter().any(|d| d.msg.contains("not found") || d.msg.contains("not configured")), "expected NoopResolver error, got: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
    }

    #[test]
    fn modules_cases() {
        let cases: Vec<Case> = serde_json::from_str(
            include_str!("cases/modules.json")
        ).expect("invalid JSON");

        for case in cases {
            let resolver = Box::new(build_resolver(&case.modules, &case.aliases, &case.manifests));
            let (tokens, _lex_errs) = lex(&case.src);
            let (chunk, parse_errs) = parser(&case.src, tokens.into_iter(), resolver).parse();

            // parse errors match the `error` substring and an optional span anchor
            if !parse_errs.is_empty() {
                let combined = parse_errs.iter().map(|d| d.msg.as_str()).collect::<Vec<_>>().join(" | ");
                let expected = case.error.as_deref().unwrap_or_else(|| panic!("unexpected parse error on {:?}: {}", case.src, combined));
                assert!(combined.contains(expected), "parse error mismatch on {:?}: got '{}', expected substring '{}'", case.src, combined, expected);
                if let Some(needle) = &case.error_span_covers {
                    let pos = case.src.find(needle).unwrap_or_else(|| panic!("error_span_covers '{}' not found in src", needle));
                    let d = &parse_errs[0];
                    assert_eq!(d.start, pos, "diag start mismatch on {:?}", case.src);
                    assert_eq!(d.end, pos + needle.len(), "diag end mismatch on {:?}", case.src);
                }
                continue;
            }

            // chunk shape checks for a clean parse
            if let Some(n) = case.expect_externs { assert_eq!(chunk.extern_table.len(), n, "extern_table size mismatch on {:?}", case.src); }
            if let Some(n) = case.expect_functions { assert_eq!(chunk.functions.len(), n, "functions count mismatch on {:?}", case.src); }

            let mut vm = crate::common::boot(&chunk, edge::embed::Limits::sandbox());
            vm.input_buffer = case.input.clone();
            // each PendingHostCall resumes with the next entry of `host_deliveries`
            let mut hr_idx = 0usize;
            let result = loop {
                match vm.run() {
                    Ok(v) => break Ok(v),
                    Err(VmErr::HostYield(SchedulerStatus::PendingHostCall)) if hr_idx < case.host_deliveries.len() => {
                        let d = &case.host_deliveries[hr_idx];
                        match &d.error {
                            Some(msg) => fail(&mut vm, d.id.into(), msg),
                            None => answer(&mut vm, d.id.into(), d.value.as_deref().unwrap_or("")),
                        };
                        hr_idx += 1;
                    }
                    Err(e) => break Err(e),
                }
            };
            match result {
                Ok(_) => {
                    if case.error.is_some() { panic!("expected error on {:?}, got success with output {:?}", case.src, vm.output); }
                    assert_eq!(vm.output, case.output, "output mismatch on: {:?}", case.src);
                }
                Err(e) => match &case.error {
                    Some(expected) => assert!(e.to_string().contains(expected.as_str()), "runtime error mismatch on {:?}: got '{}', expected substring '{}'", case.src, e, expected),
                    None => panic!("VM error on {:?}: {}", case.src, e),
                }
            }
        }
    }

    /* the modules a real host keeps which checks no cycle itself */
    #[derive(Clone, Default)]
    struct Kept(std::sync::Arc<std::sync::Mutex<edge::deps::Deps>>);

    impl edge::deps::DepHost for Kept {
        fn deps<R>(&mut self, f: impl FnOnce(&mut edge::deps::Deps) -> R) -> R { f(&mut self.0.lock().unwrap()) }
        fn fetch(&mut self, spec: &str, _pin: Option<[u8; 32]>) -> Result<Vec<u8>, String> { Err(format!("no file '{spec}'")) }
        fn native(&mut self, name: String, _id: u32, _module: std::sync::Arc<str>) -> NativeBinding { panic!("no native {name} is served") }
        fn caller_dir(&mut self) -> String { String::new() }
    }

    // a cycle the host resolver walks reports at the import that closes it
    #[test]
    fn an_import_cycle_through_a_host_resolver_reports_where_it_closes() {
        let mut host = Kept::default();
        for (spec, code) in [("./a.mini", "from .b import y\nx = 1\n"), ("./b.mini", "from .a import x\ny = 2\n"), ("./c.mini", "from .c import z\nz = 3\n")] {
            edge::deps::DepHost::deps(&mut host, |d| d.register(spec.into(), edge::deps::ModuleEntry::Code(code.into())));
        }
        for src in ["from .a import x\nprint(x)\n", "from .c import z\nprint(z)\n"] {
            let resolver = Box::new(edge::deps::HostResolver::new(String::new(), host.clone()));
            let (_, errs) = parser(src, lex(src).0.into_iter(), resolver).parse();
            let msgs: Vec<&str> = errs.iter().map(|e| e.msg.as_str()).collect();
            assert!(msgs.len() == 1 && msgs[0].contains("circular import") && errs[0].start == 5, "{src:?}: {msgs:?}");
        }
    }

    /* the chunk and the messages `src` compiles to through `resolver` */
    fn compiled_with(src: &str, resolver: TestResolver) -> (edge::code::SSAChunk, Vec<String>) {
        let (chunk, errs) = parser(src, lex(src).0.into_iter(), Box::new(resolver)).parse();
        (chunk, errs.into_iter().map(|e| e.msg).collect())
    }

    // a module a diamond reaches twice parses once whatever shape its spec takes
    #[test]
    fn a_diamond_parses_each_module_once() {
        const LEVELS: usize = 26;
        let mut resolver = TestResolver::new();
        for i in 0..LEVELS {
            let code = if i + 1 < LEVELS { format!("import m{}\nimport m{} as again\nv = m{}.v + again.v\n", i + 1, i + 1, i + 1) } else { "v = 1\n".to_string() };
            resolver = resolver.with_alias(&format!("m{i}"), &format!("m{i}.mini")).with_code(&format!("m{i}.mini"), &code);
        }
        let start = std::time::Instant::now();
        let (chunk, errs) = compiled_with("import m0\nprint(m0.v)\n", resolver);
        assert!(errs.is_empty(), "{errs:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{:?}", start.elapsed());
        let mut vm = crate::common::boot(&chunk, crate::common::corpus_limits());
        vm.run().expect("the diamond runs");
        assert_eq!(vm.output, [(1u64 << (LEVELS - 1)).to_string()]);
    }

    // imports nest 64 modules deep on a small stack and the next one is refused
    #[test]
    fn imports_nest_a_bounded_depth() {
        fn chain(n: usize) -> TestResolver {
            // the last module nests its blocks and an expression as deep as the caps allow
            let mut deepest = String::new();
            for d in 0..99 { deepest.push_str(&format!("{}if true:\n", "    ".repeat(d))); }
            deepest.push_str(&format!("{}x = {}1{}\n", "    ".repeat(99), "(".repeat(190), ")".repeat(190)));
            let mut resolver = TestResolver::new();
            for i in 0..n {
                let code = if i + 1 < n { format!("from .c{} import x\n", i + 1) } else { format!("x = 0\n{deepest}") };
                resolver = resolver.with_code(&format!("./c{i}.mini"), &code);
            }
            resolver
        }
        let run = |n: usize| std::thread::Builder::new().stack_size(1 << 20).spawn(move || compiled_with("from .c0 import x\nprint(x)\n", chain(n)).1).expect("a thread").join().expect("no overflow");
        assert_eq!(run(64), Vec::<String>::new());
        let errs = run(65);
        assert!(errs.iter().any(|e| e.contains("imports nest deeper than 64 modules")), "{errs:?}");
    }
}
