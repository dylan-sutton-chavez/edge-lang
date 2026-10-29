import type { Limits, RunOpts, ExecResult, WorkerRequest, WorkerMessage } from './protocol.ts';
import { roomPolicy, rootOf } from './policy.ts';
import type { Composition } from './system/index.ts';
import type { Permissions } from './system/perms.ts';
import type { TraceEvent } from './system/trace.ts';
import { roomScript } from './room.ts';

export type { Composition, TraceEvent };

export interface CreateWorkerOpts {
    // the program directory whose files and edge.json the page reads for the room
    baseUrl?: string
    wasmUrl?: string
    // the classic script the room runs which a composition of its own bundles
    bundleUrl?: string
    // the modules and grants that bundle serves which the room's policy follows
    composition?: Composition
    imports?: Record<string, string>
    permissions?: Permissions
    trace?: boolean
    limits?: Limits | null
}

export interface WorkerHandle {
    loadMs: number
    run(src: string, runOpts?: Omit<RunOpts, 'src'>): Promise<ExecResult>
    setPreemptInterval(interval: number): Promise<void>
    pause(): Promise<boolean>
    resume(): Promise<void>
    saveState(): Promise<Uint8Array>
    restoreState(blob: Uint8Array | ArrayBuffer): Promise<ExecResult>
    stateGlobals(): Promise<Record<string, unknown>>
    stateStack(): Promise<unknown[]>
    reset(): Promise<void>
    clearCache(): Promise<void>
    pushEvent(message: unknown): void
    onOutput(handler: (text: string) => void): void
    onChannel(handler: (channel: string, text: string) => void): void
    onTrace(handler: (event: TraceEvent) => void): void
    dispose(): void
}

interface Pending {
    resolve: (v: unknown) => void
    reject: (e: Error) => void
}

// a `WorkerRequest` without the `reqId` that `send` attaches
type DistOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;

