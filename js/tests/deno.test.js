/* The engine under Deno with no browser, undeclared names fail and missing Web APIs name themselves. */
const BASE = Deno.env.get("EDGE_CDN_BASE")?.replace(/\/$/, "");
if (!BASE) throw new Error("set EDGE_CDN_BASE (make serve)");
const WASM = `${BASE}/compiler.wasm`;

// A fresh engine per test, the query string keeps the module state apart.
async function boot(name, imports = {}, base = baseUrl) {
    const engine = await import(new URL(`../src/worker/engine.ts?deno=${name}`, import.meta.url).href);
    await engine.load({ wasmUrl: WASM, imports, baseUrl: base });
    return engine;
}

// No manifest lives here, so bare names stay undeclared.
const baseUrl = new URL("./nomanifest/", import.meta.url).href;

// A packed package runs its entry and imports its own files from inside itself, the way a published one arrives.
Deno.test("deno: a packed package imports from inside itself", async () => {
    const enc = new TextEncoder();
    const framed = (bytes) => [...enc.encode(`${bytes.length}\n`), ...bytes];
    const files = { "main.py": "from .src.hello import hello\n", "src/hello.py": "def hello(name):\n    return 'hello ' + name\n", "https://example.com/dep.edge": "" };
    const packed = [...enc.encode("EDGEPKG\x01"), ...framed(enc.encode("main.py")), ...enc.encode(`${Object.keys(files).length}\n`)];
    for (const [path, text] of Object.entries(files)) packed.push(...framed(enc.encode(path)), ...framed(enc.encode(text)));
    const dir = await Deno.makeTempDir();
    await Deno.writeFile(`${dir}/greet.edge`, new Uint8Array(packed));
    const engine = await boot("package", { greet: new URL(`file://${dir}/greet.edge`).href });
    const lines = [];
    const { out } = await engine.run({ src: "from greet import hello\nprint(hello('edge'))" }, (t) => lines.push(t));
    if (out !== "" || lines.join("").trim() !== "hello edge") throw new Error(`unexpected ${JSON.stringify([out, lines])}`);
});

/* A published package declares its own dependency by version, so the host has to read the lock the package carries rather than ask a registry at run time. */
Deno.test("deno: a packed package resolves its own version through the lock it carries", async () => {
    const enc = new TextEncoder();
    const framed = (bytes) => [...enc.encode(`${bytes.length}\n`), ...bytes];
    const pack = (entry, files) => {
        const out = [...enc.encode("EDGEPKG\x01"), ...framed(enc.encode(entry)), ...enc.encode(`${Object.keys(files).length}\n`)];
        for (const [path, text] of Object.entries(files)) out.push(...framed(enc.encode(path)), ...framed(enc.encode(text)));
        return new Uint8Array(out);
    };
    const dir = await Deno.makeTempDir();
    const dep = pack("main.py", { "main.py": "def shout(word):\n    return word.upper()\n" });
    await Deno.writeFile(`${dir}/dep.edge`, dep);
    const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", dep))].map((b) => b.toString(16).padStart(2, "0")).join("");
    const url = new URL(`file://${dir}/dep.edge`).href;

    const app = pack("main.py", {
        "main.py": "from dep import shout\n\ndef greet(name):\n    return shout('hi ' + name)\n",
        "edge.json": JSON.stringify({ imports: { dep: "0.1.0" } }),
        "edge.lock": JSON.stringify({ dep: { version: "0.1.0", url, digest: `sha256-${digest}` } }),
    });
    await Deno.writeFile(`${dir}/app.edge`, app);

    const engine = await boot("lock", { app: new URL(`file://${dir}/app.edge`).href });
    const lines = [];
    const { out } = await engine.run({ src: "from app import greet\nprint(greet('edge'))" }, (t) => lines.push(t));
    if (out !== "" || lines.join("").trim() !== "HI EDGE") throw new Error(`unexpected ${JSON.stringify([out, lines])}`);
});

