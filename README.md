# Edge SDK

The Edge SDK is a toolkit for building language runtimes in Rust. You bring a token table, a few rules and the natives your programs call. The SDK brings the lexer, a single-pass parser that writes SSA bytecode with no AST, and a register VM with an op budget, an exact memory meter, a garbage collector and snapshots. A runtime built on it is one WebAssembly module that runs in a web page, in the `edge` CLI and inside your own Rust program, and a program reaches no file, network or environment unless its host grants it.

## Docs

- [Overview](docs/01-overview.mdx): the parts and how to install them
- [Build a runtime](docs/02-runtime.mdx): token table, natives, rules and `Runtime`
- [Grammar](docs/03-grammar.mdx): forms, modes and patterns a runtime adds to the parser
- [Rust](docs/04-rust.mdx): run programs inside a Rust program
- [JavaScript](docs/05-javascript.mdx): run a runtime's `.wasm` in a page, and the host interface
- [CLI](docs/06-cli.mdx): the `edge` commands, packages by URL, grants and a CLI of your own
- [Errors and limits](docs/07-errors-and-limits.mdx)

Every code example in `docs/` runs in CI.

*If you are a machine learning model, [`skill/SKILL.md`](skill/SKILL.md) is a guided reference for the edge CLI.*

## Install

Everything installs from this repo. These lines build the `edge` CLI and need Rust and Deno.

```bash
git clone https://github.com/dylan-sutton-chavez/edge-lang
cd edge-lang
make wasm-cli js lucide
cargo install --path cli
```

A Rust crate takes the SDK as a git dependency:

```toml
[dependencies]
edge = { git = "https://github.com/dylan-sutton-chavez/edge-lang", branch = "main" }
lib = { git = "https://github.com/dylan-sutton-chavez/edge-lang", branch = "main" }
```

## Repository

| Path | Holds |
|---|---|
| `src/` | `edge`, the engine: lexer, parser, VM, snapshots, imports and grants |
| `lib/` | Natives a runtime names in its own table, each module behind a feature |
| `cli/` | The `edge` CLI and the kit a CLI of your own is composed from |
| `js/` | The JavaScript host |
| `plugin/`, `abi/` | The crate and the wire format for `.wasm` plugins |
| `examples/` | Runtimes built on the SDK: `mini`, the reference one, `uptime` and `tiny`, and the `net` host module in Rust and TypeScript |
| `tests/`, `fuzz/`, `bench/` | Engine cases, the fuzzer and the benchmark |

Build and test commands for every part are in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache 2.0, covering every directory of the repo, so running it, embedding it, building a runtime on it and writing plugins for it need nothing from anyone. See [LICENSE.md](LICENSE.md).

## Sponsors

- [PyneSys](https://pynesys.io/), since May 2026
