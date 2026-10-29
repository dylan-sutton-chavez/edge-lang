---
name: edge
description: Run, test, pack and publish Edge Lang projects with the edge CLI, with their imports, permissions, actor pools and sandbox limits. Use when working in an Edge Lang project or when the user asks how to use the edge CLI.
---

# Edge Lang

This document is self-verifying and its examples follow the cells v1 grammar. A `mini` or `yml` block followed immediately by a `text` block is a runnable cell, and the `skill` crate beside it in the repository executes every cell through the edge CLI and compares it against the `text` block. The tag on the `text` block is `Output` for a run whose stdout must equal the block, or `Error` for a failing run whose stderr contains the given text. A `mini` block tagged `skip` never runs on any engine and never pairs with a `text` block, and it always says why with one `// skip` comment at the exact construct that is nondeterministic. A `yml` block tagged `actor` runs a trusted actor pool through `edge actor`, while one tagged `untrusted` runs eval groups. Any `mini` block without a `text` pair is illustrative only. Every cell runs in a scratch directory holding the `edge.json` the runner writes, `{}`, since nothing resolves undeclared, and the `yml` cells find the same manifest beside their `actor.yml`. Verify the whole file from the repository root with `make test-skill`, which builds the CLI first, or with `cargo run -p skill -- skill/SKILL.md --edge <path to edge>`.

Edge Lang is an engine for building language runtimes, a single-pass compiler to bytecode and a register VM in one WebAssembly binary. The `edge` CLI runs mini, the reference runtime built on it, on macOS, Linux and WSL, and the JS host runs the same `edge.wasm` in browsers and in JavaScript runtimes such as Deno. Mini opens a block with a header colon and deeper indentation, writes `fn`, `true`, `false`, `nil`, `&&`, `||`, `!` and `else if`, starts a comment with `//` and divides toward zero at `~/`, where `%` takes the sign of the number it divides. A test reads `nil`, `false`, `0`, NaN and `""` as false and every list, map, set and tuple as true, empty or not. Every module is a file or a package declared in `edge.json` and resolved at compile time. Programs are deterministic, and a program reaches no network or file unless its `edge.json` grants it.

Use this skill to drive the edge CLI. It covers running, testing, packing and publishing a project, declaring what it imports, granting it the network and files, running actor pools and staying inside the sandbox limits.

## The working loop

A project is any folder with `.mini` files and an `edge.json` declaring every module they import. The loop is always the same.

1. Write or edit the `.mini` files.
2. Run the entry point with `edge run main.mini`.
3. Add tests in `*_test.mini` files and run `edge test`.
4. Pack a release with `edge build` when the program must run elsewhere.

```bash
edge init myapp                          # scaffold main.mini, edge.json and index.html
cd myapp
edge run main.mini                       # run the entry point
echo 'assert 6 * 7 == 42' > answer_test.mini
edge test                                # run every *_test.mini under the folder
edge add slugify=./slugify_mod.wasm      # declare a module by path, by URL or by registry name
edge lock                                # pin each declared version and URL in edge.lock
edge build                               # pack a portable ./app.edge bundle
```

`edge init` writes `main.mini`, which prints one line, an `index.html` that runs it in a browser, and an `edge.json` holding only the engine that made it.

