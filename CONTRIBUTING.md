# Contributing to Edge

Thanks for taking the time to contribute. Security reports follow [SECURITY.md](SECURITY.md).

## Issues

Include a minimal failing script, the command used to run it, and the expected and actual behavior.

## Pull Requests

For a large change, open an issue or email [c.sutton.dylan@gmail.com](mailto:c.sutton.dylan@gmail.com) first. Pull requests are welcome in every part under the Apache 2.0 License, see [README.md](README.md#license).

- New behavior comes with tests, and docs describe the code as it is after the change.
- Changes to the language, the CLI, or package behavior update `skill/SKILL.md`.
- Changes to the engine keep `make bench` passing, and significant ones run the [fuzzer](https://edgepython.com/docs/edge-python/internals/runbook).

Run `make check` before sending, and the maintainer runs CI once the pull request is open.

## Style

Comments are one line, at most one per block, and deleted when redundant. No file-header comment or docstring. A doc edit rewrites the sentence or paragraph it changes instead of adding one beside it, so a page never grows past what is new. Comments and docs use no colons, semicolons, or em-dashes.

## Building

> [!IMPORTANT]
> Every command runs from the repo root through GNU Make, on Linux, macOS or Windows. The first CLI build compiles SpiderMonkey and wants clang, python3 and `llvm-objdump` on the path, which on macOS is a symlink to `/usr/bin/objdump`.

```bash
make wasm       # compiler.wasm, CI ships the smaller make wasm-ship
make wasm-cli   # the speed build the CLI embeds
make js         # js/src stripped to js/dist
make cli        # the edge binary, embedding wasm-cli, js and the Lucide names
make lint
cargo build --release   # the .rlib Rust embedders link
```

`make cli-release TARGET=x86_64-unknown-linux-musl` builds the static Linux binary in Docker, since musl-gcc has no C++. Every build uses the one Rust release `rust-toolchain.toml` pins.

## Testing

`make test` runs `tests/cases/vm.json` under `Limits::sandbox()`, so a budget, memory, or call-depth regression fails instead of hanging. Its memcheck run recounts every slot at each collection, and `tests/memory.rs` checks the model never counts less than the allocator hands out.

`make bench` prices each WebAssembly instruction at 0.82 ns, the `wasm_regular_op_cost` of nearcore, so `bench/.snapshot` keeps the same `[seconds, MB]` per case on every machine. It fails when the snapshot was taken with another Rust, a case and its entry do not pair, or the mean or a single case moves past its threshold. `make bench-update` takes the snapshot again.

> [!NOTE]
> The JS, CLI and skill suites read the builds from a CDN served on loopback, and the lock cases of the CLI serve their own registry through `EDGE_SITE_BASE`. Only `edge add` of an unknown name and `edge publish` with a bad token reach production.

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

`make fuzz` runs a fuzzing campaign, and `make miri SUITE=vm` runs one module under Miri, as CI does around the clock.

## Docs

`docs/` holds MDX pages two folders deep at most, with a numeric prefix on every path segment. Each opens with a frontmatter holding a `title` and a `description` and has one top-level heading, the rules `edge build` holds package docs to. An `edge-python` block followed by an `output` block becomes a playground on the real engine, and a blockquote opening with a marker such as `[!NOTE]`, `[!QUESTION]`, `[!CARDS]` or `[!COPY]` becomes a box, as [Registry](https://edgepython.com/docs/edge-python/reference/registry#package-docs) lists.

## CI

[`main.yml`](.github/workflows/main.yml) runs each part as a composite action that calls the same `make` targets, against the CDN tree it stages. Nothing in it holds a secret, so a pull request from a fork runs it whole.
