# Contributing

Thanks for taking the time to contribute. Security reports follow [SECURITY.md](SECURITY.md).

## Issues

Include a minimal failing script, the command used to run it, and the expected and actual behavior.

## Pull requests

For a large change, open an issue or email [c.sutton.dylan@gmail.com](mailto:c.sutton.dylan@gmail.com) first. Pull requests are welcome in every part under the Apache 2.0 License, see [README.md](README.md#license).

- New behavior comes with tests, and docs describe the code as it is after the change.
- Changes to the engine keep `make bench` and `make bench-layout` passing.
- A change to the public API keeps the promise of [Stability](#stability).
- `make check` passes before sending, and the maintainer runs CI once the pull request is open.

## Style

- A comment is one lowercase line with no colon, semicolon, dash or icon, at most one per block, and goes when it is redundant.
- No comment heads a file, and no doc comment or `#[doc = "..."]` holds text in code. Docs live in `docs/`, `README.md` and this file, and the help text of a command is an `#[arg(help = "...")]`.
- A doc edit rewrites the sentence or paragraph it changes instead of adding one beside it, so a page never grows past what is new.
- Docs use no colons, semicolons or em-dashes in their prose.
- A commit message is a type, then a lowercase sentence of the behavior the change leaves, with no period, body or trailer, as in `fix: a list pattern counts no line ends or comments as items`.

`make lint-comments` holds every comment of the repo to the first rule.

## Architecture

The crate `interpreter` is the one crate the repo publishes, with one `Cargo.toml`, one `Cargo.lock` and one version. Its engine in `src/engine` is laid out along the path a program takes from its source to the register loop, the parts beyond the engine sit beside it behind features, and `src/lib.rs` makes the crate of its directories.

Building from a clone takes Rust and Deno.

```bash
git clone https://github.com/dylan-sutton-chavez/interpreter
cd interpreter
make js
cargo install --path . --features cli
```

| Path | Holds |
|---|---|
| `src/engine/syntax` | The lexer over the token table a runtime brings, the single-pass parser, the chunk it writes and its diagnostics |
| `src/engine/code` | The opcode table `ops!` declares, the fold pass and the calls tools compile through without running anything |
| `src/engine/value` | Values and the heap with its sweep, FOREIGN kinds, equality and hashing, and the meter |
| `src/engine/lang` | The `Language` a runtime implements, its natives and signatures, and the `Cx` they run with |
| `src/engine/vm` | The lowering, the register loop and its driver, frames and sections, generators, the callbacks of natives and snapshots |
| `src/engine/deps` | Manifests, locks and bundles, the module walk and the grant logic |
| `src/engine/targets` | The exports of every runtime's `.wasm` in `web`, and the allocator and `Script` a native binary boots a runtime with in `native` |
| `src/engine/abi` | The wire format plugins and hosts share, the handle table and the bridge that answers plugins |
| `src/engine/lib` | `interpreter::lib`, the kernels a runtime names in its own table, one module per feature |
| `src/engine/plugin` | `interpreter::plugin`, the kit and the macros a `.wasm` plugin is written with |
| `src/engine/cli` | `interpreter::cli` behind the `cli-*` features, the `Kit` with its hosts and each command |
| `src/main.rs` | The `interpreter` binary the `cli` feature wires |
| `src/engine/util` | Hashing, SHA-256, the probe table, byte search, JSON escapes and the near names an error suggests, shared by the rest |
| `src/engine/unsafe.rs` | Every unsafe region of the crate, each with the argument that it is sound |

The rest of the repo sits beside the crate and never ships in it, past the `js/dist`, `examples/net/net.rs` and `cli/setup/uninstall.sh` its package carries.

| Path | Holds |
|---|---|
| `examples/lox` | Lox, the crate `interpreter-lox` that `interpreter.wasm`, the tests of the CLI and the bench build on |
| `examples/cli` | `runner`, a composition of two commands |
| `examples/net` | The `net` host module of the CLI and the JS host, and the cases both run |
| `tests` | The engine's test binaries, the test rig in `tests/rig` and the CLI's tests in `tests/cli` |
| `bench` | The bench runner, its cases and `bench/.snapshot` |
| `plugin/example` | `slugify-mod`, the plugin the tests load |
| `js` | The JS host in `js/src`, its tests and the `js/dist` that `make js` writes |
| `cdn` | The scripts that stage and serve the tree of builds |
| `make` | The Deno scripts the Makefile runs, and `make/names.txt` |
| `cli` | The install scripts in `cli/setup`, at the URL users fetch them from, and `cli/musl.sh` for the static Linux build |
| `.railsbit` | [Verification](.railsbit/README.md), the properties the engine is to be proved against |

`Cargo.toml` denies unsafe code everywhere but `src/engine/unsafe.rs`, and `make lint` fails when another file of `src/` holds unsafe code, past `src/engine/plugin/macros.rs` whose unsafe expands in a plugin's own crate. `clippy.toml` keeps the engine from files, the network, the clock, the environment and maps with a random seed, which only the native host in `src/engine/targets/native` and the CLI in `src/engine/cli` may reach. A build with no `cli-*` feature compiles none of the CLI's dependencies, and `build.rs` does nothing for it. `make lint` also fails when `src/` past `src/engine/cli` or `plugin/` holds a name `make/names.txt` denies, and builds each module of `interpreter::lib` alone so a module never leans on one it does not name.

## Stability

The crate keeps the promise of [Stability](docs/09-stability.mdx), so a change within 1.x only adds.

- `make api` holds the public items to `tests/api.txt` and `tests/api-wasm.txt`. A change that adds items takes the listings again with `make api-update` and the pull request shows the new lines. A change that removes or alters a line waits for 2.0.
- A new public enum or struct with public fields is `#[non_exhaustive]`, which clippy enforces, and so is every variant with named fields, and `cargo semver-checks` refuses one added without it in a minor release. A type left frozen carries `#[expect(clippy::exhaustive_structs)]` or `#[expect(clippy::exhaustive_enums)]` with its reason, and [Stability](docs/09-stability.mdx#frozen-types) lists it.
- `Cargo.toml` denies `unreachable_pub` and `unnameable_types`, so an item is `pub` only when `src/lib.rs` exports it and every type a public signature shows has a path. Both hold in the release set of features and in the build with `unstable-internals`, the one that exports every `pub` item, and `make lint-rust` clippies both.
- An item with a better replacement is marked `#[deprecated]` with a note naming it, and stays.
- A feature only adds. A new one never changes what a build without it does, and a feature whose name starts with `unstable-` is the place for an API that may still move.
- A new plugin import or runtime export is a new name, such as `interpreter_op_v2`, that raises `PLUGIN_ABI`, `RUNTIME_ABI` or `NATIVE_ABI` of `interpreter::abi`. A name that shipped never changes its signature or its meaning.
- A new bundle format raises `FORMAT` in `src/engine/deps/bundle.rs` and keeps the reader of every older one. A change of the snapshot body raises `FORMAT` in `src/engine/vm/snapshot.rs`.
- `rust-version` rises only in a minor release, to a Rust release out for at least twelve weeks.

## Features

| Feature | Adds |
|---|---|
| `std` | The default, the native host in `interpreter::native` and every part that needs an operating system |
| `lib` | `interpreter::lib` with no kernel module, for a runtime that writes its own kernels |
| `bytes`, `data`, `date`, `io`, `iter`, `map`, `num`, `seq`, `set`, `spec`, `text`, `types` | One module of `interpreter::lib` each, with the modules it calls, and `types` brings `lib::reflect` too |
| `full` | Every module of `interpreter::lib` |
| `plugin` | `interpreter::plugin` |
| `exports` | The exports of a runtime's `.wasm` and of its native library, with the grant logic hosts read, written by `interpreter::exports!` |
| `coverage` | The executed instructions `VM::ran` reads, and with a `cli-*` feature the lines each native run executed |
| `cli`, `cli-*` | `interpreter::cli`, the kit and its commands, as [CLI](docs/06-cli.mdx#a-cli-of-your-own) lists |
| `unstable-internals`, `unstable-layout` | The engine's own tests and the shifted build of `make bench-layout`, with no promise |

A runtime crate names its own features as it likes. Lox's `web` turns on `interpreter/exports`.

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
| Composition | A binary that builds a kit, as `src/main.rs` does for `interpreter` and `examples/cli/runner.rs` for a smaller one |

## Commands

> [!IMPORTANT]
> Every command runs from the repo root through GNU Make, on Linux, macOS or WSL, with the Rust release `rust-toolchain.toml` pins.

| Command | Does |
|---|---|
| `make check` | `make wasm`, `make lint` and `make test`, the bar before sending |
| `make wasm` | Lox's `interpreter.wasm` in `target/wasm32-unknown-unknown/release` |
| `make wasm-ship` | `make wasm-small` and `make wasm-cli-opt`, the builds CI ships |
| `make wasm-small` | The small `interpreter.wasm` that ships, on nightly through `wasm-opt -Oz` |
| `make wasm-cli` | The speed build of `interpreter.wasm` the tests of the CLI and the bench run |
| `make wasm-cli-opt` | An optimized copy of the speed build, beside it as `interpreter-cli.wasm` |
| `make wasm-rig` | The test rig's `rig.wasm` the CLI's rig test runs |
| `make native` | Lox as a native library and a crash build of it under `target/native/`, for the CLI's native tests |
| `make size` | The raw and brotli size of the shipped `interpreter.wasm` |
| `make js` | `js/src` stripped to `js/dist`, with Deno |
| `make plugin` | `slugify-mod`, the example plugin the tests load |
| `make cli` | The `interpreter` binary with the `cli` feature, embedding `js/dist` |
| `make cli-release TARGET=...` | A release binary, the static Linux one in Docker since musl-gcc has no C++ |
| `make lint` | `make lint-rust` and `make lint-js` |
| `make lint-rust` | The unsafe gate, the comment and name lints, shear, clippy for the host and wasm, `lib` and each module of `interpreter::lib` alone and the exports |
| `make lint-comments` | Every comment of the repo against the first rule of [Style](#style) |
| `make lint-names` | The names `make/names.txt` denies in the engine and the example plugin |
| `make lint-js` | Deno's lint and type checks over `js/`, `make/` and `examples/net/` |
| `make lint-cli` | Clippy over the CLI with every `cli-*` feature, each component alone and the composition without wasmtime, then `cargo doc`, which fails on a broken link of the README the crate docs include |
| `make test` | The suites of the engine, of `interpreter::lib` and of the test rig, then Lox's with the book's own test suite |
| `make bench` | The instruction count of every bench case against `bench/.snapshot` |
| `make bench-update` | The snapshot taken again for a change that makes cases faster |
| `make bench-accept-slower` | The snapshot taken again for a change that makes a case slower and says so |
| `make bench-layout` | The same count on a build with its static data shifted |
| `make api` | The public API on the nightly the Makefile pins against `tests/api.txt` and `tests/api-wasm.txt` |
| `make api-update` | The same listings written again for a change that adds items |
| `make stage PARTS=...`, `make serve` | The tree of builds in `_cdn/`, and that tree served on loopback |
| `make browsers` | The Chromium the JS and `--web` tests drive, installed once |
| `make test-js`, `make test-cli SUITE=...` | The JS and CLI suites against a served tree, `SUITE` one of `engine`, `network`, `actors`, `consistency`, `docs` and `native`, or none for all |
| `make version TAG=vX.Y.Z` | Fails unless the tag is a valid version equal to the one in `Cargo.toml` |
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

`docs/` holds MDX pages two folders deep at most, with a numeric prefix on every path segment. Each opens with a frontmatter holding a `title` and a `description` and has one top-level heading, the rules `interpreter build` holds package docs to. `cargo test --features cli --lib docs::` holds `docs/` to them.

Every example runs. `make test-cli SUITE=docs` runs the `rust` blocks as doctests of the crate with the `cli` feature, `make test-cli SUITE=network` runs each `$ interpreter` transcript and each `lox` block against the `output` block after it, and `make test-js` runs each `js` block against the `text` block after it.

## CI

[`main.yml`](.github/workflows/main.yml) runs each part as a composite action that calls the same `make` targets, against the tree of builds it stages. Every push and pull request packs the crate with `make js` and `cargo publish --dry-run` in the wasm job, so the package carries `js/dist`. A push to `main` or a tag merges the tested parts into one tree, and a tag attaches it to its release as `cdn.tar.gz`.

Versions follow [SemVer](https://semver.org) and the promise of [Stability](docs/09-stability.mdx). The api job runs `make api` on every push and pull request, and the publish job runs `cargo semver-checks` for the host and for wasm32 so a tag that breaks the last release's API without a new major never ships. That job alone reads a secret and any other run skips it, so a pull request from a fork runs the rest whole.

## Releases

A release is a tag `vX.Y.Z` equal to the crate's version, and only that tag runs the publish job.

1. Set `version` under `[workspace.package]` in `Cargo.toml`, run `cargo update --workspace --offline` so `Cargo.lock` follows for the workspace crates alone, and commit both.
2. Run `cargo semver-checks -p interpreter`, which says which part of the version the change since the last release bumps.
3. Run `make api`, so the listings match the API the tag ships.
4. Run `make version TAG=vX.Y.Z`, then push the tag.

The wasm job checks the tag against the version and packs the crate with `cargo publish --dry-run`. The publish job checks the tag again, runs `cargo semver-checks` for the host and for wasm32 and puts the crate on crates.io. Its first step asks the crates.io index for the crate, and when the index holds no release of it, as for the first 1.0.0, it says so and both checks skip, since there is no release to compare against. `make api` guards that release alone.
