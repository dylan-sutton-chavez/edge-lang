import perms, { check, held } from "../src/system/perms.ts";
import net from "../../examples/net/net.ts";
import { opened } from "../src/system/gate.ts";

// the grant logic lives in the runtime so these tests reach it through its exports
const BASE = Deno.env.get("EDGE_CDN_BASE")?.replace(/\/$/, "");
if (!BASE) throw new Error("set EDGE_CDN_BASE (make serve)");
const stub = () => 0;
const { instance } = await WebAssembly.instantiate(await (await fetch(`${BASE}/edge.wasm`)).arrayBuffer(), { env: { host_print: stub, host_call_native: stub, host_fetch_bytes: stub, host_send: stub } });
const wasm = instance.exports;

const raises = (fn, name, message) => {
    try {
        fn();
    } catch (e) {
        if (e.name !== name || (message !== undefined && e.message !== message)) throw new Error(`unexpected ${e.name}: ${e.message}`);
        return;
    }
    throw new Error(`expected a ${name}`);
};

const denied = (fn, message) => raises(fn, "denied", message);

/* what a root listing `scopes` gives `pkg` of net or the bare module for none */
const grant = (pkg, scopes) => held(wasm, [[{ [pkg]: scopes.length ? scopes.map((scope) => `net:${scope}`) : ["net"] }, pkg]], pkg, "net");

// a fixture on loopback that a test closes once it is done
const serve = (answer) => Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, answer);

Deno.test("perms: a package holds what its importer grants its key, and an importer passes on only what it holds", () => {
    const root = { all: ["net:a.test"], main: ["net:b.test", "net:c.test"], http: ["net"] };
    const scopes = (chain, pkg, module = "net") => {
        const got = held(wasm, chain, pkg, module);
        return typeof got === "string" ? null : JSON.stringify(got.scopes);
    };
    if (scopes([[root, "main"]], "main") !== '["a.test","b.test","c.test"]') throw new Error("main net");
    if (scopes([[root, "http"]], "http") !== '["a.test"]') throw new Error("an entry without a scope adds none");
    if (scopes([[{ http: ["net"] }, "http"]], "http") !== "[]") throw new Error("an entry without a scope holds the module alone");
    if (scopes([[root, "http"]], "http", "db") !== null || scopes([[{}, "analytics"]], "analytics") !== null) throw new Error("an ungranted module");
    if (scopes([], "main") !== null) throw new Error("code outside any package holds nothing");
    const granted = [{ pay: ["net:api.stripe.com", "net:a.test"] }, "pay"];
    if (scopes([granted, [{ all: ["net:a.test"], http: ["net:api.stripe.com", "net:other.test"] }, "http"]], "http") !== '["a.test","api.stripe.com"]') throw new Error("passed on past what the importer holds");
    if (scopes([granted, [{ dns: ["net"] }, "dns"]], "dns") !== "[]") throw new Error("a bare entry passes on with any entry for its module");
    if (held(wasm, [[root, "http"]], "http", "db") !== "'http' imports db, which edge.json does not grant it") throw new Error("the words an ungranted import fails with");
});

Deno.test("perms: a refusal names what the package holds instead", () => {
    denied(() => grant("main", ["a.test", "b.test"]).need("c.test"), "'main' has no net:c.test, edge.json grants it net:a.test, net:b.test");
    denied(() => grant("analytics", []).need("c.test"), "'analytics' has no net:c.test, edge.json grants it nothing");
    grant("main", ["a.test"]).need("a.test");
});

