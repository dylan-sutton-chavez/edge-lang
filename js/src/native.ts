import type { EdgeExports } from './wasm.ts';
import type { EdgeValue } from './rt.ts';
import { errMsg } from './util.ts';

/* a registered native fn tagged with how `host_call_native` dispatches it */
interface NativeFnBase {
    (...args: unknown[]): unknown
    __edge_kind?: 'wasmpdk' | 'host'
    __edge_name?: string
}

export interface WasmPdkFn extends NativeFnBase {
    __edge_kind: 'wasmpdk'
    __edge_alloc: (size: number) => number
    __edge_free?: (ptr: number, len: number) => void
    __edge_memory: WebAssembly.Memory
    // where a system call the plugin waited on settles
    __edge_resume?: (call: number, answer: number, out: number) => number
}

/* a host call of one package answering with a value or a promise settled later */
interface HostFn extends NativeFnBase {
    __edge_kind: 'host'
    __edge_name: string
    __edge_module: string
    call: (...args: EdgeValue[]) => unknown
}

export type NativeFn = WasmPdkFn | HostFn;

interface NativeModuleResult {
    names: string[]
    fns: NativeFn[]
}

const ABI_VERSION = 1;

/* plugin exports and host calls by the `baseId` of `register_native_module` */
export const nativeTable: NativeFn[] = [];

// the plugin calls running now with the innermost last for `edge_call_id` to answer
export const running: number[] = [];

// the plugin waiting on each call id resumed once its system call settles
export const waiting = new Map<number, WasmPdkFn>();

export function resetNativeTable(): void {
    nativeTable.length = 0;
    running.length = 0;
    waiting.clear();
}

/* the 7 `env.edge_*` imports of plugins bridging guest and engine memory */
function makeGuestEnv(edgeExports: EdgeExports) {
    const edgeMem = () => new Uint8Array(edgeExports.memory.buffer);
    const edgeView = () => new DataView(edgeExports.memory.buffer);

    return (guestExports: { memory: WebAssembly.Memory }) => {
        const gMem = () => new Uint8Array(guestExports.memory.buffer);
        const gView = () => new DataView(guestExports.memory.buffer);

        const stage = (ptr: number, len: number): number => {
            const c = edgeExports.wasm_alloc(Math.max(1, len));
            if (len) edgeMem().set(gMem().subarray(ptr, ptr + len), c);
            return c;
        };
        const unstage = (c: number, len: number): void => edgeExports.wasm_free(c, Math.max(1, len));

        return {
            edge_op: (op: number, recv: number, name_ptr: number, name_len: number, argv_ptr: number, argc: number, out: number): number => {
                const eName = stage(name_ptr, name_len);
                const argvLen = Math.max(4, argc * 4);
                const eArgv = edgeExports.wasm_alloc(argvLen);
                const eOut = edgeExports.wasm_alloc(4);
                for (let i = 0; i < argc; i++) {
                    edgeView().setUint32(eArgv + i * 4, gView().getUint32(argv_ptr + i * 4, true), true);
                }
                const ret = edgeExports.host_edge_op(op, recv, eName, name_len, eArgv, argc, eOut);
                if (ret === 0 && out) gView().setUint32(out, edgeView().getUint32(eOut, true), true);
                unstage(eName, name_len);
                edgeExports.wasm_free(eArgv, argvLen);
                edgeExports.wasm_free(eOut, 4);
                return ret;
            },

            edge_encode: (tag: number, ptr: number, len: number): number => {
                const c = stage(ptr, len);
                const h = edgeExports.host_edge_encode(tag, c, len);
                unstage(c, len);
                return h;
            },

            edge_decode: (h: number, out_tag: number, dst: number, dst_max: number): number => {
                const bufLen = Math.max(1, dst_max);
                const eTag = edgeExports.wasm_alloc(4);
                const eBuf = edgeExports.wasm_alloc(bufLen);
                const ret = edgeExports.host_edge_decode(h, eTag, eBuf, dst_max);
                gView().setUint32(out_tag, edgeView().getUint32(eTag, true), true);
                if (ret > 0) gMem().set(edgeMem().subarray(eBuf, eBuf + ret), dst);
                edgeExports.wasm_free(eTag, 4);
                edgeExports.wasm_free(eBuf, bufLen);
                return ret;
            },

            edge_release: (h: number): void => edgeExports.host_edge_release(h),

            edge_call_id: (): number => running[running.length - 1] ?? 0,

            edge_throw: (kind: number, msg_ptr: number, msg_len: number): void => {
                const c = stage(msg_ptr, msg_len);
                edgeExports.host_edge_throw(kind, c, msg_len);
                unstage(c, msg_len);
            },

            edge_take_error: (out_kind: number, dst: number, dst_max: number): number => {
                const bufLen = Math.max(1, dst_max);
                const eKind = edgeExports.wasm_alloc(4);
                const eBuf = edgeExports.wasm_alloc(bufLen);
                const ret = edgeExports.host_edge_take_error(eKind, eBuf, dst_max);
                if (ret >= 0) {
                    gView().setUint32(out_kind, edgeView().getUint32(eKind, true), true);
                    if (ret > 0) gMem().set(edgeMem().subarray(eBuf, eBuf + ret), dst);
                }
                edgeExports.wasm_free(eKind, 4);
                edgeExports.wasm_free(eBuf, bufLen);
                return ret;
            },
        };
    };
}

