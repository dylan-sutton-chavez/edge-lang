import { standsIn } from './protocol.ts';
import type { Composition } from './system/index.ts';
import { roomScript } from './room.ts';

// a policy names only plain origins and sources and anything else stays out of reach
const PLAIN_ORIGIN = /^https?:\/\/[a-z0-9.-]+(:\d+)?$/;
const PLAIN_SOURCE = /^(?:https?|wss?):\/\/[a-z0-9.-]+(?::(?:\d{1,5}|\*))?$/;

/* the origin of an absolute url when a policy can name it as written */
function originOf(spec: string): string[] {
    try {
        const { origin } = new URL(spec);
        return PLAIN_ORIGIN.test(origin) ? [origin] : [];
    } catch {
        return [];
    }
}

/* what a room may fetch and the root edge.json the page gives or finds */
export async function rootOf(given: { imports?: Record<string, string> | null, permissions?: unknown }, base: string | null): Promise<{ specs: string[], manifest: Record<string, unknown> }> {
    if (standsIn(given) || !base) return { specs: Object.values(given.imports ?? {}), manifest: {} };
    const json = (name: string): Promise<Record<string, unknown>> =>
        fetch(new URL(name, base), { credentials: 'omit' }).then((r) => (r.ok ? r.json() : {})).catch(() => ({}));
    const manifest = await json('edge.json');
    const specs = Object.values((manifest['imports'] ?? {}) as Record<string, unknown>).map(String);
    // the lock names where each release lives so the room may reach it
    const locked = Object.values(await json('edge.lock')).map((entry) => String((entry as { url?: unknown } | null)?.url ?? ''));
    return { specs: [...specs, ...locked], manifest };
}

/* the room runs its script and the engine and connects only to imports and grants */
export async function roomPolicy(specs: string[], section: unknown, { modules, access }: Composition): Promise<string> {
    const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(roomScript)));
    // only a grant component bounds what a module reaches so without one modules add nothing
    const reach = (access?.granted(section) ?? []).flatMap(([name, scope]) => {
        const module = modules.find((m) => m.name === name);
        return module?.origins && module.scope(scope) ? module.origins(scope).filter((source) => PLAIN_SOURCE.test(source)) : [];
    });
    const sources = [...new Set([...specs.flatMap(originOf), ...reach])];
    return [
        "default-src 'none'",
        `script-src 'sha256-${btoa(String.fromCharCode(...digest))}' 'wasm-unsafe-eval'`,
        'worker-src blob:',
        `connect-src ${sources.length > 0 ? sources.join(' ') : "'none'"}`,
    ].join('; ');
}
