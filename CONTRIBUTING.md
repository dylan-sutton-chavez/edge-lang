# Contributing

Thanks for taking the time to contribute. Security reports follow [SECURITY.md](SECURITY.md).

## Issues and pull requests

An issue holds a minimal failing script, the command that runs it, and the expected and actual behavior. For a large change, open an issue or email [c.sutton.dylan@gmail.com](mailto:c.sutton.dylan@gmail.com) first. Contributions are under the Apache 2.0 License.

- New behavior comes with tests, and the docs describe the code as it is after the change.
- `make check`, `make bench` and `make bench-layout` pass before sending.
- A change to the public API keeps the promise of [Stability](docs/09-stability.mdx).

## Style

- A comment is one lowercase line with no colon, semicolon, dash or icon, at most one per block, and no comment heads a file. `make lint-comments` holds the repo to it.
- Code holds no doc text. Docs live in `docs/`, `README.md` and this file.
- Docs use plain sentences with no colons or semicolons, and an edit rewrites the sentence it changes.
- A commit message is a type and a lowercase sentence of the behavior the change leaves, with no period, body or trailer, as in `fix: a list pattern counts no line ends or comments as items`.

## Layout

`interpreter` is the one crate the repo publishes. `src/lib.rs` is its whole public API, and the engine lives privately in `src/engine`.

| Path | Holds |
|---|---|
| `src/engine/syntax` | The lexer, the single-pass parser, the chunk it writes and its diagnostics |
| `src/engine/code` | The opcode table, the fold pass and the compile calls of tools |
| `src/engine/value` | Values, the heap with its sweep, foreign kinds and the meter |
| `src/engine/lang` | The `Language` a runtime implements, its natives and the `Cx` they run with |
| `src/engine/vm` | The lowering, the register loop, frames, generators and snapshots |
| `src/engine/deps` | Manifests, locks, bundles, the module walk and grants |
| `src/engine/targets` | The exports of a runtime's `.wasm` and the native host |
| `src/engine/abi` | The wire format plugins and hosts share |
| `src/engine/lib` | `interpreter::lib`, one module of kernels per feature |
| `src/engine/plugin` | `interpreter::plugin`, the kit a `.wasm` plugin is written with |
| `src/engine/cli` | `interpreter::cli`, the kit and its commands |
| `src/engine/unsafe.rs` | Every unsafe region of the crate, each with the argument that it is sound |
| `src/main.rs` | The `interpreter` binary |
| `examples/lox` | Lox, the runtime `interpreter.wasm`, the CLI tests and the bench build on |
| `tests` | The engine's test binaries, the test rig in `tests/rig` and the CLI's tests in `tests/cli` |
| `js`, `cdn` | The JS host, and the scripts that stage and serve builds |
| `bench`, `plugin/example`, `make`, `cli/setup` | The bench, the example plugin, the Makefile's Deno scripts and the install scripts |

`Cargo.toml` denies unsafe code outside `src/engine/unsafe.rs`. `clippy.toml` keeps the engine away from files, the network, the clock and the environment, which only the native host and the CLI reach.

## Words

| Word | Means |
|---|---|
| Front | What turns a source into a chunk |
| Chunk | The bytecode the parser writes, with its constants, names and functions |
| Kernel | A native of `interpreter::lib` that `lib::table!` names |
| Rows, lowered, laid | The three tiers, as compiled, as register code, and with callees inlined as sections |
| Section | A callee laid inside its caller behind a guard |
| Window, level, pool | The registers of a frame, one depth of recursion, and the caches of one chunk |
| Step, job, redo | A call a native asks the loop for, a native's callback run as frames, and an op run again from its start |
| Site, shape | The inline cache of one instruction, and the field layout it keys by |
| Walk, grant | Import resolution step by step, and what a package may reach |
| Kit | `interpreter::cli::Kit`, a command line over one runtime |

## Commands

Every command runs from the repo root through GNU Make, on Linux, macOS or WSL, with the toolchain `rust-toolchain.toml` pins.

| Command | Does |
|---|---|
| `make check` | `make wasm`, `make lint` and `make test`, the bar before sending |
| `make test` | The engine, `interpreter::lib`, the test rig and Lox with the book's test suite |
| `make lint` | `make lint-rust` and `make lint-js` |
| `make lint-rust` | The unsafe gate, the comment and name lints, shear, and clippy for every feature set and wasm |
| `make lint-comments`, `make lint-names`, `make lint-js` | The comment rule, the denied names, and Deno's checks |
| `make lint-cli` | Clippy over every `cli-*` feature and `cargo doc` |
| `make bench`, `make bench-layout` | Each bench case's instruction count against `bench/.snapshot`, and the same with static data shifted |
| `make bench-update`, `make bench-accept-slower` | The bench snapshot taken again after a change that is faster, or one that is slower and says so |
| `make api`, `make api-update` | The public API against `tests/api.txt` and `tests/api-wasm.txt`, and the listings taken again after a change that adds items |
| `make wasm`, `make wasm-cli`, `make wasm-ship` | Lox's `interpreter.wasm`, its speed build for the tests and the bench, and the builds CI ships |
| `make wasm-rig`, `make native`, `make plugin` | The test rig's `.wasm`, Lox as a native library, and the example plugin |
| `make js` | `js/dist` from `js/src`, with Deno |
| `make cli`, `make cli-release TARGET=...` | The `interpreter` binary, and a release build of it |
| `make stage serve`, `make browsers` | The tree of builds served on loopback, and the Chromium the browser tests drive |
| `make test-js`, `make test-cli SUITE=...` | The JS and CLI suites against a served tree, `SUITE` one of `engine`, `network`, `actors`, `consistency`, `docs` and `native` |
| `make version TAG=vX.Y.Z` | Fails unless the tag equals the version in `Cargo.toml` |

The JS and CLI suites read their builds from a tree served in a terminal of its own.

```bash
make stage serve
export INTERPRETER_CDN_BASE=http://127.0.0.1:8788
make browsers   # once
make test-js test-cli
```

## Tests

Every case runs under `Limits::sandbox()`, so a regression fails instead of hanging. The engine's cases are written for `tests/rig`, a test runtime with every kernel, and a behavior Lox shows is tested in `examples/lox/tests`.

| Kind | Lives in |
|---|---|
| A program with what it prints, raises, reads and sends | `tests/cases/vm`, one YAML file per topic |
| A kernel on plain values with its result or its failure | `tests/lib/cases`, one YAML file per module |
| The tokens and bytecode of a source and its diagnostics | `tests/cases/syntax` |

- A case states a rule, so its expected output comes from the rule and never from what the engine printed last.
- A bench count that moves with the static data means a lookup follows an address, and it keys by an id or the content instead.
- Every docs example runs. `make test-cli SUITE=docs` runs the Rust blocks, `SUITE=network` the transcripts and Lox blocks, and `make test-js` the JS blocks.

## Releases

A release is a tag `vX.Y.Z` equal to the crate's version, and only a tag runs the publish job.

1. Set `version` in `Cargo.toml`, run `cargo update --workspace --offline`, and commit both.
2. Run `cargo semver-checks -p interpreter` to see which part of the version the change bumps.
3. Run `make api` and `make version TAG=vX.Y.Z`, then push the tag.

CI checks the tag, runs `cargo semver-checks` for the host and for wasm32, and publishes the crate.
