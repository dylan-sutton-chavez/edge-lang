import type { Permissions } from './system/perms.ts';
import type { TraceEvent } from './system/trace.ts';

/* run caps with memory in MB where a missing field keeps the sandbox value */
export interface Limits {
    memory?: number
    ops?: number
}

export interface LoadOpts {
    wasmUrl?: string
    // the engine bytes a room receives from its page instead of fetching them
    wasm?: ArrayBuffer | null
    imports?: Record<string, string> | null
    // what the root manifest of the embedder grants beside the imports it declares
    permissions?: Permissions | null
    // whether each run reports what it reached which only a listening embedder pays for
    trace?: boolean | null
    // the program directory where its files and edge.json live
    baseUrl?: string | null
    limits?: Limits | null
}

/* whether an embedder's imports or grants stand in for the edge.json at its base */
export const standsIn = ({ imports, permissions }: { imports?: Record<string, string> | null, permissions?: unknown }): boolean =>
    Object.keys(imports ?? {}).length > 0 || (permissions !== undefined && permissions !== null);

export interface RunOpts {
    src: string
    repl?: boolean
    // the script `src` came from where relative imports resolve or the project root if absent
    entry?: string
    incremental?: boolean
    input?: string
}

export interface ExecResult {
    out: string
    ms: number
    exitCode?: number
}

/* requests from main to worker where `reqId` pairs each answer and one-way types omit it */
export type WorkerRequest =
    | { type: 'load', reqId: number, opts: LoadOpts }
    | ({ type: 'run', reqId: number } & RunOpts)
    | { type: 'set-preempt-interval', reqId: number, interval: number }
    | { type: 'pause', reqId: number }
    | { type: 'resume', reqId: number }
    | { type: 'save-state', reqId: number }
    | { type: 'restore-state', reqId: number, blob: Uint8Array | ArrayBuffer }
    | { type: 'state-globals', reqId: number }
    | { type: 'state-stack', reqId: number }
    | { type: 'reset', reqId: number }
    | { type: 'clear-cache', reqId: number }
    | { type: 'push-event', reqId?: number, message: string }
    | { type: 'dispose', reqId?: number }
    // a file read for the room with status 0 when the page refused or failed
    | { type: 'file', reqId?: number, id: number, status: number, contentType: string, body: ArrayBuffer | null };

/* pushes from worker to main where `response` and `error` may answer a `reqId` */
export type WorkerMessage =
    | { type: 'line', text: string }
    // one line of a named output under the name the program sent it to
    | { type: 'channel', channel: string, text: string }
    | { type: 'trace', event: TraceEvent }
    | { type: 'read', id: number, url: string }
    | { type: 'response', reqId?: number, result: unknown }
    | { type: 'error', reqId?: number, message: string };
