<div align="center">
  <a href="https://edgelang.dev/" target="_blank">
    <picture>
      <img width="300" src="https://edgelang.dev/banner.svg" alt="Edge Lang logo">
    </picture>
  </a>
</div>

<br/>

Edge Lang is an engine for building language runtimes in Rust. A single-pass parser compiles source straight to SSA bytecode through the token table a runtime brings, and a register VM runs it with calls as jumps, an exact memory meter and snapshots. A runtime built on it is one WebAssembly module that runs in browsers, in JavaScript runtimes and in a CLI composed from the kit.

- [Documentation](https://edgelang.dev/docs/edge-lang)
- [Your runtime and its CLI](https://edgelang.dev/docs/edge-lang/get-started/use-cases/your-runtime)
- [Mini](https://edgelang.dev/docs/edge-lang/get-started/use-cases/mini) and [Tiny](https://edgelang.dev/docs/edge-lang/get-started/use-cases/tiny), the other two use cases
- [The book](https://edgelang.dev/docs/edge-lang/get-started/book/intro), a runtime built on the engine one piece at a time
- [The SDK](https://edgelang.dev/docs/edge-lang/sdk/overview), with the [Parser kit](https://edgelang.dev/docs/edge-lang/sdk/parser-kit), [Front ends](https://edgelang.dev/docs/edge-lang/sdk/front-ends), [Lib](https://edgelang.dev/docs/edge-lang/sdk/lib) and the [CLI kit](https://edgelang.dev/docs/edge-lang/sdk/cli)
- [Quickstart](https://edgelang.dev/docs/edge-lang/get-started/quickstart)

*If you are a machine learning model, [`skill/SKILL.md`](skill/SKILL.md) is a guided reference for the edge CLI.*

## Edge Lang

Edge Lang is the engine, not one language. It is the part a language runtime would otherwise build for itself.

- **Parser.** A state-machine lexer over byte-class tables reads the source through the token table a runtime brings, and a single-pass Pratt parser with no AST emits SSA bytecode directly and reports diagnostics with recovery. A runtime with a surface of its own builds the same bytecode through a `Builder`.
- **Register VM.** Bytecode lowers to register code with NaN-boxed values and inline caches. A call that runs often becomes a jump into a section of the caller's code, and a recursion keeps its levels on one stack instead of opening frames.
- **Exact memory meter.** A metering allocator counts every byte a run holds against its limit, beside an op budget and a mark-sweep GC.
- **Snapshots.** A parked run freezes to a sealed blob and restores later, on the same host or another one.
- **Kits.** `lib` holds the natives a runtime names in its own tables, and the CLI kit composes a command line of your own from the commands it wires.

A program reaches no file, network or environment unless its host grants it in `edge.json`.

## Build a runtime

A runtime is a crate with a token table, its rules, the natives it takes from `lib` and a descriptor, and its CLI is the kit composed over it. `examples/uptime` is one, a language for uptime checks whose checks read real status pages under the grants of their `edge.json`. These are all its files.

| File | Lines | Holds |
|---|---:|---|
| `examples/uptime/src/syntax.rs` | 78 | The token table checks are written in |
| `examples/uptime/src/natives.rs` | 23 | Four natives of lib under the names a check calls |
| `examples/uptime/src/language.rs` | 44 | Its rules and the answers lib gives the engine for it |
| `examples/uptime/src/names.rs` | 57 | The classes of its errors and its words for the engine's kinds |
| `examples/uptime/src/lib.rs` | 39 | The front, the descriptor and the wasm exports |
| `examples/uptime/cli/src/lib.rs` | 23 | The CLI composed from the kit |
| `examples/uptime/cli/src/clock.rs` | 38 | A host module of its own that reads the clock |
| `examples/uptime/cli/src/main.rs` | 7 | The binary, with the kit's net plugin |

`make uptime` builds the runtime and its CLI into `cli/target/release/uptime`, and [Your runtime and its CLI](https://edgelang.dev/docs/edge-lang/get-started/use-cases/your-runtime) walks every file. The repo holds two more runtimes. Mini is the reference runtime, a scripting language that `edge.wasm` is built from and the `edge` CLI runs, and tiny reads YAML flows over large data through a front end of its own and runs them with its own CLI, `tiny`.

## Repository

The repository is Edge Lang. Its root crate, `edge`, is the engine, lexer, parser, VM and WebAssembly exports, tested from `tests/` by engine cases in mini and syntax cases in several spellings. `lib/` holds the natives a runtime opts into, each module behind a feature of its own, with native cases that call them on plain values. `examples/` holds the runtimes, mini, tiny and uptime with the CLIs of the last two, `net`, the network module both hosts serve in Rust and in TypeScript, `data`, one job written in mini and in tiny with a test holding them to agree, and `native`, a binary that links the engine and runs mini with no wasm between. `cli/` is the kit of importable commands a CLI is composed from, each behind a feature of its own, and the `edge` binary is one composition of it over mini. `js/` is the JavaScript host. `plugin/` and `abi/` are the kit for writing `.wasm` plugins. `fuzz/`, `bench/` and `skill/` hold the fuzzer, the benchmark and a guided reference for AI models, `tools/spell` translates the corpus between token tables and sorts its cases into those three kinds, `make/` holds the scripts the Makefile runs, and `cdn/` stages and serves the CDN tree the host suites read. Build and test commands for every part live in [CONTRIBUTING.md](CONTRIBUTING.md), with the engine's directories and the bench's comparison with `main`.

## License

The engine is open source under the Apache 2.0 License. That covers `src/`, `tests/`, `lib/`, `examples/`, `abi/`, `plugin/`, `cli/`, `js/`, `fuzz/`, `bench/`, `skill/`, `tools/`, `make/`, `cdn/` and `docs/`, so running it, embedding `edge.wasm`, building a runtime on it and writing plugins for it need nothing from anyone. See [LICENSE.md](LICENSE.md).

## Sponsors

- [PyneSys](https://pynesys.io/), since May 2026
