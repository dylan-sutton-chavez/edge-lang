---
name: edge-python
description: Write, run, test and package Edge Python programs with the edge CLI. Use when editing .py files in an Edge Python project or when the user asks for Edge Python code.
---

# Edge Python

This document is self-verifying and its examples follow the cells v1 grammar. A `python` or `yml` block followed immediately by a `text` block is a runnable cell, and the `skill` crate beside it in the repository executes every cell through the edge CLI and compares it against the `text` block. The tag on the `text` block is `Output` for a run whose stdout must equal the block, or `Error` for a failing run whose stderr contains the given text. A `python` block tagged `skip` never runs on any engine and never pairs with a `text` block, and it always says why with one comment at the exact construct that is nondeterministic. A `yml` block tagged `actor` runs a trusted actor pool through `edge actor`, while one tagged `untrusted` runs eval groups. Any `python` block without a `text` pair is illustrative only. Every cell runs in a scratch directory holding this `edge.json`, since nothing resolves undeclared, and the `yml` cells find the same manifest beside their `actor.yml`. Verify the whole file from the repository root with `cargo run -p skill -- skill/SKILL.md`.

```json
{
  "imports": {
    "json": "https://cdn.edgepython.com/std/json.wasm",
    "re": "https://cdn.edgepython.com/std/re.wasm",
    "math": "https://cdn.edgepython.com/std/math.wasm",
    "struct": "https://cdn.edgepython.com/std/struct.wasm",
    "test": "https://cdn.edgepython.com/std/test.py"
  }
}
```

Edge Python is sandboxed Python compiled in a single pass to bytecode and executed by a register VM. It is one WebAssembly binary, hosted by the JS host, a JavaScript package built on the browser's sandbox model that runs in browsers and in JavaScript runtimes such as Deno, and by the `edge` CLI. There is no bundled stdlib, every module is an external package declared in `edge.json` and resolved at compile time, the official packages included. Programs are deterministic, there is no file, network or environment access unless a declared module grants it.

Use this skill to write correct Edge Python on the first try. The language is CPython 3.15, its syntax, builtins and error messages, minus what the sandbox and the engine leave out, and what is left out fails with an error. Read the delta section before writing non-trivial code.

## The working loop

A project is any folder with `.py` files and an `edge.json` declaring every module they import. The loop is always the same.

1. Write or edit the `.py` files.
2. Run the entry point with `edge run main.py`.
3. Add tests in `*_test.py` files and run `edge test`.
4. Compile a release with `edge build` when the program must run elsewhere.

```bash
edge init myapp        # scaffold main.py, an empty edge.json and index.html
cd myapp
edge add json          # declare each package the code imports
edge lock              # resolve every declared version into edge.lock
edge run main.py       # run the entry point
edge test              # discover and run every *_test.py
edge build main.py     # compile to one native binary, ./main
edge publish           # pack the project and send it to the registry
```

Piping a script works too, which is how the cells of this document run.

```bash
echo 'print(6 * 7)' | edge run
```

When a file path is given, piped stdin instead feeds `input()`, one line per call.

## CLI reference

Bare `edge` prints help and exits 0. `edge -v` prints the version. `Ctrl+C` exits 130. Errors print to stderr and exit 1.

### Global flags

| Flag | Effect |
|---|---|
| `--manifest <file>` | Use this manifest instead of `./edge.json` |

### edge run

`edge run [file]` executes a `.py` script or a packed `.edge` bundle, auto-detected by content. With no file it reads the script from stdin. A bare `edge run` in a terminal with no pipe errors. `edge run -c 'print(1)'` runs inline code instead of a file or stdin, and piped stdin then feeds `input()`.

Run flags.

| Flag | Effect |
|---|---|
| `--events <f>` | Each line of the file or FIFO feeds one `receive()` call, EOF parks the script |
| `--save-state <f>` | When the script parks on an unservable wait, write a snapshot blob, print `state saved` to stderr and exit 0 |
| `--restore-state <f>` | Boot from a snapshot blob instead of a script and keep running |
| `--preempt <n>` | Yield every `n` loop back-edges so even a tight loop stays snapshottable, 0 disables |
| `--web` | Run on the browser host in headless Chrome, refusing the four flags above since they belong to the native engine |

`raise SystemExit(code)` with no argument or an integer exits cleanly with that code. Any other uncaught error prints a traceback and exits 1.

`--web` serves the JS host and the engine from the binary on a loopback port, so only the modules a manifest declares by URL leave the machine. It drives a Chrome the system already has, or one it offers to download once into `~/.local/share/edge/chromium`, which `edge uninstall` offers to remove. `EDGE_CHROME_PATH` names a browser directly and skips the search.

### edge test

`edge test [path]` discovers `*_test.py` recursively, skipping hidden dirs, `node_modules`, `target` and `dist`. A file argument runs exactly that file. Each file executes in a fresh interpreter and state never leaks between files. The project must declare `test`, otherwise the runner stops with `declare test in edge.json (edge add test)`. Exit code is 0 when everything passes, 1 when a file fails or no tests are found, 2 when the engine cannot start. See the test package section for the API.

`edge test --web` runs the same files on the browser host in headless Chrome, one browser for the suite with a fresh page per file, so a package can prove it answers the same in a browser.

### edge init, edge add, edge remove, edge lock

`edge init [name]` scaffolds `main.py`, an empty `edge.json` and `index.html`, with `--bare` skipping the HTML. `edge add json` looks each name up in the registry at its newest version, or at the one `json@0.1.0` names, writes one `imports` entry holding that version alone, and prints it with what the package asks the root to grant. `edge add foo=<url>` registers a custom URL verbatim, also under `imports`, since each host tells a `.py` and a `.wasm` module apart by the artifact, and a `.js` or `.mjs` URL is refused before anything is written. `edge add` keeps `extends` and any other key already there. `edge remove` deletes entries. Unknown names abort the whole command before any write, and neither command touches `edge.lock`. The same registry answers over HTTP, where `GET https://edgepython.com/api/search?q=<term>` finds a package and every address it names answers as data under `/api`, so `/package/<name>` reads at `/api/package/<name>` without taking it.