```json
{
  "edge": "0.9.7"
}
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

`edge run [file]` executes a `.mini` script, a packed `.edge` bundle or an app binary, auto-detected by content. With no file it reads the script from stdin. A bare `edge run` in a terminal with no pipe errors. `edge run -c 'print(1)'` runs inline code instead of a file or stdin, and piped stdin then feeds `input()`.

Run flags.

| Flag | Effect |
|---|---|
| `--events <f>` | Each line of the file or FIFO feeds one `receive()` call, EOF parks the script |
| `--save-state <f>` | When the script parks on an unservable wait, write a snapshot blob, print `state saved` to stderr and exit 0 |
| `--restore-state <f>` | Boot from a snapshot blob instead of a script and keep running |
| `--preempt <n>` | Yield every `n` loop back-edges so even a tight loop stays snapshottable, 0 disables |
| `--memory <MB>` | Memory the run may hold, 256 by default |
| `--ops <n>` | Operations the run may spend, 100 million by default |
| `--web` | Run on the browser host in headless Chrome, refusing the four snapshot flags since they belong to the native engine |

`exit(code)` ends the run cleanly with that code and no traceback, and `exit()` ends it with 0. Any other uncaught error prints a traceback and exits 1.

`--web` serves the JS host and the engine from the binary on a loopback port, so only the modules a manifest declares by URL leave the machine. It drives a Chrome the system already has, or one it offers to download once into `~/.local/share/edge/chromium`, which `edge uninstall` offers to remove. `EDGE_CHROME_PATH` names a browser directly and skips the search.

`send(channel, line)` writes one line to a named output outside an actor pool, in order with what the run prints. A plain name prints its lines under the name, and `file:<path>` writes a file in the directory the run started in, a path below it with no `..`. Only the root package writes a file channel without a grant.

```mini
send("report", "github up")
print("checked 1 service")
```

```text Output
report
github up
checked 1 service
```

### edge test

`edge test [path]` discovers `*_test.mini` recursively, skipping hidden dirs and `dist`. A file argument runs exactly that file. Each file executes in a fresh session and state never leaks between files. A file passes when it ends cleanly or exits with 0, so a failed `assert` fails it, and its output never decides it. The exit code is 0 when everything passes, 1 when a file fails or no tests are found, and 2 when the engine cannot start. `--memory` and `--ops` apply as they do for `edge run`.

`edge test --web` runs the same files on the browser host in headless Chrome, one browser for the suite with a fresh page per file, so a package can prove it answers the same in a browser.

### edge init, edge add, edge remove, edge lock

`edge init [name]` scaffolds `main.mini`, `edge.json` and `index.html` in a new directory, or in the current one with no name. `--bare` skips the HTML. It never overwrites a file, and refuses with the name of the first one that already exists.

`edge add <name>` looks a name up in the registry at its newest version, or at the one `<name>@0.1.0` names, writes one `imports` entry holding that version alone, and prints it with what the package asks the root to grant. `edge add foo=<url>` or `edge add foo=./path` registers a custom spec verbatim, also under `imports`, since each host tells a `.mini` and a `.wasm` module apart by the artifact, and a `.js` or `.mjs` spec is refused before anything is written. `edge add` keeps `extends` and any other key already there. `edge remove` deletes entries. Unknown names abort the whole command before any write, and neither command touches `edge.lock`. The same registry answers over HTTP, where `GET https://edgelang.dev/api/search?q=<term>` finds a package and every address it names answers as data under `/api`.

`edge lock` turns each declared version into the URL and digest of that release, pins each bare URL to the digest of its bytes, and writes `edge.lock` beside the manifest, rebuilding the whole file each time. It is the only command that asks the registry where a name points, so `edge run`, `edge test` and `edge build` read the lock and resolve nothing themselves. A version with no entry, or one whose entry holds another release, fails with `'<name>' is not locked, run edge lock`. It also walks every package in the tree, each through its own `edge.lock`, and writes nothing while a package lacks what it lists from its importer. `edge run` and `edge test` repeat that check on the nearest `edge.json` above the script or the tested path before compiling, and `edge build` on the project's own.

### edge serve

A static dev server with live reload for the current directory. `--host` defaults to `127.0.0.1`, `--port` to 5173, `--open` opens a browser. On a loopback address it answers only a request whose `Host` names that address, with 421 for any other, and a path through a dot entry or a link out of the directory answers 404.

### edge build

Three mutually exclusive modes.

| Mode | Default output | Artifact |
|---|---|---|
| `edge build` | `app.edge` | Raw bundle for hosts and pools that already have `edge` |
| `edge build --app` | `app` | Standalone binary, runs offline on the same OS and CPU with nothing installed |
| `edge build --web` | `dist/` | Browser distribution with the vendored JS host and packages |

`--out <path>` overrides the default. The bundle contains every `.mini` and `.wasm` under the project plus `edge.json`, each `edge.lock`, the `README.md` and any `LICENSE` at the project root, together with each module the manifest declares by URL and the files it imports. When `edge.json` declares a `docs` directory, a `.edge` also carries its `.mdx` pages under a reserved `@docs/` prefix, checked against the rendering convention first, and an app binary leaves them out. The entry is `main.mini`, `app.mini` or `index.mini`, the first present. An app binary accepts only the snapshot flags `--save-state`, `--restore-state`, `--preempt` and `--events`.

### edge publish

