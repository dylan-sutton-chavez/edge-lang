import { bfsPrefetch } from '../prefetch.ts';
import type { Packages } from '../prefetch.ts';
import { makeEdgeEnv, resumePlugin } from '../env.ts';
import type { DeferredHostCall } from '../env.ts';
import { makeRt } from '../rt.ts';
import type { Rt, EdgeValue } from '../rt.ts';
import { nativeTable, resetNativeTable, waiting } from '../native.ts';
import type { Access, Held, HostModule } from '../system/index.ts';
import { printed, traced } from '../system/trace.ts';
import type { TraceEvent } from '../system/trace.ts';
import type { Permissions } from '../system/perms.ts';
import type { EdgeExports } from '../wasm.ts';
import { standsIn } from '../protocol.ts';
import type { Limits, LoadOpts, RunOpts, ExecResult } from '../protocol.ts';
import { described, fault, named, writeBytes } from '../util.ts';
import { denied } from '../system/error.ts';
import { gated } from '../system/gate.ts';

const TE = new TextEncoder();
const TD = new TextDecoder();

/* the packed status of `run_start` and `run_resume` matching `src/targets/web/exports.rs` */
const STATUS_KIND_SHIFT = 29;
const STATUS_DONE = 0;
const STATUS_PENDING_EVENT = 3;
const STATUS_ERROR = 4;
const STATUS_PENDING_HOST_CALL = 5;
const STATUS_EXIT = 6; // a clean exit with the exit code in the low 8 bits
const STATUS_PREEMPTED = 7; // a preempt tick that resumes with no host action

interface ExecuteOpts extends Omit<RunOpts, 'src' | 'entry'> {
    // a restore reads its source and entry once an instance says where they lie
    src: string | ((e: EdgeExports) => string)
    entry?: string | ((e: EdgeExports) => string)
    payload: Uint8Array
    start: (e: EdgeExports, ptr: number, n: number) => number
    onLine?: (text: string) => void
}

/* one line of a named output handed to whoever listens for it */
export type OnChannel = (channel: string, text: string) => void;

let wasmModule: WebAssembly.Module | null = null;
let edgeExports: EdgeExports | null = null;
let importsMap: Record<string, string> | null = null;
let permissionsMap: Permissions | null = null;
// the host modules the composition wired which every run may import
let modules: readonly HostModule[] = [];
// the grant component the composition wired or null when every package holds each module
let grants: Access | null = null;
// where the named outputs of the running program go or null while nothing listens
let channelSink: OnChannel | null = null;
// the trace the embedder asked for and the start time its events count from
let tracing = false;
let emitTrace: ((event: TraceEvent) => void) | null = null;
let runStart = 0;
const since = () => performance.now() - runStart;
// the program directory and the page reader for its files a room cannot fetch itself
let programBase: string | null = null;
let readFile: ((url: string) => Promise<Response>) | null = null;
// resolves the pending wait of a run when `pushEvent` delivers a wake-up
let eventWaiter: (() => void) | null = null;
// events pushed before the VM could take them and drained at the next `PENDING_EVENT` yield
const pendingEvents: string[] = [];
/* waiting host calls keyed by VM call id and drained together on `PENDING_HOST_CALL` */
const pendingHostCalls = new Map<number, DeferredHostCall>();
// back edges between preempt yields where 0 disables them
let preemptEvery = 0;
// the caps declared at load where null keeps every run on the sandbox profile
let limits: Limits | null = null;
let pauseRequested = false;
// set from run entry so a pause during boot waits
let running = false;
// settles `pause` once the run actually parks
let pauseAck: ((parked: boolean) => void) | null = null;
// resolved by `resume` to release a parked run
let resumeGate: (() => void) | null = null;
// both caches outlive runs so nothing refetches or probes edge.json again until `clearCache`
const fetchedSources = new Map<string, Uint8Array>();
const knownMissing = new Set<string>();
// the package dirs this instance serves host modules to and what those modules opened
const servedDirs = new Set<string>();
let opened: { close(): void }[] = [];
// the packages the last walk met which decide who writes a file channel
let walked: Packages | null = null;