`edge lock` turns each declared version into the URL and digest of that release and writes `edge.lock` beside the manifest, rebuilding the whole file each time. It is the only command that asks the registry where a name points, so `edge run`, `edge test` and `edge build` read the lock and resolve nothing themselves. A version with no entry, or one whose entry holds another release, fails with `'json' is not locked, run edge lock`. It also walks every package in the tree, each through its own `edge.lock`, and writes nothing while a package lacks what it lists from its importer, a check `edge run`, `edge test` and `edge build` repeat before compiling.

### edge serve

A static dev server with live reload for the current directory. `--host` defaults to `127.0.0.1`, `--port` to 5173, `--open` opens a browser.

### edge build

`edge build [file]` compiles a program to one static native binary, the project's `main.py` when no file is given. The VM is the compiler: each opcode's handler is a function of the VM, the program's functions call them in order with their operands constant, and LLVM inlines them and optimizes the program at `-O3` before linking the rest of the VM, compiled once. The binary runs with nothing installed, reads `input()` from stdin, takes `--events <file>` like `edge run`, and prints the same output, errors and exit code.

| Flag | Effect |
|---|---|
| `-o`, `--out <path>` | Output path, the script's name by default. A path ending in `.o` stops at the object |
| `--wasm` | A `.wasm` for WASI that imports only `fd_write` and `proc_exit` |
| `--target <triple>` | `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin` or `wasm32-wasip1`, the host's by default |
| `--opt speed\|size` | What LLVM optimizes for, `speed` by default |
| `--strip` | Leave the symbols out |

The first build asks to download LLVM's tools and the linker from the Rust release Edge is built with, about 200 MB once into `~/.cache/edge/llvm`, the way `--web` offers Chromium. Without a terminal it refuses, and `EDGE_LLVM_ROOT` names a Rust sysroot that has `llvm-tools` instead. An `apple-darwin` target only builds on a Mac, which has the SDK it links against.

### edge web

`edge web` writes `dist/`, a browser distribution with the vendored JS host, the compiler and every package, `--out <dir>` choosing another folder. The entry is `main.py`, `app.py` or `index.py` when present.

### edge publish

`edge publish` packs the project as one `.edge` and uploads it, reading `EDGE_TOKEN` for a token made at `/settings#tokens`, which signs the request and never leaves the machine. The bundle carries every `.py` and `.wasm` under the project plus `edge.json`, its lock, the `README.md` and any `LICENSE` at the project root, each module the manifest declares by URL and the files it imports, and the `.mdx` pages of a declared `docs` directory under a reserved `@docs/` prefix, checked against the rendering convention first. `--out <file>` writes the `.edge` instead of sending it, which `edge run` and an actor eval group also run. The artifact is the whole request. The registry opens it and reads the `name`, the `version`, the description, the repository, the `LICENSE` and the pages under `@docs/` out of the bytes it is about to store, so nothing is declared twice and a listing shows what you shipped. A name is first come and permanent, a version is never overwritten, and the bundle is stored exactly as packed. One artifact is 10 MB at most, an account claims 10 names and publishes 60 versions a day, and its packages add up to 50 MB when it signs in by address alone or a gigabyte with GitHub or Google linked.

Where a package runs is not declared and not yet worked out, so a release carries no claim about its hosts.

### edge actor

`edge actor <file>` runs an actor pool from an `actor.yml` manifest, resolving group imports through the `edge.json` beside it or the one `--manifest` names. See the actors section for the schema and the two execution models.

### edge uninstall

Interactive removal of the binary and PATH entries.

### Environment variables

| Variable | Effect |
|---|---|
| `EDGE_COMPILER_WASM` | Path to `compiler.wasm` for the CLI build |
| `EDGE_CDN_BASE` | Serve the official CDN origin from another base for module downloads and `edge web`, used by tests and staging |
| `EDGE_LLVM_ROOT` | A Rust sysroot with `llvm-tools` that `edge build` uses instead of downloading its own |

## The Python delta

A program that compiles behaves as it does in CPython, apart from the three design choices this section lists. What Edge Python leaves out is missing for the sandbox or for the design of the engine, and it fails with an error instead of behaving differently, at compile time whenever the compiler can see it. This is the section to internalize.

### Left out for the sandbox

- No stdlib. Every module is an external package, so `import os`, `import sys` and `import asyncio` fail at compile time.
- No dynamic imports. `__import__` is a compile error that names the builtin. `eval`, `exec` and `compile` work, under the same budget and grants, but their text imports only packages the program resolved and system modules its package is granted, cannot suspend, and inside a function it gets a copy of the locals.
- No `open`, a compile error as well. `input()` reads from a host fed buffer with no prompt argument.
- No clock, randomness or OS access without a grant, so the same source and input give the same output on every run.

```python
open("data.txt")
```

```text Error
'open' is not supported
```

### Left out by design

Values are NaN-boxed and frames keep their locals in registers, so two equal numbers are one value and `locals()` returns a fresh copy instead of the frame itself. Each of these fails at compile time with a message naming what to use instead.

- `is` against anything but `None`, `True`, `False`, `...` or `NotImplemented`. Compare with `==`, which on an instance without `__eq__` already compares identity.
- Metaclasses, `__del__`, `__getattribute__` and any dunder outside the supported list.
- A list, dict or set as a default argument. Default to `None` and build the value in the body.
- Named escapes such as `\N{BULLET}`.

```python
a = [1]
b = a
print(a is b)
```

```text Error
'is' compares only with None, True, False, ... or NotImplemented
```

