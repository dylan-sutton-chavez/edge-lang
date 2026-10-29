#[cfg(feature = "coverage")]
mod test {
    use edge::code::{ImportKind, OpCode, SSAChunk, Value};

    use crate::common::{lex, parser, TestResolver};

    const HELPER: &str = "fn double(n):\n    return n * 2\nfn shout(s):\n    return s.upper()\n";

    const MAIN: &str = "from helper import double, shout\nfn pick(c):\n    return 'taken' if c else 'skipped'\nfn both(a):\n    return a && 'short-circuited'\nprint(pick(true), both(0), double(2), shout('a'))\nif pick(true) == 'taken':\n    print('then')\nelse:\n    print('never')\nraise ValueError('stop')\nprint('after')\n";

    /* the ips loading the str `text` so a branch is found by what it loads */
    fn loads(chunk: &SSAChunk, text: &str) -> Vec<usize> {
        let at = chunk.constants.iter().position(|c| matches!(c, Value::Str(s) if s == text)).unwrap_or_else(|| panic!("no constant {text:?}"));
        let ips: Vec<usize> = chunk.instructions.iter().enumerate().filter(|(_, i)| i.opcode == OpCode::LoadConst && i.operand as usize == at).map(|(ip, _)| ip).collect();
        assert!(!ips.is_empty(), "no load of {text:?}");
        ips
    }

    fn function<'c>(chunk: &'c SSAChunk, name: &str) -> &'c SSAChunk {
        chunk.functions.iter().find(|f| chunk.names.get(f.3 as usize).is_some_and(|n| n.split('_').next() == Some(name))).map(|f| &f.1).unwrap_or_else(|| panic!("no function {name}"))
    }

    #[test]
    fn coverage_marks_each_instruction_that_ran_down_to_one_branch_of_a_line() {
        let resolver = Box::new(TestResolver::new().with_code("helper", HELPER).with_alias("helper", "helper"));
        let (tokens, _) = lex(MAIN);
        let (chunk, errors) = parser(MAIN, tokens.into_iter(), resolver).parse();
        assert!(errors.is_empty(), "{:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());

        let mut vm = crate::common::boot(&chunk, edge::embed::Limits::sandbox());
        assert!(vm.run().is_err(), "the run ends in the ValueError it raises");

        // inside one line only the branch a conditional or a short circuit took
        let pick = function(&chunk, "pick");
        assert!(loads(pick, "taken").iter().all(|&ip| vm.ran(pick, ip)));
        assert!(loads(pick, "skipped").iter().all(|&ip| !vm.ran(pick, ip)));
        let both = function(&chunk, "both");
        assert!(loads(both, "short-circuited").iter().all(|&ip| !vm.ran(both, ip)));

        // the taken branch ran but not the other one or the code past the raise
        assert!(loads(&chunk, "then").iter().all(|&ip| vm.ran(&chunk, ip)));
        assert!(loads(&chunk, "never").iter().all(|&ip| !vm.ran(&chunk, ip)));
        assert!(loads(&chunk, "stop").iter().all(|&ip| vm.ran(&chunk, ip)));
        assert!(loads(&chunk, "after").iter().all(|&ip| !vm.ran(&chunk, ip)));

        // the imported module answers under its own chunk with method calls left as written
        let helper = chunk.imports.iter().find_map(|entry| match &entry.kind { ImportKind::Code(sub) if entry.spec == "helper" => Some(sub), _ => None }).expect("helper imported as code");
        for name in ["double", "shout"] {
            let body = function(helper, name);
            assert!((0..body.instructions.len()).all(|ip| vm.ran(body, ip)), "{name} ran whole");
        }
        assert!(!vm.ran(&chunk, chunk.instructions.len()), "an ip past the chunk never ran");
    }
}