// feeds the lazy rt and env getters and throws while the instance boots
const requireExports = (): EdgeExports => {
    if (!edgeExports) throw new Error('engine.load() must be called first');
    return edgeExports;
};

// what a composition without grants gives every package of each module
const UNBOUNDED: Held = { scopes: null, need() {} };

/* the host modules every later run serves whose names no import may take */
export function wire(...wired: HostModule[]): void {
    modules = wired;
}

/* the grants every later run holds packages to where null opens each wired module */
export function access(component: Access | null): void {
    grants = component;
}

/* a room asks its page for program files and fetches everything else itself */
const read = (url: string): Promise<Response> => (readFile && programBase && url.startsWith(programBase) ? readFile(url) : fetch(url));

/* one load serves many runs and each run instantiates the engine fresh */
export async function load({ wasmUrl, wasm = null, imports = null, permissions = null, trace = false, baseUrl = null, limits: caps = null }: LoadOpts, reader: ((url: string) => Promise<Response>) | null = null): Promise<{ loadMs: number }> {
    const t0 = performance.now();
    if (permissions) grantor();
    importsMap = imports;
    permissionsMap = permissions;
    tracing = Boolean(trace);
    programBase = baseUrl ? new URL('./', baseUrl).href : null;
    readFile = reader;
    limits = caps;

    // a room gets the engine bytes from its page while elsewhere the engine fetches them
    if (wasm) {
        wasmModule = await WebAssembly.compile(wasm);
    } else {
        if (!wasmUrl) throw new Error('load: wasmUrl is required');
        // no SRI and the browser decodes any content encoding before `compileStreaming` sees it
        const response = await fetch(wasmUrl);
        if (!response.ok) throw new Error(`fetch failed for '${wasmUrl}' (${response.status})`);
        const wrapped = new Response(response.body, { headers: { 'Content-Type': 'application/wasm' } });
        wasmModule = await WebAssembly.compileStreaming(wrapped);
    }

    return { loadMs: performance.now() - t0 };
}

export async function run(opts: RunOpts, onLine?: (text: string) => void, onTrace?: (event: TraceEvent) => void, onChannel?: OnChannel): Promise<ExecResult> {
    running = true;
    channelSink = onChannel ?? null;
    emitTrace = tracing && onTrace ? onTrace : null;
    const line = onLine && emitTrace ? (text: string) => { emitTrace?.(printed(since(), text)); onLine(text); } : onLine;
    try {
        const payload = TE.encode(opts.src);
        // a REPL input runs incremental so the instance and its interpreter persist
        if (opts.repl) {
            return await execute({ ...opts, onLine: line, payload, incremental: true, start: (e, ptr, n) => e.repl_eval(ptr, n) });
        }
        return await execute({ ...opts, onLine: line, payload, start: (e, ptr, n) => e.run_start(ptr, n) });
    }
    finally {
        running = false;
        channelSink = null;
        emitTrace = null;
    }
}