// A version nothing resolved names the command that resolves it, rather than blaming the fetch it would have made.
Deno.test("deno: a version no lock holds names edge lock", async () => {
    const enc = new TextEncoder();
    const framed = (bytes) => [...enc.encode(`${bytes.length}\n`), ...bytes];
    const files = { "main.py": "print(1)\n", "edge.json": JSON.stringify({ imports: { dep: "0.1.0" } }) };
    const out = [...enc.encode("EDGEPKG\x01"), ...framed(enc.encode("main.py")), ...enc.encode(`${Object.keys(files).length}\n`)];
    for (const [path, text] of Object.entries(files)) out.push(...framed(enc.encode(path)), ...framed(enc.encode(text)));
    const dir = await Deno.makeTempDir();
    await Deno.writeFile(`${dir}/app.edge`, new Uint8Array(out));

    const engine = await boot("unlocked", { app: new URL(`file://${dir}/app.edge`).href });
    let failed = "";
    try {
        await engine.run({ src: "import app" });
    } catch (e) {
        failed = String(e);
    }
    if (!failed.includes("'dep' is not locked, run edge lock")) throw new Error(`unexpected ${JSON.stringify(failed)}`);
});

Deno.test("deno: an undeclared name fails at compile time", async () => {
    const engine = await boot("undeclared");
    const { out } = await engine.run({ src: "import json\nprint(1)" });
    if (!out.includes("module 'json' is not provided by this host and no edge.json declares it")) throw new Error(`unexpected output ${JSON.stringify(out)}`);
});

Deno.test("deno: send() names the actor scheduler it lacks", async () => {
    const engine = await boot("send");
    const lines = [];
    const missing = "send() needs an actor scheduler, missing in this runtime";
    const caught = await engine.run({ src: "try:\n    send('g', 'x')\nexcept RuntimeError as e:\n    print(e)" }, (t) => lines.push(t));
    if (caught.out !== "" || lines.join("").trim() !== missing) throw new Error(`unexpected ${JSON.stringify([caught.out, lines])}`);
    const { out } = await engine.run({ src: "send('g', 'x')" });
    if (!out.includes(missing) || !out.includes("<input>:1:1")) throw new Error(`unexpected output ${JSON.stringify(out)}`);
});

Deno.test("deno: a manifest beside a module joins its relative targets once", async () => {
    const dir = await Deno.makeTempDir();
    await Deno.mkdir(`${dir}/pkg`);
    await Deno.writeTextFile(`${dir}/edge.json`, JSON.stringify({ imports: { pkg: "./pkg/entry.py" } }));
    await Deno.writeTextFile(`${dir}/pkg/edge.json`, JSON.stringify({ imports: { _impl: "./impl.py" } }));
    await Deno.writeTextFile(`${dir}/pkg/entry.py`, "from _impl import value\n");
    await Deno.writeTextFile(`${dir}/pkg/impl.py`, "value = 42\n");
    const engine = await boot("facade", {}, `file://${dir}/`);
    const lines = [];
    const { out } = await engine.run({ src: "from pkg import value\nprint(value)" }, (t) => lines.push(t));
    await Deno.remove(dir, { recursive: true });
    if (out !== "" || lines.join("").trim() !== "42") throw new Error(`unexpected ${JSON.stringify([out, lines])}`);
});

/* A project on disk, the edge.json a run reads its grants from beside its files. */
async function project(files) {
    const dir = await Deno.makeTempDir();
    for (const [path, text] of Object.entries(files)) {
        const at = `${dir}/${path}`;
        await Deno.mkdir(at.slice(0, at.lastIndexOf("/")), { recursive: true });
        await Deno.writeTextFile(at, text);
    }
    return new URL(`file://${dir}/`).href;
}

// What a run printed and what it failed with, from a fresh engine.
async function output(name, src, base) {
    const engine = await boot(name, {}, base);
    const lines = [];
    const { out } = await engine.run({ src }, (t) => lines.push(t));
    return { out, text: lines.join("").trim() };
}

