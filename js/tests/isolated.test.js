// deno-lint-ignore no-import-prefix
import { chromium } from "npm:playwright@latest";
import { Buffer } from "node:buffer";

// The staged CDN this run tests, served on loopback from cdn/ in CI and locally alike.
const BASE = Deno.env.get("EDGE_CDN_BASE")?.replace(/\/$/, "");
if (!BASE) throw new Error("set EDGE_CDN_BASE (make serve)");
// The host arrives as an argument, a literal import would have Deno fetch it too.
const HOST = "https://cdn.edgelang.dev/js/src/index.js";

/* A page with a session runs a program, and only what the program was granted reaches it. */
Deno.test("js: a worker holds nothing of the page and reaches only its grants", async () => {
    // The page's own origin, it answers every request with the cookies it carried.
    const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (req) =>
        new Response(req.headers.get("cookie") ?? "none", { headers: { "access-control-allow-origin": "*" } }));
    const origin = `http://127.0.0.1:${server.addr.port}`;
    const browser = await chromium.launch();
    const strays = [];
    try {
        const page = await browser.newPage();
        await page.route("**/*", async (r) => {
            const u = new URL(r.request().url());
            if (u.origin === origin) return r.continue();
            if (u.host !== "cdn.edgelang.dev") { strays.push(u.href); return r.abort(); }
            const res = await fetch(BASE + u.pathname);
            return r.fulfill({ status: res.status, headers: { "access-control-allow-origin": "*", "content-type": res.headers.get("content-type") }, body: Buffer.from(await res.arrayBuffer()) });
        });
        await page.goto(origin);
        const spawned = page.waitForEvent("worker");
        const got = await page.evaluate(async ([host, origin]) => {
            document.cookie = "session=secret";
            const { createWorker } = await import(host);
            const worker = await createWorker({ permissions: { main: ["net:127.0.0.1"] } });
            const lines = [];
            worker.onOutput((text) => lines.push(text));
            await worker.run(`import net\nr = net.request('GET', '${origin}/')\nnet.response(r)\nprint(net.read(r))`);
            return { page: await fetch("/").then((r) => r.text()), room: lines.join("").trim() };
        }, [HOST, origin]);
        if (got.page !== "session=secret" || got.room !== "b'none'") throw new Error(`the room carried the page's cookie ${JSON.stringify(got)}`);

        const frame = page.frames().find((f) => f !== page.mainFrame());
        const room = await frame.evaluate(() => ({ origin, policy: document.querySelector("meta").content }));
        if (room.origin !== "null") throw new Error(`the room shares an origin with the page, ${room.origin}`);
        if (!room.policy.includes("http://127.0.0.1:*")) throw new Error(`unexpected policy ${room.policy}`);

        // A grant that is not a plain host is refused before the run, and stays out of the policy all the same.
        const refused = await page.evaluate(async (host) => {
            const worker = await (await import(host)).createWorker({ permissions: { main: ["net:a.test;script-src"] } });
            return worker.run("import net\nprint(1)").then(() => "ran", (e) => e.message);
        }, HOST);
        if (!refused.includes("give net the scope 'a.test;script-src', which it does not have")) throw new Error(`unexpected ${refused}`);
        const policy = await page.frames().at(-1).evaluate(() => document.querySelector("meta").content);
        if (policy.includes("a.test")) throw new Error(`a malformed grant reached the policy ${policy}`);

        // Straight from the worker, past every grant, only the browser stands in the way.
        const reached = await (await spawned).evaluate(() => fetch("https://other.test/").then(() => "reached", () => "refused"));
        if (reached !== "refused" || strays.length) throw new Error(`the browser let the room through ${JSON.stringify([reached, strays])}`);

        // An engine that cannot start rejects createWorker and leaves no frame behind.
        await page.route("**/worker/bundle.js", (r) => r.fulfill({ headers: { "access-control-allow-origin": "*" }, contentType: "text/javascript", body: "throw new Error('boom')" }));
        const failed = await page.evaluate(async (host) => {
            const frames = document.querySelectorAll("iframe").length;
            const message = await (await import(host)).createWorker().then(() => "", (e) => e.message);
            return { message, left: document.querySelectorAll("iframe").length - frames };
        }, HOST);
        if (!failed.message.includes("boom") || failed.left !== 0) throw new Error(`unexpected ${JSON.stringify(failed)}`);
    } finally {
        await browser.close();
        await server.shutdown();
    }
});
