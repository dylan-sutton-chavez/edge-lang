// deno-lint-ignore no-import-prefix
import { chromium } from "npm:playwright@latest";
import { readdirSync, readFileSync } from "node:fs";
import { Buffer } from "node:buffer";

// the page loads the staged tree from a host no network answers
const CDN_HOST = "edge.test";
const BASE = Deno.env.get("EDGE_CDN_BASE")?.replace(/\/$/, "");
if (!BASE) throw new Error("set EDGE_CDN_BASE (make serve)");

const REPO = new URL("../../", import.meta.url).pathname;
const DOCS = new URL("../../docs/", import.meta.url);

/* each js block of a page with the text block after it, what it logs */
function examples(text) {
    const fences = [...text.matchAll(/^```(\S*)[^\n]*\n([\s\S]*?)^```$/gm)].map((m) => ({ lang: m[1], body: m[2] }));
    return fences.flatMap((f, i) => f.lang === "js" ? [{ code: f.body, logs: fences[i + 1]?.lang === "text" ? fences[i + 1].body.trimEnd().split("\n") : [] }] : []);
}

/* every js example of docs/ logs in a page what the page shows */
Deno.test("docs: every js example runs in a page", async () => {
    const browser = await chromium.launch();
    const failed = [];
    let ran = 0;
    try {
        for (const name of readdirSync(DOCS).filter((n) => n.endsWith(".mdx")).sort()) {
            for (const { code, logs } of examples(readFileSync(new URL(name, DOCS), "utf8"))) {
                ran += 1;
                const page = await browser.newPage();
                const seen = [];
                page.on("console", (m) => { if (m.type() === "log") seen.push(m.text()); });
                await page.route("**/*", async (r) => {
                    const u = new URL(r.request().url());
                    if (u.host === CDN_HOST) {
                        const res = await fetch(BASE + u.pathname + u.search);
                        const headers = Object.fromEntries([...res.headers].filter(([k]) => k !== "content-encoding" && k !== "content-length"));
                        return r.fulfill({ status: res.status, headers, body: Buffer.from(await res.arrayBuffer()) });
                    }
                    if (u.host !== "localhost") return r.abort();
                    try { return r.fulfill({ contentType: "text/html", body: readFileSync(REPO + u.pathname.slice(1)) }); }
                    catch { return r.fulfill({ status: 404 }); }
                });
                await page.goto("http://localhost/js/tests/index.html");
                // the page serves dist/ where this test serves the staged tree
                const module = code.replaceAll('"./dist/', `"https://${CDN_HOST}/`);
                try {
                    await page.evaluate(async (src) => {
                        const url = URL.createObjectURL(new Blob([src], { type: "text/javascript" }));
                        await import(url);
                    }, module);
                    if (JSON.stringify(seen) !== JSON.stringify(logs)) failed.push(`${name}:\n${code}\n  logged ${JSON.stringify(seen)}\n  page shows ${JSON.stringify(logs)}`);
                } catch (e) {
                    failed.push(`${name}:\n${code}\n  threw ${e.message}`);
                }
                await page.close();
            }
        }
    } finally {
        await browser.close();
    }
    if (ran === 0) throw new Error("docs/ holds no js example");
    if (failed.length) throw new Error(failed.join("\n\n"));
});
