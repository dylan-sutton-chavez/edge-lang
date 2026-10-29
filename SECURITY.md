# Security at Edge Lang

Thanks for taking the time to report a vulnerability. Edge Lang runs every program in a sandbox, so the sandbox boundary is the security boundary.

## Reporting

Email [c.sutton.dylan@gmail.com](mailto:c.sutton.dylan@gmail.com). Do not open a public GitHub issue for a security report.

Include a minimal reproducing script with the command used to run it, the expected and actual behavior, and the release version or commit hash.

## Scope

Everything in this repository is in scope, whether the issue is a sandbox escape, memory corruption, or a budget bypass under `Limits::sandbox()`.

- The engine in `src/`, its compiler, VM, grant logic and snapshot handling.
- `lib/`, the natives a runtime opts into.
- The CLI kit in `cli/`, with its wasmtime configuration and the commands it composes.
- `net` in `examples/net`, the network module both hosts serve.
- The plugin kit in `plugin/` and the ABI in `abi/`.
- The JS host in `js/`.
- The three runtimes in `examples/`, mini, tiny and uptime, with their CLIs, `edge`, `tiny` and `uptime`.

The only exception is the CDN, and the documentation website, which are served infrastructure rather than shipped code.

## Response

Reports are acknowledged as soon as possible. Fixes land on the main branch and ship in the next release.

Edge Lang has not reached 1.0. Some areas are on the roadmap to be hardened further, and there are no monetary rewards at this time. Reporters are credited by name in the release notes unless they prefer to stay anonymous.

Only the latest release and the main branch receive security fixes.
