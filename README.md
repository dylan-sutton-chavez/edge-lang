# interpreter

`interpreter` is a Rust crate for building language runtimes. You write a token table, the rules of your language and the natives its programs call. The crate gives you a lexer, a single-pass parser that writes bytecode with no AST, and a register VM with an op budget, an exact memory meter, a garbage collector and snapshots.

A runtime compiles to one WebAssembly module that runs in a web page, in the `interpreter` CLI and inside a Rust program. A program reaches no file, network or environment unless its host grants it.

## Install

```bash
cargo add interpreter --features full
```

The CLI builds from this repo with Rust and Deno.

```bash
git clone https://github.com/dylan-sutton-chavez/interpreter
cd interpreter
make wasm-cli js lucide
cargo install --path cli
```

## Features

| Feature | Turns on |
|---|---|
| `std` | The standard library, on by default |
| `bytes`, `data`, `date`, `io`, `iter`, `map`, `num`, `reflect`, `seq`, `set`, `spec`, `text`, `types` | One module of `interpreter::lib`, the ready natives a runtime names in its own table |
| `full` | Every module of `interpreter::lib` |
| `plugin` | `interpreter::plugin`, for writing `.wasm` plugins a program imports |
| `runtime`, `perms` | The exports of a runtime's `.wasm` and the grant logic its hosts read |

## Docs

| Page | Covers |
|---|---|
| [Overview](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/01-overview.mdx) | What the crate holds and how to install it |
| [Build a runtime](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/02-runtime.mdx) | A whole runtime, its token table, natives and language |
| [Grammar](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/03-grammar.mdx) | Forms, modes and patterns a runtime adds to the parser |
| [Rust](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/04-rust.mdx) | Running programs inside a Rust program |
| [JavaScript](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/05-javascript.mdx) | Running a runtime's `.wasm` in a page, and the ABI any host drives |
| [CLI](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/06-cli.mdx) | The commands, packages, grants and a CLI of your own |
| [Errors and limits](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/07-errors-and-limits.mdx) | How errors read and every limit a run has |
| [Plugins](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/08-plugins.mdx) | Writing a `.wasm` plugin |

[`examples/lox`](https://github.com/dylan-sutton-chavez/interpreter/tree/main/examples/lox) is Lox, the language of the book Crafting Interpreters, as a whole runtime. It passes the book's test suite and is the one `interpreter.wasm` is built from. The crate and the CLI hold no runtime, and a project names the `.wasm` or the native library it runs on.

## License

Apache 2.0 for every directory of the repo, see [LICENSE.md](https://github.com/dylan-sutton-chavez/interpreter/blob/main/LICENSE.md). The book's tests in `examples/lox/tests/suite` and its benchmarks in `bench/cases/book` keep their own MIT license, written beside them.

## Sponsors

- [PyneSys](https://pynesys.io/), since May 2026