`edge publish app.edge` uploads a `.edge` packed by `edge build`, reading `EDGE_TOKEN` for a token made at `edgelang.dev/settings#tokens`, which signs the request and never leaves the machine. The artifact is the whole request. The registry opens it and reads the `name`, the `version`, the description, the repository, the `LICENSE` and the pages under `@docs/` out of the bytes it is about to store, so nothing is declared twice and a listing shows what you shipped. A name is first come and permanent, a version is never overwritten and publishing one twice exits 2, and the bundle is stored exactly as packed. One artifact is 10 MB at most, an account claims 10 names and publishes 60 versions a day, and its packages add up to 50 MB when it signs in by address alone or a gigabyte with GitHub or Google linked.

### edge actor

`edge actor <file>` runs an actor pool from an `actor.yml` manifest, resolving group imports through the `edge.json` beside it or the one `--manifest` names. See the actors section for the schema and the two execution models.

### edge uninstall

Interactive removal of the binary and PATH entries.

### Environment variables

| Variable | Effect |
|---|---|
| `EDGE_TOKEN` | The registry token `edge publish` signs with |
| `EDGE_CHROME_PATH` | The browser `--web` drives |
| `EDGE_CDN_BASE` | Serve the official CDN origin from another base for module downloads and `edge build --web`, used by tests and staging |
| `EDGE_SITE_BASE` | Ask another registry than `https://edgelang.dev`, used by tests and staging |
| `EDGE_WASM` | Path to the `edge.wasm` a build of the CLI embeds |

## Imports

Every import resolves at compile time through a host resolver. The compiler flattens each module into the bytecode and the VM fetches nothing at run time.

Dotted specs import files. A leading dot anchors at the importing file, one extra dot per directory up. Without it the spec anchors at the nearest `edge.json` dir. The `.mini` suffix is implicit.

```mini
from .lib.helpers import slug
from ..shared.util import chunks
from lib.helpers import slug as sl
```

Not supported. `from . import x` and any form of dynamic import.

Bare names resolve through `edge.json`, walking up from the importing file with the nearest manifest winning. The manifest maps each name to a path, a URL, or a `major.minor.patch` version naming a registry package under `imports`, and `extends` may name a parent manifest. A version resolves through the `edge.lock` beside the manifest that declared it, which every host reads before the compiler sees the manifest, so the compiler only ever meets a path or a URL. An `edge` field names the lowest engine the project runs on, `major.minor.patch`, and it runs on that version or any later one, never on an earlier one, which stops with `this project needs edge 1.0.0, this is 0.9.7`. The artifact decides the kind, `.mini` is a code module and `.wasm` a native plugin, so a manifest never classifies a package, and a `.js` or `.mjs` import fails with `module 'charts' is JavaScript, ship a .mini or a .wasm`, since no JavaScript loads besides the host itself. `permissions` grants the host modules, see that section. `name`, `version`, `description`, `repository` and `docs` are the registry fields, ignored by the compiler and shape-checked by the CLI. Only what nothing else supplies belongs there, so an author and a date come from the publishing account and the license is read from the packed `LICENSE` file.

```json
{
  "imports": {
    "utils": "./lib/utils.mini",
    "slugify": "https://example.com/slugify_mod.wasm"
  }
}
```

A registry name resolves only when declared, `edge add <name>` then `edge lock` writes each entry, and an undeclared name fails at compile time.

```mini
import nothere
```

```text Error
module 'nothere' is not provided by this host and no edge.json declares it
```

The CLI adds a `help:` line that says to declare the name or import it by a relative path, and a run asks no registry about it. Modules are singletons with shared mutable state, and an import cycle fails at compile time with `circular import: '<spec>'` naming the module it comes back to.

## Snapshots

The module body itself runs as a coroutine the host drives. `receive()` parks it until a host event or an actor message arrives, and `send(group, body)` hands a string to an actor group inside a pool. A host call or a `receive()` that waits inside a coroutine or a generator stepped by `send` or `next` raises `RuntimeError`, only the module body, the plain functions it calls and the generators a `for` loop steps can wait.

The CLI can serialize the full interpreter state, heap, globals and the suspended module body, and restore it later. This is how long-running or event-driven programs survive process restarts.

