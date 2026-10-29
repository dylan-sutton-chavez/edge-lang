export type EdgeValue = null | boolean | number | bigint | string | Uint8Array | ArrayBuffer | EdgeValue[] | { [key: string]: EdgeValue };

/* the handle codec `makeRt` returns for the values a host call takes and answers */
export interface Rt {
    decodeStr(h: number): string
    decodeInt(h: number): number | bigint
    decodeBool(h: number): boolean
    decodeFloat(h: number): number
    encodeStr(s: string): number
    encodeInt(n: number | bigint): number
    encodeBool(b: boolean): number
    encodeFloat(f: number): number
    encodeNone(): number
    decodeAny(h: number): EdgeValue
    encodeAny(v: EdgeValue): number
}

import type { EdgeExports } from './wasm.ts';

const TD = new TextDecoder();
const TE = new TextEncoder();

// the tags of `pub mod tag` in `abi/src/lib.rs`
const TAG_NONE = 0;
const TAG_BOOL = 1;
const TAG_INT = 2;
const TAG_FLOAT = 3;
const TAG_BYTES = 4;
const TAG_RAW = 5;
const TAG_LIST = 6;
// what the engine answers for a value no transit value holds
const TAG_INVALID = 0xFFFFFFFF;
const TAG_DICT = 7;

const U64_MASK = (1n << 64n) - 1n;
const I128_MAX = (1n << 127n) - 1n;
const I128_MIN = -(1n << 127n);
const SAFE_MAX = BigInt(Number.MAX_SAFE_INTEGER);

function readI128(v: DataView, off: number): number | bigint {
    const big = (v.getBigInt64(off + 8, true) << 64n) | v.getBigUint64(off, true);
    return big >= -SAFE_MAX && big <= SAFE_MAX ? Number(big) : big;
}

function writeI128(v: DataView, off: number, n: number | bigint): void {
    const big = BigInt(n);
    if (big > I128_MAX || big < I128_MIN) throw new RangeError(`int out of i128 range: ${n}`);
    v.setBigUint64(off, big & U64_MASK, true);
    v.setBigInt64(off + 8, big >> 64n, true);
}

/* wraps `host_edge_decode` and `host_edge_encode` so host calls skip NaN boxing */
export function makeRt(getExports: () => EdgeExports): Rt {
    return {
        decodeStr: (h) => decodeStr(getExports(), h),
        decodeInt: (h) => decodeInt(getExports(), h),
        decodeBool: (h) => decodeBool(getExports(), h),
        decodeFloat: (h) => decodeFloat(getExports(), h),
        encodeStr: (s) => encodeStr(getExports(), s),
        encodeInt: (n) => encodeInt(getExports(), n),
        encodeBool: (b) => encodeBool(getExports(), b),
        encodeFloat: (f) => encodeFloat(getExports(), f),
        encodeNone: () => getExports().host_edge_encode(TAG_NONE, 0, 0),
        /* any handle to a plain JS value as a host call takes its arguments */
        decodeAny: (h) => decodeAny(getExports(), h),
        /* encodes integral numbers as ints and the other numbers as floats */
        encodeAny: (v) => encodeAny(getExports(), v),
    };
}

// decodes once and retries with the size asked for when the buffer is short
function rawDecode(exps: EdgeExports, handle: number): { tag: number, bytes: Uint8Array } {
    const tagPtr = exps.wasm_alloc(4);
    let cap = 256;
    let dstPtr = exps.wasm_alloc(cap);
    let n = exps.host_edge_decode(handle, tagPtr, dstPtr, cap);
    if (n < 0) {
        const needed = -n;
        exps.wasm_free(dstPtr, cap);
        cap = needed;
        dstPtr = exps.wasm_alloc(cap);
        n = exps.host_edge_decode(handle, tagPtr, dstPtr, cap);
    }
    const tag = new DataView(exps.memory.buffer).getUint32(tagPtr, true);
    exps.wasm_free(tagPtr, 4);
    const bytes = n > 0 ? new Uint8Array(exps.memory.buffer, dstPtr, n).slice() : new Uint8Array(0);
    exps.wasm_free(dstPtr, cap);
    return { tag, bytes };
}

function decodeBytes(exps: EdgeExports, handle: number, expectedTag: number): Uint8Array {
    const { tag, bytes } = rawDecode(exps, handle);
    if (tag !== expectedTag) throw new Error(`expected tag ${expectedTag}, got ${tag}`);
    return bytes;
}

const decodeStr = (exps: EdgeExports, h: number) => TD.decode(decodeBytes(exps, h, TAG_BYTES));
const decodeInt = (exps: EdgeExports, h: number) => {
    const b = decodeBytes(exps, h, TAG_INT);
    return readI128(new DataView(b.buffer, b.byteOffset, 16), 0);
};
const decodeBool = (exps: EdgeExports, h: number) => decodeBytes(exps, h, TAG_BOOL)[0] !== 0;
const decodeFloat = (exps: EdgeExports, h: number) => {
    const b = decodeBytes(exps, h, TAG_FLOAT);
    return new DataView(b.buffer, b.byteOffset, 8).getFloat64(0, true);
};