/* the core of run and restore that builds the instance and prefetches before driving `start` */
async function execute({ src, payload, start, entry = '', onLine, incremental = false, input }: ExecuteOpts): Promise<ExecResult> {
    if (!wasmModule) throw new Error('engine.load() must be called first');

    /* rt comes first so the env can decode handles during deferred host calls */
    const rt = makeRt(requireExports);

    /* incremental runs reuse the instance and the stable `onLine` keeps its old closures posting */
    let exports: EdgeExports;
    if (incremental && edgeExports) {
        exports = edgeExports;
    } else {
        exports = await makeInstance(wasmModule, onLine, rt);
    }

    // the engine resolves the walk and every relative import from the script it runs
    const entryBytes = TE.encode(typeof entry === 'string' ? entry : entry(exports));
    const entryPtr = writeBytes(exports, entryBytes);
    exports.set_entry(entryPtr, entryBytes.length);
    exports.wasm_free(entryPtr, Math.max(1, entryBytes.length));

    await bfsPrefetch(typeof src === 'string' ? src : src(exports), exports, {
        baseUrl: programBase,
        read,
        knownMissing,
        given: given(),
        // without grants the walk carries no field and reserves no holder
        carry: grants ? [grants.field, ...grants.holders].join('\n') : '',
        reserved: modules.map((module) => module.name),
        fetchedSources,
    }, (packages) => serveModules(exports, packages));

    // stdin from the host with one `input` call per line
    if (input && exports.set_input) {
        const inputBytes = TE.encode(input);
        const ptr = writeBytes(exports, inputBytes);
        exports.set_input(ptr, inputBytes.length);
        exports.wasm_free(ptr, Math.max(1, inputBytes.length));
    }

    // caps the next `run_start` or `repl_eval` where a zero field keeps the sandbox value
    if (limits && exports.set_limits) {
        exports.set_limits(BigInt(Math.round((limits.memory ?? 0) * 1048576)), BigInt(limits.ops ?? 0));
    }

    const t0 = performance.now();
    runStart = t0;
    // the only wall clock reading of a trace so a reader can date every event
    emitTrace?.({ kind: 'run', at: 0, epoch: Date.now() });
    pendingHostCalls.clear();
    // the engine copies the source out before the call returns
    const payloadPtr = writeBytes(exports, payload);
    const status = start(exports, payloadPtr, payload.length);
    exports.wasm_free(payloadPtr, Math.max(1, payload.length));
    let result: ExecResult;
    try {
        result = await drive(exports, rt, status, t0);
    } finally {
        // only a REPL keeps its requests and sockets open for its next input
        if (!incremental) closeModules();
    }

    return result;
}

/* the component the embedder's grants need since grants nothing reads would leave runs unbounded */
function grantor(): Access {
    if (!grants) throw new Error('permissions need a grant component and this composition wires none');
    return grants;
}

/* the root edge.json the embedder declared in place of the one at its base */
function given(): Record<string, unknown> | null {
    if (!standsIn({ imports: importsMap, permissions: permissionsMap })) return null;
    return { imports: importsMap ?? {}, ...(permissionsMap && { [grantor().field]: permissionsMap }) };
}

/* serves every package the walk met each host module its importers grant */
function serveModules(exports: EdgeExports, packages: Packages): string[] {
    const problem = grants && grants.check(exports, packages[grants.field], modules);
    if (problem) return [`edge.json at '${packages.root}edge.json': ${problem}`];
    walked = packages;
    // only an undeclared name or a plugin can reach a host module
    if (!packages.needed) return [];
    for (const [dir, pkg, chain] of packages.dirs) {
        if (servedDirs.has(dir)) continue;
        servedDirs.add(dir);
        for (const module of modules) {
            const spec = TE.encode(`system:${module.name}@${dir}`);
            const grant = grants ? grants.held(exports, chain, pkg, module.name) : UNBOUNDED;
            if (typeof grant === 'string') {
                const msg = TE.encode(grant);
                register(exports, spec, msg, (s, m) => exports.register_module_error(s, spec.length, m, msg.length));
                continue;
            }
            const served = module.open(pkg, grant);
            opened.push(served);
            const calls = Object.entries(served.calls);
            const baseId = nativeTable.length;
            for (const [name, call] of calls) {
                const reported = traced(pkg, module, name, gated(module, grant, name, call), () => emitTrace, since);
                nativeTable.push(Object.assign(() => {}, { __edge_kind: 'host' as const, __edge_name: name, __edge_module: module.name, call: reported }));
            }
            const names = TE.encode(calls.map(([name]) => name).join('\n'));
            register(exports, spec, names, (s, n) => exports.register_native_module(s, spec.length, n, names.length, baseId));
        }
    }
    return [];
}

// a dir in one spelling since `./lib/` and `lib/` name the same package
const plainDir = (dir: string) => dir.replace(/^(\.\/)+/, '');