```bash
edge run app.mini --save-state state.bin   # writes the blob when the script parks
edge run --restore-state state.bin         # resumes from the blob
edge run app.mini --preempt 500            # makes even a bare while true snapshottable
```

A snapshot is taken when the script parks on a wait the engine cannot serve, for example `receive()` with no events left. Without `--save-state` such a park is an error. The blob embeds a bytecode fingerprint and a digest of the runtime's natives and only restores into the same program on the same runtime, and a SHA-256 seal makes a damaged blob fail to restore instead of running. The seal stops damage, not forgery, so a blob that passes through untrusted hands needs a signature of the host. A restored run keeps the op budget it saved and the events and input lines it had not read, capped by the run's `--ops` and `--memory` or their defaults, and a blob whose heap needs more memory than that is refused. Feed a resumed run with `--events file`, one `receive()` line per call.

## Host modules

The network comes from `net`, which the CLI and the browser both serve, and the CLI also serves `files`, which reads the local files a grant names. A host module needs no `imports` entry, only a grant from the root `edge.json`, and an import without one fails at compile time. A program reaches no clock or environment at all.

```mini
import net
```

```text Error
'main' imports net, which edge.json does not grant it
```

`permissions` maps each holder to a list of `module:scope` entries. The holder is `main` for the code of the package itself, `all` for it and every package it imports, an import key for the package that key imports, or `eval` for the most a bundle may grant in an eval group. `net:<host>` allows exactly that host, a lowercase name or an IPv4 as `a.b.c.d` with no scheme or port and no IPv6, and `net:<host>/<prefix>` bounds it to that path prefix and what sits under it however a server decodes the path. `files:<path>` allows one file below the directory of the package's `edge.json`, and `files:<dir>/` every file under that directory. An entry without a scope, like `"net"`, lets the package import the module and reach nothing.

```json
{
  "permissions": {
    "main": ["net:www.githubstatus.com", "files:checks/hosts.txt"],
    "analytics": ["net:api.telemetry.com"]
  }
}
```

The root grants each package it imports under its import key. A dependency lists in its own `edge.json` what its code needs under `main` and what each of its imports needs under that key, and passes on only what it holds, so its importer has to give it all of that. `edge lock` stops until every package gets what it lists, and no import may be named `all`, `main` or `eval`. A package is what an import key brings in and the nearest `edge.json` above its files, the `name` it declares never counts, code it reaches by a relative import is its own, and a package two importers share loads once for each. A grant under the key of a local module with no `edge.json` of its own reaches nothing and stops the run before it compiles. Each call checks its scope again, a call outside the grant raises `PermissionError`, and a host module with the requests and sockets it opens answers only code of the package that imported it, so another package reaching `analytics.net` gets `PermissionError` too. A `.wasm` plugin reaches the same calls through the `Sys` op of the ABI with the grants of its package, and finishes a call that waits in `__edge_resume`.

### net

`request(method, url, headers, body, inflate, timeout)` returns an id at once, `response(id)` waits for `[status, headers]`, and `read(id)` returns the next body chunk as `bytes` or `nil` at the end and after. With `inflate` as `'gzip'` a read hands back the inflated bytes, and a body arrives one chunk per read so a large one never sits in memory whole. `timeout` bounds the head and each body read to that many seconds, 30 unless set and at most 86400, and a read counts it only while it waits. `connect(url)` opens a WebSocket whose messages come through `read`, `send(id, data)` writes to it and `close(id)` aborts either. Headers are `[name, value]` pairs or a dict and a body is `bytes`, a `str` or `nil`. A failed connection raises `OSError` from the call that meets it, and in a browser CORS applies on top of the grant. A package holds at most 128 requests and sockets open at once, and a socket keeps at most 1024 unread messages and 16 MB. A url names its host right after the scheme, with no user before it and no backslash, space or control character, else `ValueError`, while an `@` in the path stays. A non-ASCII host raises, pass its punycode. The path is resolved before the check and sent as resolved, so a `.` or `..` segment applies however it is spelled and text outside the unreserved set crosses as escaped UTF-8, and a fragment is dropped. No host follows a redirect, a 301, 302, 303, 307 or 308 raises `OSError` and the new address needs its own `request`. Headers fetch keeps for the host, like `Host`, `Cookie`, `Origin` or any `Sec-` or `Proxy-` one, raise `ValueError`.

