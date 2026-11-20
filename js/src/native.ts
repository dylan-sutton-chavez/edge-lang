import type { InterpreterExports } from './wasm.ts';
import type { InterpreterValue } from './rt.ts';
import { errMsg } from './util.ts';

/* a registered native fn tagged with how `host_call_native` dispatches it */
interface NativeFnBase {
    (...args: unknown[]): unknown
    __interpreter_kind?: 'wasmpdk' | 'host'
    __interpreter_name?: string
}

export interface WasmPdkFn extends NativeFnBase {
    __interpreter_kind: 'wasmpdk'
    __interpreter_alloc: (size: number) => number
    __interpreter_free?: (ptr: number, len: number) => void
    __interpreter_memory: WebAssembly.Memory
    // where a system call the plugin waited on settles
    __interpreter_resume?: (call: number, answer: number, out: number) => number
}

/* a host call of one package answering with a value or a promise settled later */
interface HostFn extends NativeFnBase {
    __interpreter_kind: 'host'
    __interpreter_name: string
    __interpreter_module: string
    call: (...args: InterpreterValue[]) => unknown
}

export type NativeFn = WasmPdkFn | HostFn;

interface NativeModuleResult {
    names: string[]
    fns: NativeFn[]
}

const PLUGIN_ABI = 1;

/* plugin exports and host calls by the `baseId` of `register_native_module` */
export const nativeTable: NativeFn[] = [];

// the plugin calls running now with the innermost last for `interpreter_call_id` to answer
export const running: number[] = [];

// the plugin waiting on each call id resumed once its system call settles
export const waiting = new Map<number, WasmPdkFn>();

export function resetNativeTable(): void {
    nativeTable.length = 0;
    running.length = 0;
    waiting.clear();
}

/* the 7 `env.interpreter_*` imports of plugins bridging guest and engine memory */
function makeGuestEnv(interpreterExports: InterpreterExports) {
    const interpreterMem = () => new Uint8Array(interpreterExports.memory.buffer);
    const interpreterView = () => new DataView(interpreterExports.memory.buffer);

    return (guestExports: { memory: WebAssembly.Memory }) => {
        const gMem = () => new Uint8Array(guestExports.memory.buffer);
        const gView = () => new DataView(guestExports.memory.buffer);

        const stage = (ptr: number, len: number): number => {
            const c = interpreterExports.wasm_alloc(Math.max(1, len));
            if (len) interpreterMem().set(gMem().subarray(ptr, ptr + len), c);
            return c;
        };
        const unstage = (c: number, len: number): void => interpreterExports.wasm_free(c, Math.max(1, len));

        return {
            interpreter_op: (op: number, recv: number, name_ptr: number, name_len: number, argv_ptr: number, argc: number, out: number): number => {
                const eName = stage(name_ptr, name_len);
                const argvLen = Math.max(4, argc * 4);
                const eArgv = interpreterExports.wasm_alloc(argvLen);
                const eOut = interpreterExports.wasm_alloc(4);
                for (let i = 0; i < argc; i++) {
                    interpreterView().setUint32(eArgv + i * 4, gView().getUint32(argv_ptr + i * 4, true), true);
                }
                const ret = interpreterExports.host_interpreter_op(op, recv, eName, name_len, eArgv, argc, eOut);
                if (ret === 0 && out) gView().setUint32(out, interpreterView().getUint32(eOut, true), true);
                unstage(eName, name_len);
                interpreterExports.wasm_free(eArgv, argvLen);
                interpreterExports.wasm_free(eOut, 4);
                return ret;
            },

            interpreter_encode: (tag: number, ptr: number, len: number): number => {
                const c = stage(ptr, len);
                const h = interpreterExports.host_interpreter_encode(tag, c, len);
                unstage(c, len);
                return h;
            },

            interpreter_decode: (h: number, out_tag: number, dst: number, dst_max: number): number => {
                const bufLen = Math.max(1, dst_max);
                const eTag = interpreterExports.wasm_alloc(4);
                const eBuf = interpreterExports.wasm_alloc(bufLen);
                const ret = interpreterExports.host_interpreter_decode(h, eTag, eBuf, dst_max);
                gView().setUint32(out_tag, interpreterView().getUint32(eTag, true), true);
                if (ret > 0) gMem().set(interpreterMem().subarray(eBuf, eBuf + ret), dst);
                interpreterExports.wasm_free(eTag, 4);
                interpreterExports.wasm_free(eBuf, bufLen);
                return ret;
            },

            interpreter_release: (h: number): void => interpreterExports.host_interpreter_release(h),

            interpreter_call_id: (): number => running[running.length - 1] ?? 0,

            interpreter_throw: (kind: number, msg_ptr: number, msg_len: number): void => {
                const c = stage(msg_ptr, msg_len);
                interpreterExports.host_interpreter_throw(kind, c, msg_len);
                unstage(c, msg_len);
            },

            interpreter_take_error: (out_kind: number, dst: number, dst_max: number): number => {
                const bufLen = Math.max(1, dst_max);
                const eKind = interpreterExports.wasm_alloc(4);
                const eBuf = interpreterExports.wasm_alloc(bufLen);
                const ret = interpreterExports.host_interpreter_take_error(eKind, eBuf, dst_max);
                if (ret >= 0) {
                    gView().setUint32(out_kind, interpreterView().getUint32(eKind, true), true);
                    if (ret > 0) gMem().set(interpreterMem().subarray(eBuf, eBuf + ret), dst);
                }
                interpreterExports.wasm_free(eKind, 4);
                interpreterExports.wasm_free(eBuf, bufLen);
                return ret;
            },
        };
    };
}

