const ROOT = new URL("../../", import.meta.url);
// the kinds of line the program routes and how often each comes
const KINDS = ["star", "star", "push", "issue"];
const LINES = 500;

// one channel a kind and a file the page takes by name
const ROUTER = `var stars = 0;
var pushes = 0;
var line = input();
while (line != "end") {
    if (line == "star") {
        stars = stars + 1;
        send("stars", line);
    } else if (line == "push") {
        pushes = pushes + 1;
        send("file:pushes.csv", line);
    } else {
        send("other", line);
    }
    line = input();
}
send("totals", text(stars) + "," + text(pushes));
`;

/* refuses an edge.wasm older than its sources so no test reads a stale one */
async function fresh() {
    const built = (await Deno.stat(new URL("target/wasm32-unknown-unknown/release/edge.wasm", ROOT))).mtime;
    const dirs = ["src", "lib/src", "examples/lox/src"].map((dir) => new URL(`${dir}/`, ROOT));
    while (dirs.length > 0) {
        const dir = dirs.pop();
        for await (const entry of Deno.readDir(dir)) {
            const at = new URL(entry.isDirectory ? `${entry.name}/` : entry.name, dir);
            if (entry.isDirectory) dirs.push(at);
            else if ((await Deno.stat(at)).mtime > built) throw new Error(`edge.wasm is older than ${at.pathname}, run make wasm`);
        }
    }
}

// a fresh engine with no module wired and nothing bounding it
async function boot(name) {
    await fresh();
    const engine = await import(new URL(`../src/worker/engine.ts?channels=${name}`, import.meta.url).href);
    const wasmUrl = new URL("target/wasm32-unknown-unknown/release/edge.wasm", ROOT).href;
    await engine.load({ wasmUrl, limits: { ops: 2000000000, memory: 512 } });
    return engine;
}

Deno.test("channels: each channel a program sends to reaches the page as its own stream", async () => {
    const lines = Array.from({ length: LINES }, () => KINDS[Math.floor(Math.random() * KINDS.length)]);
    const engine = await boot("router");
    const channels = new Map();
    const printed = [];
    const { out } = await engine.run({ src: ROUTER, input: [...lines, "end"].join("\n") }, (t) => printed.push(t), undefined, (channel, text) => {
        if (!channels.has(channel)) channels.set(channel, []);
        channels.get(channel).push(text);
    });
    if (out !== "" || printed.length > 0) throw new Error(`unexpected ${JSON.stringify([out, printed])}`);
    const count = (kind) => lines.filter((line) => line === kind).length;
    const want = { stars: count("star"), "file:pushes.csv": count("push"), other: count("issue"), totals: 1 };
    const named = Object.keys(want).filter((name) => want[name] > 0).sort();
    if ([...channels.keys()].sort().join() !== named.join()) throw new Error(`the run wrote the channels ${JSON.stringify([...channels.keys()])}`);
    for (const name of named) {
        if (channels.get(name).length !== want[name]) throw new Error(`${name} took ${channels.get(name).length} lines and not ${want[name]}`);
    }
    if (channels.get("totals")[0] !== `${want.stars},${want["file:pushes.csv"]}`) throw new Error(`the totals read ${channels.get("totals")[0]}`);
});

Deno.test("channels: a run nobody listens to reads the refusal its runtime words", async () => {
    const engine = await boot("none");
    // nothing takes the named output so the send fails and the program says so
    const { out } = await engine.run({ src: "send(\"total\", \"1\");\n" });
    if (out !== "send needs an actor scheduler which this runtime lacks\n[line 1] in script") throw new Error(`unexpected ${JSON.stringify(out)}`);
});

Deno.test("words: a run's error reads in the words of its runtime", async () => {
    const engine = await boot("words");
    // a line of input is text and only instances have properties in Lox
    const { out } = await engine.run({ src: "var event = input();\nprint event.action;\n", input: '{"type":"IssuesEvent"}' });
    if (out !== "Only instances have properties.\n[line 2] in script") throw new Error(`unexpected ${JSON.stringify(out)}`);
});
