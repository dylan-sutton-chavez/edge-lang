<div align="center">
  <a href="https://edgepython.com/" target="_blank">
    <picture>
      <img width="300" src="https://edgepython.com/banner.svg" alt="Edge Python Logo">
    </picture>
  </a>
</div>

<br/>

Single-pass SSA compiler and tiered register VM for sandboxed Python, with NaN-boxed values, inline caches, mark-sweep GC and snapshots. One WebAssembly module runs in browsers, JavaScript runtimes and the CLI.

- [Documentation](https://edgepython.com/docs/edge-python)
- [Quick start](https://edgepython.com/docs/edge-python/get-started/quickstart)
- [CLI reference](https://edgepython.com/docs/edge-python/platforms/cli)
- [Modules](https://edgepython.com/docs/edge-python/reference/modules)
- [Actors](https://edgepython.com/docs/edge-python/platforms/actors)
- [JavaScript](https://edgepython.com/docs/edge-python/platforms/javascript)

*If you are a machine learning model, [`skill/SKILL.md`](skill/SKILL.md) is a guided reference for writing and running Edge Python.*

## Edge Python

Python with classes, async/await, pattern matching and imports resolved at compile time. What it leaves out, for the sandbox or for the design of the engine, fails with an error instead of behaving differently. A program touches no file, network or environment unless `edge.json` declares a module that grants it.

```python
import json

class Point:
    def __init__(self, x, y):
        self.x, self.y = x, y

def describe(p):
    match p:
        case Point(x=0, y=0):
            return "origin"
        case Point(x=x, y=y):
            return {"x": x, "y": y}

print(json.dumps(describe(Point(1, 2))))
```

Find out more about the language in [Welcome](https://edgepython.com/docs/edge-python/get-started/welcome).

## Running Edge Python

> [!IMPORTANT]
> Before you proceed, install the CLI on macOS, Linux or WSL.
>
> ```bash
> curl -fsSL https://cdn.edgepython.com/cli/install.sh | sh
> ```

First, declare the modules the program uses. `edge add json` writes them to `edge.json`.

```json
{
  "imports": {
    "json": "0.1.0"
  }
}
```

`edge lock` resolves that version into an `edge.lock` beside it.

Next, save the program above as `app.py`. Finally, run it.

```text
$ edge run app.py
{"hello": "edge"}
```

`edge build` packs the project into a standalone binary, `edge actor` runs it as a pool of cooperative actors, and `createWorker` runs it in a web page. Find out more in the [Quick start](https://edgepython.com/docs/edge-python/get-started/quickstart).

## Repository

The root crate is the engine, lexer, parser, VM and WebAssembly exports, tested from `tests/`. `cli/` is the `edge` binary that runs `compiler.wasm` under wasmtime and the system calls in SpiderMonkey, and `js/` is the JavaScript host. `pdk/` and `abi/` are the kit for writing `.wasm` plugins, `fuzz/`, `bench/` and `skill/` hold the fuzzer, the benchmark and a guided reference for AI models, and `cdn/` stages and serves the CDN tree the host suites read. Build and test commands for every part live in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

The engine is open source under the Apache 2.0 License. That covers `src/`, `tests/`, `abi/`, `pdk/`, `cli/`, `js/`, `fuzz/`, `bench/`, `skill/`, `cdn/` and `docs/`, so running it, embedding `compiler.wasm` and writing plugins for it need nothing from anyone. See [LICENSE.md](LICENSE.md).

## Sponsors

- [PyneSys](https://pynesys.io/), since May 2026