`id(x)` is unique while `x` lives and survives a snapshot, and two equal numbers share one. An instance hashes by identity, and a class that defines `__eq__` is unhashable. `bytearray` is a mutable bytes and `memoryview` a view of the bytes of one, which writes through to a bytearray.

The two behaviors that differ by design follow.

### Order and NaN

- Sets iterate and print in insertion order, not hash order. Present them through `sorted` to match CPython.
- Every NaN is one value. A NaN is found in a list holding another NaN, and a set keeps a single one.

### Reduced pattern matching

`match` supports literal patterns, captures, the `_` wildcard, OR patterns with `|`, guards with `if`, `as` captures, and sequence patterns like `[x, y]`, `(x, (y, z))`, `x, y` or `[first, *rest]` whose items nest any of these. Sequence patterns match only list and tuple subjects. Mapping patterns `{"k": v, **rest}` take literal keys, class patterns `Point(x, y=0)` read `__match_args__` and `int(n)` binds the subject, and `Color.RED` compares by value.

```python
def describe(value):
    match value:
        case 0 | 1:
            return "small"
        case [first, *rest]:
            return f"list of {len(rest) + 1}"
        case n if n < 0:
            return "negative"
        case _:
            return "other"

print(describe([10, 20, 30]))
```

```text Output
list of 3
```

`match` is a soft keyword. A parenthesized subject like `match (a, b):` works as a statement, and `match(a, b)` in expression position still parses as a call.

### Missing pieces by type

- `tuple` and `range` have only `index` and `count`, `frozenset` only the set methods that change nothing, and `frozendict` only the dict ones. The dunders of the operators and builtins a value answers are methods, so `'ab'.__len__()` and `int.__add__(1, 2)` work, and an operand of another type answers `NotImplemented`.
- Codecs are UTF-8, UTF-16, UTF-32, ASCII, Latin-1 and cp1252, with the `strict`, `ignore`, `replace`, `backslashreplace` and `xmlcharrefreplace` error handlers.
- `zip(..., strict=True)` raises `ValueError` on inputs of unequal length. `round` uses ties-to-even and always returns int for one argument, float for two.
- The `int_to_bytes` and `int_from_bytes` builtins cap at 8 bytes, while the `int.to_bytes` and `int.from_bytes` methods take ints of any size.

### Async without an event loop

Coroutines follow the protocol of Python, `await`, `send`, `throw`, `close`, async generators, `async for`, `async with` and async comprehensions, with the async dunders and `aiter` and `anext`. The engine ships no event loop and no `asyncio`, which comes later with the standard library, so a coroutine runs under a driver that calls `send`. `await` outside an `async def` is a `SyntaxError`, the top of a module included. See the async section.

### Compile time versus run time

Import failures and syntax errors in the program are compile-time diagnostics and can never be caught with `try`. Code that compiles but looks like a slip, such as `5()`, an invalid escape like `"\d"` or `assert (x, "msg")`, prints a `SyntaxWarning` to stderr before the program runs, worded as Python words it. In text that `eval`, `exec` or `compile` parse they raise `SyntaxError`, and everything else raises normal catchable exceptions at run time.

## Imports

Every import resolves at compile time through a host resolver. The compiler flattens each module into the bytecode and the VM fetches nothing at run time.

```python
import math
from json import dumps, loads
from math import sqrt as root
from re import *

print(root(16.0), loads(dumps({"ok": True}))["ok"])
```

```text Output
4.0 True
```

Dotted specs import files. A leading dot anchors at the importing file, one extra dot per directory up. Without it the spec anchors at the nearest `edge.json` dir. The `.py` suffix is implicit.

```python
from .lib.helpers import slugify
from ..shared.util import chunks
from lib.helpers import slugify as sl
```

Not supported. `from . import x` and any form of dynamic import.

Bare names resolve through `edge.json`, walking up from the importing file with the nearest manifest winning. The manifest maps each name to a path, a URL, or a `major.minor.patch` version naming a registry package under `imports`, and `extends` may name a parent manifest. A version resolves through the `edge.lock` beside the manifest that declared it, which every host reads before the compiler sees the manifest, so the compiler only ever meets a path or a URL. An `edge` field names the lowest engine the project runs on, `major.minor.patch`, and it runs on that version or any later one, never on an earlier one, which stops with `this project needs edge 0.7.0, this is 0.6.9`. The artifact decides the kind, `.py` is a code module and `.wasm` a native plugin, so a manifest never classifies a package, and a `.js` or `.mjs` import fails with `module 'charts' is JavaScript, ship a .py or a .wasm`, since no JavaScript loads besides the host's own system calls. `permissions` grants the system modules, see that section. `name`, `version`, `description`, `repository` and `docs` are the registry fields, ignored by the compiler and shape-checked by the CLI. Only what nothing else supplies belongs there, so an author and a date come from the publishing account and the license is read from the packed `LICENSE` file.

```json
{
  "imports": {
    "utils": "./lib/utils.py",
    "mypkg": "https://example.com/mypkg.wasm"
  }
}
```

The official names `json`, `re`, `math`, `struct` and `test` resolve only when declared, `edge add <name>` then `edge lock` writes each entry, and an undeclared name fails at compile time with `module '<name>' is not provided by this host and no edge.json declares it`, and the CLI adds a `help:` line with the `edge add` command for an official name. The CLI keeps the std packages inside the binary, so they need no network there. Modules are singletons with shared mutable state, an import cycle raises `RuntimeError` at startup, and inside an imported module `__name__` is its canonical spec so `if __name__ == "__main__":` blocks are skipped on import. `import_module(name)` looks up a module already bound by a plain `import` in scope.

## Builtins

The global namespace holds exactly 74 builtin functions, the type objects, the exception classes, `NotImplemented`, `Ellipsis` and `__name__`. Nothing else exists, and names like `help` or `exit` are simply undefined.

### Output and input

