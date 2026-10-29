const ROOT = new URL("../../", import.meta.url);
// as many records as the pinned hour holds each the size of a small event
const RECORDS = 85569;

/* an hour of records made here so the suite downloads nothing and writes nowhere */
async function hour() {
    const lines = [];
    const noise = new Uint8Array(80);
    for (let n = 0; n < RECORDS; n++) {
        crypto.getRandomValues(noise);
        const pad = [...noise].map((b) => b.toString(16).padStart(2, "0")).join("");
        lines.push(`{"type":"PushEvent","id":${n},"pad":"${pad}"}\n`);
    }
    const gz = new Blob([lines.join("")]).stream().pipeThrough(new CompressionStream("gzip"));
    return new Uint8Array(await new Response(gz).arrayBuffer());
}

/* refuses a runtime older than its sources so no test reads a stale one */
async function fresh(runtime) {
    const built = (await Deno.stat(new URL(`target/wasm32-unknown-unknown/release/${runtime}.wasm`, ROOT))).mtime;
    const dirs = ["src", "lib/src", `examples/${runtime === "edge" ? "mini" : runtime}/src`].map((dir) => new URL(`${dir}/`, ROOT));
    while (dirs.length > 0) {
        const dir = dirs.pop();
        for await (const entry of Deno.readDir(dir)) {
            const at = new URL(entry.isDirectory ? `${entry.name}/` : entry.name, dir);
            if (entry.isDirectory) dirs.push(at);
            else if ((await Deno.stat(at)).mtime > built) throw new Error(`${runtime}.wasm is older than ${at.pathname}, run make wasm wasm-tiny`);
        }
    }
}

// a fresh engine per test with net wired and nothing bounding it
async function boot(name, runtime, ops = 0) {
    await fresh(runtime);
    const at = (path) => new URL(`${path}?stop=${name}`, import.meta.url).href;
    const engine = await import(at("../src/worker/engine.ts"));
    const { default: net } = await import(at("../../examples/net/net.ts"));
    engine.wire(net);
    const wasmUrl = new URL(`target/wasm32-unknown-unknown/release/${runtime}.wasm`, ROOT).href;
    await engine.load({ wasmUrl, limits: ops ? { ops } : null });
    return engine;
}

/* the pinned hour on loopback a chunk per pull counting what the host took */
async function serve() {
    const bytes = await hour();
    let stopped = () => {};
    const seen = { pulled: 0, size: bytes.length, cancelled: new Promise((resolve) => { stopped = resolve; }) };
    const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, () => {
        let at = 0;
        const body = new ReadableStream({
            pull(controller) {
                if (at >= bytes.length) return controller.close();
                const end = Math.min(at + 65536, bytes.length);
                controller.enqueue(bytes.slice(at, end));
                seen.pulled = at = end;
            },
            cancel() { stopped(true); },
        }, { highWaterMark: 0 });
        return new Response(body, { headers: { "content-type": "application/gzip" } });
    });
    return { server, seen, url: `http://127.0.0.1:${server.addr.port}/2026-10-01-15.json.gz` };
}

/* whether the fixture saw its body cancelled within a few seconds of the run */
const cancelled = (seen) => Promise.race([seen.cancelled, new Promise((resolve) => setTimeout(() => resolve(false), 5000))]);

// mini reads the body until it has enough lines or the hour ends
const reader = (url, enough) => `import net

r = net.request('GET', '${url}', nil, nil, 'gzip')
net.response(r)
lines = 0
chunk = net.read(r)
while chunk is !nil && lines < ${enough}:
    lines += chunk.count(b'\\n')
    chunk = net.read(r)
net.close(r)
print(lines)
`;

Deno.test("stop: a flow past its budget aborts its request before the hour ends", async () => {
    const { server, seen, url } = await serve();
    const engine = await boot("budget", "tiny", 2000000);
    const { out } = await engine.run({ src: `from: ${url}\ntotal:\n  count:\n` });
    const stopped = await cancelled(seen);
    await server.shutdown();
    // tiny words a spent budget in its own classes
    if (!out.includes("OutOfBudget, the run spent its budget of steps")) throw new Error(`unexpected ${JSON.stringify(out)}`);
    if (!stopped || seen.pulled >= seen.size / 2) throw new Error(`the host took ${seen.pulled} of ${seen.size} bytes, cancelled ${stopped}`);
});

Deno.test("stop: a program that closes its request early reads only the start of the hour", async () => {
    const { server, seen, url } = await serve();
    const engine = await boot("close", "edge");
    const lines = [];
    const { out } = await engine.run({ src: reader(url, 1000) }, (t) => lines.push(t));
    const stopped = await cancelled(seen);
    await server.shutdown();
    if (out !== "" || Number(lines.join("")) < 1000) throw new Error(`unexpected ${JSON.stringify([out, lines])}`);
    if (!stopped || seen.pulled >= seen.size / 2) throw new Error(`the host took ${seen.pulled} of ${seen.size} bytes, cancelled ${stopped}`);
});

Deno.test("stop: a program that reads to the end takes the whole hour", async () => {
    const { server, seen, url } = await serve();
    const engine = await boot("whole", "edge");
    const lines = [];
    const { out } = await engine.run({ src: reader(url, RECORDS + 1) }, (t) => lines.push(t));
    await server.shutdown();
    if (out !== "" || lines.join("").trim() !== String(RECORDS)) throw new Error(`unexpected ${JSON.stringify([out, lines])}`);
    if (seen.pulled !== seen.size) throw new Error(`the host took ${seen.pulled} of ${seen.size} bytes`);
});
