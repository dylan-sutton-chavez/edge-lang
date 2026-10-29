# Contributing to Edge Lang

Thanks for taking the time to contribute. Security reports follow [SECURITY.md](SECURITY.md).

## Issues

Include a minimal failing script, the command used to run it, and the expected and actual behavior.

## Pull Requests

For a large change, open an issue or email [c.sutton.dylan@gmail.com](mailto:c.sutton.dylan@gmail.com) first. Pull requests are welcome in every part under the Apache 2.0 License, see [README.md](README.md#license).

- New behavior comes with tests, and docs describe the code as it is after the change.
- Changes to the CLI or package behavior update `skill/SKILL.md`.
- Changes to the engine keep `make bench` and `make bench-layout` passing, and significant ones run the [fuzzer](https://edgelang.dev/docs/edge-lang/internals/runbook).

Run `make check` before sending, and the maintainer runs CI once the pull request is open.

## Style

Comments are one line, at most one per block, and deleted when redundant. No file-header comment or docstring. A doc edit rewrites the sentence or paragraph it changes instead of adding one beside it, so a page never grows past what is new. Comments and docs use no colons, semicolons, or em-dashes.

## Architecture

The engine in `src/` is laid out along the path a program takes from its source to the register loop, and `src/lib.rs` makes the crate `edge` of its directories.

| Path | Holds |
|---|---|
| `src/syntax` | The lexer over the token table a runtime brings, the single-pass parser, the chunk it writes and its diagnostics |
| `src/code` | The opcode table `edge::ops!` declares, the fold pass and the calls tools compile through without running anything |
| `src/value` | Values and the heap with its sweep, FOREIGN kinds, equality and hashing, and the meter |
| `src/lang` | The `Language` a runtime implements, its natives and signatures, and the `Cx` they run with |
| `src/vm` | The lowering, the register loop and its driver, frames and sections, generators, the callbacks of natives and snapshots |
| `src/deps` | Manifests, locks and bundles, the module walk and the grant logic |
| `src/targets` | The exports of every runtime's `.wasm` and the allocator and `Script` a native binary boots a runtime with |
| `src/abi.rs`, `src/abi` | The wire format of plugins and the bridge that answers them |
| `src/util` | Hashing, the probe table, byte search, JSON escapes and the near names an error suggests, shared by the rest |
| `src/unsafe.rs` | Every unsafe region of the engine, each with the argument that it is sound |

`src/lib.rs` denies `unsafe_code` everywhere else, and `make lint` fails when another file of `src/` holds unsafe code or when `src/`, `lib/src` or `plugin/` holds a name `make/names.txt` denies. [Overview](https://edgelang.dev/docs/edge-lang/sdk/overview) follows a program through these directories, and [Design](https://edgelang.dev/docs/edge-lang/internals/design) covers the loop and the lowering in depth.

## Building

> [!IMPORTANT]
> Every command runs from the repo root through GNU Make, on Linux, macOS or Windows.

```bash
make wasm       # edge.wasm while CI ships the smaller make wasm-ship
make wasm-cli   # the speed build the CLI embeds
make js         # js/src stripped to js/dist
make cli        # the edge binary, embedding wasm-cli, js and the Lucide names
make lint
cargo build --release   # the .rlib Rust embedders link
```

`make js`, `make lucide` and so `make cli` need Deno, and `make lucide` fetches the icon names Lucide publishes over the network. `make cli-release TARGET=x86_64-unknown-linux-musl` builds the static Linux binary in Docker, since musl-gcc has no C++. Every build uses the one Rust release `rust-toolchain.toml` pins.

## Testing

`make test` runs the engine's suites, lib's and those of the runtimes in `examples`. Every case runs under `Limits::sandbox()`, so a budget, memory, or call-depth regression fails instead of hanging. The tests count memory through the same metering allocator as `edge.wasm`, and `tests/memory.rs` checks that a run gives back every byte it took once its VM drops.

New behavior comes with cases of three kinds. A case states a rule, so its expected output comes from what the rule gives and never from what the engine printed last, and a quirk a case finds is a bug, fixed with the case that holds it.

| Kind | Holds | Lives in |
|---|---|---|
| Engine | A program in mini with what it prints, raises, reads and sends | `tests/cases/vm`, one YAML file per topic, beside `tests/cases/snapshot.json` and `tests/cases/modules.json` |
| Native | A kernel of lib called on plain values, with its result or the words of its failure | `lib/tests/cases`, one YAML file per module of lib |
| Syntax | Sources that compile to one program in several spellings, the tokens they lex to, and what the lexer and the parser report | `tests/cases/syntax` |

A new engine case goes at the end of its topic file, so the places other files name stay put, and an existing case changes only with the rule it states. [Testing](https://edgelang.dev/docs/edge-lang/sdk/testing) covers the fields of each kind and every test that runs them.

`make bench` counts the WebAssembly instructions each case of `tests/cases/vm` executes, so `bench/.snapshot` keeps the same `[instructions, MB]` per case on every machine. The snapshot on `main` holds the numbers of `main`, so the bench of a branch is its comparison with `main`, case by case and in the geometric mean. It fails when the snapshot was taken with another Rust, a case and its entry do not pair, or the mean or a single case moves past its threshold, the cases that grew listed apart from those that shrank. A change that makes cases faster takes the snapshot again with `make bench-update` in the same pull request. That refuses a case that grew past its threshold, and `make bench-accept-slower` takes it too, so a pull request that makes a case slower says so outright.

`make bench-layout` runs every case on `edge.wasm` and on a build with its static data shifted, by a padding static of 64 bytes and a global base 64 bytes higher, and fails when a case runs another count on the two. A count that moves with the data means that some lookup follows an address, and such a lookup keys by an id or by the content instead. CI runs it after the bench.

`make miri SUITE=vm` runs one suite under Miri, and `CASE` one test of it by its exact name, as CI does around the clock. Miri takes hours to read the whole corpus, so under Miri the corpus tests run the sample `tests/cases/miri.tsv` names, a case on each line by its key, its topic file and its place there. The sample runs every opcode the corpus runs and reaches the unsafe sites the corpus reaches on a budget of 5,000 ops, and two tests of `make test` hold it to the corpus.

- `miri::test::the_sample_names_cases_of_the_corpus` fails once a sampled case moves or changes.
- `miri::test::coverage_of_the_sample_holds_every_opcode_the_corpus_runs` fails once a corpus case runs an opcode no sampled case runs, and names the cheapest case to add for it as a line of the file.

When the first one fails, this writes the file again with the key and the place of each case it names.

```bash
MIRI_TSV_OUT=tests/cases/miri.tsv cargo test --locked -p edge --test tests miri::test::the_sample_names_cases_of_the_corpus
```

> [!NOTE]
> The JS and CLI suites read the builds from a CDN served on loopback, the lock cases of the CLI serve their own registry through `EDGE_SITE_BASE`, and the skill suite reaches no server. Production answers only the registry packages the JS suite imports, `edge add` of an unknown name and `edge publish` with a bad token.

First, stage the builds and serve them in a terminal of their own.

```bash
make stage serve
```

Next, point the suites at it and run them.

```bash
export EDGE_CDN_BASE=http://127.0.0.1:8788
make browsers   # once
make test-js test-cli test-skill
```

`make fuzz` runs a fuzzing campaign, as CI does around the clock.

## Docs

`docs/` holds MDX pages two folders deep at most, with a numeric prefix on every path segment. Each opens with a frontmatter holding a `title` and a `description` and has one top-level heading, the rules `edge build` holds package docs to. An `edge` block followed by an `output` block becomes a playground on the real engine, and a blockquote opening with a marker such as `[!NOTE]`, `[!QUESTION]`, `[!CARDS]` or `[!COPY]` becomes a box, as [Registry](https://edgelang.dev/docs/edge-lang/reference/registry#package-docs) lists.

## CI

[`main.yml`](.github/workflows/main.yml) runs each part as a composite action that calls the same `make` targets, against the CDN tree it stages. Nothing in it holds a secret, so a pull request from a fork runs it whole.