`print(*args, sep=' ', end='\n')` accepts `file` and `flush` and ignores them. `input()` reads one host fed line with no prompt.

```python
print("a", "b", sep="-", end="!\n")
```

```text Output
a-b!
```

### Numeric

`abs`, `round`, `min`, `max`, `sum`, `pow`, `divmod`, `bin`, `oct`, `hex`. `round` breaks ties to even. `min` and `max` accept variadic args or one iterable plus `key` and `default`.

```python
print(round(2.5), round(3.5), round(1.55, 1))
print(divmod(7, 2), max("xy", "abcde", key=len))
print(pow(2, 10, 100))
```

```text Output
2 4 1.6
(3, 1) abcde
24
```

### Conversion

`int`, `float`, `str`, `bool`, `list`, `tuple`, `set`, `frozenset`, `dict`, `frozendict`, `bytes`, `chr`, `ord`, and `sentinel(name)` for an object equal only to itself. `int` truncates toward zero and parses bases 2 to 36 or 0 for auto-detect. `int("nan")` style failures raise `ValueError` and `int(float("inf"))` raises `OverflowError`.

```python
print(int("ff", 16), int("0b101", 0), int(-3.7))
print(float("inf") > 1e308, ord("A"), chr(97))
```

```text Output
255 5 -3
True 65 a
```

### Iteration

`len`, `range`, `sorted`, `reversed`, `enumerate`, `zip`, `iter`, `next`, `map`, `filter`, `all`, `any`, `slice`. The iterators they return compute each item when it is asked for.

```python
print(list(enumerate("ab", start=1)))
print(list(zip([1, 2, 3], "ab")))
print(sorted([3, 1, 2], reverse=True), any([0, "", 3]))
```

```text Output
[(1, 'a'), (2, 'b')]
[(1, 'a'), (2, 'b')]
[3, 2, 1] True
```

### Types and attributes

`type`, `object`, `isinstance`, `issubclass`, `callable`, `id`, `hash`, `repr`, `ascii`, `format`, `getattr`, `hasattr`, `setattr`, `delattr`, `vars`, `dir`, `globals`, `locals`, `import_module`, `super`, `property`, `staticmethod`, `classmethod`. `isinstance` accepts a tuple of types and `bool` is a subclass of `int`. `x.__class__` is the same object as `type(x)`. `getattr`, `hasattr` and `setattr` behave like `obj.name`, properties and `__getattr__` included, and a `getattr` default answers only an `AttributeError`. `format(x, spec)` with a spec on a value that has no format of its own, a list or an instance without `__format__`, raises `TypeError`, and `f"{x!s:>10}"` pads its `str()`. `hash(x)` agrees for equal values, `hash(1) == hash(1.0)`, and raises `TypeError` for a list, dict or set. `vars(x)` and `x.__dict__` return the live attribute dict of an instance, so writes show on both sides, while `globals()` returns a copy whose mutation binds nothing.

```python
print(isinstance(True, int), callable(len))
print(format(255, "08x"), repr("it's"))
```

```text Output
True True
000000ff "it's"
```

### Bytes helpers

`bytes_fromhex`, `int_from_bytes(b, order)` and `int_to_bytes(n, length, order)` with a limit of 8 bytes and unsigned values.

```python
print(bytes_fromhex("ff00"), int_from_bytes(b"\x01\x00", "little"))
```

```text Output
b'\xff\x00' 1
```

### Exceptions

The catchable tree under `Exception` is `ArithmeticError` with `OverflowError`, `ZeroDivisionError` and `FloatingPointError`, `LookupError` with `IndexError` and `KeyError`, `RuntimeError` with `RecursionError`, `NotImplementedError` and `PythonFinalizationError`, `OSError`, also named `IOError` and `EnvironmentError`, with `PermissionError`, `FileNotFoundError`, `FileExistsError`, `TimeoutError`, `ConnectionError` and its subclasses and the rest of the OS errors, `ValueError` with `UnicodeError` and its `UnicodeEncodeError`, `UnicodeDecodeError` and `UnicodeTranslateError`, `ImportError` with `ModuleNotFoundError`, `SyntaxError` with `IndentationError` and `TabError`, `NameError` with `UnboundLocalError`, `Warning` with `UserWarning`, `DeprecationWarning` and the other warning classes, plus `TypeError`, `AttributeError`, `StopIteration`, `StopAsyncIteration`, `AssertionError`, `MemoryError`, `BufferError`, `EOFError`, `ReferenceError`, `SystemError` and `ExceptionGroup`. Under `BaseException` sit `SystemExit`, `KeyboardInterrupt`, `GeneratorExit` and `BaseExceptionGroup`, which `except Exception` does not catch.

Handlers name one class, a tuple or nothing, and a bare `except` must come last. `ExceptionGroup(msg, excs)` raises several exceptions as one, with `exceptions`, `split`, `subgroup` and `derive`, and `except* T` handles the part of a group whose members are `T` while the rest is raised again. `except X as e` binds the exception, `e.args` is its argument tuple and `e.add_note(text)` adds to `e.__notes__`. `finally` runs on every exit path including `return`, `break` and `continue`. `raise X from Y` sets `__cause__`, an exception raised while another is handled keeps it as `__context__`, and an uncaught chain prints each exception in turn with its notes after the message.

```python
try:
    {}["missing"]
except (KeyError, IndexError) as e:
    print(type(e).__name__, e.args)
finally:
    print("always")
```

```text Output
KeyError ('missing',)
always
```

A class deriving from a builtin exception joins its tree, so `except ValueError` catches it. The constructor arguments become `e.args`, which `super().__init__(...)` or `Exception.__init__(self, ...)` resets, and `str(e)` follows `args` unless the class defines `__str__`.

```python
class Bad(ValueError):
    def __init__(self, code):
        super().__init__("bad code", code)
        self.code = code

try:
    raise Bad(7)
except ValueError as e:
    print(type(e).__name__, e.code, e.args)
```