Deno.test("perms: a malformed permissions section says what it needs", () => {
    const valid = { all: ["net:a.test"], main: ["net:api.example.com", "net", "net:10.0.0.255", "net:api.example.com/v2/items", "net:a.test/", "net:a.test/v1.2/@dylan"] };
    if (check(wasm, undefined, [net]) !== null || check(wasm, valid, [net]) !== null) throw new Error(`a valid section, ${check(wasm, valid, [net])}`);
    const scope = (entry) => [{ main: [`net:${entry}`] }, `permissions for 'main' give net the scope '${entry}', which it does not have`];
    const cases = [
        [{ main: "net:api.example.com" }, "permissions for 'main' must be a list of entries such as \"net:api.example.com\""],
        [{ main: ["db:/tmp"] }, "permissions for 'main' name 'db', which is not a host module (net)"],
        ...["https://api.example.com/", "a.test;script-src", "user@a.test", "API.example.com", "a.test/api/..", "a.test/caf%C3%A9", "a.test//api", "[::1]", "0177.0.0.1", "2130706433", "0x7f.1"].map(scope),
        [["net"], "permissions must map each package to a list of entries"],
        // holders read in name order whichever order the page wrote them in
        [{ zeta: ["db:/"], alpha: ["net:API"] }, "permissions for 'alpha' give net the scope 'API', which it does not have"],
    ];
    for (const [section, want] of cases) {
        if (check(wasm, section, [net]) !== want) throw new Error(`${JSON.stringify(section)} gave ${check(wasm, section, [net])}`);
    }
    if (check(wasm, { main: ["net"] }, []) !== "permissions for 'main' name 'net', which is not a host module ()") throw new Error("a host serving no module");
});

Deno.test("perms: a room reaches at most each scoped entry a section lists", () => {
    const section = { main: ["net:a.test", "net", "db:x:y"], all: ["net:b.test/api"], bad: "net:c.test", worse: [1, null, ":x"] };
    if (JSON.stringify(perms.granted(section)) !== '[["net","a.test"],["db","x:y"],["net","b.test/api"]]') throw new Error(`unexpected ${JSON.stringify(perms.granted(section))}`);
    for (const none of [undefined, null, ["net:a.test"], "net:a.test"]) if (perms.granted(none).length) throw new Error(`a section of ${JSON.stringify(none)} granted`);
    if (perms.field !== "permissions" || JSON.stringify(perms.holders) !== '["all","main","eval"]') throw new Error("perms carries the permissions field and its holders");
});

Deno.test("net: a trace shows the method and place a call reached and never its query", () => {
    const shows = [
        [["request", ["GET", "https://a.test/x?token=1#f"], 3], '{"scope":"GET a.test/x","id":3}'],
        [["connect", ["wss://a.test:8443/live?k=1"], 4], '{"scope":"a.test:8443/live","id":4}'],
        [["response", [3], [200, []]], '{"id":3,"status":200}'],
        [["read", [3], new Uint8Array(5)], '{"id":3,"bytes":5}'],
        [["read", [3], null], '{"id":3,"bytes":0}'],
        [["request", ["GET", "not a url"]], '{"scope":"GET "}'],
    ];
    for (const [[call, args, value], want] of shows) {
        if (JSON.stringify(net.show(call, args, value)) !== want) throw new Error(`${call} showed ${JSON.stringify(net.show(call, args, value))}`);
    }
    if (JSON.stringify(net.origins("a.test/api")) !== '["https://a.test:*","http://a.test:*","wss://a.test:*","ws://a.test:*"]') throw new Error("a scope reaches its whole host");
});

Deno.test("net: a grant that bounds nothing reaches every host", async () => {
    const open = opened(net, "main", { scopes: null, need() { throw new Error("an unbounded grant was asked to refuse"); } });
    for (const url of ["http://localhost:1/", "http://127.0.0.1:1/"]) {
        const id = open.calls.request("GET", url);
        raises(() => open.calls.read("x"), "invalid", "an id is an int from net.request or net.connect");
        await open.calls.response(id).catch(() => {});
    }
    open.close();
});

Deno.test("net: a package reaches only its hosts and the ids it opened", async () => {
    const server = serve(() => new Response("hi"));
    const at = `http://127.0.0.1:${server.addr.port}/`;
    const main = opened(net, "main", grant("main", ["127.0.0.1"]));
    const other = opened(net, "analytics", grant("analytics", []));
    denied(() => main.calls.request("GET", "http://evil.example/"), "'main' has no net:evil.example, edge.json grants it net:127.0.0.1");
    denied(() => other.calls.connect("ws://127.0.0.1:1/"), "'analytics' has no net:127.0.0.1, edge.json grants it nothing");
    const id = main.calls.request("GET", at);
    raises(() => other.calls.read(id), "invalid", `no open request or socket ${id}`);
    for (const bad of [null, "one", 1.5, true, 2n ** 60n]) raises(() => main.calls.read(bad), "invalid", "an id is an int from net.request or net.connect");
    const [status] = await main.calls.response(id);
    const body = new TextDecoder().decode(await main.calls.read(id));
    if (await main.calls.read(id) !== null || await main.calls.read(id) !== null) throw new Error("a read past the end");
    main.close();
    await server.shutdown();
    if (status !== 200 || body !== "hi") throw new Error(`unexpected ${status} ${body}`);
});