/* why the package sending now may not write a file channel or null */
function refuseSend(channel: string): Error | null {
    const path = channel.startsWith('file:') ? channel.slice(5) : null;
    const exports = edgeExports;
    if (path === null || !walked || walked.dirs.length <= 1 || !exports?.send_package) return null;
    const len = exports.send_package();
    const dir = plainDir(TD.decode(new Uint8Array(exports.memory.buffer, exports.out_ptr(), len)));
    if (dir === plainDir(walked.root)) return null;
    const found = walked.dirs.find(([at]) => plainDir(at) === dir);
    if (!found) return denied(`code at '${dir}' belongs to no package of the run so it writes no ${channel}`);
    // a package writes below its own directory where its grants read their paths
    if (dir !== '' && path.startsWith(dir)) {
        const [, pkg, chain] = found;
        const below = path.slice(dir.length);
        const writes = modules.some((module) => {
            const held = grants ? grants.held(exports, chain, pkg, module.name) : UNBOUNDED;
            return typeof held !== 'string' && (module.writes?.(held, below) ?? false);
        });
        if (writes) return null;
    }
    return denied(`'${found[1]}' has no grant that reaches ${channel}, only the root package writes a file channel without one`);
}

/* stages two texts for a registration export and frees them once it copied them */
function register(exports: EdgeExports, spec: Uint8Array, text: Uint8Array, call: (spec: number, text: number) => void): void {
    const [s, t] = [writeBytes(exports, spec), writeBytes(exports, text)];
    call(s, t);
    exports.wasm_free(s, Math.max(1, spec.length));
    exports.wasm_free(t, Math.max(1, text.length));
}

/* aborts every request and socket the host modules left open */
function closeModules(): void {
    for (const served of opened) served.close();
    opened = [];
}

/* a fresh instance with the module registry and native table reset */
async function makeInstance(module: WebAssembly.Module, onLine: ((text: string) => void) | undefined, rt: Rt): Promise<EdgeExports> {
    const env = makeEdgeEnv({
        getExports: requireExports,
        onLine: onLine ?? (() => {}),
        // the sink is read per line so a kept instance serves each later run
        onChannel: (channel, text) => {
            // a run nobody listens to is told its named output reached nothing
            if (!channelSink) return false;
            channelSink(channel, text);
            return true;
        },
        refuseSend,
        fetchedSources,
        rt,
        captureHostCall: (id, call) => { pendingHostCalls.set(id, call); },
    });
    const { exports } = await WebAssembly.instantiate(module, { env } as unknown as WebAssembly.Imports);
    edgeExports = exports as unknown as EdgeExports;
    // the runtime names its error classes before a run so no host call reenters it
    named(edgeExports);
    edgeExports.reset_modules();
    applyPreemptInterval(edgeExports);
    resetNativeTable();
    closeModules();
    servedDirs.clear();
    walked = null;
    return edgeExports;
}

function settlePause(parked: boolean): void {
    const ack = pauseAck;
    pauseAck = null;
    if (ack) ack(parked);
}

