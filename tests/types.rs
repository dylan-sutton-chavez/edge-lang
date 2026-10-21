/* The types `edge build` proves, over the small edge.ept in cases/types.ept (the shipped one comes from Spider Monkey),
   each tree the reviewer accepts beside a broken copy it must refuse. */
mod test {
    use compiler::types::{check, read::Rules, typed};

    // Its first line made the hash of these sources, as Spider Monkey writes it for the shipped file.
    fn ept() -> String {
        let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases/types.ept")).expect("tests/cases/types.ept");
        text.replacen("SOURCES", compiler::types::read::SOURCES, 1)
    }

    // The tags a read of `name` on `line` has, as code runs.
    fn read(src: &str, line: usize, name: &str) -> Vec<(u8, u8)> {
        let all = compiler::types::reads(src, &ept()).expect("the tree is accepted");
        all.into_iter().find(|r| r.0 == line && r.1 == name).unwrap_or_else(|| panic!("no read of {name} on line {line}")).2
    }

    // The tree of `src`, its rules and code kept for the test's whole run.
    fn typed_of(src: &str) -> compiler::types::Typed<'static> {
        let rules = Box::leak(Box::new(Rules::read(&ept()).unwrap()));
        typed(Box::leak(Box::new(compiler::code::compile(src).0)), rules).expect("the tree is accepted")
    }

    // The code of tag `n`, a run of one.
    fn code(n: &str) -> (u8, u8) { Rules::read(&ept()).unwrap().tags.iter().find(|t| t.0 == n).map(|t| (t.1, t.1)).unwrap() }

    #[test]
    fn a_loop_keeps_its_ints_and_a_call_its_result() {
        let src = "def f(n):\n    s = 0\n    for i in range(n):\n        if i % 2 == 0:\n            s = s + i\n    return s\nx = f(10)\nprint(x)\n";
        assert_eq!(read(src, 5, "i"), [code("int")]);
        // range(10) holds 0..=9, so `i` never reaches 10.
        let t = typed_of(src);
        let body = t.prog.bodies[1].chunk;
        let i = body.names.iter().position(|n| n.starts_with("i_")).map(|k| body.canonical.get(k).map_or(k, |&c| c as usize)).unwrap();
        let his: Vec<i64> = t.tree.ctxs.iter().filter(|c| c.body == 1).flat_map(|c| c.seen.iter().flatten()).map(|s| s.slots[i]).filter(|v| v.tags == 1 << code("int").0).map(|v| v.hi).collect();
        assert!(!his.is_empty() && his.iter().all(|&h| h <= 9), "{his:?}");
        assert_eq!(read(src, 6, "s"), [code("int")]);
        assert_eq!(read(src, 8, "x"), [code("int")]);
    }

    #[test]
    fn two_branches_on_one_test_never_meet() {
        // x and y are each int or str, and the path with an int and a str is never taken.
        let src = "def f(n):\n    c = n < 3\n    x = 1 if c else 'a'\n    y = 2 if c else 'b'\n    return x + y\nprint(f(1))\n";
        let t = typed_of(src);
        let f = t.tree.ctxs.iter().find(|c| c.body == 1).expect("f has a context");
        assert_eq!(compiler::types::runs(f.ret.tags), [code("int"), code("str")]);
        assert_eq!(compiler::types::runs(f.raises.tags), [code("MemoryError")], "only the int sum may run out of memory, no TypeError");
    }

    #[test]
    fn isinstance_narrows_and_leaves_the_other_side_dead() {
        let src = "def f(v):\n    if isinstance(v, int):\n        return v + 1\n    return 'no'\nprint(f(2))\n";
        let t = typed_of(src);
        let dead = t.dead(1);
        assert!(dead.iter().any(|d| *d), "the str return is dead for an int argument");
        assert!(t.count().0 > 0);
        // A twin called with a str too keeps both returns.
        assert!(!typed_of(&src.replace("print(f(2))", "print(f(2), f('s'))")).dead(1).iter().any(|d| *d));
    }

    #[test]
    fn a_value_of_fixed_truth_takes_one_branch() {
        // None is always false and a function always true, so the other branch is dead.
        let t = typed_of("def f(v):\n    if v:\n        return 1\n    return 'n'\nprint(f(None))\n");
        assert_eq!(compiler::types::runs(t.tree.ctxs.iter().find(|c| c.body == 1).unwrap().ret.tags), [code("str")]);
        assert_eq!(read("def g():\n    return 0\nx = 1 if g else 'a'\nprint(x)\n", 4, "x"), [code("int")]);
    }

    #[test]
    fn a_tree_that_misses_a_path_is_refused() {
        let src = "class P:\n    def __init__(self, x):\n        self.x = x\ndef g(p):\n    return p.x\nprint(g(P(3)))\n";
        let t = typed_of(src);
        assert!(check::types_check(&t.prog, &t.tree).is_ok());
        // Each broken copy: a state dropped, a result claimed narrower, a table entry claimed narrower.
        let mut a = typed_of(src);
        let (x, ip) = a.tree.ctxs.iter().enumerate().find_map(|(x, c)| c.seen.iter().skip(1).position(|s| !s.is_empty()).map(|ip| (x, ip + 1))).unwrap();
        a.tree.ctxs[x].seen[ip].clear();
        assert!(check::types_check(&a.prog, &a.tree).is_err());
        let mut b = typed_of(src);
        let at = b.prog.bodies.iter().position(|x| x.chunk.qualname == "g").unwrap() as u32;
        let g = b.tree.ctxs.iter_mut().find(|c| c.body == at).unwrap();
        g.ret = compiler::types::Ty::of(1 << code("str").0);
        assert!(check::types_check(&b.prog, &b.tree).is_err());
        let mut c = typed_of(src);
        for v in c.tree.tables.values_mut() { v.tags = 0; }
        assert!(check::types_check(&c.prog, &c.tree).is_err());
    }

    #[test]
    fn an_object_stays_off_the_heap_only_while_it_never_leaves() {
        let sites = |src: &str| { let t = typed_of(src); check::no_heap(&t.prog, &t.tree).len() };
        let kept = "class P:\n    def __init__(self, x):\n        self.x = x\ndef g(n):\n    p = P(n)\n    return p.x\nprint(g(3))\n";
        assert_eq!(sites(kept), 1);
        assert_eq!(sites(&kept.replace("return p.x", "return p")), 0, "returned, it leaves its frame");
        assert_eq!(sites(&kept.replace("return p.x", "q = [p]\n    return 1")), 0, "kept in a list, it leaves its frame");
        // Two sites met in one list or one result leave as instances of their class.
        let two = "class P:\n    def __init__(self, x):\n        self.x = x\nkeep = []\ndef f():\n    a = P(1)\n    keep.append(a)\n    b = P(2)\n    keep.append(b)\nf()\n";
        assert_eq!(sites(two), 0);
        assert_eq!(sites(&kept.replace("    p = P(n)\n    return p.x", "    return P(1) if n < 2 else P(2)")), 0);
        assert_eq!(sites(&kept.replace("return p.x", "return p is None")), 0, "compared by identity");
        assert_eq!(sites(&kept.replace("return p.x", "return id(p)")), 0, "its identity asked");
        // A stand-in, not a value: any use past its attributes and the program's own code drops it.
        for (used, why) in [("isinstance(p, P)", "isinstance"), ("type(p)", "type"), ("repr(p)", "repr"), ("f'{p}'", "format"), ("print(p)", "a builtin")] {
            assert_eq!(sites(&kept.replace("return p.x", &format!("q = {used}\n    return p.x"))), 0, "{why}");
        }
        assert_eq!(sites(&kept.replace("return p.x", "return h(p)\ndef h(o):\n    return o.x")), 1, "handed to the program's own code");
    }

    #[test]
    fn a_value_takes_only_the_first_row_that_takes_it_whole() {
        // Rows `int int`, `int *`, `* *` in that order: each pair of arguments meets one row alone.
        for (args, ret, raises) in [("1, 2", "int", None), ("1, 's'", "bool", Some("ValueError")), ("'r', 's'", "str", Some("TypeError"))] {
            let t = typed_of(&format!("def f(a, b):\n    return a ^ b\nprint(f({args}))\n"));
            let f = t.tree.ctxs.iter().find(|c| c.body == 1).expect("f has a context");
            assert_eq!((compiler::types::runs(f.ret.tags), compiler::types::runs(f.raises.tags)), (vec![code(ret)], raises.map(code).into_iter().collect()), "{args}");
        }
    }

    #[test]
    fn eight_values_of_any_type_meet_a_row_per_type_and_place_in_one_step() {
        // A row per place and type, as the shipped file has for BuildTuple@8: trying each class of each operand together is 26^8.
        let names: Vec<String> = Rules::read(&ept()).unwrap().tags.iter().take(25).map(|t| t.0.clone()).collect();
        let mut text = ept();
        for p in 0..8 { for n in &names { let mut w = vec!["*"; 8]; w[p] = n; text += &format!("rule BuildTuple@8 {} -> tuple\n", w.join(" ")); } }
        let src = "def g(a, b):\n    return a - b\ny = g('a', 'b')\nt = (y, y, y, y, y, y, y, y)\nprint(t)\n";
        // y may be any type: a tuple by the rows that name its type, a list by the last row when none does.
        let t = compiler::types::reads(src, &(text + "rule BuildTuple@8 * * * * * * * * -> list\n")).unwrap().into_iter().find(|r| r.0 == 5 && r.1 == "t").unwrap().2;
        assert_eq!(t, [(code("list").0, code("tuple").0)]);
    }

    #[test]
    fn a_loop_gets_what_its_for_iter_row_says() {
        // A list's row gives its items, as item() does; a str's row its own answer, where item() knows nothing.
        assert_eq!(read("xs = [1, 2]\nfor x in xs:\n    print(x)\n", 3, "x"), [code("int")]);
        let src = "for c in 'ab':\n    print(c)\n";
        let got = compiler::types::reads(src, &(ept() + "rule GetIter str ->\nrule ForIter str -> str\n")).unwrap();
        assert_eq!(got.into_iter().find(|r| r.0 == 2 && r.1 == "c").unwrap().2, [code("str")]);
    }

    #[test]
    fn a_branch_learns_from_the_row_that_made_its_test() {
        // `min` has a row of one operand before the one of two: the test of `min(a, b)` learns only from the second.
        let t = typed_of("def f(a, b):\n    if min(a, b):\n        return a\n    return 'n'\nprint(f(1, 2))\n");
        assert_eq!(compiler::types::runs(t.tree.ctxs.iter().find(|c| c.body == 1).unwrap().ret.tags), [code("int"), code("str")]);
    }

    #[test]
    fn module_code_keeps_its_types() {
        // A name deleted and read again ends: unbound alone holds no integer.
        assert_eq!(read("x = 5\ndel x\ntry:\n    print(x)\nexcept NameError:\n    print('gone')\n", 5, "NameError"), [code("type")]);
        let src = "x = 1\ny = x + 1\nprint(y)\n";
        assert_eq!(read(src, 3, "y"), [code("int")]);
        assert!(!typed_of(src).tree.wild);
    }

    #[test]
    fn a_function_handed_to_a_builtin_runs_only_where_its_row_calls_it() {
        let wild = |src: &str| typed_of(src).tree.wild;
        assert!(!wild("def f():\n    return 1\nprint(callable(f))\n"));
        assert!(wild("def f():\n    return 1\nprint(repr(f))\n"));
        // A builtin called through Call by its own line, and one with no line is code nothing knows.
        assert!(!wild("print(sorted([2, 1]))\n"));
        assert!(wild("def f(x):\n    return x\nprint(map(f, [1]))\n"));
        assert!(wild("print(hash(3))\n"));
        // `calls *` may call any operand, and a function among them runs.
        assert!(!wild("print(filter(None, [1]))\n"));
        assert!(wild("def f(x):\n    return x\nprint(filter(f, [1]))\n"));
    }

    #[test]
    fn every_program_of_the_corpus_gets_a_tree_the_reviewer_accepts() {
        let rules = Rules::read(&ept()).unwrap();
        let cases: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases/vm.json")).unwrap()).unwrap();
        let mut refused = Vec::new();
        for src in cases.iter().filter_map(|c| c["src"].as_str()) {
            let (chunk, errs) = compiler::code::compile(src);
            if errs.is_empty() && let Err(e) = typed(&chunk, &rules) { refused.push((e, src)); }
        }
        assert!(refused.is_empty(), "{} refused, the first: {:?}", refused.len(), refused.first());
    }

    #[test]
    fn a_file_of_other_sources_or_lines_is_refused() {
        assert!(Rules::read(&ept().replacen(compiler::types::read::SOURCES, &"0".repeat(64), 1)).is_err());
        assert!(Rules::read(&(ept() + "rule Add int int -> int | soon a0\n")).is_err());
        assert!(Rules::read(&(ept() + "frobnicate Add\n")).is_err());
        assert!(Rules::read(&(ept() + "tag int 1 1\n")).is_err(), "a tag is a name and one code");
        // The shipped file holds rules only once Spider Monkey's rules are copied in for these very sources.
        if let Err(e) = compiler::types::rules() { assert!(e.contains("other Edge sources"), "{e}"); }
    }
}
