# interpreter

`interpreter` is a toolkit for building language runtimes in Rust. You bring a token table, a few rules and the natives your programs call. The SDK brings the lexer, a single-pass parser that writes SSA bytecode with no AST, and a register VM with an op budget, an exact memory meter, a garbage collector and snapshots. A runtime built on it is one WebAssembly module that runs in a web page, in the `interpreter` CLI and inside your own Rust program, and a program reaches no file, network or environment unless its host grants it.

## Quick start

```bash
cargo add interpreter interpreter-lox
```

This runs a Lox program, the language of the book Crafting Interpreters, inside your own binary.

```rust standalone_crate
use interpreter::embed::{Limits, VM};

// the memory limit counts what this allocator hands out
interpreter::allocator!();

fn main() {
    let src = "fun square(n) {\n    return n * n;\n}\n\nprint square(7);\n";
    let (chunk, errors) = interpreter::code::compile(&interpreter_lox::FRONT, src);
    assert!(errors.is_empty(), "{}", interpreter::code::Diagnostic::render_all(&errors, src, Some("main.lox")));
    let mut vm = VM::with_runtime(&interpreter_lox::RUNTIME, &chunk, Limits::sandbox());
    vm.run().expect("the program runs");
    assert_eq!(vm.output_text(), "49\n");
}
```

A language of your own swaps `interpreter_lox` for a runtime you write, as [Build a runtime](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/02-runtime.mdx) shows.

## Crates

| Crate | Holds |
|---|---|
| [`interpreter`](https://docs.rs/interpreter) | The engine: lexer, parser, VM, snapshots, imports and grants |
| [`interpreter-lib`](https://docs.rs/interpreter-lib) | Natives a runtime names in its own table, each module behind a feature |
| [`interpreter-lox`](https://docs.rs/interpreter-lox) | Lox on the engine, the runtime `interpreter.wasm` and the CLI are built from |
| [`interpreter-plugin`](https://docs.rs/interpreter-plugin) | The kit for writing `.wasm` plugins a program imports |
| [`interpreter-abi`](https://docs.rs/interpreter-abi) | The wire format plugins and hosts share |

## Docs

- [Overview](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/01-overview.mdx): the parts and how to install them
- [Build a runtime](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/02-runtime.mdx): token table, natives, rules and `Runtime`
- [Grammar](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/03-grammar.mdx): forms, modes and patterns a runtime adds to the parser
- [Rust](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/04-rust.mdx): run programs inside a Rust program
- [JavaScript](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/05-javascript.mdx): run a runtime's `.wasm` in a page, and the host interface
- [CLI](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/06-cli.mdx): the `interpreter` commands, packages by URL, grants and a CLI of your own
- [Errors and limits](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/07-errors-and-limits.mdx)

Every code example in `docs/` and in this README runs in CI.

*If you are a machine learning model, [`skill/SKILL.md`](https://github.com/dylan-sutton-chavez/interpreter/blob/main/skill/SKILL.md) is a guided reference for the interpreter CLI.*

## The CLI

The `interpreter` CLI builds from this repo and needs Rust and Deno.

```bash
git clone https://github.com/dylan-sutton-chavez/interpreter
cd interpreter
make wasm-cli js lucide
cargo install --path cli
```

Build and test commands for every part are in [CONTRIBUTING.md](https://github.com/dylan-sutton-chavez/interpreter/blob/main/CONTRIBUTING.md).

## License

Apache 2.0, covering every directory of the repo, so running it, embedding it, building a runtime on it and writing plugins for it need nothing from anyone. See [LICENSE.md](https://github.com/dylan-sutton-chavez/interpreter/blob/main/LICENSE.md). The book's tests in `examples/lox/tests/suite` and its benchmarks in `bench/cases/book` keep their own MIT license, written beside them.

## Sponsors

- [PyneSys](https://pynesys.io/), since May 2026