```text Output
Bad 7 ('bad code', 7)
```

## Type methods

Methods live on the builtin types, `bool` answering those of `int`, and `NoneType` has none. Read off the type, a method is unbound and takes its receiver first, so `str.lower("AB")` and `map(str.strip, lines)` work.

### str

`encode`, `upper`, `lower`, `strip`, `lstrip`, `rstrip`, `capitalize`, `title`, `casefold`, `swapcase`, `isdigit`, `isalpha`, `isalnum`, `isspace`, `isupper`, `islower`, `istitle`, `isdecimal`, `isnumeric`, `isidentifier`, `isascii`, `isprintable`, `startswith`, `endswith`, `find`, `rfind`, `index`, `rindex`, `count`, `split`, `rsplit`, `join`, `replace`, `removeprefix`, `removesuffix`, `splitlines`, `partition`, `rpartition`, `center`, `ljust`, `rjust`, `zfill`, `expandtabs`, `translate`, `maketrans`, `format`, `format_map`. Indices count code points. `startswith` and `endswith` accept a tuple of prefixes. `format` takes positional and named fields with specs.

```python
print(" hello ".strip(), "a,b,c".split(",", 1))
print("-".join(["x", "y"]), "Hello".casefold(), "{0}{1}{0}".format("a", "b"))
print("file.py".removesuffix(".py"), "5".zfill(3))
```

```text Output
hello ['a', 'b,c']
x-y hello aba
file 005
```

### list

`append`, `extend`, `insert`, `remove`, `pop`, `clear`, `copy`, `reverse`, `index`, `count`, `sort` with `key` and `reverse`. Slice assignment resizes, and `+=` extends in place.

```python
xs = ["bb", "a", "ccc"]
xs.sort(key=len)
xs[1:1] = ["z"]
print(xs, xs.pop())
```

```text Output
['a', 'z', 'bb'] ccc
```

### dict

Insertion ordered. `keys`, `values`, `items` return live views, plus `get`, `update`, `pop`, `popitem`, `setdefault`, `fromkeys`, `copy`, `clear`. `popitem` removes the most recently inserted pair. Numerically equal keys collapse, so `1`, `1.0` and `True` are one key.

```python
d = dict.fromkeys(["a", "b"], 0)
d.update({"c": 1})
print(d.pop("a"), d.setdefault("d", 4), list(d))
print({1: "x", True: "y"})
```

```text Output
0 4 ['b', 'c', 'd']
{1: 'y'}
```

### set

`add`, `remove`, `discard`, `pop`, `clear`, `update`, `copy`, `union`, `intersection`, `difference`, `symmetric_difference`, `intersection_update`, `difference_update`, `symmetric_difference_update`, `issubset`, `issuperset`, `isdisjoint`. Named methods accept any iterable while the operators require sets on both sides. A set iterates and prints in insertion order, which CPython does not, so print through `sorted` when the output must match it.

```python
print(sorted({1, 2, 3} & {2, 3, 4}))
print({1, 2}.issubset({1, 2, 3}), {1}.isdisjoint({2}))
```

```text Output
[2, 3]
True True
```

### int and float

`int` has no fixed width, and decimal text of more than 4,300 digits raises `ValueError` both ways. It has `bit_length`, `bit_count`, `to_bytes` and the classmethod `from_bytes`. `float` has `is_integer`.

```python
print((255).bit_length(), (5).bit_count(), (258).to_bytes(2).hex())
print((4.0).is_integer(), int.from_bytes(b"\x01\x02", "big"))
```

```text Output
8 2 0102
True 258
```

### bytes

The bytes methods of Python 3.15, from `decode` and `hex(sep)` to `partition`, `translate` and the `is` predicates, with `fromhex` and `maketrans` on the type. Case methods and whitespace are ASCII only, and `bytearray` adds `resize` and `take_bytes` to the list methods it has.

```python
print(b"\x00\x01".hex(), b"a,b".split(b","), b"ABC".lower())
```

```text Output
0001 [b'a', b'b'] b'abc'
```

## Functions and classes

Functions support defaults, keyword arguments, `*args`, `**kwargs`, keyword-only parameters after `*` or `*args`, and positional-only parameters before `/`, plus call-site unpacking. `f.__annotations__` evaluates the annotations on first read, and `f.__code__` describes the parameters and locals. Lambdas are single expressions. Decorators work on functions and classes, stacked bottom-up. Closures capture variables by reference, see the gotchas section.

```python
def greet(name, *, punct="!"):
    return f"hi {name}{punct}"

print(greet("edge", punct="?"))
```

```text Output
hi edge?
```

Classes support single and multiple inheritance with C3 linearization, zero-argument `super()`, `property` with setters and deleters, descriptors, `staticmethod` and `classmethod`, and class decorators. A class may derive from `dict`, `list`, `set`, `frozenset`, `str`, `bytes`, `tuple`, `int` or `float`, and its instances are real values of that type with the methods and attributes of the class, where `super()` reaches the builtin methods and a dict subclass answers missing keys with `__missing__`. There is no two-argument `super()` form. Dunders are looked up on the class, assigning one on an instance has no effect. `type(name, bases, members)` builds a class, and `__mro__`, `__bases__` and `__dict__` describe one.

The supported dunders are `__init__`, `__new__`, `__init_subclass__`, `__set_name__`, `__slots__`, `__get__`, `__set__`, `__delete__`, `__setattr__`, `__delattr__`, `__getattr__`, `__hash__`, `__call__`, `__repr__`, `__str__`, `__format__`, `__bool__`, `__len__`, `__iter__`, `__next__`, `__getitem__`, `__setitem__`, `__delitem__`, `__missing__`, `__contains__`, `__enter__`, `__exit__`, `__index__`, `__int__`, `__float__`, `__abs__`, `__neg__`, `__pos__`, `__invert__`, `__class_getitem__`, `__match_args__`, the arithmetic and bitwise operators with their reflected and in-place forms including `@` through `__matmul__`, and the six comparisons. A class body that binds any other dunder fails at compile time. Returning `NotImplemented` from an arithmetic dunder triggers the reflected fallback. A user `__hash__` keys dicts and sets with `__eq__` deciding equal keys, and a class defining `__eq__` alone is unhashable.

