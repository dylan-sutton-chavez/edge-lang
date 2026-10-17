import { bfsPrefetch } from '../prefetch.ts';
import type { Packages } from '../prefetch.ts';
import { makeCompilerEnv, resumePlugin } from '../env.ts';
import type { DeferredHostCall } from '../env.ts';
import { makeRt } from '../rt.ts';
import type { Rt, EdgeValue } from '../rt.ts';
import { nativeTable, resetNativeTable, waiting } from '../native.ts';
import { SYSTEM } from '../system/index.ts';
import type { Host } from '../system/index.ts';
import { masker, printed, traced } from '../system/trace.ts';
import type { TraceEvent } from '../system/trace.ts';
import { check, held, scopes } from '../system/grants.ts';
import type { Permissions } from '../system/grants.ts';
import type { CompilerExports } from '../wasm.ts';
import type { Limits, LoadOpts, RunOpts, ExecResult } from '../protocol.ts';
import { fault, writeBytes } from '../util.ts';

const TE = new TextEncoder();
const TD = new TextDecoder();

/* Packed status from `run_start` / `run_resume`, mirrors `src/wasm/exports.rs`. */
const STATUS_KIND_SHIFT = 29;
const STATUS_DONE = 0;
const STATUS_PENDING_TIMER = 1;
const STATUS_PENDING_EVENT = 3;
const STATUS_ERROR = 4;
const STATUS_PENDING_HOST_CALL = 5;
const STATUS_EXIT = 6; // uncaught SystemExit, clean termination, low 8 bits = exit code
const STATUS_PREEMPTED = 7; // preempt tick, resumes with no host action

interface ExecuteOpts extends RunOpts {
    payload: Uint8Array
    start: (e: CompilerExports, ptr: number, n: number) => number
    onLine?: (text: string) => void
}

// Worker-lifetime state
let wasmModule: WebAssembly.Module | null = null;
let compilerExports: CompilerExports | null = null;
let importsMap: Record<string, string> | null = null;
let permissionsMap: Permissions | null = null;
let secretsMap: Record<string, string> | null = null;
// Whether the embedder asked for a trace, where the current run sends it, and when that run began.
let tracing = false;
let emitTrace: ((event: TraceEvent) => void) | null = null;
let runStart = 0;
const since = () => performance.now() - runStart;
// The program's directory and the page's reader of its files, which a room cannot fetch itself.
let programBase: string | null = null;
let readFile: ((url: string) => Promise<Response>) | null = null;
// Resolves run()'s current `await` when a `PendingEvent` wake-up arrives via `pushEvent`.
let eventWaiter: (() => void) | null = null;
// Events `pushEvent`'d before the VM was ready (no `compilerExports`, or no paused run yet). Drained at the next `PENDING_EVENT` yield.
const pendingEvents: string[] = [];
/* System calls still waiting, captured by env.host_call_native, keyed by the VM-assigned call_id. Drained concurrently in the PENDING_HOST_CALL branch. */
const pendingHostCalls = new Map<number, DeferredHostCall>();
// Back-edges between preempt yields, 0 disables.
let preemptEvery = 0;
// Caps the embedder declared at load, null leaves every run on the sandbox profile.
let limits: Limits | null = null;
let pauseRequested = false;
// True from run entry so mid-boot pause waits.
let running = false;
// Resolves pause() once actually parked.
let pauseAck: ((parked: boolean) => void) | null = null;
// Resolves at resume() to release a preempt hold.
let resumeGate: (() => void) | null = null;
// Source and missing caches outlive runs, no refetch or edge.json re-probe until `clearCache()`.
const fetchedSources = new Map<string, Uint8Array>();
const knownMissing = new Set<string>();
// The package dirs this instance already serves system modules to, and what they opened.
const servedDirs = new Set<string>();
let opened: { close(): void }[] = [];

// compilerExports for the lazy rt/env getters, throws while the instance boots.
const requireExports = (): CompilerExports => {
    if (!compilerExports) throw new Error('engine.load() must be called first');
    return compilerExports;
};

/* A room asks its page for the program's own files and fetches everything else itself. */
const read = (url: string): Promise<Response> => (readFile && programBase && url.startsWith(programBase) ? readFile(url) : fetch(url));