/* annotates each plugin export with the `__edge_alloc` and `__edge_memory` of its guest */
async function instantiatePlugin(module: WebAssembly.Module, edgeExports: EdgeExports): Promise<NativeModuleResult> {
    const envFactory = makeGuestEnv(edgeExports);
    // the getter reads `instance` lazily since env functions only fire once it is bound
    const env = envFactory({ get memory() { return instance.exports.memory as WebAssembly.Memory; } });
    const instance = await WebAssembly.instantiate(module, { env }).catch((e: unknown) => { throw new Error(`it fails to instantiate, ${errMsg(e)}`); });

    // the CLI makes these checks in this order and so does this host
    if (!(instance.exports.memory instanceof WebAssembly.Memory)) throw new Error('it exports no memory');
    if (typeof instance.exports.__edge_abi_version !== 'function') {
        throw new Error(`it is missing '__edge_abi_version() -> u32', see /reference/abi for the contract`);
    }
    const v = (instance.exports.__edge_abi_version as () => number)();
    if (v !== ABI_VERSION) throw new Error(`it speaks ABI v${v}, this host expects v${ABI_VERSION}`);
    if (typeof instance.exports.__edge_alloc !== 'function') {
        throw new Error(`it is missing '__edge_alloc(size: u32) -> *mut u8', see /reference/abi for the contract`);
    }

    const names: string[] = [];
    const fns: NativeFn[] = [];
    for (const [k, value] of Object.entries(instance.exports)) {
        if (k === 'memory' || typeof value !== 'function') continue;
        // keeps the convention exports and drops ABI internals such as `__edge_alloc`
        if (k.startsWith('__') && !k.startsWith('__class_') && !k.startsWith('__const_') && !k.startsWith('__fn_')) continue;
        const v = value as WasmPdkFn;
        names.push(k);
        v.__edge_alloc = instance.exports.__edge_alloc as (size: number) => number;
        // older plugins lack it so callers use `?.`
        v.__edge_free = instance.exports.__edge_free as ((ptr: number, len: number) => void) | undefined;
        v.__edge_memory = instance.exports.memory as WebAssembly.Memory;
        v.__edge_resume = instance.exports.__edge_resume as WasmPdkFn['__edge_resume'];
        v.__edge_kind = 'wasmpdk';
        fns.push(v);
    }
    return { names, fns };
}

/* compiles and instantiates plugin bytes beside the engine instance its imports bridge to */
export async function loadNativeModule(bytes: Uint8Array, edgeExports: EdgeExports): Promise<NativeModuleResult> {
    const module = await WebAssembly.compile(bytes as BufferSource);
    const result = await instantiatePlugin(module, edgeExports);
    annotateNames(result);
    return result;
}

/* pairs each fn with its declared name for the errors about it */
function annotateNames({ names, fns }: NativeModuleResult): void {
    for (let i = 0; i < fns.length; i++) {
        const fn = fns[i], name = names[i];
        if (fn && name) fn.__edge_name = name;
    }
}
