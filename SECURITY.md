# Security

Thanks for taking the time to report a vulnerability. `interpreter` runs every program of a `.wasm` runtime in a sandbox, and that boundary is the security boundary. A native runtime runs outside it, as [wasm or native](docs/06-cli.mdx#wasm-or-native) says.

## Reporting

Email [c.sutton.dylan@gmail.com](mailto:c.sutton.dylan@gmail.com), and never open a public GitHub issue for a security report. Include a minimal reproducing script, the command that runs it, the expected and actual behavior, and the release version or commit hash.

## Scope

Everything in the repo is in scope, such as a sandbox escape, memory corruption or a budget bypass under `Limits::sandbox()`.

- The engine in `src/engine`, with its compiler, VM, grant logic and snapshot handling.
- `interpreter::lib` in `src/engine/lib`, the natives a runtime opts into.
- The CLI kit in `src/engine/cli` behind the `cli-*` features, with its wasmtime configuration and the commands it composes.
- `net` in `examples/net`, the network module both hosts serve.
- The plugin kit in `src/engine/plugin` and the ABI in `src/engine/abi`.
- The JS host in `js/`.
- Lox in `examples/lox`, the runtime `interpreter.wasm` is built from, and its `lox` binary.

A native runtime is out of scope past the warning the CLI prints and the child process it runs in.

## Response

Reports are acknowledged as soon as possible. Fixes land on the main branch and ship in the next release, and only the latest release and the main branch receive them.