/* Engine orchestrator, internal to the Worker. Consumers use `createWorker` in `src/index.ts`. Lifecycle is `load` once -> many `run` cycles -> `dispose`, and each run instantiates the compiler fresh with no state leak. */
export async function load({ wasmUrl, wasm = null, imports = null, permissions = null, secrets = null, trace = false, baseUrl = null, limits: caps = null }: LoadOpts, reader: ((url: string) => Promise<Response>) | null = null): Promise<{ loadMs: number }> {
    const t0 = performance.now();
    importsMap = imports;
    permissionsMap = permissions;
    secretsMap = secrets;
    tracing = Boolean(trace);
    programBase = baseUrl ? new URL('./', baseUrl).href : null;
    readFile = reader;
    limits = caps;

    // A room receives the compiler's bytes from its page, anywhere else the engine fetches them.
    if (wasm) {
        wasmModule = await WebAssembly.compile(wasm);
    } else {
        if (!wasmUrl) throw new Error('load: wasmUrl is required');
        // Plain fetch, no SRI. The browser decodes any Content-Encoding (br/gzip) before compileStreaming.
        const response = await fetch(wasmUrl);
        if (!response.ok) throw new Error(`fetch failed for '${wasmUrl}' (${response.status})`);
        const wrapped = new Response(response.body, { headers: { 'Content-Type': 'application/wasm' } });
        wasmModule = await WebAssembly.compileStreaming(wrapped);
    }

    return { loadMs: performance.now() - t0 };
}

export async function run(opts: RunOpts, onLine?: (text: string) => void, onTrace?: (event: TraceEvent) => void): Promise<ExecResult> {
    running = true;
    const mask = masker(secretsMap);
    emitTrace = tracing && onTrace ? (event) => onTrace(event.kind === 'call' ? { ...event, scope: mask(event.scope) } : event) : null;
    // A print lands in the trace beside the calls, its secrets hidden before the cut so none shows half.
    const line = onLine && emitTrace ? (text: string) => { emitTrace?.(printed(since(), mask(text))); onLine(text); } : onLine;
    try {
        const payload = TE.encode(opts.src);
        // REPL inputs keep the interpreter alive in the wasm instance, implying incremental so the instance itself persists too.
        if (opts.repl) {
            return await execute({ ...opts, onLine: line, payload, incremental: true, start: (e, ptr, n) => e.repl_eval(ptr, n) });
        }
        return await execute({ ...opts, onLine: line, payload, start: (e, ptr, n) => e.run_start(ptr, n) });
    }
    finally {
        running = false;
        emitTrace = null;
    }
}

/* Shared run/restore core, instance, host imports, prefetch, then drive `start`. */
async function execute({ src, payload, start, entry = '', onLine, incremental = false, input }: ExecuteOpts): Promise<ExecResult> {
    if (!wasmModule) throw new Error('engine.load() must be called first');

    /* rt built first (lazy getter) so makeCompilerEnv can decode handles during deferred host calls. */
    const rt = makeRt(requireExports);

    /* Incremental mode reuses the existing wasm instance so module-level state (imports, defs) persists across runs. `onLine` lives in worker.ts and is stable, so old env closures still post correctly. */
    let exports: CompilerExports;
    if (incremental && compilerExports) {
        exports = compilerExports;
    } else {
        exports = await makeInstance(wasmModule, onLine, rt);
    }

    // The engine resolves the walk and every relative import from the script it runs.
    const entryBytes = TE.encode(entry);
    const entryPtr = writeBytes(exports, entryBytes);
    exports.set_entry(entryPtr, entryBytes.length);
    exports.wasm_free(entryPtr, Math.max(1, entryBytes.length));

    await bfsPrefetch(src, exports, {
        baseUrl: programBase,
        read,
        knownMissing,
        importsMap,
        permissions: permissionsMap,
        fetchedSources,
        compilerExports: exports,
        rt,
    }, (packages) => serveSystem(exports, packages));

    // Host-fed stdin, one input() call per line.
    if (input && exports.set_input) {
        const inputBytes = TE.encode(input);
        const ptr = writeBytes(exports, inputBytes);
        exports.set_input(ptr, inputBytes.length);
        exports.wasm_free(ptr, Math.max(1, inputBytes.length));
    }

    // Caps the next run_start or repl_eval, a zero field keeps the sandbox value.
    if (limits && exports.set_limits) {
        exports.set_limits(BigInt(Math.round((limits.memory ?? 0) * 1048576)), BigInt(limits.ops ?? 0));
    }

    const t0 = performance.now();
    runStart = t0;
    // The one wall clock reading of a trace, so a reader can place every event in its own time.
    emitTrace?.({ kind: 'run', at: 0, epoch: Date.now() });
    pendingHostCalls.clear(); // drop any stale captures from a prior run
    // The compiler copies the source out before the call returns.
    const payloadPtr = writeBytes(exports, payload);
    // The SyntaxWarnings of the program print before it runs, as Python prints them.
    if (!incremental && exports.compile_warnings) {
        const n = exports.compile_warnings(payloadPtr, payload.length);
        if (n > 0) console.warn(TD.decode(new Uint8Array(exports.memory.buffer, exports.out_ptr(), n)));
    }
    const status = start(exports, payloadPtr, payload.length);
    exports.wasm_free(payloadPtr, Math.max(1, payload.length));
    let result: ExecResult;
    try {
        result = await drive(exports, rt, status, t0);
    } finally {
        // A finished run owns no requests or sockets, a REPL keeps them for its next input.
        if (!incremental) closeSystem();
    }

    return result;
}

