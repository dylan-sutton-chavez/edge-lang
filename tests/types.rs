/* The types `edge build` proves, over the small edge.ept in cases/types.ept (the shipped one comes from Spider Monkey),
   each tree the reviewer accepts beside a broken copy it must refuse. */
mod test {
    use compiler::types::{check, read::Rules, typed};

    fn ept() -> String { std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/cases/types.ept")).expect("tests/cases/types.ept") }

    // The tags a read of `name` on `line` has, as code runs.
    fn read(src: &str, line: usize, name: &str) -> Vec<(u8, u8)> {
        let all = compiler::types::reads(src, &ept()).expect("the tree is accepted");
        all.into_iter().find(|r| r.0 == line && r.1 == name).unwrap_or_else(|| panic!("no read of {name} on line {line}")).2
    }

    // The code of tag `n`, a run of one.
    fn code(n: &str) -> (u8, u8) { Rules::read(&ept()).unwrap().tags.iter().find(|t| t.0 == n).map(|t| (t.1, t.1)).unwrap() }

    #[test]
    fn a_loop_keeps_its_ints_and_a_call_its_result() {
        let src = "def f(n):\n    s = 0\n    for i in range(n):\n        if i % 2 == 0:\n            s = s + i\n    return s\nx = f(10)\nprint(x)\n";
        assert_eq!(read(src, 5, "i"), [code("int")]);
        assert_eq!(read(src, 6, "s"), [code("int")]);
        assert_eq!(read(src, 8, "x"), [code("int")]);
    }

    #[test]
    fn two_branches_on_one_test_never_meet() {
        // x and y are each int or str, and the path with an int and a str is never taken.
        let src = "def f(n):\n    c = n < 3\n    x = 1 if c else 'a'\n    y = 2 if c else 'b'\n    return x + y\nprint(f(1))\n";
        let rules = Rules::read(&ept()).unwrap();
        let (chunk, _) = compiler::code::compile(src);
        let t = typed(&chunk, &rules).expect("accepted");
        let f = t.tree.ctxs.iter().find(|c| c.body == 1).expect("f has a context");
        assert_eq!(compiler::types::runs(f.ret.tags), [code("int"), code("str")]);
        assert_eq!(compiler::types::runs(f.raises.tags), [code("MemoryError")], "only the int sum may run out of memory, no TypeError");
    }

    #[test]
    fn isinstance_narrows_and_leaves_the_other_side_dead() {
        let src = "def f(v):\n    if isinstance(v, int):\n        return v + 1\n    return 'no'\nprint(f(2))\n";
        let rules = Rules::read(&ept()).unwrap();
        let (chunk, _) = compiler::code::compile(src);
        let t = typed(&chunk, &rules).expect("accepted");
        let dead = t.dead(1);
        assert!(dead.iter().any(|d| *d), "the str return is dead for an int argument");
        assert!(t.count().0 > 0);
        // A twin called with a str too keeps both returns.
        let (chunk, _) = compiler::code::compile(&src.replace("print(f(2))", "print(f(2), f('s'))"));
        assert!(!typed(&chunk, &rules).unwrap().dead(1).iter().any(|d| *d));
    }

    #[test]
    fn a_tree_that_misses_a_path_is_refused() {
        let src = "class P:\n    def __init__(self, x):\n        self.x = x\ndef g(p):\n    return p.x\nprint(g(P(3)))\n";
        let rules = Rules::read(&ept()).unwrap();
        let (chunk, _) = compiler::code::compile(src);
        let t = typed(&chunk, &rules).expect("accepted");
        assert!(check::types_check(&t.prog, &t.tree).is_ok());
        // Each broken copy: a state dropped, a result claimed narrower, a table entry claimed narrower.
        let mut a = typed(&chunk, &rules).unwrap();
        let (x, ip) = a.tree.ctxs.iter().enumerate().find_map(|(x, c)| c.seen.iter().skip(1).position(|s| !s.is_empty()).map(|ip| (x, ip + 1))).unwrap();
        a.tree.ctxs[x].seen[ip].clear();
        assert!(check::types_check(&a.prog, &a.tree).is_err());
        let mut b = typed(&chunk, &rules).unwrap();
        let at = b.prog.bodies.iter().position(|x| x.chunk.qualname == "g").unwrap() as u32;
        let g = b.tree.ctxs.iter_mut().find(|c| c.body == at).unwrap();
        g.ret = compiler::types::Ty::of(1 << code("str").0);
        assert!(check::types_check(&b.prog, &b.tree).is_err());
        let mut c = typed(&chunk, &rules).unwrap();
        for v in c.tree.tables.values_mut() { v.tags = 0; }
        assert!(check::types_check(&c.prog, &c.tree).is_err());
    }

    #[test]
    fn an_object_stays_off_the_heap_only_while_it_never_leaves() {
        let rules = Rules::read(&ept()).unwrap();
        let sites = |src: &str| { let (chunk, _) = compiler::code::compile(src); let t = typed(&chunk, &rules).unwrap(); check::no_heap(&t.prog, &t.tree).len() };
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
    }

    #[test]
    fn module_code_keeps_its_types() {
        let src = "x = 1\ny = x + 1\nprint(y)\n";
        assert_eq!(read(src, 3, "y"), [code("int")]);
        let rules = Rules::read(&ept()).unwrap();
        let (chunk, _) = compiler::code::compile(src);
        assert!(!typed(&chunk, &rules).unwrap().tree.wild);
    }

    #[test]
    fn a_file_of_another_commit_or_line_is_refused() {
        assert!(Rules::read(&ept().replacen("8855578768056f9e68dab3f2a3361e158ed7d9e2", "0000000000000000000000000000000000000000", 1)).is_err());
        assert!(Rules::read(&(ept() + "rule Add int int -> int | soon a0\n")).is_err());
        assert!(Rules::read(&(ept() + "frobnicate Add\n")).is_err());
        assert!(Rules::read(&(ept() + "tag int 1 1\n")).is_err(), "a tag is a name and one code");
    }
}
