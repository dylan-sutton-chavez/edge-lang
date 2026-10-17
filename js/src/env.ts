import { nativeTable, running, waiting } from './native.ts';
import type { WasmPdkFn } from './native.ts';
import type { CompilerExports } from './wasm.ts';
import type { Rt, EdgeValue } from './rt.ts';
import { errMsg, fault, writeBytes, ERR_RUNTIME, ERR_TYPE } from './util.ts';

const TD = new TextDecoder();
const TE = new TextEncoder();

/* A system call still waiting, the driver awaits it and delivers what it settles with. */
export interface DeferredHostCall {
    module: string
    name: string
    pending: Promise<unknown>
}

interface CompilerEnv {
    host_print(ptr: number, len: number): void
    host_call_native(id: number, call_id: number, argv_ptr: number, argc: number, out_ptr: number): number
    host_fetch_bytes(specPtr: number, specLen: number, hashPtr: number, outLenPtr: number): number
    host_send(groupPtr: number, groupLen: number, bodyPtr: number, bodyLen: number): number
}

interface MakeCompilerEnvOpts {
    getExports: () => CompilerExports
    onLine: (text: string) => void
    fetchedSources: Map<string, Uint8Array>
    rt?: Rt
    captureHostCall?: (id: number, call: DeferredHostCall) => void
}

/* The `env.*` imports the compiler declares (host_print, host_call_native, host_fetch_bytes, host_send), wired to closure-captured engine state. */
export function makeCompilerEnv({ getExports, onLine, fetchedSources, rt, captureHostCall }: MakeCompilerEnvOpts): CompilerEnv {
    const readStr = (ptr: number, len: number) => TD.decode(new Uint8Array(getExports().memory.buffer, ptr, len));
    const setU32 = (ptr: number, v: number) => new DataView(getExports().memory.buffer).setUint32(ptr, v, true);

    return {
        host_print: (ptr, len) => onLine(readStr(ptr, len)),

        /* A system call gets decoded values, a plugin gets its argv staged in guest memory. */
        host_call_native: (id, call_id, argv_ptr, argc, out_ptr) => {
            const fn = nativeTable[id];
            if (!fn) {
                stashError(getExports(), `native id ${id} not registered`);
                return 1;
            }

            const exports = getExports();

            if (fn.__edge_kind === 'system') {
                const handles = Array.from(new Uint32Array(exports.memory.buffer, argv_ptr, argc));
                // The trailing slot holds the kwargs, which a system call never takes.
                if (!rt || handles.pop() !== 0) {
                    stashError(exports, `${fn.__edge_module}.${fn.__edge_name} takes positional arguments only`, ERR_TYPE);
                    return 1;
                }
                try {
                    const result = fn.call(...handles.map((h) => rt.decodeAny(h)));
                    if (result instanceof Promise) {
                        if (!captureHostCall) throw new Error(`${fn.__edge_module}.${fn.__edge_name} waits, and no driver awaits it`);
                        captureHostCall(call_id, { module: fn.__edge_module, name: fn.__edge_name, pending: result });
                        return 2;
                    }
                    setU32(out_ptr, rt.encodeAny(result as EdgeValue));
                    return 0;
                } catch (e) {
                    const [kind, message] = fault(e);
                    stashError(exports, message, kind);
                    return 1;
                }
            }

            // wasmpdk, stage argv, call, copy back. Views as fns because `fn(...)` can re-enter `wasm_alloc` and detach a cached view.
            const guestView = () => new DataView(fn.__edge_memory.buffer);
            const compView = () => new DataView(exports.memory.buffer);

            const argvLen = Math.max(4, argc * 4);
            const g_argv = fn.__edge_alloc(argvLen);
            const g_out = fn.__edge_alloc(4);
            for (let i = 0; i < argc; i++) {
                guestView().setUint32(g_argv + i * 4, compView().getUint32(argv_ptr + i * 4, true), true);
            }

            let status: number;
            running.push(call_id);
            try {
                status = fn(g_argv, argc, g_out) as number;
            } catch (e) {
                stashError(exports, `native module trapped: ${errMsg(e)}`);
                return 1;
            } finally {
                running.pop();
            }
            if (status === 0) {
                compView().setUint32(out_ptr, guestView().getUint32(g_out, true), true);
            }
            // A plugin waiting on a system call finishes in its resume export once that settles.
            if (status === 2 && !fn.__edge_resume) {
                stashError(exports, `native module '${fn.__edge_name}' waits on a system call but exports no __edge_resume`);
                status = 1;
            }
            if (status === 2) waiting.set(call_id, fn);
            // Optional export, pre-__edge_free plugins still leak.
            fn.__edge_free?.(g_argv, argvLen);
            fn.__edge_free?.(g_out, 4);
            return status;
        },

        /* Serves the bytes prefetch already fetched, their `#sha256-...` pin checked when they arrived. */
        host_fetch_bytes: (specPtr, specLen, _hashPtr, outLenPtr) => {
            const spec = readStr(specPtr, specLen);
            const bytes = fetchedSources.get(spec);
            if (bytes === undefined) { setU32(outLenPtr, 0); return 0; }

            const exps = getExports();
            const ptr = exps.wasm_alloc(bytes.length);
            new Uint8Array(exps.memory.buffer, ptr, bytes.length).set(bytes);
            setU32(outLenPtr, bytes.length);
            return ptr;
        },

        /* No actor scheduler lives in a page or a worker, so send() raises its missing-scheduler error. */
        host_send: () => 1,
    };
}

