# Contributing

Thanks for taking the time to contribute. Security reports follow [SECURITY.md](SECURITY.md).

## Issues

Include a minimal failing script, the command used to run it, and the expected and actual behavior.

## Pull requests

For a large change, open an issue or email [c.sutton.dylan@gmail.com](mailto:c.sutton.dylan@gmail.com) first. Pull requests are welcome in every part under the Apache 2.0 License, see [README.md](README.md#license).

- New behavior comes with tests, and docs describe the code as it is after the change.
- Changes to the engine keep `make bench` and `make bench-layout` passing.
- `make check` passes before sending, and the maintainer runs CI once the pull request is open.

## Style

- A comment is one lowercase line with no colon, semicolon, dash or icon, at most one per block, and goes when it is redundant.
- No comment heads a file and no doc comment explains code, past the help text clap reads and the `# Safety` section clippy asks for.
- A doc edit rewrites the sentence or paragraph it changes instead of adding one beside it, so a page never grows past what is new.
- Docs use no colons, semicolons or em-dashes in their prose.
- A commit message is a type, then a lowercase sentence of the behavior the change leaves, with no period, body or trailer, as in `fix: a list pattern counts no line ends or comments as items`.

`make lint-comments` holds every comment of the repo to the first rule.

## Architecture

The crate `interpreter` is the one crate the repo publishes, with one `Cargo.toml`, one `Cargo.lock` and one version. Its engine in `src/` is laid out along the path a program takes from its source to the register loop, the parts beyond the engine sit beside it behind features, and `src/lib.rs` makes the crate of its directories.

Building from a clone takes Rust and Deno.

```bash
git clone https://github.com/dylan-sutton-chavez/interpreter
cd interpreter
make js
cargo install --path . --features cli
```

| Path | Holds |
|---|---|
| `src/syntax` | The lexer over the token table a runtime brings, the single-pass parser, the chunk it writes and its diagnostics |
| `src/code` | The opcode table `interpreter::ops!` declares, the fold pass and the calls tools compile through without running anything |
| `src/value` | Values and the heap with its sweep, FOREIGN kinds, equality and hashing, and the meter |
| `src/lang` | The `Language` a runtime implements, its natives and signatures, and the `Cx` they run with |
| `src/vm` | The lowering, the register loop and its driver, frames and sections, generators, the callbacks of natives and snapshots |
| `src/deps` | Manifests, locks and bundles, the module walk and the grant logic |
| `src/targets` | The exports of every runtime's `.wasm` and the allocator and `Script` a native binary boots a runtime with |
| `src/abi` | The wire format plugins and hosts share, the handle table and the bridge that answers plugins |
| `src/lib` | `interpreter::lib`, the kernels a runtime names in its own table, one module per feature |
| `src/plugin` | `interpreter::plugin`, the kit and the macros a `.wasm` plugin is written with |
| `src/cli` | `interpreter::cli` behind the `cli-*` features, the `Kit` with its hosts and each command, and `src/main.rs` the `interpreter` binary the `cli` feature wires |
| `src/util` | Hashing, the probe table, byte search, JSON escapes and the near names an error suggests, shared by the rest |
| `src/unsafe.rs` | Every unsafe region of the crate, each with the argument that it is sound |

Lox lives in `examples/lox`, a crate the repo never publishes, which `interpreter.wasm`, the tests of the CLI and the bench build on. The CLI's tests live in `tests/cli` and its two example compositions in `examples/cli`. `cli/setup` keeps the install scripts at the URL users fetch them from, and `cli/musl.sh` the static Linux build.

`Cargo.toml` denies unsafe code everywhere but `src/unsafe.rs`, and `make lint` fails when another file of `src/` holds unsafe code, past `src/plugin/macros.rs` whose unsafe expands in a plugin's own crate. `clippy.toml` keeps the engine from files, the network, the clock, the environment and maps with a random seed, which only the native host in `src/targets/native` and the CLI in `src/cli` may reach. A build with no `cli-*` feature compiles none of the CLI's dependencies, and `build.rs` does nothing for it. `make lint` also fails when `src/` past `src/cli` or `plugin/` holds a name `make/names.txt` denies, and builds each module of `interpreter::lib` alone so a module never leans on one it does not name.

## Terminology

The engine names its own pieces, and these words mean the same thing in every file.

| Word | Means |
|---|---|
| Front | What turns a source into a chunk, the parser over a token table or a front of a runtime's own |
| Chunk | The stack bytecode the parser writes, with its constants, names and nested functions |
| Kernel | A native of `interpreter::lib` with no name of its own until `interpreter::lib::table!` gives it one |
| Rows | The first tier, where a chunk runs as it was compiled with its operand stack in the frame's registers |
| Lowered | The second tier, where a hot loop or function runs as register code |
| Laid | The third tier, where a function called often runs with its callees inlined as sections |
| Section | A callee laid inside its caller's code behind a guard, entered and left as a jump |
| Pool | The caches and the windows of one chunk |
| Window | The run of registers one frame holds, reused at each level of its pool |
| Level | One depth of recursion of a pool, with the window it runs in and the registers a laid call keeps |
| Turn | One step of the driver between runs of the register loop |
| Floor | A class body the loop runs in the frame that defines it |
| Step | What a native returns to ask the loop for a call it never makes itself |
| Job | A native's callback running as loop frames until its caller resumes |
| Redo | An op that opened a frame for user code running again from its start, replaying what it already got |
| Site | The inline cache of one instruction |
| Shape | The layout of an instance's fields that sites key by |
| Walk | The import resolution a host drives step by step |
| Grant | What a package may reach, passed down the importer chain under the holders `all`, `main` and its import key |
| Kit | `interpreter::cli::Kit`, a command line over one runtime with the commands, fields and host modules wired into it |
| Composition | A binary that builds a kit, as `src/main.rs` does for `interpreter` and `examples/cli` for two smaller ones |

## Commands

> [!IMPORTANT]
> Every command runs from the repo root through GNU Make, on Linux, macOS or Windows, with the Rust release `rust-toolchain.toml` pins.

| Command | Does |
|---|---|
| `make check` | `make wasm`, `make lint` and `make test`, the bar before sending |
| `make wasm` | `interpreter.wasm`, while CI ships the smaller `make wasm-ship` |
| `make wasm-cli` | The speed build of `interpreter.wasm` the tests of the CLI and the bench run |
| `make native` | Lox as a native library and a crash build of it under `target/native/`, for the CLI's native tests |
| `make js` | `js/src` stripped to `js/dist`, with Deno |
| `make cli` | The `interpreter` binary with the `cli` feature, embedding `js` |
| `make cli-release TARGET=...` | A release binary, the static Linux one in Docker since musl-gcc has no C++ |
| `make lint-cli` | Clippy over the CLI with every `cli-*` feature, each component alone and the composition without wasmtime |
| `make lint` | The unsafe gate, the comment and name lints, shear, clippy for the host and wasm, the exports and the JS lints |
| `make test` | The suites of the engine, of `interpreter::lib` and of the test rig, then Lox's with the book's own test suite |
| `make bench` | The instruction count of every bench case against `bench/.snapshot` |
| `make bench-layout` | The same count on a build with its static data shifted |
| `make test-js`, `make test-cli SUITE=...` | The JS and CLI suites against a served tree |
| `cargo build --release` | The `.rlib` Rust embedders link |
| `cargo install --path . --features cli` | The `interpreter` binary from the clone |

## Testing

Every case runs under `Limits::sandbox()`, so a budget, memory or call-depth regression fails instead of hanging. The tests count memory through the same metering allocator as `interpreter.wasm`, and `tests/engine/memory.rs` checks that a run gives back every byte it took once its VM drops.

The engine reaches past what Lox spells, so its cases are written for `tests/rig`, a test runtime whose token table switches on the grammar kit over every kernel. It is no example and never ships. A behavior Lox shows is tested in Lox, in `examples/lox/tests`.

| Kind | Holds | Lives in |
|---|---|---|
| Engine | A program for the test rig with what it prints, raises, reads and sends | `tests/cases/vm`, one YAML file per topic, beside `snapshot.json`, `modules.json` and `abi.json` |
| Native | A kernel called on plain values, with its result or the words of its failure | `tests/lib/cases`, one YAML file per module |
| Syntax | The tokens a source lexes to, the bytecode it compiles to and what the lexer and the parser report | `tests/cases/syntax` |

Each test binary of the engine is a folder of `tests/` with its `main.rs`, `engine`, `lib` and `unmetered`, which runs without the metering allocator. `tests/cli` holds the CLI's test binaries, one root file each with its JSON and YAML cases and the `suite` they share, which `Cargo.toml` lists with the `cli-*` features each needs.

- A case states a rule, so its expected output comes from what the rule gives and never from what the engine printed last, and a quirk a case finds is a bug fixed with the case that holds it.
- A new engine case goes at the end of its topic file, so the places other files name stay put, and an existing case changes only with the rule it states.
- The bench snapshot on `main` holds the numbers of `main`, and it fails when the mean or a single case moves past its threshold. A change that makes cases faster takes the snapshot again with `make bench-update`, and one that makes a case slower says so with `make bench-accept-slower`.
- A bench count that moves with the static data means some lookup follows an address, and that lookup keys by an id or by the content instead.

The JS and CLI suites read the builds from a tree served on loopback, staged and served in a terminal of its own.

```bash
make stage serve
export INTERPRETER_CDN_BASE=http://127.0.0.1:8788
make browsers   # once
make test-js test-cli
```

## Docs

`docs/` holds MDX pages two folders deep at most, with a numeric prefix on every path segment. Each opens with a frontmatter holding a `title` and a `description` and has one top-level heading, the rules `interpreter build` holds package docs to. Every example runs. `make test-cli SUITE=docs` runs the `rust` blocks as doctests of the crate with the `cli` feature, `make test-cli SUITE=network` runs each `$ interpreter` transcript and each `lox` block against the `output` block after it, and `make test-js` runs each `js` block against the `text` block after it.

## CI

[`main.yml`](.github/workflows/main.yml) runs each part as a composite action that calls the same `make` targets, against the tree of builds it stages. Every push and pull request packs the crate with `make js` and `cargo publish --dry-run` in the wasm job, so the package carries `js/dist`, and only a tag `vX.Y.Z` equal to the crate's version runs the publish job, which puts it on crates.io once the tree is whole and `cargo semver-checks` finds no break of the last release's API that the version does not make a new major. Versions follow [SemVer](https://semver.org), and `cargo semver-checks -p interpreter` tells before a tag which part a change bumps. That job alone reads a secret and any other run skips it, so a pull request from a fork runs the rest whole.