```python
class Vector:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def __add__(self, other):
        return Vector(self.x + other.x, self.y + other.y)

    def __repr__(self):
        return f"Vector({self.x}, {self.y})"

class Scaled(Vector):
    pass

print(Scaled(1, 2) + Vector(10, 20))
```

```text Output
Vector(11, 22)
```

Context managers implement `__enter__` and `__exit__(exc_type, exc_value, traceback)` where the traceback argument is always `None`. A truthy `__exit__` suppresses the exception.

Pure functions are memoized automatically after two identical calls. The VM detects purity by the absence of I/O, mutation, raising and free-name reads, so naive recursive code is fast and side-effecting calls skip the cache safely.

## Async

A coroutine runs only while something drives it. `await` runs one in place and gives its return value, and what the innermost awaitable yields passes up to whoever calls `send` on the outermost coroutine. A driver of a few lines is the event loop.

```python
class Tick:
    def __await__(self):
        yield "tick"

async def dbl(n):
    await Tick()
    return n * 2

def run_all(*coros):
    results = [None] * len(coros)
    ready = list(enumerate(coros))
    while ready:
        i, coro = ready.pop(0)
        try:
            coro.send(None)
            ready.append((i, coro))
        except StopIteration as done:
            results[i] = done.value
    return results

print(run_all(dbl(1), dbl(2), dbl(3)))
```

```text Output
[2, 4, 6]
```

`throw` raises where the coroutine waits, reaching the awaitable it waits on first, so a cancellation is a `throw` the coroutine may catch, and `close` raises `GeneratorExit` there. `StopIteration` carries the return value.

```python
class CancelledError(BaseException):
    pass

class Tick:
    def __await__(self):
        yield "tick"

async def worker():
    try:
        await Tick()
    except CancelledError:
        return "cleaned up"

w = worker()
w.send(None)
try:
    w.throw(CancelledError())
except StopIteration as done:
    print(done.value)
```

```text Output
cleaned up
```

The module body itself runs as a coroutine the host drives. `receive()` parks it until a host event or an actor message arrives, and `send(group, body)` hands a string to an actor group, raising `RuntimeError` outside an actor pool. A host call or a `receive()` that waits inside a coroutine or a generator stepped by `send` or `next` raises `RuntimeError`, only the module body, the plain functions it calls and the generators a `for` loop steps can wait.

### Snapshots

The CLI can serialize the full interpreter state, heap, globals and the suspended module body, and restore it later. This is how long-running or event-driven programs survive process restarts.

```bash
edge run app.py --save-state state.bin     # writes the blob when the script parks
edge run --restore-state state.bin         # resumes from the blob
edge run app.py --preempt 500              # makes even while-True snapshottable
```

A snapshot is taken when the script parks on a wait the engine cannot serve, for example `receive()` with no events left. Without `--save-state` such a park is an error. The blob embeds a bytecode fingerprint and only restores into the same program, and a SHA-256 seal makes a damaged blob fail to restore instead of running. The seal stops damage, not forgery, so a blob that passes through untrusted hands needs a signature of the host. A restored run keeps the op budget it saved, capped by the limits it boots with. Feed a resumed run with `--events file`, one `receive()` line per call.

## Std packages

Five official packages, each declared with `edge add <name>` then `edge lock` and imported by bare name on both hosts. A version in `imports` names the release and the lock pins its bytes.

### json

`loads(s)` with optional `object_hook`, `object_pairs_hook`, `parse_float`, `parse_int` and `parse_constant`. `dumps(obj)` with `indent`, `sort_keys`, `ensure_ascii`, `check_circular`, `allow_nan`, `skipkeys`, `default`, `separators` and `cls`. Parse failures raise `ValueError`, non-serializable values raise `TypeError` unless `default` handles them. Integers round-trip at 128-bit and non-finite floats map to `NaN` and `Infinity`.

```python
import json

data = json.loads('{"n": 21, "xs": [1, 2]}')
print(json.dumps(data, sort_keys=True))
print(json.dumps({"bad": object()}, default=lambda o: type(o).__name__))
```

```text Output
{"n": 21, "xs": [1, 2]}
{"bad": "object"}
```

### math

Constants `pi`, `e`, `tau`, `inf`, `nan`. Functions `sqrt`, `cbrt`, `exp`, `exp2`, `expm1`, `pow`, `log`, `log2`, `log10`, `log1p`, the trig and hyperbolic families, `atan2`, `hypot`, `dist`, `degrees`, `radians`, `erf`, `erfc`, `gamma`, `lgamma`, `fabs`, `fmod`, `remainder`, `copysign`, `ldexp`, `modf`, `frexp`, `floor`, `ceil`, `trunc`, `isnan`, `isinf`, `isfinite`, `isclose`, `fsum`, `prod`, and the integer functions `factorial`, `gcd`, `lcm`, `isqrt`, `comb`, `perm` at 128-bit. Domain errors raise `ValueError`, and a result too large for a float or a 128-bit int raises `OverflowError`. `nextafter`, `ulp` and `sumprod` are not supported.

```python
import math

print(math.gcd(12, 18), math.factorial(10), math.isqrt(17))
print(math.floor(math.pi), math.isfinite(math.inf))
```

```text Output
6 3628800 4
3 False
```

### re

`re` works as in Python, on a backtracking engine with a step budget that raises `RuntimeError` against catastrophic backtracking. `compile(pattern, flags)` returns a `Pattern` with the same methods as the module, the flags are `re.I`, `re.M` and `re.S`, and a bad pattern raises `re.error`, which is `ValueError`.