/* Hands a settled system call to its waiting plugin, then delivers what the plugin answers. */
export function resumePlugin(exports: CompilerExports, rt: Rt, id: number, fn: WasmPdkFn, settled: { value: unknown } | { kind: number, message: string }): number {
    waiting.delete(id);
    // A failed call reaches the plugin as a zero handle with its error stashed.
    let answer = 0;
    if ('value' in settled) answer = rt.encodeAny(settled.value as EdgeValue);
    else stashError(exports, settled.message, settled.kind);
    const out = fn.__edge_alloc(4);
    let status: number;
    running.push(id);
    try {
        status = fn.__edge_resume?.(id, answer, out) ?? 1;
    } catch (e) {
        stashError(exports, `native module trapped: ${errMsg(e)}`);
        status = 1;
    } finally {
        running.pop();
    }
    const handle = new DataView(fn.__edge_memory.buffer).getUint32(out, true);
    fn.__edge_free?.(out, 4);
    if (status === 0) return exports.set_host_result_by_id(id, handle);
    // Still waiting, the system call it just made settles on the same id.
    if (status === 2) {
        waiting.set(id, fn);
        return 0;
    }
    const [kind, message] = takeError(exports);
    return exports.set_host_error_by_id(id, kind, rt.encodeAny(message));
}

/* The error a plugin stashed, as its kind and message. */
function takeError(exports: CompilerExports): [number, string] {
    let size = 256;
    for (;;) {
        const kind = exports.wasm_alloc(4);
        const buf = exports.wasm_alloc(size);
        const got = exports.host_edge_take_error(kind, buf, size);
        const result: [number, string] = [new DataView(exports.memory.buffer).getUint32(kind, true), TD.decode(new Uint8Array(exports.memory.buffer, buf, Math.max(0, got)))];
        exports.wasm_free(kind, 4);
        exports.wasm_free(buf, size);
        if (got >= 0) return result;
        if (got === -1) return [ERR_RUNTIME, 'native call failed'];
        size = -got;
    }
}

function stashError(exports: CompilerExports, message: string, kind = ERR_RUNTIME): void {
    const bytes = TE.encode(message);
    const ptr = writeBytes(exports, bytes);
    exports.host_edge_throw(kind, ptr, bytes.length);
    exports.wasm_free(ptr, Math.max(1, bytes.length));
}
