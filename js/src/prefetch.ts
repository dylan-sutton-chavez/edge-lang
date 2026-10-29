import { fetchModule } from './fetch.ts';
import { loadNativeModule, nativeTable } from './native.ts';
import type { EdgeExports } from './wasm.ts';
import type { Chain } from './system/index.ts';
import { errMsg, writeBytes } from './util.ts';

const TD = new TextDecoder();
const TE = new TextEncoder();

interface PrefetchCtx {
    fetchedSources: Map<string, Uint8Array>
    knownMissing: Set<string>
    // the root edge.json the embedder declared in place of the one at its base
    given?: Record<string, unknown> | null
    // the carried field then the holders it reserves one per line
    carry: string
    // the host module names no import may take
    reserved: string[]
    baseUrl?: string | null
    read?: (url: string) => Promise<Response>
}

/* every manifest dir of a run with its package and chain plus the root section */
export interface Packages {
    dirs: [string, string, Chain][]
    root: string
    // whether anything reaches a host module or an undeclared name or a plugin
    needed: boolean
    [field: string]: unknown
}

// what the engine walk asks of this host next as written to the out buffer
type Step = { fetch: string } | { plugin: string } | { system: Packages } | { undeclared: string[] } | { done: string[] };

/* a hint for a spec whose scheme is insecure or missing and null otherwise */
function schemeHint(spec: string): string | null {
    if (spec.startsWith('http://')) {
        return `'${spec}' uses http://; browsers block http subresources from an https page `
             + `(mixed content), so the fetch never leaves. Use https:// (an SSL connection).`;
    }
    // a schemeless dotted first segment looks like a domain but resolves as a path
    const relative = spec.startsWith('.') || spec.startsWith('/') || spec.includes('://');
    const firstSegment = spec.split('/')[0];
    if (!relative && firstSegment !== undefined && firstSegment.includes('.')) {
        return `'${spec}' has no scheme, so it resolved as a path on your own origin. `
             + `If it's a URL, prefix it with https://.`;
    }
    return null;
}

/* the engine walks the imports and this host answers each step it asks for */
export async function bfsPrefetch(rootSrc: string, exports: EdgeExports, ctx: PrefetchCtx, serve: (packages: Packages) => string[]): Promise<void> {
    const { fetchedSources, knownMissing, given } = ctx;

    // what the embedder declared stands in for the root edge.json like any manifest
    if (given) {
        fetchedSources.set('edge.json', TE.encode(JSON.stringify(given)));
        knownMissing.delete('edge.json');
    }

    // a walk export takes staged buffers and leaves the next step in the out buffer
    const call = (buffers: Uint8Array[], run: (at: number[]) => number): Step => {
        const at = buffers.map((bytes) => writeBytes(exports, bytes));
        let len: number;
        try { len = run(at); } finally { buffers.forEach((bytes, i) => exports.wasm_free(at[i]!, Math.max(1, bytes.length))); }
        return JSON.parse(TD.decode(new Uint8Array(exports.memory.buffer, exports.out_ptr(), len))) as Step;
    };

    // a runtime before the carry export reads the grant field it was built with
    if (exports.walk_carry) {
        const carry = TE.encode(ctx.carry);
        const at = writeBytes(exports, carry);
        exports.walk_carry(at, carry.length);
        exports.wasm_free(at, Math.max(1, carry.length));
    }
    const [src, system] = [TE.encode(rootSrc), TE.encode(ctx.reserved.join('\n'))];
    let step = call([src, system], ([s, m]) => exports.walk_start(s!, src.length, m!, system.length));

    for (;;) {
        if ('fetch' in step) {
            const spec = step.fetch;
            let bytes = fetchedSources.get(spec);
            if (bytes === undefined && !knownMissing.has(spec)) {
                bytes = (await fetchModule(spec, ctx)) ?? undefined;
                if (bytes) fetchedSources.set(spec, bytes);
            }
            // a manifest or lock is only probed so only a missing module earns a hint
            const probe = spec.endsWith('edge.json') || spec.endsWith('edge.lock');
            const [answer, kind] = bytes ? [bytes, 0] : [TE.encode(probe ? '' : schemeHint(spec) ?? ''), 1];
            step = call([answer], ([at]) => exports.walk_fetched(at!, answer.length, kind));
        } else if ('plugin' in step) {
            const len = exports.walk_plugin_bytes();
            const bytes = new Uint8Array(exports.memory.buffer, exports.out_ptr(), len).slice();
            // a plugin that does not load fails at its import as a refused one does
            let refused = '';
            try {
                const { names, fns } = await loadNativeModule(bytes, exports);
                const baseId = nativeTable.length;
                for (const fn of fns) nativeTable.push(fn);
                const [spec, listed] = [TE.encode(step.plugin), TE.encode(names.join('\n'))];
                const [s, n] = [writeBytes(exports, spec), writeBytes(exports, listed)];
                exports.register_native_module(s, spec.length, n, listed.length, baseId);
                exports.wasm_free(s, Math.max(1, spec.length));
                exports.wasm_free(n, Math.max(1, listed.length));
            } catch (e) {
                refused = `is not a valid plugin, ${errMsg(e)}`;
            }
            const why = TE.encode(refused);
            step = call([why], ([at]) => exports.walk_plugin(refused ? 2 : 0, at!, why.length));
        } else if ('system' in step) {
            const failures = TE.encode(serve(step.system).join('\0'));
            step = call([failures], ([at]) => exports.walk_served(at!, failures.length));
        } else if ('undeclared' in step) {
            // a page has no `edge add` to suggest so undeclared names keep the generic help
            const none = new Uint8Array();
            step = call([none], ([at]) => exports.walk_known(at!, 0));
        } else {
            if (step.done.length) throw new Error(`could not pre-fetch every imported module:\n  ${step.done.join('\n  ')}`);
            return;
        }
    }
}