Deno.test("deno: a system module answers only to its grant", async () => {
    const bare = await output("ungranted", "import time", baseUrl);
    if (!bare.out.includes("'main' imports time, which edge.json does not grant it")) throw new Error(`unexpected ${JSON.stringify(bare)}`);
    const base = await project({ "edge.json": JSON.stringify({ permissions: { main: ["time:wall"] } }) });
    const src = "import time\nprint(time.now() > 10 ** 18)\ntry:\n    time.now('monotonic')\nexcept PermissionError as e:\n    print(e)";
    const granted = await output("granted", src, base);
    if (granted.out !== "" || granted.text !== "True\n'main' has no time:monotonic, edge.json grants it time:wall") throw new Error(`unexpected ${JSON.stringify(granted)}`);
});

Deno.test("deno: a grant belongs to the package it names", async () => {
    const files = {
        "clock/edge.json": JSON.stringify({ name: "clock" }),
        "clock/main.py": "import time\n\ndef now():\n    return time.now()\n",
    };
    const manifest = (permissions) => JSON.stringify({ imports: { clock: "./clock/main.py" }, permissions });
    const parent = await output("parent-only", "from clock import now\nprint(now() > 0)", await project({ ...files, "edge.json": manifest({ main: ["time:wall"] }) }));
    if (!parent.out.includes("'clock' imports time, which edge.json does not grant it")) throw new Error(`unexpected ${JSON.stringify(parent)}`);
    const child = await output("child", "from clock import now\nprint(now() > 0)", await project({ ...files, "edge.json": manifest({ clock: ["time:wall"] }) }));
    if (child.out !== "" || child.text !== "True") throw new Error(`unexpected ${JSON.stringify(child)}`);
});

// A package named main answers to its import key, so it never borrows the grant of main.
Deno.test("deno: a package named main never takes the grant of main", async () => {
    const got = await output("named-main", "from clock import now\nprint(now() > 0)", await project({
        "clock/edge.json": JSON.stringify({ name: "main" }),
        "clock/main.py": "import time\n\ndef now():\n    return time.now()\n",
        "edge.json": JSON.stringify({ imports: { clock: "./clock/main.py" }, permissions: { main: ["time:wall"] } }),
    }));
    if (!got.out.includes("'clock' imports time, which edge.json does not grant it")) throw new Error(`unexpected ${JSON.stringify(got)}`);
});

// No JavaScript loads besides the host's own, so an import of one fails where it is written.
Deno.test("deno: a JavaScript import is refused at compile time", async () => {
    const base = await project({ "edge.json": JSON.stringify({ imports: { charts: "./charts.js" } }), "charts.js": "export const draw = () => 1;\n" });
    const got = await output("javascript", "from charts import draw", base);
    if (!got.out.includes("module 'charts' is JavaScript, ship a .py or a .wasm")) throw new Error(`unexpected ${JSON.stringify(got)}`);
});

// A module the root imports by a ./ path sits in the root's own dir, so it is the program's code.
Deno.test("deno: a helper the root imports by a ./ path belongs to main", async () => {
    const base = await project({
        "edge.json": JSON.stringify({ imports: { helper: "./helper.py" }, permissions: { main: ["time:wall"] } }),
        "helper.py": "import time\n\ndef later():\n    return time.now() > 0\n",
    });
    const got = await output("helper", "from helper import later\nprint(later())", base);
    if (got.out !== "" || got.text !== "True") throw new Error(`unexpected ${JSON.stringify(got)}`);
});

