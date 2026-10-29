const ROOT = new URL("../../", import.meta.url);
const PINNED = "2a9282966415bf7208e2e9bea5dc59239e60dd2d2b572523033d590d989207ed";
// the slice examples/data cuts from the hour and the hash both suites pin it to
const SLICE = "e3c1dfebae5772e1523b88b639b882446b20f694f58dc17de7e39b3438e85366";
const STEPS = [["WatchEvent", 3], ["IssuesEvent", 12], ["PushEvent", 500], ["CreateEvent", 250], ["PullRequestEvent", 160], ["IssueCommentEvent", 70], ["DeleteEvent", 300]];
// the channels of the canonical flow where a page takes a file name too
const CHANNELS = ["top_stars", "busy_branches", "file:activity.csv", "issues_opened", "time_to_close"];

const sha = async (text) => [...new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)))].map((b) => b.toString(16).padStart(2, "0")).join("");

// the pinned hour kept in memory so the suite writes nothing into the repo
let fetched = null;

/* the pinned hour fetched once per run and checked against its hash */
function hour() {
    const digest = async (bytes) => [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))].map((b) => b.toString(16).padStart(2, "0")).join("");
    fetched ??= (async () => {
        const got = new Uint8Array(await (await fetch("https://data.gharchive.org/2026-10-01-15.json.gz")).arrayBuffer());
        if (await digest(got) !== PINNED) throw new Error("the hour data.gharchive.org served is not the one the tests pin");
        return got;
    })();
    return fetched;
}

/* refuses a runtime older than its sources so no test reads a stale one */
async function fresh(runtime) {
    const built = (await Deno.stat(new URL(`target/wasm32-unknown-unknown/release/${runtime}.wasm`, ROOT))).mtime;
    const dirs = ["src", "lib/src", `examples/${runtime}/src`].map((dir) => new URL(`${dir}/`, ROOT));
    while (dirs.length > 0) {
        const dir = dirs.pop();
        for await (const entry of Deno.readDir(dir)) {
            const at = new URL(entry.isDirectory ? `${entry.name}/` : entry.name, dir);
            if (entry.isDirectory) dirs.push(at);
            else if ((await Deno.stat(at)).mtime > built) throw new Error(`${runtime}.wasm is older than ${at.pathname}, run make wasm-tiny`);
        }
    }
}

/* every step-th event of each type of the hour as the slice holds them */
async function slice() {
    const gz = await hour();
    const body = new Response(new Blob([gz]).stream().pipeThrough(new DecompressionStream("gzip")));
    const seen = new Map();
    const kept = [];
    for (const line of (await body.text()).split("\n")) {
        if (line === "") continue;
        const kind = line.split('"type":"')[1]?.split('"')[0] ?? "";
        const step = STEPS.find(([name]) => name === kind)?.[1];
        if (step === undefined) continue;
        const n = seen.get(kind) ?? 0;
        if (n % step === 0) kept.push(line);
        seen.set(kind, n + 1);
    }
    const cut = `${kept.join("\n")}\n`;
    if (await sha(cut) !== SLICE) throw new Error("the slice cut from the hour is not the one examples/data pins");
    return kept;
}

/* the canonical flow reading the lines the host feeds in place of its url */
async function flow() {
    const text = await Deno.readTextFile(new URL("examples/data/github-hour.flow.yaml", ROOT));
    return `from: input\n${text.split("\n").slice(1).join("\n")}`;
}

// a fresh engine over the flow runtime with no module wired and nothing bounding it
async function boot(name) {
    await fresh("tiny");
    const engine = await import(new URL(`../src/worker/engine.ts?channels=${name}`, import.meta.url).href);
    const wasmUrl = new URL("target/wasm32-unknown-unknown/release/tiny.wasm", ROOT).href;
    await engine.load({ wasmUrl, limits: { ops: 2000000000, memory: 512 } });
    return engine;
}

Deno.test("channels: each report of the canonical flow reaches the page as its own stream", async () => {
    const records = await slice();
    const engine = await boot("flow");
    const channels = new Map();
    const lines = [];
    const { out } = await engine.run({ src: await flow(), input: records.join("\n") }, (t) => lines.push(t), undefined, (channel, text) => {
        if (!channels.has(channel)) channels.set(channel, []);
        channels.get(channel).push(text);
    });
    if (out !== "" || lines.length > 0) throw new Error(`unexpected ${JSON.stringify([out, lines])}`);
    if ([...channels.keys()].sort().join() !== [...CHANNELS].sort().join()) throw new Error(`the run wrote the channels ${JSON.stringify([...channels.keys()])}`);

    // the slice holds the rows each report must write and the test counts them itself
    const kept = records.map((line) => JSON.parse(line)).filter((event) => !String(event.actor?.login ?? "").endsWith("[bot]"));
    const stars = new Map();
    const opened = new Set();
    const minutes = new Set();
    for (const event of kept) {
        minutes.add(`${event.created_at.slice(11, 16)} ${event.type}`);
        if (event.type === "WatchEvent") stars.set(event.repo.name, (stars.get(event.repo.name) ?? 0) + 1);
        if (event.type === "IssuesEvent" && event.payload.action === "opened") opened.add(event.repo.name);
    }
    const rows = (name) => channels.get(name).length;
    // top keeps ten rows and a count has one row per key it counted
    const want = [["top_stars", 10], ["issues_opened", opened.size], ["file:activity.csv", minutes.size + 1]];
    for (const [name, n] of want) {
        if (rows(name) !== n) throw new Error(`${name} wrote ${rows(name)} lines and not ${n}`);
    }
    if (channels.get("file:activity.csv")[0] !== "minute,type,count") throw new Error(`the csv channel led with ${channels.get("file:activity.csv")[0]}`);
    const greatest = Math.max(...stars.values());
    if (JSON.parse(channels.get("top_stars")[0]).count !== greatest) throw new Error(`top_stars led with ${channels.get("top_stars")[0]} and the greatest count is ${greatest}`);
    for (const row of channels.get("busy_branches")) {
        if (JSON.parse(row).count < 2) throw new Error(`busy_branches kept the row ${row} which pushed once`);
    }
});

Deno.test("channels: a run nobody listens to reads the refusal its runtime words", async () => {
    const engine = await boot("none");
    // nothing takes the named output so the send fails and the flow says so
    const { out } = await engine.run({ src: "from: input\ntotal:\n  count:\n", input: '{"type":"WatchEvent"}' });
    if (!out.includes("the host keeps no channel total")) throw new Error(`unexpected ${JSON.stringify(out)}`);
});

Deno.test("words: a run's error reads in the words of its runtime", async () => {
    const engine = await boot("words");
    // a record lacking the field its filter reads stops where the flow names it
    const { out } = await engine.run({ src: 'from: input\nwhere: payload.action == "opened"\nopened:\n  count:\n', input: '{"type":"IssuesEvent","payload":{}}' });
    if (out !== 'where at line 2: MissingField, no field "action"') throw new Error(`unexpected ${JSON.stringify(out)}`);
});
