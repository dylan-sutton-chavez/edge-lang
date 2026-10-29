import type { EdgeValue } from '../../js/src/rt.ts';
import { failed, invalid } from '../../js/src/system/error.ts';
import type { Held, HostModule, Shown } from '../../js/src/system/index.ts';

type Message = Uint8Array | string | null;

/* a request in flight with the seconds a read waits and the stall it met */
type InFlight = { kind: 'request', abort: () => void, head: Promise<[number, [string, string][]]>, body: Promise<ReadableStreamDefaultReader<Uint8Array> | null>, wait: number, stalled: Error | null };

/* what an id answers to as a request in flight or an open socket */
type Stream =
    | InFlight
    | { kind: 'socket', abort: () => void, socket: WebSocket, messages: Message[], waiting: ((message: Message) => void)[], ended: boolean, bytes: number, flooded: boolean, target: string };

// one counter for every package so an id never names two streams
let next = 1;

// requests and sockets one package may hold open at once
const OPEN = 128;

// the seconds a request waits for its answer to begin by default and at most
const TIMEOUT = 30;
const TIMEOUT_MAX = 86400;

// messages and their bytes a socket keeps unread before it closes rather than hold more
const QUEUED = 1024;
const QUEUED_BYTES = 16 << 20;

/* the bytes a socket message holds as both hosts count them */
const size = (message: Message) => (typeof message === 'string' ? new TextEncoder().encode(message).length : message?.byteLength ?? 0);

const text = (value: EdgeValue | undefined, what: string): string => {
    if (typeof value !== 'string') throw invalid(`${what} must be a str`);
    return value;
};

const content = (value: EdgeValue | undefined): Uint8Array<ArrayBuffer> | string | undefined => {
    if (value === null || value === undefined) return undefined;
    if (value instanceof Uint8Array) return value as Uint8Array<ArrayBuffer>;
    if (typeof value === 'string') return value;
    throw invalid('a body must be bytes, a str or None');
};

// headers fetch keeps for the host are refused not dropped so both hosts answer alike
const FORBIDDEN = new Set([
    'accept-charset', 'accept-encoding', 'access-control-request-headers', 'access-control-request-method', 'connection', 'content-length',
    'cookie', 'cookie2', 'date', 'dnt', 'expect', 'host', 'keep-alive', 'origin', 'referer', 'set-cookie', 'te', 'trailer',
    'transfer-encoding', 'upgrade', 'via',
]);

/* the headers a program sends as pairs each checked in order */
const pairs = (value: EdgeValue | undefined): [string, string][] => {
    if (value === null || value === undefined) return [];
    const shape = () => invalid('headers are [name, value] pairs or a dict');
    if (typeof value !== 'object' || value instanceof Uint8Array || value instanceof ArrayBuffer) throw shape();
    const entries = Array.isArray(value) ? value : Object.entries(value);
    return entries.map((pair) => {
        if (!Array.isArray(pair) || pair.length !== 2) throw shape();
        const name = text(pair[0] as EdgeValue, 'a header name');
        if (FORBIDDEN.has(name.toLowerCase()) || /^(?:proxy|sec)-/i.test(name)) throw invalid(`the header '${name}' belongs to the host, a program cannot set it`);
        return [name, text(pair[1] as EdgeValue, 'a header value')];
    });
};

// a dotted lowercase host as a scope names it and a url reaches it
const HOST = /[a-z0-9.-]+/;

// an ip4 in the one spelling every parser reads alike with no leading zeros
const OCTET = '(?:25[0-5]|2[0-4]\\d|1\\d\\d|[1-9]?\\d)';
const IP4 = new RegExp(`^${OCTET}(?:\\.${OCTET}){3}$`);

/* whether a host reads alike everywhere since browsers take a numeric ending as an ip4 */
function plainHost(host: string): boolean {
    const last = host.split('.').filter(Boolean).pop() ?? '';
    return !/^(?:\d+|0x[0-9a-f]*)$/i.test(last) || IP4.test(host);
}

// a whole scope as one lowercase host and the path prefix bounding its reach
const SCOPE = new RegExp(`^(${HOST.source})((?:/[A-Za-z0-9\\-._~!$&'()*+,=:@]+)*/?)$`);