/* runs one program in a room of its own behind a `postMessage` proxy */
export async function createWorker(opts: CreateWorkerOpts = {}): Promise<WorkerHandle> {
    if (typeof document === 'undefined') throw new Error('createWorker needs a page, missing in this runtime');
    const base = opts.baseUrl ? new URL('./', opts.baseUrl).href : null;
    // the engine and the JS host's own bundle sit beside the host wherever it ships
    const wasmUrl = opts.wasmUrl ?? new URL('../../edge.wasm', import.meta.url).href;
    const bundleUrl = opts.bundleUrl ?? new URL('./worker/bundle.js', import.meta.url).href;
    // the page fetches and hands over the engine so the room loads no code itself
    const [source, wasm, composition, root] = await Promise.all([
        download(bundleUrl).then((r) => r.text()),
        download(wasmUrl).then((r) => r.arrayBuffer()),
        opts.composition ?? own(),
        rootOf(opts, base),
    ]);
    const section = opts.permissions ?? (composition.access ? root.manifest[composition.access.field] : undefined);
    const { port, close } = await openRoom(source, await roomPolicy(root.specs, section, composition));

    let reqIdCounter = 0;
    const pending = new Map<number, Pending>();
    let outputHandler: ((text: string) => void) | null = null;
    let channelHandler: ((channel: string, text: string) => void) | null = null;
    let traceHandler: ((event: TraceEvent) => void) | null = null;

    const tell = (msg: WorkerRequest) => port.postMessage(msg);

    const send = <T = unknown>(payload: DistOmit<WorkerRequest, 'reqId'>): Promise<T> => new Promise((resolve, reject) => {
        const reqId = ++reqIdCounter;
        pending.set(reqId, { resolve: resolve as (v: unknown) => void, reject });
        tell({ ...payload, reqId });
    });

    /* feeds a string to the `receive` queue of the running script */
    const pushEvent = (message: unknown) => tell({ type: 'push-event', message: String(message) });

    /* reads program files for the room only inside its directory and without page cookies */
    const read = async (id: number, url: string): Promise<void> => {
        const reply = { type: 'file' as const, id, status: 0, contentType: '', body: null as ArrayBuffer | null };
        try {
            if (base && new URL(url).href.startsWith(base)) {
                const res = await fetch(url, { credentials: 'omit' });
                reply.status = res.ok ? 200 : res.status;
                reply.contentType = res.headers.get('content-type') ?? '';
                reply.body = res.ok ? await res.arrayBuffer() : null;
            }
        } catch { /* status 0 reads as a failed fetch in the room */ }
        tell(reply);
    };

    port.onmessage = ({ data }: MessageEvent<WorkerMessage>) => {
        switch (data.type) {
            case 'line':
                if (outputHandler) outputHandler(data.text);
                return;
            case 'channel':
                channelHandler?.(data.channel, data.text);
                return;
            case 'trace':
                traceHandler?.(data.event);
                return;
            case 'read':
                void read(data.id, data.url);
                return;
            case 'response':
            case 'error': {
                if (data.reqId == null) {
                    // an error with no request means the worker failed and nothing will answer
                    if (data.type === 'error') {
                        for (const cb of pending.values()) cb.reject(new Error(data.message));
                        pending.clear();
                    }
                    return;
                }
                const cb = pending.get(data.reqId);
                if (!cb) return;
                pending.delete(data.reqId);
                if (data.type === 'error') cb.reject(new Error(data.message));
                else cb.resolve(data.result);
                return;
            }
        }
    };

    // only plain options cross to the room since a composition holds functions
    const { imports, permissions, trace, limits } = opts;
    // a room whose engine fails to start leaves no frame behind
    const ready = await send<{ loadMs: number }>({ type: 'load', opts: { imports, permissions, trace, limits, baseUrl: base, wasm } })
        .catch((e: unknown) => { close(); throw e; });

    return {
        loadMs: ready.loadMs,

        // a worker runs the one program its `baseUrl` names so a run cannot point elsewhere
        run: (src, runOpts = {}) => 'baseUrl' in runOpts
            ? Promise.reject(new Error('baseUrl belongs to createWorker, a worker runs one program'))
            : send<ExecResult>({ type: 'run', src, ...runOpts }),
        /* preempts every `interval` back edges where 0 disables it */
        setPreemptInterval: (interval) => send<void>({ type: 'set-preempt-interval', interval }),
        /* resolves true once the program parks */
        pause: () => send<boolean>({ type: 'pause' }),
        resume: () => send<void>({ type: 'resume' }),
        /* throws when no program is paused */
        saveState: () => send<Uint8Array>({ type: 'save-state' }),
        /* boots from a blob and resolves like `run` */
        restoreState: (blob) => send<ExecResult>({ type: 'restore-state', blob }),
        stateGlobals: () => send<Record<string, unknown>>({ type: 'state-globals' }),
        stateStack: () => send<unknown[]>({ type: 'state-stack' }),
        reset: () => send<void>({ type: 'reset' }),
        clearCache: () => send<void>({ type: 'clear-cache' }),
        pushEvent,

        onOutput(handler: (text: string) => void) { outputHandler = handler; },
        /* each line of a named output of the program as the run writes it */
        onChannel(handler: (channel: string, text: string) => void) { channelHandler = handler; },
        /* what each run reaches reported only by a worker created with `trace` on */
        onTrace(handler: (event: TraceEvent) => void) { traceHandler = handler; },

        dispose() {
            tell({ type: 'dispose' });
            close();
            for (const cb of pending.values()) cb.reject(new Error('worker disposed'));
            pending.clear();
        },
    };
}

/* an engine file the page fetches so the room never loads code */
async function download(url: string): Promise<Response> {
    const res = await fetch(url);
    if (!res.ok) throw new Error(`fetch failed for '${url}' (${res.status})`);
    return res;
}

/* the JS host's own composition which its build bundles beside the room's script */
const own = async (): Promise<Composition> => (await import(new URL('./worker/composition.js', import.meta.url).href)).default;

/* a sandboxed frame of opaque origin that no page cookie or storage or DOM reaches */
async function openRoom(source: string, policy: string): Promise<{ port: MessagePort, close: () => void }> {
    const frame = document.createElement('iframe');
    frame.setAttribute('sandbox', 'allow-scripts');
    frame.hidden = true;
    frame.srcdoc = `<!DOCTYPE html><meta http-equiv="Content-Security-Policy" content="${policy}"><script>${roomScript}</script>`;
    const loaded = new Promise((resolve) => { frame.onload = resolve; });
    document.body.append(frame);
    await loaded;
    const { port1, port2 } = new MessageChannel();
    frame.contentWindow?.postMessage(source, '*', [port2]);
    return { port: port1, close: () => { port1.close(); frame.remove(); } };
}
