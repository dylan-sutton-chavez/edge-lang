import type { EdgeExports } from './wasm.ts';
import { SystemError } from './system/error.ts';

export const errMsg = (e: unknown): string => e instanceof Error ? e.message : String(e);

// the error kinds of `abi/src/lib.rs` where a custom one carries its class in the message
export const ERR_TYPE = 0;
export const ERR_VALUE = 1;
export const ERR_RUNTIME = 2;
export const ERR_CUSTOM = 6;

const TD = new TextDecoder();

// the fields of a runtime's descriptor the host reads
interface Described { denied_error?: string, failed_error?: string, snapshot?: { magic: number, runtime_at?: number, runtime?: string, source_len_at: number, entry_len_at?: number, source_at: number } }

// each runtime instance's descriptor read once
const descriptors = new WeakMap<EdgeExports, Described>();

/* the descriptor the runtime writes which a runtime without one leaves empty */
export const described = (exports: EdgeExports): Described => {
    let found = descriptors.get(exports);
    if (found) return found;
    const len = exports.runtime_descriptor?.() ?? 0;
    found = len > 0 ? JSON.parse(TD.decode(new Uint8Array(exports.memory.buffer, exports.out_ptr(), len))) as Described : {};
    descriptors.set(exports, found);
    return found;
};

// the classes each runtime instance names for a refused and a failed call
const classes = new WeakMap<EdgeExports, { denied?: string, failed?: string }>();

/* the classes the runtime's descriptor names which a runtime without one leaves out */
export const named = (exports: EdgeExports): { denied?: string, failed?: string } => {
    let found = classes.get(exports);
    if (found) return found;
    const d = described(exports);
    found = { denied: d.denied_error, failed: d.failed_error };
    classes.set(exports, found);
    return found;
};

/* the kind and message of a failed system call with the class its runtime names */
export const fault = (e: unknown, exports: EdgeExports): [number, string] => {
    if (!(e instanceof SystemError)) return [ERR_RUNTIME, errMsg(e)];
    if (e.role === 'invalid') return [ERR_VALUE, e.message];
    const name = named(exports)[e.role];
    return name ? [ERR_CUSTOM, `${name}: ${e.message}`] : [ERR_RUNTIME, e.message];
};

export const writeBytes = (exports: EdgeExports, bytes: Uint8Array): number => {
    const ptr = exports.wasm_alloc(Math.max(1, bytes.length));
    new Uint8Array(exports.memory.buffer, ptr, bytes.length).set(bytes);
    return ptr;
};