| Function | Returns |
|---|---|
| `match`, `search`, `fullmatch` | A `Match` or `None` |
| `findall` | The text of every match, or its groups when the pattern has them |
| `finditer` | Each `Match` in turn |
| `sub`, `subn` | Substituted string, `\\1` and `\\g<name>` expand groups, `subn` also counts |
| `split` | The pieces between matches |
| `escape` | The text quoted to match as written |

A `Match` answers `group`, `groups`, `groupdict`, `span`, `start`, `end` and `m[n]`, and a group that took no part reads as `None`.

Supported syntax covers classes, anchors, quantifiers with lazy forms, capturing, non-capturing and named groups, backreferences, alternation, lookahead and fixed-width lookbehind, plus inline flags `(?i)`, `(?s)` and `(?m)`. Not supported, `\p{...}`, atomic groups, possessive quantifiers, conditionals, scoped flags and `bytes` patterns.

```python
import re

print(re.findall(r"\d+", "a1b22"))
print(re.sub(r"(\w+)@(\w+)", r"\2@\1", "user@host"))
print(re.search(r"(\d+)-(\d+)", "12-34").groups())
```

```text Output
['1', '22']
host@user
('12', '34')
```

### struct

`pack(fmt, *values)` returns `bytes`, `unpack(fmt, data)` returns a tuple, `calcsize(fmt)` returns an int, all with the codes and repeat counts of Python. A format with no prefix or `@` aligns each item the way C does, and `<`, `>`, `!` and `=` pick a byte order with standard sizes and no padding. A `Struct` keeps a format, with `unpack_from` and `iter_unpack`. A bad value raises `struct.error`, which is `ValueError`, and `pack_into` is not supported.

```python
import struct

buf = struct.pack("<hh", 258, -1)
print(buf.hex(), struct.unpack("<hh", buf), struct.calcsize("<hh"))
```

```text Output
0201ffff (258, -1) 4
```

### test

The test framework, imported by bare name and driven by `edge test` discovery. Test files do not need to call `run()` themselves, the runner evaluates the file and then invokes the driver, so a file that registers no tests fails.

- `@fixture` registers a factory under its function name, built fresh per test.
- `@test("description", *uses)` registers a test and injects named fixtures by keyword.
- `with raises(ExcType):` asserts the block raises, accepts a class or a tuple.
- Assertions are plain `assert`.
- `run()` executes everything registered, prints verdicts and raises `SystemExit(0)` or `SystemExit(1)`.

```python
from test import fixture, test, raises, run

@fixture
def numbers():
    return [1, 2, 3]

@test("sum adds up", "numbers")
def total(numbers):
    assert sum(numbers) == 6

@test("division by zero raises")
def div():
    with raises(ZeroDivisionError):
        1 / 0

run()
```

```text Output
pass. sum adds up
pass. division by zero raises
2 passed, 0 failed
```

## System modules

The project files, the network, the clock and the values a host keeps come from four system modules that ship inside Edge Python, `fs`, `net`, `time` and `secret`. They need no `imports` entry, only a grant from the root `edge.json`, and an import without one fails at compile time.

```python
import time
```

```text Error
'main' imports time, which edge.json does not grant it
```

`permissions` maps each holder to a list of `module:scope` entries. The holder is `main` for the code of the package itself, `all` for it and every package it imports, an import key for the package that key imports, or `eval` for the most a bundle may grant in an eval group. `net:<host>` allows exactly that host, a lowercase name or an IPv4 as `a.b.c.d` with no scheme or port and no IPv6, `net:<host>/<prefix>` bounds it to that path prefix and what sits under it however a server decodes the path, `time:wall`, `time:monotonic` and `time:zone` allow one clock call each, `secret:<NAME>` allows reading that one value, the name in uppercase letters, digits and underscores, and `fs:./<folder>` allows reading the project files under that folder, `fs:.` the whole project. An entry without a scope, like `"net"`, lets the package import the module and reach nothing.

```json
{
  "permissions": {
    "main": ["net:api.example.com", "time:wall"],
    "analytics": ["net:api.telemetry.com"]
  }
}
```

The root grants each package it imports under its import key. A dependency lists in its own `edge.json` what its code needs under `main` and what each of its imports needs under that key, and passes on only what it holds, so its importer has to give it all of that. `edge lock` stops until every package gets what it lists, and no import may be named `all`, `main` or `eval`. A package is what an import key brings in and the nearest `edge.json` above its files, the `name` it declares never counts, code it reaches by a relative import is its own, and a package two importers share loads once for each. Each call checks its scope again, a call outside the grant raises `PermissionError`, a subclass of `OSError`, and a system module with the requests and sockets it opens answers only code of the package that imported it, so another package reaching `pepito.time` gets `PermissionError` too. Every system module also takes `batch(calls)`, a list of `[name, *args]` lists answered in one crossing with the results in order, where the first failure raises. A `.wasm` plugin reaches the same calls through the `Sys` op of the ABI with the grants of its package, and finishes a call that waits in `__edge_resume`.

### time

`now()` returns nanoseconds since the epoch, `now('monotonic')` nanoseconds that never go back, and `zone()` the pair `[name, offset]`, the IANA zone and its offset in seconds. Without a `time` scope anywhere a program reads no clock.

### net