/* services yields until the run is done or fails or exits */
async function drive(exports: EdgeExports, rt: Rt, status: number, t0: number): Promise<ExecResult> {
    while (true) {
        const kind = (status >>> STATUS_KIND_SHIFT) & 7;
        if (kind === STATUS_DONE || kind === STATUS_ERROR || kind === STATUS_EXIT) break;
        if (kind === STATUS_PREEMPTED) {
            // a macrotask so queued `postMessage` events land first
            await new Promise((r) => setTimeout(r, 0));
        } else if (kind === STATUS_PENDING_EVENT) {
            // drains early events where the first wakes the waiter and the rest queue for `receive`
            let injected = 0;
            while (pendingEvents.length > 0) {
                const msg = pendingEvents[0];
                if (msg === undefined || !injectEvent(msg)) break;
                pendingEvents.shift();
                injected++;
            }
            if (injected === 0) {
                await new Promise<void>(r => { eventWaiter = r; });
            }
        } else if (kind === STATUS_PENDING_HOST_CALL) {
            if (pendingHostCalls.size === 0) throw new Error('PENDING_HOST_CALL without captured args (engine/host drift)');
            const batch = [...pendingHostCalls];
            pendingHostCalls.clear();
            // a failed call raises only in its own coroutine and never sinks the batch
            const outcomes = await Promise.allSettled(batch.map(async ([id, call]) => {
                let rv: number;
                try {
                    const value = await call.pending;
                    const plugin = waiting.get(id);
                    rv = plugin ? resumePlugin(exports, rt, id, plugin, { value }) : exports.set_host_result_by_id(id, rt.encodeAny(value as EdgeValue));
                } catch (e) {
                    const [kind, message] = fault(e, exports);
                    const plugin = waiting.get(id);
                    rv = plugin ? resumePlugin(exports, rt, id, plugin, { kind, message }) : exports.set_host_error_by_id(id, kind, rt.encodeAny(message));
                }
                if (rv !== 0) throw new Error(`host-call ${id} delivery returned ${rv} for '${call.module}.${call.name}'`);
            }));
            const drift = outcomes.find((o) => o.status === 'rejected');
            if (drift) throw (drift as PromiseRejectedResult).reason;
        } else {
            // an unknown kind bails out instead of looping forever
            break;
        }

        /* every yield parks here so a pause asked mid-wait lands once the wait ends */
        if (pauseRequested) {
            settlePause(true);
            await new Promise<void>((r) => { resumeGate = r; });
        }

        status = exports.run_resume();
    }
    // the run is over so a pause that never parked must not hang
    pauseRequested = false;
    settlePause(false);
    // an exit carries its code in the low 8 bits and ends without a traceback
    if (((status >>> STATUS_KIND_SHIFT) & 7) === STATUS_EXIT) {
        return { out: '', ms: performance.now() - t0, exitCode: status & 0xFF };
    }
    // only an error leaves text in the out buffer
    const len = ((status >>> STATUS_KIND_SHIFT) & 7) === STATUS_ERROR ? exports.out_len() : 0;
    const ms = performance.now() - t0;
    const out = len > 0
        ? TD.decode(new Uint8Array(exports.memory.buffer, exports.out_ptr(), len))
        : '';
    return { out, ms };
}

/* the source and entry a blob embeds read where the descriptor lays them */
function snapshotHead(blob: Uint8Array, exports: EdgeExports): { source: string, entry: string } {
    const head = described(exports).snapshot;
    if (!head) throw new Error('restoring a snapshot needs a newer edge.wasm');
    const at = head.source_at;
    if (blob.length < at) throw new Error('not an edge snapshot');
    const v = new DataView(blob.buffer, blob.byteOffset, blob.byteLength);
    if (v.getUint32(0, true) !== head.magic) throw new Error('not an edge snapshot');
    // a blob another runtime saved is refused before its source is walked
    if (head.runtime_at !== undefined && head.runtime !== undefined && v.getBigUint64(head.runtime_at, true).toString(16).padStart(16, '0') !== head.runtime) {
        throw new Error('snapshot was saved under another runtime');
    }
    const len = Number(v.getBigUint64(head.source_len_at, true));
    // the entry's name follows the source in a blob of a runtime that keeps one
    const named = head.entry_len_at === undefined ? 0 : Number(v.getBigUint64(head.entry_len_at, true));
    if (at + len + named > blob.length) throw new Error('not an edge snapshot');
    return { source: TD.decode(blob.subarray(at, at + len)), entry: TD.decode(blob.subarray(at + len, at + len + named)) };
}

/* an older wasm lacks the export and only preemption needs it */
function applyPreemptInterval(exports: EdgeExports): void {
    if (!exports.set_preempt_interval) {
        if (preemptEvery > 0) throw new Error('preemption needs a newer edge.wasm');
        return;
    }
    exports.set_preempt_interval(preemptEvery);
}

