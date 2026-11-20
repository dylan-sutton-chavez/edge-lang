# interpreter

`interpreter` is a Rust crate for building language runtimes. You write a token table, the rules of your language and the natives its programs call, and the crate gives you a lexer, a single-pass parser that writes bytecode with no AST, and a register VM with an op budget, an exact memory meter, a garbage collector and snapshots. A runtime compiles to one WebAssembly module that runs in a web page and in the `interpreter` CLI, and the same crate links into a Rust program.

[Overview](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/01-overview.mdx#install) installs the crate, the CLI and Lox, the runtime the docs run, and [CONTRIBUTING.md](https://github.com/dylan-sutton-chavez/interpreter/blob/main/CONTRIBUTING.md) covers building from a clone.

## Docs

| Page | Covers |
|---|---|
| [Overview](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/01-overview.mdx) | What the crate holds, how to install it and the CLI, and Lox |
| [Build a runtime](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/02-runtime.mdx) | A whole runtime, its token table, natives and language |
| [Grammar](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/03-grammar.mdx) | Forms, readings and patterns a runtime adds to the parser |
| [Rust](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/04-rust.mdx) | Running programs inside a Rust program |
| [JavaScript](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/05-javascript.mdx) | Running a runtime's `.wasm` in a page, and the ABI any host drives |
| [CLI](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/06-cli.mdx) | The commands, packages, grants, actors and a CLI of your own |
| [Errors and limits](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/07-errors-and-limits.mdx) | How errors read and every limit a run has |
| [Plugins](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/08-plugins.mdx) | Writing a `.wasm` plugin |
| [Stability](https://github.com/dylan-sutton-chavez/interpreter/blob/main/docs/09-stability.mdx) | What a 1.x release keeps, the exceptions, and how the formats and the ABI evolve |

## License

Apache 2.0 for every directory of the repo, as [LICENSE.md](https://github.com/dylan-sutton-chavez/interpreter/blob/main/LICENSE.md) sets out. The book's tests in `examples/lox/tests/suite` and its benchmarks in `bench/cases/book` keep their own MIT license, written beside them.

## Sponsors

- [PyneSys](https://pynesys.io/), since May 2026
