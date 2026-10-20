#[cfg(feature = "coverage")]
mod test {
    use compiler::lexer::lex;
    use compiler::code::{ImportKind, OpCode, Parser, SSAChunk, Value};
    use compiler::embed::VM;

    use crate::common::TestResolver;

    const HELPER: &str = "def double(n):\n    return n * 2\ndef shout(s):\n    return s.upper()\n";

    const MAIN: &str = "\
from helper import double, shout
def pick(c):
    return 'taken' if c else 'skipped'
def both(a):
    return a and 'short-circuited'
print(pick(True), both(0), double(2), shout('a'))
if pick(True) == 'taken':
    print('then')
else:
    print('never')
raise ValueError('stop')
print('after')
";

    /* The ips of `chunk` that load the str constant `text`, so a branch is found by what it loads and not by its bytecode. */
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
        let (chunk, errors) = Parser::with_resolver(MAIN, tokens.into_iter(), resolver).parse();
        assert!(errors.is_empty(), "{:?}", errors.iter().map(|d| &d.msg).collect::<Vec<_>>());

        let mut vm = VM::new(&chunk);
        assert!(vm.run().is_err(), "the run ends in the ValueError it raises");

        // Inside one line, only the branch a conditional or a short-circuit took.
        let pick = function(&chunk, "pick");
        assert!(loads(pick, "taken").iter().all(|&ip| vm.ran(pick, ip)));
        assert!(loads(pick, "skipped").iter().all(|&ip| !vm.ran(pick, ip)));
        let both = function(&chunk, "both");
        assert!(loads(both, "short-circuited").iter().all(|&ip| !vm.ran(both, ip)));

        // A statement on the branch taken ran, the other branch and what follows the raise did not.
        assert!(loads(&chunk, "then").iter().all(|&ip| vm.ran(&chunk, ip)));
        assert!(loads(&chunk, "never").iter().all(|&ip| !vm.ran(&chunk, ip)));
        assert!(loads(&chunk, "stop").iter().all(|&ip| vm.ran(&chunk, ip)));
        assert!(loads(&chunk, "after").iter().all(|&ip| !vm.ran(&chunk, ip)));

        // An imported module answers under its own chunk, found in the same tree.
        let helper = chunk.imports.iter().find_map(|entry| match &entry.kind { ImportKind::Code(sub) if entry.spec == "helper" => Some(sub), _ => None }).expect("helper imported as code");
        // A method call runs as the compiler wrote it, every one of its instructions marked.
        for name in ["double", "shout"] {
            let body = function(helper, name);
            assert!((0..body.instructions.len()).all(|ip| vm.ran(body, ip)), "{name} ran whole");
        }
        assert!(!vm.ran(&chunk, chunk.instructions.len()), "an ip past the chunk never ran");
    }
}