/* whether a grant may give net a scope with no dot segment in its prefix */
function scope(asked: string): boolean {
    const [, host, prefix] = SCOPE.exec(asked) ?? [];
    return host !== undefined && plainHost(host) && prefix!.split('/').every((segment) => segment !== '.' && segment !== '..');
}

// a url every parser reads alike with the host right after the scheme
const PLAIN = new RegExp(`^((?:https?|wss?)://)(${HOST.source})((?::\\d{1,5})?)([/?#].*)?$`, 'i');

// what a path may carry as written while the rest crosses as escaped UTF-8 bytes
const KEPT = /^[a-z0-9\-._~!$&'()*+,;=:@/%]$/i;

// spaces and control characters and backslashes can move hosts and lone surrogates have no bytes
const sendable = (c: string) => c > ' ' && c !== '\\' && c !== '\x7f' && (c.length > 1 || c < '\ud800' || c > '\udfff');

// a segment for this directory or the one above it spelled plainly or escaped
const HERE = /^(?:\.|%2e)$/i;
const ABOVE = /^(?:\.|%2e){2}$/i;

/* applies dot segments however they are spelled and never climbs above the root */
function resolve(path: string): string {
    const out: string[] = [];
    const segments = path.replace(/^\//, '').split('/');
    for (const [i, segment] of segments.entries()) {
        const above = ABOVE.test(segment);
        if (above) out.pop();
        if (!above && !HERE.test(segment)) out.push(segment);
        else if (i === segments.length - 1) out.push('');
    }
    return `/${out.join('/')}`;
}

/* a url as its lowercase host and the escaped resolved path both hosts send */
function plain(url: string): { host: string, path: string, target: string } {
    const [, scheme, name, port, rest = '/'] = ([...url].every(sendable) ? PLAIN.exec(url) : null) ?? [];
    const host = name?.toLowerCase();
    if (host === undefined || !plainHost(host)) throw invalid(`'${url}' is not a plain url, net takes scheme://host/path with no user, backslash or space`);
    // a fragment never leaves the host so a grant never reaches it
    const [addressed] = rest.split('#') as [string];
    const at = addressed.indexOf('?');
    const path = [...resolve(at === -1 ? addressed : addressed.slice(0, at))].map((c) => (KEPT.test(c) ? c : encodeURIComponent(c))).join('');
    return { host, path, target: `${scheme!.toLowerCase()}${host}${port}${path}${at === -1 ? '' : addressed.slice(at)}` };
}

// an escape a server may undo before routing where every server decodes the unreserved ones
const ESCAPE = /%([0-9a-f]{2})/gi;
const UNRESERVED = /[A-Za-z0-9\-._~]/;
const byte = (hex: string) => String.fromCharCode(parseInt(hex, 16));
const decode = (text: string, only = /[^]/) => text.replace(ESCAPE, (escape, hex: string) => (only.test(byte(hex)) ? byte(hex) : escape));

/* every path a server might route `path` to decoded up to twice */
const readings = (path: string): string[] =>
    [decode(path, UNRESERVED), decode(path), decode(decode(path))].map((read) => resolve(read.replace(/\\/g, '/').replace(/;[^/]*/g, '')));

/* a request or a socket reaches the host and the resolved path of its url */
function reaches(call: string, args: EdgeValue[]): string | null {
    if (call !== 'request' && call !== 'connect') return null;
    const { host, path } = plain(text(args[call === 'request' ? 1 : 0] ?? null, 'a url'));
    return `${host}${path}`;
}

/* a scope reaches its whole host or the paths under its prefix in every reading */
function covers(scope: string, reached: string): boolean {
    const at = reached.indexOf('/');
    const [host, path] = at === -1 ? [reached, ''] : [reached.slice(0, at), reached.slice(at)];
    if (scope.split('/')[0] !== host) return false;
    const prefix = scope.slice(host.length).replace(/\/$/, '');
    // a prefix holds only when every reading a server might take stays under it
    return prefix === '' || readings(path).every((read) => read === prefix || read.startsWith(`${prefix}/`));
}

/* the path shows only once the host is held so a refused host reads alone */
function asked(held: string[], reached: string): string {
    const host = reached.split('/')[0]!;
    return held.some((scope) => scope.split('/')[0] === host) ? reached : host;
}

/* the seconds a request waits for its answer to begin where none gives the default */
function seconds(value: EdgeValue): number {
    if (value === null) return TIMEOUT;
    const n = typeof value === 'number' ? value : typeof value === 'bigint' ? Number(value) : NaN;
    if (!(n > 0 && n <= TIMEOUT_MAX)) throw invalid('timeout must be a number of seconds above 0 and up to 86400');
    return n;
}

const failure = (what: string, e: unknown) => failed(`${what} failed, ${e instanceof Error ? e.message : String(e)}`);

// statuses that hand a request elsewhere which only a new request may follow
const REDIRECTS = [301, 302, 303, 307, 308];

/* the next chunk where a read waiting past the timeout fails every later one too */
async function chunk(request: InFlight): Promise<Message> {
    await request.head;
    const reader = await request.body;
    if (!reader) return null;
    if (request.stalled) throw request.stalled;
    // the timeout counts from the moment the program asked and only while it waits
    const timer = setTimeout(() => {
        request.stalled = failed(`reading the response failed, nothing arrived within ${request.wait} s`);
        reader.cancel().catch(() => {});
        request.abort();
    }, request.wait * 1000);
    try {
        const { done, value } = await reader.read();
        if (request.stalled) throw request.stalled;
        return done ? null : value;
    } catch (e) {
        throw request.stalled ?? failure('reading the response', e);
    } finally {
        clearTimeout(timer);
    }
}

/* the calls and ids of a package whose urls the host checks against its grant */
function open(_pkg: string, _held: Held) {
    const streams = new Map<number, Stream>();

    // a whole number a page holds exactly names an id
    const stream = (id: EdgeValue | undefined): Stream => {
        if (typeof id !== 'number' || !Number.isSafeInteger(id)) throw invalid('an id is an int from net.request or net.connect');
        const found = streams.get(id);
        if (!found) throw invalid(`no open request or socket ${id}`);
        return found;
    };

    /* refuses another request or socket while the package holds `OPEN` of them */
    const room = () => {
        if (streams.size >= OPEN) throw failed(`net holds ${OPEN} open requests and sockets for this package, close one first`);
    };

    /* returns the request id at once while head and body arrive through `response` and `read` */
    function request(method: EdgeValue = null, url: EdgeValue = null, headers: EdgeValue = null, body: EdgeValue = null, inflate: EdgeValue = null, timeout: EdgeValue = null): number {
        const place = plain(text(url, 'a url'));
        const init = { method: text(method ?? 'GET', 'a method'), headers: pairs(headers), body: content(body) };
        if (inflate !== null && inflate !== 'gzip') throw invalid("inflate must be None or 'gzip'");
        const wait = seconds(timeout);
        room();
        const controller = new AbortController();
        // an answer that has not begun within the timeout aborts the request
        let late = false;
        const timer = setTimeout(() => { late = true; controller.abort(); }, wait * 1000);
        const answer = fetch(place.target, { ...init, signal: controller.signal, redirect: 'manual' }).finally(() => clearTimeout(timer));
        const head = answer.then((res): [number, [string, string][]] => {
            // redirects stay unfollowed since a browser hides the target that a new request checks
            if (res.type === 'opaqueredirect' || REDIRECTS.includes(res.status)) {
                throw failed(`net.request to ${place.target} was redirected, request the new address with its own net.request`);
            }
            const received: [string, string][] = [];
            res.headers.forEach((value, name) => received.push([name, value]));
            return [res.status, received];
        }, (e) => { throw late ? failed(`net.request to ${place.target} failed, no answer within ${wait} s`) : failure(`net.request to ${place.target}`, e); });
        // a request nobody reads still settles so its failure never surfaces as unhandled
        head.catch(() => {});
        const reader = (res: Response) => (inflate ? res.body?.pipeThrough(new DecompressionStream('gzip')) : res.body)?.getReader() ?? null;
        const id = next++;
        streams.set(id, { kind: 'request', abort: () => controller.abort(), head, body: answer.then(reader, () => null), wait, stalled: null });
        return id;
    }

    /* the status and headers of a request once they arrive */
    function response(id: EdgeValue = null): Promise<[number, [string, string][]]> {
        const found = stream(id);
        if (found.kind !== 'request') throw invalid('response takes a request from net.request');
        return found.head;
    }

    /* the next body chunk or socket message or None once it ended */
    function read(id: EdgeValue = null): Message | Promise<Message> {
        const found = stream(id);
        if (found.kind === 'request') return chunk(found);
        if (found.messages.length > 0) {
            const message = found.messages.shift()!;
            found.bytes -= size(message);
            return message;
        }
        if (found.flooded) throw failed(`the socket to ${found.target} closed once the messages nobody read filled its queue`);
        if (found.ended) return null;
        return new Promise((resolve) => found.waiting.push(resolve));
    }

    /* aborts a request or closes a socket */
    function close(id: EdgeValue = null): null {
        stream(id).abort();
        streams.delete(id as number);
        return null;
    }

    /* resolves the socket id once open while its messages arrive through `read` */
    function connect(url: EdgeValue = null): Promise<number> {
        const place = plain(text(url, 'a url'));
        room();
        // an http url opens its ws twin as every page socket now does
        const socket = new WebSocket(place.target.replace(/^http/, 'ws'));
        socket.binaryType = 'arraybuffer';
        const found: Stream = { kind: 'socket', abort: () => socket.close(), socket, messages: [], waiting: [], ended: false, bytes: 0, flooded: false, target: place.target };
        socket.onmessage = (e) => {
            if (found.flooded) return;
            const message = typeof e.data === 'string' ? e.data : new Uint8Array(e.data as ArrayBuffer);
            const waiter = found.waiting.shift();
            if (waiter) waiter(message);
            // a program that reads nothing while messages pile up gets the socket closed
            else if (found.messages.length >= QUEUED || found.bytes + size(message) > QUEUED_BYTES) {
                found.flooded = true;
                socket.close();
            } else {
                found.bytes += size(message);
                found.messages.push(message);
            }
        };
        // every later read answers None so a closed socket never hangs a reader
        socket.onclose = () => {
            found.ended = true;
            for (const waiter of found.waiting.splice(0)) waiter(null);
        };
        const id = next++;
        streams.set(id, found);
        return new Promise((resolve, reject) => {
            let opened = false;
            socket.onopen = () => {
                opened = true;
                resolve(id);
            };
            // a socket that never opened names no stream
            socket.onerror = () => {
                if (opened) return;
                streams.delete(id);
                reject(failed(`the socket to ${place.target} failed`));
            };
        });
    }

    function send(id: EdgeValue = null, data: EdgeValue = null): null {
        const found = stream(id);
        if (found.kind !== 'socket') throw invalid('send takes a socket from net.connect');
        if (found.socket.readyState !== WebSocket.OPEN) throw failed('the socket is not open');
        found.socket.send(content(data) ?? new Uint8Array(0));
        return null;
    }

    /* aborts what a finished run left open */
    function closeAll(): void {
        for (const found of streams.values()) found.abort();
        streams.clear();
    }

    return { calls: { request, response, read, close, connect, send }, close: closeAll };
}

const idOf = (value: unknown) => (typeof value === 'number' ? value : typeof value === 'bigint' ? Number(value) : undefined);

/* host and path only since the query or fragment may carry a key */
function place(url: unknown): string {
    try {
        const parsed = new URL(String(url));
        return `${parsed.host}${parsed.pathname}`;
    } catch {
        return '';
    }
}

/* what a call reached for a trace and never its body headers or query */
function show(call: string, args: EdgeValue[], value: unknown): Shown {
    if (call === 'request') return { scope: `${String(args[0])} ${place(args[1])}`, id: idOf(value) };
    if (call === 'connect') return { scope: place(args[0]), id: idOf(value) };
    const id = idOf(args[0]);
    if (call === 'response' && Array.isArray(value)) return { id, status: Number(value[0]) };
    if (call === 'read') return { id, bytes: value == null ? 0 : (value as { length: number }).length };
    return { id };
}

/* a room reaches the whole host of a scope since a policy names no path */
const origins = (asked: string): string[] => ['https', 'http', 'wss', 'ws'].map((scheme) => `${scheme}://${asked.split('/')[0]}:*`);

/* requests and sockets to the hosts a package holds as `net:<host>` or `net:<host>/<prefix>` */
export default { name: 'net', scope, reaches, covers, asked, open, origins, show } satisfies HostModule;