Deno.test("net: a url reads as every parser does, and an @ after the host is its path", async () => {
    const server = serve((req) => new Response(new URL(req.url).pathname));
    const at = `http://127.0.0.1:${server.addr.port}`;
    const main = opened(net, "main", grant("main", ["127.0.0.1"]));
    for (const url of ["http://evil.test\\@127.0.0.1/", "http://user@127.0.0.1/", `${at}@evil.test/`, `${at}/a b`, "http://12%37.0.0.1/", "127.0.0.1/", "http://[::1]/", "http://0x7f.1/", "http://2130706433/", "ftp://127.0.0.1/", "http://127.0.0.1:123456/", "http://127.0.0.1/ "]) {
        raises(() => main.calls.request("GET", url), "invalid", `'${url}' is not a plain url, net takes scheme://host/path with no user, backslash or space`);
    }
    const id = main.calls.request("GET", `${at}/@dylan`, [["accept", "text/plain"]]);
    const [status] = await main.calls.response(id);
    const path = new TextDecoder().decode(await main.calls.read(id));
    main.close();
    await server.shutdown();
    if (status !== 200 || path !== "/@dylan") throw new Error(`unexpected ${status} ${path}`);
});

Deno.test("net: a redirect is refused, the next address goes through its own request", async () => {
    const server = serve(() => new Response(null, { status: 302, headers: { location: "http://evil.example/" } }));
    const at = `http://127.0.0.1:${server.addr.port}/`;
    const main = opened(net, "main", grant("main", ["127.0.0.1"]));
    const id = main.calls.request("GET", at);
    for (const call of [main.calls.response, main.calls.read]) {
        try {
            await call(id);
            throw new Error("the redirect was followed");
        } catch (e) {
            if (e.name !== "failed" || e.message !== `net.request to ${at} was redirected, request the new address with its own net.request`) throw e;
        }
    }
    main.close();
    await server.shutdown();
});

Deno.test("net: the headers fetch keeps for the host are refused and the rest are pairs or a dict", () => {
    const main = opened(net, "main", grant("main", ["127.0.0.1"]));
    for (const name of ["Host", "cookie", "Origin", "Content-Length", "Sec-Fetch-Site", "Proxy-Authorization"]) {
        raises(() => main.calls.request("GET", "http://127.0.0.1:1/", [[name, "x"]]), "invalid", `the header '${name}' belongs to the host, a program cannot set it`);
    }
    for (const bad of ["x-probe", 1, new Uint8Array([1]), [["x-probe"]]]) {
        raises(() => main.calls.request("GET", "http://127.0.0.1:1/", bad), "invalid", "headers are [name, value] pairs or a dict");
    }
    raises(() => main.calls.request("GET", "http://127.0.0.1:1/", null, null, "zip"), "invalid", "inflate must be None or 'gzip'");
    main.close();
});

