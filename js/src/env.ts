import { nativeTable, running, waiting } from './native.ts';
import type { WasmPdkFn } from './native.ts';
import type { EdgeExports } from './wasm.ts';
import type { Rt, EdgeValue } from './rt.ts';
import { errMsg, fault, writeBytes, ERR_RUNTIME, ERR_TYPE } from './util.ts';

const TD = new TextDecoder();
const TE = new TextEncoder();

/* a waiting host call whose settled value the driver awaits and delivers */
export interface DeferredHostCall {
    module: string
    name: string
    pending: Promise<unknown>
}

interface EdgeEnv {
    host_print(ptr: number, len: number): void
    host_call_native(id: number, call_id: number, argv_ptr: number, argc: number, out_ptr: number): number
    host_fetch_bytes(specPtr: number, specLen: number, hashPtr: number, outLenPtr: number): number
    host_send(groupPtr: number, groupLen: number, bodyPtr: number, bodyLen: number): number
}

interface MakeEdgeEnvOpts {
    getExports: () => EdgeExports
    onLine: (text: string) => void
    // each named output of a run which the host takes or leaves to the program
    onChannel?: (channel: string, text: string) => boolean
    // why the code sending now may not write `channel` or null when it may
    refuseSend?: (channel: string) => Error | null
    fetchedSources: Map<string, Uint8Array>
    rt?: Rt
    captureHostCall?: (id: number, call: DeferredHostCall) => void
}

/* the `env.*` imports the engine declares wired to host state the closures capture */
export function makeEdgeEnv({ getExports, onLine, onChannel, refuseSend, fetchedSources, rt, captureHostCall }: MakeEdgeEnvOpts): EdgeEnv {
    const readStr = (ptr: number, len: number) => TD.decode(new Uint8Array(getExports().memory.buffer, ptr, len));
    const setU32 = (ptr: number, v: number) => new DataView(getExports().memory.buffer).setUint32(ptr, v, true);

    return {
        host_print: (ptr, len) => onLine(readStr(ptr, len)),

        /* a host call gets decoded values and a plugin its argv staged in guest memory */
        host_call_native: (id, call_id, argv_ptr, argc, out_ptr) => {
            const fn = nativeTable[id];
            if (!fn) {
                stashError(getExports(), `native id ${id} not registered`);
                return 1;
            }

            const exports = getExports();

            if (fn.__edge_kind === 'host') {
                const handles = Array.from(new Uint32Array(exports.memory.buffer, argv_ptr, argc));
                // the trailing slot holds keyword arguments which a host call never takes
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
                    const [kind, message] = fault(e, exports);
                    stashError(exports, message, kind);
                    return 1;
                }
            }

            // views are functions since the plugin call can reenter `wasm_alloc` and detach a cached view
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
            // a plugin waiting on a system call finishes in its resume export once that settles
            if (status === 2 && !fn.__edge_resume) {
                stashError(exports, `native module '${fn.__edge_name}' waits on a system call but exports no __edge_resume`);
                status = 1;
            }
            if (status === 2) waiting.set(call_id, fn);
            // plugins older than `__edge_free` still leak these
            fn.__edge_free?.(g_argv, argvLen);
            fn.__edge_free?.(g_out, 4);
            return status;
        },

        /* serves bytes prefetch already fetched and checked against their pin on arrival */
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

        /* a named output fails when nothing takes it or its sender may not write it */
        host_send: (groupPtr, groupLen, bodyPtr, bodyLen) => {
            const channel = readStr(groupPtr, groupLen);
            const refused = refuseSend?.(channel);
            // 2 tells the runtime to raise the error stashed here in the code that sent
            if (refused) {
                const [kind, message] = fault(refused, getExports());
                stashError(getExports(), message, kind);
                return 2;
            }
            const taken = onChannel?.(channel, readStr(bodyPtr, bodyLen));
            return taken ? 0 : 1;
        },
    };
}

/* hands a settled system call to its waiting plugin and delivers what the plugin answers */
export function resumePlugin(exports: EdgeExports, rt: Rt, id: number, fn: WasmPdkFn, settled: { value: unknown } | { kind: number, message: string }): number {
    waiting.delete(id);
    // a failed call reaches the plugin as a zero handle with its error stashed
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
    // still waiting as the system call it just made settles on the same id
    if (status === 2) {
        waiting.set(id, fn);
        return 0;
    }
    const [kind, message] = takeError(exports);
    return exports.set_host_error_by_id(id, kind, rt.encodeAny(message));
}

/* the kind and message of the error a plugin stashed */
function takeError(exports: EdgeExports): [number, string] {
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

function stashError(exports: EdgeExports, message: string, kind = ERR_RUNTIME): void {
    const bytes = TE.encode(message);
    const ptr = writeBytes(exports, bytes);
    exports.host_edge_throw(kind, ptr, bytes.length);
    exports.wasm_free(ptr, Math.max(1, bytes.length));
}