// Where the root edge.json of the run sits, and the list of its files a build or a dev server writes beside it.
let filesBase: string | null = null;
let fileIndex: Promise<Set<string>> | null = null;

/* A project file as the page serves it, each segment escaped so no name reads as a query or a fragment. */
const served = async (path: string): Promise<Response> => {
    if (!filesBase) throw new Error('none');
    const res = await read(new URL(path.split('/').map(encodeURIComponent).join('/'), filesBase).href);
    if (!res.ok) throw new Error('missing');
    return res;
};

// A page cannot list a folder over http, so a build or a dev server writes the list once and every call reads it.
const listed = (): Promise<Set<string>> => (fileIndex ??= served('edge.files').then(async (res) => new Set(await res.json() as string[])));

// A secret leaves the embedder only when a granted name asks for it, and never as anything but text.
const host: Host = {
    secret: (name) => {
        const value = secretsMap && Object.hasOwn(secretsMap, name) ? secretsMap[name] : undefined;
        return typeof value === 'string' ? value : null;
    },
    // Only a path the list names, since a server may follow a link the list left out.
    read: async (path, limit) => {
        if (!(await listed()).has(path)) throw new Error('missing');
        const bytes = new Uint8Array(await (await served(path)).arrayBuffer());
        if (bytes.length > limit) throw new Error('large');
        try {
            return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
        } catch {
            throw new Error('binary');
        }
    },
    list: async (dir, limit) => {
        const found = [...(await listed())].filter((path) => dir === '' || path.startsWith(`${dir}/`));
        if (dir !== '' && found.length === 0) throw new Error('missing');
        return found.slice(0, limit);
    },
};

/* Serves the system modules to every package the walk met, refused where its importers pass none. */
function serveSystem(exports: CompilerExports, packages: Packages): string[] {
    const problem = check(packages.permissions);
    if (problem) return [`edge.json at '${packages.root}edge.json': ${problem}`];
    filesBase = programBase ? new URL(packages.root, programBase).href : null;
    fileIndex = null;
    for (const [dir, pkg, chain] of packages.dirs) {
        if (servedDirs.has(dir)) continue;
        servedDirs.add(dir);
        const holds = held(chain);
        for (const [module, open] of Object.entries(SYSTEM)) {
            const spec = TE.encode(`system:${module}@${dir}`);
            const scoped = scopes(holds, module);
            if (scoped === null) {
                const msg = TE.encode(`'${pkg}' imports ${module}, which edge.json does not grant it`);
                exports.register_module_error(writeBytes(exports, spec), spec.length, writeBytes(exports, msg), msg.length);
                continue;
            }
            const system = open(pkg, scoped, host);
            opened.push(system);
            const calls = Object.entries(system.calls);
            const baseId = nativeTable.length;
            for (const [name, call] of calls) {
                const reported = traced(pkg, module, name, call as (...args: EdgeValue[]) => unknown, () => emitTrace, since);
                nativeTable.push(Object.assign(() => {}, { __edge_kind: 'system' as const, __edge_name: name, __edge_module: module, call: reported }));
            }
            const names = TE.encode(calls.map(([name]) => name).join('\n'));
            exports.register_native_module(writeBytes(exports, spec), spec.length, writeBytes(exports, names), names.length, baseId);
        }
    }
    // A run no package was granted a clock sleeps on the virtual one, so what it prints never depends on when it runs.
    const clock = Object.values(packages.permissions).some((entries) => entries.some((entry) => entry.startsWith('time:')));
    exports.set_wall_clock?.(clock ? 1 : 0);
    return [];
}

/* Aborts every request and socket the system modules left open. */
function closeSystem(): void {
    for (const system of opened) system.close();
    opened = [];
}

