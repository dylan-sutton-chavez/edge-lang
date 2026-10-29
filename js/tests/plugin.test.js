import { loadNativeModule } from "../src/native.ts";

const ROOT = new URL("../../", import.meta.url);
const enc = new TextEncoder();
const leb = (n) => { const out = []; do { let b = n & 0x7f; n >>>= 7; if (n !== 0) b |= 0x80; out.push(b); } while (n !== 0); return out; };
const vec = (items) => [...leb(items.length), ...items.flat()];
const name = (s) => vec([...enc.encode(s)].map((b) => [b]));
const section = (id, body) => [id, ...leb(body.length), ...body];
const body = (code) => { const b = [0x00, ...code, 0x0b]; return [...leb(b.length), ...b]; };

/* a hand-built plugin whose version export reports `abi` unless `abi` is null */
function plugin(abi) {
    const types = section(1, vec([[0x60, 0x00, 0x01, 0x7f], [0x60, 0x01, 0x7f, 0x01, 0x7f]]));
    const funcs = section(3, vec([[0], [1]]));
    const memory = section(5, vec([[0x00, 0x01]]));
    const named = [[...name("memory"), 0x02, 0x00], [...name("__edge_alloc"), 0x00, 0x01]];
    if (abi !== null) named.push([...name("__edge_abi_version"), 0x00, 0x00]);
    const exports = section(7, vec(named));
    const code = section(10, vec([body([0x41, ...leb(abi ?? 1)]), body([0x41, 0x00])]));
    return new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, ...types, ...funcs, ...memory, ...exports, ...code]);
}

Deno.test("plugin: a plugin with no version export is refused as the CLI refuses it", async () => {
    const refused = await loadNativeModule(plugin(null), {}).then(() => "", (e) => String(e));
    if (!refused.includes("missing '__edge_abi_version() -> u32'")) throw new Error(`a plugin with no version gave ${JSON.stringify(refused)}`);
    const other = await loadNativeModule(plugin(2), {}).then(() => "", (e) => String(e));
    if (!other.includes("speaks ABI v2")) throw new Error(`a v2 plugin gave ${JSON.stringify(other)}`);
    const loaded = await loadNativeModule(plugin(1), {});
    if (loaded.names.length !== 0) throw new Error(`a plugin of no functions exported ${JSON.stringify(loaded.names)}`);
});

/* a hand-built plugin whose `first` steps its argument once and returns the host's status */
function first() {
    const i32 = [0x7f];
    const types = section(1, vec([
        [0x60, ...vec(Array(7).fill(i32)), ...vec([i32])],
        [0x60, ...vec([i32]), ...vec([i32])],
        [0x60, ...vec([]), ...vec([i32])],
        [0x60, ...vec([i32, i32, i32]), ...vec([i32])],
    ]));
    const imports = section(2, vec([[...name("env"), ...name("edge_op"), 0x00, 0x00]]));
    const funcs = section(3, vec([[1], [2], [3]]));
    const memory = section(5, vec([[0x00, 0x01]]));
    // a bump allocator's next free byte since the host stages argv and out apart
    const globals = section(6, vec([[0x7f, 0x01, 0x41, ...leb(1024), 0x0b]]));
    const exports = section(7, vec([
        [...name("memory"), 0x02, 0x00],
        [...name("__edge_alloc"), 0x00, 0x01],
        [...name("__edge_abi_version"), 0x00, 0x02],
        [...name("first"), 0x00, 0x03],
    ]));
    const op = (code, recv) => [0x41, code, 0x20, recv, 0x28, 0x02, 0x00, 0x41, 0x00, 0x41, 0x00, 0x41, 0x00, 0x41, 0x00, 0x20, 0x02, 0x10, 0x00];
    const code = section(10, vec([
        body([0x23, 0x00, 0x23, 0x00, 0x20, 0x00, 0x6a, 0x24, 0x00]),
        body([0x41, 0x01]),
        // iterates argv[0] into out then steps that list and hands its status back unread
        body([...op(0x06, 0x00), 0x1a, ...op(0x07, 0x02)]),
    ]));
    return new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, ...types, ...imports, ...funcs, ...memory, ...globals, ...exports, ...code]);
}

Deno.test("plugin: an end a plugin hands back unread raises the class each runtime names for it", async () => {
    const dir = await Deno.makeTempDir();
    await Deno.writeFile(`${dir}/steps.wasm`, first());
    const runs = [
        ["edge", "from steps import first\nprint(first([7, 8]))\ntry:\n    first([])\nexcept StopIteration:\n    print('ended')\n"],
        ["uptime", "use steps\nprint(steps.first([7, 8]))\ntry\n  steps.first([])\ncatch Stop\n  print(\"ended\")\nend\n"],
    ];
    for (const [runtime, src] of runs) {
        const engine = await import(new URL(`../src/worker/engine.ts?plugin=${runtime}`, import.meta.url).href);
        await engine.load({ wasmUrl: new URL(`target/wasm32-unknown-unknown/release/${runtime}.wasm`, ROOT).href, imports: { steps: `file://${dir}/steps.wasm` }, baseUrl: `file://${dir}/` });
        const lines = [];
        const { out } = await engine.run({ src }, (t) => lines.push(t));
        if (out !== "" || lines.join("") !== "7\nended\n") throw new Error(`${runtime} printed ${JSON.stringify(lines.join(""))} and left ${JSON.stringify(out)}`);
    }
});

Deno.test("plugin: a plugin that does not load fails at its import and names the module that reached it", async () => {
    const dir = await Deno.makeTempDir();
    await Deno.writeTextFile(`${dir}/edge.json`, JSON.stringify({ imports: { badplug: "./bad.wasm", old: "./old.wasm", lib: "./lib.mini" } }));
    await Deno.writeTextFile(`${dir}/bad.wasm`, "notwasm");
    await Deno.writeFile(`${dir}/old.wasm`, plugin(2));
    await Deno.writeTextFile(`${dir}/lib.mini`, "x = 1\nimport badplug\n");
    const engine = await import(new URL("../src/worker/engine.ts?refused", import.meta.url).href);
    await engine.load({ wasmUrl: new URL("target/wasm32-unknown-unknown/release/edge.wasm", ROOT).href, baseUrl: `file://${dir}/` });
    // the WebAssembly runtime words the reason for bytes that are no wasm
    const runs = [
        ["main.mini", "print(1)\nimport badplug\n", ["error: module 'badplug' is not a valid plugin, ", "--> main.mini:2:8"]],
        ["via.mini", "import lib\n", ["module 'badplug' is not a valid plugin, ", " (via lib)\n", "--> via.mini:1:8"]],
        ["old.mini", "import old\n", ["error: module 'old' is not a valid plugin, it speaks ABI v2, this host expects v1\n", "--> old.mini:1:8"]],
    ];
    for (const [entry, src, wants] of runs) {
        const lines = [];
        const { out } = await engine.run({ src, entry }, (t) => lines.push(t));
        if (lines.length || wants.some((want) => !out.includes(want))) throw new Error(`${entry} printed ${JSON.stringify(lines.join(""))} and left ${JSON.stringify(out)}`);
    }
});
