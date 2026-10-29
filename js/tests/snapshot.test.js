const ROOT = new URL("../../", import.meta.url);

// a fresh engine over the edge.wasm make wasm builds with nothing wired
async function boot(name) {
    const engine = await import(new URL(`../src/worker/engine.ts?snapshot=${name}`, import.meta.url).href);
    await engine.load({ wasmUrl: new URL("target/wasm32-unknown-unknown/release/edge.wasm", ROOT).href });
    return engine;
}

/* resolves once the run waits on receive() */
async function parked(engine) {
    for (let i = 0; i < 200; i++) {
        if (JSON.stringify(engine.stateStack()).includes("waiting_event")) return;
        await new Promise((r) => setTimeout(r, 10));
    }
    throw new Error("the run never parked on receive()");
}

Deno.test("snapshot: a run parked on receive() restores from its blob and takes other events", async () => {
    const engine = await boot("restore");
    const lines = [];
    const src = "history = []\nwhile true:\n    m = receive()\n    if m == 'stop':\n        break\n    history.append(m)\nprint('|'.join(history))";
    const running = engine.run({ src }, (t) => lines.push(t));
    await parked(engine);
    engine.pushEvent("a");
    await parked(engine);
    const blob = engine.saveState();
    engine.pushEvent("b");
    engine.pushEvent("stop");
    await running;
    const first = lines.join("");
    lines.length = 0;
    const resumed = engine.restoreState({ blob, onLine: (t) => lines.push(t) });
    await parked(engine);
    engine.pushEvent("c");
    engine.pushEvent("stop");
    const { out } = await resumed;
    if (first !== "a|b\n") throw new Error(`the first run printed ${JSON.stringify(first)}`);
    if (out !== "" || lines.join("") !== "a|c\n") throw new Error(`the restored run printed ${JSON.stringify(lines.join(""))} and left ${JSON.stringify(out)}`);
});

Deno.test("snapshot: a blob whose source runs past its end is refused before anything boots", async () => {
    const engine = await boot("refused");
    const running = engine.run({ src: "receive()" }, () => {});
    await parked(engine);
    const blob = engine.saveState();
    engine.pushEvent("done");
    await running;
    // a source length past the blob reads as no snapshot at all
    const bent = blob.slice();
    new DataView(bent.buffer).setBigUint64(24, BigInt(blob.length), true);
    const refused = await engine.restoreState({ blob: bent }).then(() => "", (e) => String(e));
    if (!refused.includes("not an edge snapshot")) throw new Error(`a bent blob gave ${JSON.stringify(refused)}`);
});

/* the blob another runtime saves with its own identity and source sealed again */
async function foreign(blob, source) {
    const view = new DataView(blob.buffer, blob.byteOffset, blob.byteLength);
    const text = new TextEncoder().encode(source);
    // the entry's length lies past the source's and the entry right past the source
    const rest = blob.subarray(40 + Number(view.getBigUint64(24, true)), blob.length - 32);
    const out = new Uint8Array(40 + text.length + rest.length + 32);
    const head = new DataView(out.buffer);
    out.set(blob.subarray(0, 40));
    head.setBigUint64(8, view.getBigUint64(8, true) ^ 1n, true);
    head.setBigUint64(24, BigInt(text.length), true);
    out.set(text, 40);
    out.set(rest, 40 + text.length);
    out.set(new Uint8Array(await crypto.subtle.digest("SHA-256", out.subarray(0, out.length - 32))), out.length - 32);
    return out;
}

Deno.test("snapshot: a blob of another runtime is refused before its source parses and runs nothing", async () => {
    const engine = await boot("foreign");
    const running = engine.run({ src: "print('phase1')\nreceive()\nprint('resumed')" }, () => {});
    await parked(engine);
    const blob = engine.saveState();
    engine.pushEvent("done");
    await running;
    // a flow of the tiny runtime which mini could never parse
    const lines = [];
    const refused = await engine.restoreState({ blob: await foreign(blob, "from: input\ntotal:\n  count:\n"), onLine: (t) => lines.push(t) }).then(({ out }) => out, (e) => String(e));
    if (!refused.includes("snapshot was saved under another runtime")) throw new Error(`a foreign blob gave ${JSON.stringify(refused)}`);
    if (lines.length > 0) throw new Error(`a foreign blob printed ${JSON.stringify(lines)}`);
});

Deno.test("snapshot: a restored run names its script and imports from beside it", async () => {
    const dir = await Deno.makeTempDir();
    await Deno.mkdir(`${dir}/sub`);
    await Deno.writeTextFile(`${dir}/sub/helper.mini`, "fn twice(x):\n    return x + x\n");
    const src = "from .helper import twice\nmsg = receive()\nprint(twice(msg))\nraise ValueError(msg)\n";
    const engine = await import(new URL("../src/worker/engine.ts?snapshot=entry", import.meta.url).href);
    await engine.load({ wasmUrl: new URL("target/wasm32-unknown-unknown/release/edge.wasm", ROOT).href, baseUrl: `file://${dir}/` });
    const running = engine.run({ src, entry: "sub/main.mini" }, () => {});
    await parked(engine);
    const blob = engine.saveState();
    engine.pushEvent("x");
    await running;
    // the blob names its script so the restore fetches the helper beside it again
    const lines = [];
    const resumed = engine.restoreState({ blob, onLine: (t) => lines.push(t) });
    await parked(engine);
    engine.pushEvent("ab");
    const { out } = await resumed;
    await Deno.remove(dir, { recursive: true });
    if (lines.join("") !== "abab\n") throw new Error(`the restored run printed ${JSON.stringify(lines.join(""))}`);
    if (!out.includes("ValueError: ab") || !out.includes("sub/main.mini:4")) throw new Error(`the restored run failed with ${JSON.stringify(out)}`);
});