/* Fresh instance, resets module registry and native table. */
async function makeInstance(module: WebAssembly.Module, onLine: ((text: string) => void) | undefined, rt: Rt): Promise<CompilerExports> {
    const env = makeCompilerEnv({
        getExports: requireExports,
        onLine: onLine ?? (() => {}),
        fetchedSources,
        rt,
        captureHostCall: (id, call) => { pendingHostCalls.set(id, call); },
    });
    const { exports } = await WebAssembly.instantiate(module, { env } as unknown as WebAssembly.Imports);
    compilerExports = exports as unknown as CompilerExports;
    compilerExports.reset_modules();
    applyPreemptInterval(compilerExports);
    resetNativeTable();
    closeSystem();
    servedDirs.clear();
    return compilerExports;
}

function settlePause(parked: boolean): void {
    const ack = pauseAck;
    pauseAck = null;
    if (ack) ack(parked);
}

/* Service yields until Done / Error / Exit. */
async function drive(exports: CompilerExports, rt: Rt, status: number, t0: number): Promise<ExecResult> {
    while (true) {
        const kind = (status >>> STATUS_KIND_SHIFT) & 7;
        if (kind === STATUS_DONE || kind === STATUS_ERROR || kind === STATUS_EXIT) break;
        if (kind === STATUS_PREEMPTED) {
            // Macrotask, so queued postMessage events land.
            await new Promise((r) => setTimeout(r, 0));
        } else if (kind === STATUS_PENDING_TIMER) {
            const deadlineNs = exports.last_yield_deadline_ns();
            const nowNs = BigInt(Date.now()) * 1_000_000n;
            const waitMs = deadlineNs > nowNs ? Number((deadlineNs - nowNs) / 1_000_000n) : 0;
            emitTrace?.({ kind: 'sleep', at: since(), ms: waitMs });
            await new Promise(r => setTimeout(r, waitMs));
        } else if (kind === STATUS_PENDING_EVENT) {
            // Drain events buffered before VM was ready. `inject_event` wakes the waiter on the first and queues the rest for later `receive()` calls, no `await` needed.
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
            if (pendingHostCalls.size === 0) throw new Error('PENDING_HOST_CALL without captured args (compiler/host drift)');
            const batch = [...pendingHostCalls];
            pendingHostCalls.clear();
            // a failed call raises only in its own coro, so one bad fetch can't sink the batch
            const outcomes = await Promise.allSettled(batch.map(async ([id, call]) => {
                let rv: number;
                try {
                    const value = await call.pending;
                    const plugin = waiting.get(id);
                    rv = plugin ? resumePlugin(exports, rt, id, plugin, { value }) : exports.set_host_result_by_id(id, rt.encodeAny(value as EdgeValue));
                } catch (e) {
                    const [kind, message] = fault(e);
                    const plugin = waiting.get(id);
                    rv = plugin ? resumePlugin(exports, rt, id, plugin, { kind, message }) : exports.set_host_error_by_id(id, kind, rt.encodeAny(message));
                }
                if (rv !== 0) throw new Error(`host-call ${id} delivery returned ${rv} for '${call.module}.${call.name}'`);
            }));
            const drift = outcomes.find((o) => o.status === 'rejected');
            if (drift) throw (drift as PromiseRejectedResult).reason;
        } else {
            // Unknown kind, bail out instead of looping forever.
            break;
        }

        /* Every yield kind parks before the resume, a pause requested mid-wait lands once the wait ends. Events pushed while parked sit in the VM queue and are delivered by this resume. */
        if (pauseRequested) {
            settlePause(true);
            await new Promise<void>((r) => { resumeGate = r; });
        }

        status = exports.run_resume();
    }
    // Run over, a parkless pause must not hang.
    pauseRequested = false;
    settlePause(false);
    // SystemExit, low 8 bits are the exit code, not a buffer length. Finish without a traceback.
    if (((status >>> STATUS_KIND_SHIFT) & 7) === STATUS_EXIT) {
        return { out: '', ms: performance.now() - t0, exitCode: status & 0xFF };
    }
    // Only an error leaves text in the out buffer, a finished run has none.
    const len = ((status >>> STATUS_KIND_SHIFT) & 7) === STATUS_ERROR ? exports.out_len() : 0;
    const ms = performance.now() - t0;
    const out = len > 0
        ? TD.decode(new Uint8Array(exports.memory.buffer, exports.out_ptr(), len))
        : '';
    return { out, ms };
}