// one place pairs the alloc and free so their lengths cannot drift
function encodeScalar(exps: EdgeExports, tag: number, len: number, write: (ptr: number) => void): number {
    const ptr = exps.wasm_alloc(len);
    write(ptr);
    const h = exps.host_edge_encode(tag, ptr, len);
    exps.wasm_free(ptr, len);
    return h;
}
function encodeStr(exps: EdgeExports, s: string): number {
    const bytes = TE.encode(s);
    return encodeScalar(exps, TAG_BYTES, bytes.length, (ptr) => new Uint8Array(exps.memory.buffer, ptr, bytes.length).set(bytes));
}
function encodeInt(exps: EdgeExports, n: number | bigint): number {
    return encodeScalar(exps, TAG_INT, 16, (ptr) => writeI128(new DataView(exps.memory.buffer), ptr, n));
}
function encodeBool(exps: EdgeExports, b: boolean): number {
    return encodeScalar(exps, TAG_BOOL, 1, (ptr) => { new Uint8Array(exps.memory.buffer, ptr, 1)[0] = b ? 1 : 0; });
}
function encodeFloat(exps: EdgeExports, f: number): number {
    return encodeScalar(exps, TAG_FLOAT, 8, (ptr) => new DataView(exps.memory.buffer).setFloat64(ptr, f, true));
}

function decodeAny(exps: EdgeExports, handle: number): EdgeValue {
    const { tag, bytes } = rawDecode(exps, handle);
    return decodeBody(tag, bytes);
}

/* list and dict payloads hold nested nodes framed as `tag:u32le len:u32le payload` */
function decodeBody(tag: number, bytes: Uint8Array): EdgeValue {
    const view = () => new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    switch (tag) {
        case TAG_NONE: return null;
        case TAG_BOOL: return bytes[0] !== 0;
        case TAG_INT: return readI128(view(), 0);
        case TAG_FLOAT: return view().getFloat64(0, true);
        case TAG_BYTES: return TD.decode(bytes);
        // copied since nested payloads are views into the parent buffer
        case TAG_RAW: return bytes.slice();
        case TAG_LIST: case TAG_DICT: {
            const v = view();
            const count = v.getUint32(0, true);
            let pos = 4;
            const nextNode = (): EdgeValue => {
                const t = v.getUint32(pos, true);
                const len = v.getUint32(pos + 4, true);
                const payload = bytes.subarray(pos + 8, pos + 8 + len);
                pos += 8 + len;
                return decodeBody(t, payload);
            };
            if (tag === TAG_LIST) {
                return Array.from({ length: count }, nextNode);
            }
            const obj: { [key: string]: EdgeValue } = {};
            for (let i = 0; i < count; i++) {
                const key = nextNode();
                obj[String(key)] = nextNode();
            }
            return obj;
        }
        // both hosts word these alike so a program fails the same on each
        case TAG_INVALID: throw new Error('value is not a transit value');
        default: throw new Error('malformed wire value');
    }
}

/* picks the tag from `typeof` and takes a bigint as an int too */
function encodeAny(exps: EdgeExports, value: EdgeValue): number {
    if (value === null || value === undefined) return exps.host_edge_encode(TAG_NONE, 0, 0);
    if (typeof value === 'boolean') return encodeBool(exps, value);
    if (typeof value === 'bigint') return encodeInt(exps, value);
    if (typeof value === 'number') {
        return Number.isInteger(value) ? encodeInt(exps, value) : encodeFloat(exps, value);
    }
    if (typeof value === 'string') return encodeStr(exps, value);
    // composites and raw bytes serialize to one payload for a single `host_edge_encode`
    const { tag, body } = encodeNode(value);
    const ptr = exps.wasm_alloc(Math.max(1, body.length));
    new Uint8Array(exps.memory.buffer, ptr, body.length).set(body);
    const h = exps.host_edge_encode(tag, ptr, body.length);
    exps.wasm_free(ptr, Math.max(1, body.length));
    return h;
}

/* the tag and body of a nested node or a composite root */
function encodeNode(value: EdgeValue): { tag: number, body: Uint8Array } {
    if (value === null || value === undefined) return { tag: TAG_NONE, body: new Uint8Array(0) };
    if (typeof value === 'boolean') return { tag: TAG_BOOL, body: Uint8Array.of(value ? 1 : 0) };
    if (typeof value === 'number' || typeof value === 'bigint') {
        const body = new Uint8Array(typeof value === 'number' && !Number.isInteger(value) ? 8 : 16);
        const v = new DataView(body.buffer);
        if (body.length === 8) {
            v.setFloat64(0, value as number, true);
            return { tag: TAG_FLOAT, body };
        }
        writeI128(v, 0, value);
        return { tag: TAG_INT, body };
    }
    if (typeof value === 'string') return { tag: TAG_BYTES, body: TE.encode(value) };
    if (value instanceof Uint8Array) return { tag: TAG_RAW, body: value };
    if (value instanceof ArrayBuffer) return { tag: TAG_RAW, body: new Uint8Array(value) };
    if (Array.isArray(value)) {
        return { tag: TAG_LIST, body: joinNodes(value.length, value.map(encodeNode)) };
    }
    if (typeof value === 'object') {
        const nodes = Object.entries(value).flatMap(([k, v]) => [encodeNode(k), encodeNode(v)]);
        return { tag: TAG_DICT, body: joinNodes(Object.keys(value).length, nodes) };
    }
    throw new Error(`cannot encode JS value of type ${typeof value}`);
}

// a `count:u32le` then each node framed as `tag:u32le len:u32le payload`
function joinNodes(count: number, nodes: { tag: number, body: Uint8Array }[]): Uint8Array {
    const total = 4 + nodes.reduce((n, { body }) => n + 8 + body.length, 0);
    const out = new Uint8Array(total);
    const v = new DataView(out.buffer);
    v.setUint32(0, count, true);
    let pos = 4;
    for (const { tag, body } of nodes) {
        v.setUint32(pos, tag, true);
        v.setUint32(pos + 4, body.length, true);
        out.set(body, pos + 8);
        pos += 8 + body.length;
    }
    return out;
}
