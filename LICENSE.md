# License

The directory a file sits in decides the license that applies to it.

## The engine

`src/` `tests/` `lib/` `examples/` `abi/` `plugin/` `cli/` `js/` `fuzz/` `bench/` `skill/` `tools/` `make/` `cdn/` `docs/`

Licensed under the Apache License 2.0, whose text is at <https://www.apache.org/licenses/LICENSE-2.0>.

The engine builds as `edge.wasm` and as a Rust library, and driving either from your own host, or building a runtime on it, needs nothing from anyone, a closed product included.


## The book's tests and benchmarks

`examples/lox/tests/suite` `bench/cases/book`

The tests and benchmarks of Crafting Interpreters by Robert Nystrom under the MIT license, whose text with its copyright is in the LICENSE file of each.