Deno.test("net: a path scope reaches only under its prefix", () => {
    const api = opened(net, "main", grant("main", ["127.0.0.1/api"]));
    const held = "edge.json grants it net:127.0.0.1/api";
    const at = "http://127.0.0.1:1";
    for (const path of ["/api", "/api/", "/api/items", "/api/v2/items?q=1", "/api/a%5cb", "/api/x%2e", "/api/group%2Fproject", "/%61pi/x"]) api.calls.request("GET", `${at}${path}`);
    denied(() => api.calls.request("GET", `${at}/`), `'main' has no net:127.0.0.1/, ${held}`);
    denied(() => api.calls.request("GET", `${at}/apixyz`), `'main' has no net:127.0.0.1/apixyz, ${held}`);
    denied(() => api.calls.request("GET", `${at}/other/api`), `'main' has no net:127.0.0.1/other/api, ${held}`);
    // a host no scope names reads as the host alone whatever path was asked
    denied(() => api.calls.request("GET", "http://evil.example/api"), `'main' has no net:evil.example, ${held}`);
    for (const path of ["/api/../secret", "/api/%2e%2e/secret", "/api/.%2e/secret", "/api/./../secret"]) {
        denied(() => api.calls.request("GET", `${at}${path}`), `'main' has no net:127.0.0.1/secret, ${held}`);
    }
    // a step a server may still take by decoding or dropping a parameter is refused
    for (const path of ["/api/..%2fsecret", "/api/%2E%2E%2Fsecret", "/api/..;/secret", "/api/%252e%252e/secret"]) {
        denied(() => api.calls.request("GET", `${at}${path}`), `'main' has no net:127.0.0.1${path}, ${held}`);
    }
    api.close();
    const slash = opened(net, "main", grant("main", ["127.0.0.1/api/"]));
    slash.calls.request("GET", `${at}/api`);
    slash.close();
});

Deno.test("net: both hosts send one reading of a path", async () => {
    const seen = [];
    const server = serve((req) => {
        seen.push(new URL(req.url).pathname + new URL(req.url).search);
        return new Response("ok");
    });
    const at = `http://127.0.0.1:${server.addr.port}`;
    const main = opened(net, "main", grant("main", ["127.0.0.1"]));
    const sent = async (url) => {
        const id = main.calls.request("GET", at + url);
        await main.calls.response(id);
        return seen.pop();
    };
    // no count of dot segments climbs above the root into the host
    const cases = [
        ["/a/b/../c", "/a/c"],
        ["/a/./b", "/a/b"],
        ["/a/b/..", "/a/"],
        ["/..", "/"],
        ["/a/%2e%2e/b", "/b"],
        ["/Español", "/Espa%C3%B1ol"],
        ["/@dylan", "/@dylan"],
        ["/items?q=a/../b", "/items?q=a/../b"],
        ["/keep#gone", "/keep"],
        ["/../@evil.test/x", "/@evil.test/x"],
        ["/%2E%2E/.evil.test/x", "/.evil.test/x"],
        ["/a{b}", "/a%7Bb%7D"],
    ];
    for (const [asked, want] of cases) {
        const got = await sent(asked);
        if (got !== want) throw new Error(`${asked} reached ${got}, want ${want}`);
    }
    main.close();
    await server.shutdown();
});

Deno.test("net: inflate hands back the bytes a gzip body holds", async () => {
    const gzip = new Uint8Array(await new Response(new Blob(["hello from gzip\n"]).stream().pipeThrough(new CompressionStream("gzip"))).arrayBuffer());
    const server = serve(() => new Response(gzip, { headers: { "content-type": "application/gzip" } }));
    const main = opened(net, "main", grant("main", ["127.0.0.1"]));
    const id = main.calls.request("GET", `http://127.0.0.1:${server.addr.port}/`, null, null, "gzip");
    const text = new TextDecoder().decode(await main.calls.read(id));
    if (await main.calls.read(id) !== null) throw new Error("more than the inflated text");
    main.close();
    await server.shutdown();
    if (text !== "hello from gzip\n") throw new Error(`unexpected ${JSON.stringify(text)}`);
});

Deno.test("net: a socket that closed reads None every time and refuses a send", async () => {
    const server = serve((req) => {
        const { socket, response } = Deno.upgradeWebSocket(req);
        socket.onmessage = (e) => (e.data === "close" ? socket.close() : socket.send(`echo ${e.data}`));
        return response;
    });
    const main = opened(net, "main", grant("main", ["127.0.0.1"]));
    const id = await main.calls.connect(`ws://127.0.0.1:${server.addr.port}/`);
    main.calls.send(id, "ping");
    const heard = await main.calls.read(id);
    main.calls.send(id, "close");
    const ends = [await main.calls.read(id), await main.calls.read(id)];
    raises(() => main.calls.send(id, "late"), "failed", "the socket is not open");
    main.close();
    await server.shutdown();
    if (heard !== "echo ping" || ends.some((end) => end !== null)) throw new Error(`unexpected ${JSON.stringify([heard, ends])}`);
});
