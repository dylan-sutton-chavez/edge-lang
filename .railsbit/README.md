# Verification

This folder holds the rules the engine is to be proved against. They are written in Vfy, which [Railsbit](https://dev.railsbit.com) runs on the compiled crate. A rule proves a property for every input of every public function, or shows the input that breaks it, and covers every release after it lands. Railsbit is in a closed beta, and interpreter is one of the projects it is to verify.

| Property | What the rule proves |
|---|---|
| No undefined behavior | Every `ub.` place a public function reaches is clear |
| No panic | No public function reaches `core::panicking` |
| No hang | Every loop and every recursion ends |

Each proof is exported as a Coq attestation, so it can be checked without trusting the engine that found it. Each rule lands here as a `.vfy` file. None has landed yet, so the first is shown in outline.

```vfy
pub let policy = {name = "no undefined behavior", action = "fail", severity = "high"}

-- each place of undefined behavior a public function reaches is proved clear or broken
pub fn check(g: Graph) -> Outcome =
  judged([at(g, n) for f in g.functions for n in f.body.under
          if starts(n.op, "ub.") and report.reachable(n)], "no input reaches undefined behavior")

fn at(g: Graph, n: Node) -> Mark =
  if ok?(prove [before n] n.inputs[0] == 0 by walk | solver within work 20000) { clear() }
  else match search [before n] n.inputs[0] != 0 by bounds within work 20000 {
    ok lead => match run(lead) { ok t => hit(g, n, report.around(n), t.text), gap r => stay(r.reason) }
    gap r => stay(r.reason)
  }
```