/* preempts every `n` back edges where 0 disables it */
export function setPreemptInterval(n: number): void {
    preemptEvery = Math.max(0, n | 0);
    if (edgeExports) applyPreemptInterval(edgeExports);
}

/* resolves true once the run parks */
export function pause(): Promise<boolean> {
    if (!running) return Promise.resolve(false);
    pauseRequested = true;
    return new Promise((r) => { pauseAck = r; });
}

/* releases a run held by a pause and does nothing otherwise */
export function resume(): void {
    pauseRequested = false;
    const gate = resumeGate;
    resumeGate = null;
    if (gate) gate();
}

/* serializes the parked run and throws when none is parked */
export function saveState(): Uint8Array {
    if (!edgeExports) throw new Error('nothing to save: no run has started');
    const len = Number(edgeExports.save_state());
    if (len === -2) throw new Error(new TextDecoder().decode(new Uint8Array(edgeExports.memory.buffer, edgeExports.out_ptr(), edgeExports.out_len())));
    if (len < 0) throw new Error('nothing to save: the program is not paused');
    return new Uint8Array(edgeExports.memory.buffer, edgeExports.out_ptr(), len).slice();
}

/* boots from the source the blob embeds and continues from its saved state */
export async function restoreState({ blob, onLine, onChannel }: { blob: Uint8Array | ArrayBuffer, onLine?: (text: string) => void, onChannel?: OnChannel }): Promise<ExecResult> {
    running = true;
    channelSink = onChannel ?? null;
    try {
        const payload = blob instanceof Uint8Array ? blob : new Uint8Array(blob);
        // the embedded source drives prefetch from the entry it names so restored imports resolve
        return await execute({ src: (e) => snapshotHead(payload, e).source, entry: (e) => snapshotHead(payload, e).entry, payload, onLine, start: (e, ptr, n) => e.restore_state(ptr, n) });
    }
    finally { running = false; channelSink = null; }
}

/* the module bindings of the parked program as JSON */
export function stateGlobals(): Record<string, unknown> {
    if (!edgeExports) return {};
    const len = edgeExports.state_globals();
    return JSON.parse(TD.decode(new Uint8Array(edgeExports.memory.buffer, edgeExports.out_ptr(), len)));
}

/* the coroutines of the parked program as JSON */
export function stateStack(): unknown[] {
    if (!edgeExports) return [];
    const len = edgeExports.state_stack();
    return JSON.parse(TD.decode(new Uint8Array(edgeExports.memory.buffer, edgeExports.out_ptr(), len)));
}

/* injects into the paused VM or returns false so the caller buffers */
function injectEvent(message: string): boolean {
    if (!edgeExports) return false;
    const bytes = TE.encode(message);
    const ptr = writeBytes(edgeExports, bytes);
    const status = edgeExports.run_push_event(ptr, bytes.length);
    edgeExports.wasm_free(ptr, Math.max(1, bytes.length));
    return status === 0;
}

/* queues an event for `receive` and buffers it until the VM waits on `PENDING_EVENT` */
export function pushEvent(message: unknown): boolean {
    const msg = String(message);
    if (!injectEvent(msg)) {
        pendingEvents.push(msg);
        return true;
    }
    if (eventWaiter) {
        const w = eventWaiter;
        eventWaiter = null;
        w();
    }
    return true;
}

export function reset(): void {
    if (edgeExports) edgeExports.reset_modules();
    resetNativeTable();
    pendingHostCalls.clear();
    closeModules();
    servedDirs.clear();
}

/* forgets every fetched module and missing manifest so the next run fetches afresh */
export function clearCache(): void {
    fetchedSources.clear();
    knownMissing.clear();
}

export function dispose(): void {
    wasmModule = null;
    edgeExports = null;
    importsMap = null;
    permissionsMap = null;
    tracing = false;
    programBase = null;
    readFile = null;
    fetchedSources.clear();
    knownMissing.clear();
    resetNativeTable();
    pendingHostCalls.clear();
    closeModules();
    servedDirs.clear();
}
