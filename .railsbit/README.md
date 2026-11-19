# Verification

The engine is verified by rules written in Vfy, which [Railsbit](https://dev.railsbit.com) runs on the compiled crate. A rule proves a property for every input from every public function, or shows the input that breaks it. No new version is published until these rules hold.

Railsbit is in a closed beta at dev.railsbit.com, and interpreter is one of the projects it verifies.

| Property | What the rule proves |
|---|---|
| No undefined behaviour | Every `ub.` place a public function reaches is clear |
| No panic | No public function reaches `core::panicking` |
| No hang | Every loop and every recursion ends |

Each proof is exported as a Coq attestation, so it can be checked without trusting the engine that found it.

The first rule, in outline.

```vfy
pub let policy = {name = "no undefined behaviour", action = "fail", severity = "high"}

-- each place of undefined behaviour a public function reaches is proved clear or broken
pub fn check(g: Graph) -> Outcome =
  judged([at(g, n) for f in g.functions for n in f.body.under
          if starts(n.op, "ub.") and report.reachable(n)], "no input reaches undefined behaviour")

fn at(g: Graph, n: Node) -> Mark =
  if ok?(prove [before n] n.inputs[0] == 0 by walk | solver within work 20000) { clear() }
  else match search [before n] n.inputs[0] != 0 by bounds within work 20000 {
    ok lead => match run(lead) { ok t => hit(g, n, report.around(n), t.text), gap r => stay(r.reason) }
    gap r => stay(r.reason)
  }
```

The rules land in this folder as `.vfy` files, and every trace of the repo runs them.