```mini skip
import net

r = net.request("GET", "https://www.githubstatus.com/api/v2/status.json", [["accept", "application/json"]], nil)
status, headers = net.response(r) // skip since a live server answers as it stands that day
body = net.read(r)
```

The CLI runs these calls in Rust and the browser in TypeScript, and one suite of cases holds both to the same answers, so a program answers the same with and without `--web`.

### files

`open(path, inflate)` returns an id once a grant reaches the path and the file opens, `read(id)` returns the next chunk as `bytes` or `nil` at the end, and `close(id)` drops it. A path is names joined by slashes below the package's `edge.json`, with no `.` or `..` segment, no leading slash and no backslash. With `inflate` as `'gzip'` a read hands back the inflated bytes. A refused path raises `ValueError`, a path no grant reaches `PermissionError`, and a missing file `OSError`. The JS host serves no files, and an untrusted run reads only below its pool's `edge.json`.

```mini
import files
```

```text Error
'main' imports files, which edge.json does not grant it
```

## Actors

`edge actor` runs many isolated programs as cooperative actors over a few threads, share-nothing with message passing. There are two execution models and the manifest chooses per group.

```yaml
runtime:
  listen: tcp://127.0.0.1:7777   # optional, its presence makes the actor a live server
  control: tcp://127.0.0.1:9090  # optional loopback address of a live server's HTTP endpoint
  durable: tmp/actor/log         # message log path, replays unfinished messages on restart
  schedulers: auto               # or a fixed number
  max_actors: 1000000            # ceiling on the actors of each group

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
      print("got " + msg)
    replicas: 4
    seed: ["hello"]
```

```text Output
got hello
```

### The untrusted model

`eval: true` groups compile each message as its own program in a fresh wasm instance with its own memory, capped by the `memory` limit of the group and by three times that plus 64 MiB of linear memory, and cut off by a deadline the host enforces from outside after `limits: timeout:` seconds of wall-clock time, waits included, ten unless set and at most 300. An eval group holds four actors unless `replicas` says otherwise. No state survives between messages. A bundle that carries its own `edge.json` resolves through it, any other message through the manifest of the pool. Either way `.wasm` plugins are refused, remote modules load only from `https://cdn.edgelang.dev/`, and `send()` has no scheduler, so a send raises and the pool names the group it asked for. A bundle grants its own permissions in its own `edge.json` as any root does, but only within what the pool grants `eval`, so an entry past it refuses the run before it compiles, and a snippet holds nothing. A failed eval run prints its traceback under its group and counts as a crash. A `code` or `run` group is trusted instead, it keeps state, can send and can use `net` and `files` under the grants of the pool, so reach for `eval` when the code is not yours.

```yml untrusted
groups:
  guest:
    eval: true
    seed: ["print(6 * 7)"]
```

```text Output
42
```

With `listen:` the actor accepts one `<group> <body>` line per TCP message, and with `control:` beside it a live server also exposes an HTTP control endpoint on a loopback address, `GET /stats`, `POST /pub/<group>` and `POST /eval/<group>` for eval groups. Without `control:` it serves no HTTP at all. A `/pub` answers 202 once its message is written and synced to the log, and 503 when the log fails. Untrusted bundles arrive as base64 `.edge` payloads behind an `EDGEPKG:` marker and run from memory in a fresh instance.

## Sandbox limits

Programs run under a fixed budget. Exceeding one raises the matching exception, catchable except the op-limit `RuntimeError` whose handler re-raises on its first operation because the budget is still exhausted. Memory and operations can be raised, with `--memory <MB>` and `--ops <n>` on `edge run` and `edge test`, `limits` on `createWorker` and `limits:` on an actor group. Memory counts every byte the allocator hands the program, frames and caches included, and a number in a container takes only 8 bytes, so a list of numbers is far cheaper than an object per row.

| Limit | Value | Raised |
|---|---|---|
| Call depth | 1000 frames, 128 nested through natives and generators | `RecursionError` |
| Comparison depth of nested containers, for `==`, `!=` and `in` | 100 | `RecursionError` |
| Operations | 100 million | `RuntimeError` |
| Memory | 256 MB | `MemoryError` |
| Source size | 10 MiB | Compile error |
| Nesting of expressions, targets and patterns | 200 | Compile error |
| Indentation depth | 100 | Compile error |
| Instructions per chunk | 65535 | Compile error |