// A package below the root holds what its importer passes it, and never more than that importer holds.
Deno.test("deno: a grandchild holds what its importer passes it", async () => {
    const files = (passes) => ({
        "clock/edge.json": JSON.stringify({ name: "clock", imports: { trace: "./trace/main.py" }, permissions: passes }),
        "clock/main.py": "from trace import stamp\n\ndef now():\n    return stamp()\n",
        "clock/trace/edge.json": JSON.stringify({ name: "trace" }),
        "clock/trace/main.py": "import time\n\ndef stamp():\n    return time.now() > 0\n",
    });
    const manifest = (permissions) => JSON.stringify({ imports: { clock: "./clock/main.py" }, permissions });
    const src = "from clock import now\nprint(now())";
    const unpassed = await output("unpassed", src, await project({ ...files({}), "edge.json": manifest({ main: ["time:wall"], clock: ["time:wall"], trace: ["time:wall"] }) }));
    if (!unpassed.out.includes("'trace' imports time, which edge.json does not grant it")) throw new Error(`unexpected ${JSON.stringify(unpassed)}`);
    const unheld = await output("unheld", src, await project({ ...files({ trace: ["time:wall"] }), "edge.json": manifest({ main: ["time:wall"] }) }));
    if (!unheld.out.includes("'trace' imports time, which edge.json does not grant it")) throw new Error(`unexpected ${JSON.stringify(unheld)}`);
    const passed = await output("passed", src, await project({ ...files({ trace: ["time:wall"] }), "edge.json": manifest({ clock: ["time:wall"] }) }));
    if (passed.out !== "" || passed.text !== "True") throw new Error(`unexpected ${JSON.stringify(passed)}`);
});

Deno.test("deno: net reaches only the hosts its package holds", async () => {
    const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (req) => new Response(`got ${new URL(req.url).pathname}`));
    const port = server.addr.port;
    const base = await project({ "edge.json": JSON.stringify({ permissions: { main: ["net:127.0.0.1"] } }) });
    const src = [
        "import net",
        `r = net.request('GET', 'http://127.0.0.1:${port}/items')`,
        "status, headers = net.response(r)",
        "print(status, net.read(r), net.read(r))",
        "try:",
        `    net.request('GET', 'http://localhost:${port}/')`,
        "except PermissionError as e:",
        "    print(e)",
    ].join("\n");
    const got = await output("net", src, base);
    await server.shutdown();
    if (got.out !== "" || got.text !== "200 b'got /items' None\n'main' has no net:localhost, edge.json grants it net:127.0.0.1") throw new Error(`unexpected ${JSON.stringify(got)}`);
});

// The pdk example, built by `make plugin`.
const PLUGIN = new URL("../../target/wasm32-unknown-unknown/release/slugify_mod.wasm", import.meta.url);

Deno.test("deno: a plugin reaches system calls and awaits the ones that wait", async () => {
    const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, () => new Response("ok", { status: 201 }));
    const base = await project({ "edge.json": JSON.stringify({ imports: { slugify_mod: "./slugify_mod.wasm" }, permissions: { main: ["time:wall", "net:127.0.0.1"] } }) });
    await Deno.writeFile(new URL("slugify_mod.wasm", base), await Deno.readFile(PLUGIN));
    const src = [
        "from slugify_mod import wall_ns, status_of",
        "print(wall_ns() > 10 ** 18)",
        `print(status_of('http://127.0.0.1:${server.addr.port}/'))`,
        "try:",
        "    status_of('http://127.0.0.1:1/')",
        "except OSError as e:",
        "    print(type(e).__name__)",
        "try:",
        "    status_of('http://evil.example/')",
        "except PermissionError as e:",
        "    print(e)",
    ].join("\n");
    const got = await output("plugin-system", src, base);
    await server.shutdown();
    if (got.out !== "" || got.text !== "True\n201\nOSError\n'main' has no net:evil.example, edge.json grants it net:127.0.0.1") throw new Error(`unexpected ${JSON.stringify(got)}`);
});

Deno.test("deno: a malformed permissions section stops the run", async () => {
    const base = await project({ "edge.json": JSON.stringify({ permissions: { main: "time:wall" } }) });
    const engine = await boot("malformed", {}, base);
    let message = "";
    try {
        await engine.run({ src: "print(1)" });
    } catch (e) {
        message = e.message;
    }
    if (!message.includes("edge.json at 'edge.json': permissions for 'main' must be a list of entries")) throw new Error(`unexpected ${JSON.stringify(message)}`);
});