/* Header layout mirrors vm/snapshot.rs `header`. */
function snapshotSource(blob: Uint8Array): string {
    if (blob.length < 24) throw new Error('not an edge-python snapshot');
    const v = new DataView(blob.buffer, blob.byteOffset, blob.byteLength);
    if (v.getUint32(0, true) !== 0x4E535045) throw new Error('not an edge-python snapshot');
    const len = Number(v.getBigUint64(16, true));
    if (24 + len > blob.length) throw new Error('not an edge-python snapshot');
    return TD.decode(blob.subarray(24, 24 + len));
}

/* Older wasm lacks the export, only preemption needs it. */
function applyPreemptInterval(exports: CompilerExports): void {
    if (!exports.set_preempt_interval) {
        if (preemptEvery > 0) throw new Error('preemption needs a newer compiler.wasm');
        return;
    }
    exports.set_preempt_interval(preemptEvery);
}

/* Preempt every `n` back-edges, 0 disables. */
export function setPreemptInterval(n: number): void {
    preemptEvery = Math.max(0, n | 0);
    if (compilerExports) applyPreemptInterval(compilerExports);
}

/* Park the run, resolves true once parked. */
export function pause(): Promise<boolean> {
    if (!running) return Promise.resolve(false);
    pauseRequested = true;
    return new Promise((r) => { pauseAck = r; });
}

/* Release a pause-held run, no-op otherwise. */
export function resume(): void {
    pauseRequested = false;
    const gate = resumeGate;
    resumeGate = null;
    if (gate) gate();
}

/* Serialize the parked run, throws when none. */
export function saveState(): Uint8Array {
    if (!compilerExports) throw new Error('nothing to save: no run has started');
    const len = Number(compilerExports.save_state());
    if (len === -2) throw new Error(new TextDecoder().decode(new Uint8Array(compilerExports.memory.buffer, compilerExports.out_ptr(), compilerExports.out_len())));
    if (len < 0) throw new Error('nothing to save: the program is not paused');
    return new Uint8Array(compilerExports.memory.buffer, compilerExports.out_ptr(), len).slice();
}

/* Boot from the blob's embedded source, continue from the saved state. Resolves like run(). */
export async function restoreState({ blob, onLine }: { blob: Uint8Array | ArrayBuffer, onLine?: (text: string) => void }): Promise<ExecResult> {
    running = true;
    try {
        const payload = blob instanceof Uint8Array ? blob : new Uint8Array(blob);
        // The embedded source drives prefetch so restored imports resolve.
        return await execute({ src: snapshotSource(payload), payload, onLine, start: (e, ptr, n) => e.restore_state(ptr, n) });
    }
    finally { running = false; }
}

/* Parked program's module bindings as JSON. */
export function stateGlobals(): Record<string, unknown> {
    if (!compilerExports) return {};
    const len = compilerExports.state_globals();
    return JSON.parse(TD.decode(new Uint8Array(compilerExports.memory.buffer, compilerExports.out_ptr(), len)));
}

/* Parked program's coroutines as JSON. */
export function stateStack(): unknown[] {
    if (!compilerExports) return [];
    const len = compilerExports.state_stack();
    return JSON.parse(TD.decode(new Uint8Array(compilerExports.memory.buffer, compilerExports.out_ptr(), len)));
}

/* Inject directly into the paused VM. Returns false if the VM isn't ready yet (no compilerExports, or no paused run) so callers can buffer. */
function injectEvent(message: string): boolean {
    if (!compilerExports) return false;
    const bytes = TE.encode(message);
    const ptr = writeBytes(compilerExports, bytes);
    const status = compilerExports.run_push_event(ptr, bytes.length);
    compilerExports.wasm_free(ptr, Math.max(1, bytes.length));
    return status === 0;
}

/* Push a string into the VM's event queue, wakes `receive()`. Buffers if the VM isn't paused on PENDING_EVENT yet, the driver loop drains the buffer at the next yield, so callers never need to know about the VM's readiness window. */
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
    if (compilerExports) compilerExports.reset_modules();
    resetNativeTable();
    pendingHostCalls.clear();
    closeSystem();
    servedDirs.clear();
}

/* Forgets every module fetched and every manifest found missing, the next run fetches afresh. */
export function clearCache(): void {
    fetchedSources.clear();
    knownMissing.clear();
}

export function dispose(): void {
    wasmModule = null;
    compilerExports = null;
    importsMap = null;
    permissionsMap = null;
    secretsMap = null;
    tracing = false;
    programBase = null;
    readFile = null;
    fetchedSources.clear();
    knownMissing.clear();
    resetNativeTable();
    pendingHostCalls.clear();
    closeSystem();
    servedDirs.clear();
}