`request(method, url, headers, body)` returns an id at once, `response(id)` waits for `[status, headers]`, and `read(id)` returns the next body chunk as `bytes` or `None` at the end. `connect(url)` opens a WebSocket whose messages come through `read`, `send(id, data)` writes to it and `close(id)` aborts either. Headers are `[name, value]` pairs or a dict and a body is `bytes`, a `str` or `None`. A failed connection raises `OSError` from the call that meets it, and in a browser CORS applies on top of the grant. A url names its host right after the scheme, with no user before it and no backslash, space or control character, else `ValueError`, while an `@` in the path stays. A non-ASCII host raises, pass its punycode. The path is resolved before the check and sent as resolved, so a `.` or `..` segment applies however it is spelled and text outside the unreserved set crosses as escaped UTF-8, and a fragment is dropped. No host follows a redirect, a 301, 302, 303, 307 or 308 raises `OSError` and the new address needs its own `request`. Headers fetch keeps for the host, like `Host`, `Cookie`, `Origin` or any `Sec-` or `Proxy-` one, raise `ValueError`.

```python
import net

r = net.request("GET", "https://api.example.com/items", [["accept", "application/json"]], None)
status, headers = net.response(r)
body = net.read(r)
```

The CLI and the browser run the same JavaScript for these calls, the browser in its Worker and the CLI in SpiderMonkey, so a program answers the same with and without `--web`.

### secret

`read(name)` returns the value the host keeps under that name as a `str`. A name the package does not hold raises `PermissionError`, and one it holds with no value kept raises `OSError`. The CLI reads `EDGE_SECRET_<name>` at the moment of the call and nothing else of its environment, with `--web` it hands the page only the granted values, and a page that embeds the engine passes them to `createWorker` as `secrets`.

```python
import secret

token = secret.read("GITHUB_TOKEN")
```

### fs

`read(path)` returns the text of a project file as a `str`, and `list(dir='.')` every file under a folder in order, both written from the root `edge.json` such as `shop/app/product.rb`. A path with `..`, a leading `/` or a name starting with a dot raises `PermissionError`, like one outside the granted folders, and a file that is missing, not UTF-8 or over 10 MB raises `OSError`. The CLI reads the disk or the bundle and never follows a link, and a page reads the files beside its program, listed in the `edge.files` that `edge web` writes and `edge serve` answers. Nothing writes.

```python
import fs

for path in fs.list("shop"):
    print(fs.read(path))
```

## Actors

`edge actor` runs many isolated programs as cooperative actors over a few threads, share-nothing with message passing. There are two execution models and the manifest chooses per group.

```yaml
runtime:
  listen: tcp://127.0.0.1:7777   # optional, its presence makes the actor a live server
  durable: tmp/actor/log         # WAL path, replays unfinished messages on restart
  schedulers: auto               # or a fixed number
  max_actors: 1000000

groups:
  actor:
    run: app                     # script path or project directory
    replicas: 100                # ceiling, actors spawn on demand
    retry: 2                     # re-deliveries before a message is dropped dead
    seed: ["first message"]      # delivered before the pool starts
    out: stdout                  # stdout, null or file://path
    limits:                      # per-actor sandbox overrides
      memory: 64                 # MB
      preempt: 500
```

Each group picks exactly one of `run`, `code` or `eval: true`. Without `listen:` the pool exits once every message is processed. An actor hands the thread on while it waits and every `preempt` back-edges, and a live server stops reading while its actors fall behind.

### The trusted model

Actors keep state between messages, pick work up with the `receive()` builtin and forward with the `send(group, body)` builtin, strings only, never blocking, and neither needs an import.

```yml actor
groups:
  actor:
    code: |
      msg = receive()
      print(f"got {msg}")
    replicas: 4
    seed: ["hello"]
```

```text Output
got hello
```

### The untrusted model

`eval: true` groups compile each message as its own program in a fresh wasm instance with its own memory, capped by the `memory` limit of the group and by three times that plus 64 MiB of linear memory, and cut off by a deadline the host enforces from outside after `limits: timeout:` seconds of wall-clock time, waits included, ten unless set and at most 300. No state survives between messages. A bundle that carries its own `edge.json` resolves through it, any other message through the manifest of the pool. Either way `.wasm` plugins are refused, remote modules load only from `https://cdn.edgepython.com/` and `send()` has no scheduler, so untrusted code cannot send or load modules from disk. A bundle grants its own permissions in its own `edge.json` as any root does, but only within what the pool grants `eval`, so an entry past it refuses the run before it compiles, and a snippet holds nothing. A `code` or `run` group is trusted instead, it keeps state, can send and can use `fs`, `net`, `time` and `secret` under the grants of the pool, so reach for `eval` when the code is not yours.

```yml untrusted
groups:
  guest:
    eval: true
    seed: ["print(6 * 7)"]
```

```text Output
42
```

With `listen:` the actor accepts one `<group> <body>` line per TCP message and exposes an HTTP control endpoint, `GET /stats`, `POST /pub/<group>` and `POST /eval/<group>` for eval groups. Untrusted bundles arrive as base64 `.edge` payloads behind an `EDGEPKG:` marker and run from memory in a fresh instance.

## Sandbox limits

Programs run under a fixed budget. Exceeding one raises the matching exception, catchable except the op-limit `RuntimeError` whose handler re-raises on its first operation because the budget is still exhausted. Memory and operations can be raised, with `--memory <MB>` and `--ops <n>` on `edge run` and `edge test`, `limits` on `createWorker` and `limits:` on an actor group. Memory counts every byte the allocator hands the program, frames and caches included, and a number in a container takes only 8 bytes, so a list of numbers is far cheaper than an object per row.

| Limit | Value | Raised |
|---|---|---|
| Call depth | Frames up to a sixteenth of the memory limit, 128 nested through builtins, dunders and generators, an `eval` counting three | `RecursionError` |
| Comparison depth of nested containers, for `==`, `!=` and `in` | 100 | `RecursionError` |
| Operations | 100 million | `RuntimeError` |
| Memory | 256 MB | `MemoryError` |
| Source size | 10 MiB | Compile error |
| Nesting of expressions, targets and patterns | 200 | Compile error |
| Indentation depth | 100 | Compile error |
| Instructions per chunk | 65535 | Compile error |
| `repr` output | 1M chars | Truncated |