/* compiles and instantiates plugin bytes beside the engine instance its imports bridge to with each export annotated by its guest */
export async function loadNativeModule(bytes: Uint8Array, interpreterExports: InterpreterExports): Promise<NativeModuleResult> {
    const module = await WebAssembly.compile(bytes as BufferSource);
    const envFactory = makeGuestEnv(interpreterExports);
    // the getter reads `instance` lazily since env functions only fire once it is bound
    const env = envFactory({ get memory() { return instance.exports.memory as WebAssembly.Memory; } });
    const instance = await WebAssembly.instantiate(module, { env }).catch((e: unknown) => { throw new Error(`it fails to instantiate, ${errMsg(e)}`); });

    // the CLI makes these checks in this order and so does this host
    if (!(instance.exports.memory instanceof WebAssembly.Memory)) throw new Error('it exports no memory');
    if (typeof instance.exports.__interpreter_abi_version !== 'function') {
        throw new Error(`it is missing '__interpreter_abi_version() -> u32', see docs/08-plugins.mdx for the contract`);
    }
    const v = (instance.exports.__interpreter_abi_version as () => number)();
    if (!Number.isInteger(v) || v < 1 || v > PLUGIN_ABI) throw new Error(`it speaks ABI v${v}, this host serves v1 to v${PLUGIN_ABI}`);
    if (typeof instance.exports.__interpreter_alloc !== 'function') {
        throw new Error(`it is missing '__interpreter_alloc(size: u32) -> *mut u8', see docs/08-plugins.mdx for the contract`);
    }

    const names: string[] = [];
    const fns: NativeFn[] = [];
    for (const [k, value] of Object.entries(instance.exports)) {
        if (k === 'memory' || typeof value !== 'function') continue;
        // keeps the convention exports and drops ABI internals such as `__interpreter_alloc`
        if (k.startsWith('__') && !k.startsWith('__class_') && !k.startsWith('__const_') && !k.startsWith('__fn_')) continue;
        const v = value as WasmPdkFn;
        names.push(k);
        v.__interpreter_alloc = instance.exports.__interpreter_alloc as (size: number) => number;
        // older plugins lack it so callers use `?.`
        v.__interpreter_free = instance.exports.__interpreter_free as ((ptr: number, len: number) => void) | undefined;
        v.__interpreter_memory = instance.exports.memory as WebAssembly.Memory;
        v.__interpreter_resume = instance.exports.__interpreter_resume as WasmPdkFn['__interpreter_resume'];
        v.__interpreter_kind = 'wasmpdk';
        // the declared name the errors about it give
        v.__interpreter_name = k;
        fns.push(v);
    }
    return { names, fns };
}
