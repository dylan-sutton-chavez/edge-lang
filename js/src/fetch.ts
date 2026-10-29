interface FetchCtx {
    baseUrl?: string | null
    // how the engine reaches a url where a room reads program files through its page
    read?: (url: string) => Promise<Response>
    knownMissing: Set<string>
}

// specs are root relative and the URL join clamps escapes at the origin
const requestUrl = (target: string, baseUrl?: string | null): string =>
    target.includes('://') ? target : new URL(target, baseUrl ?? self.location.href).toString();

/* a module whose pin the engine checks or null when the fetch or status fails */
export async function fetchModule(spec: string, ctx: FetchCtx): Promise<Uint8Array | null> {
    const { baseUrl, read = fetch, knownMissing } = ctx;

    // the `#sha256-` fragment stays in the verified spec but never reaches the request URL
    const fragAt = spec.indexOf('#sha256-');
    const target = fragAt === -1 ? spec : spec.slice(0, fragAt);

    let resp: Response;
    try {
        resp = await read(requestUrl(target, baseUrl));
    } catch (e) {
        // a manifest probe is opportunistic but a module that fails to fetch earns a warning
        if (spec.endsWith('edge.json')) knownMissing.add(spec);
        else if (!spec.endsWith('edge.lock')) console.warn(`[edge] fetch failed for '${spec}':`, e);
        return null;
    }

    if (!resp.ok) {
        if (resp.status === 404 && spec.endsWith('edge.json')) knownMissing.add(spec);
        // an absent lock warns elsewhere and is never remembered so writing one is enough
        else if (!spec.endsWith('edge.lock')) console.warn(`[edge] ${resp.status} for '${spec}' at ${resp.url}`);
        return null;
    }

    // a .wasm answered with HTML or text is a relative spec hitting an SPA fallback
    if (target.endsWith('.wasm')) {
        const ct = (resp.headers.get('content-type') || '').toLowerCase();
        if (ct.includes('html') || ct.startsWith('text/')) {
            console.warn(`[edge] '${spec}' served as '${ct || 'no content-type'}', not a wasm module`);
            return null;
        }
    }

    return new Uint8Array(await resp.arrayBuffer());
}
