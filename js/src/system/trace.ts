import type { EdgeValue } from '../rt.ts';

/* One thing a run did beyond its own memory, timed from the start of the run, never a secret value, a body or a query. */
export type TraceEvent =
    | { kind: 'run', at: number, epoch: number }
    | { kind: 'call', at: number, ms: number, pkg: string, call: string, scope: string, outcome: string, id?: number, status?: number, bytes?: number }
    | { kind: 'print', at: number, text: string };

// What a print carries into the trace, its line in full stays in the output.
const PRINT = 120;

const idOf = (value: unknown) => (typeof value === 'number' ? value : typeof value === 'bigint' ? Number(value) : undefined);

/* The host and path a url reaches, the query and the fragment left out since either may carry a key. */
function place(url: unknown): string {
    try {
        const parsed = new URL(String(url));
        return `${parsed.host}${parsed.pathname}`;
    } catch {
        return '';
    }
}

// A secret this short shows nothing of itself, a longer one its first two characters.
const SHOWN = 8;

/* Hides each value the host keeps for the program, as written, as a url path and as the place a url reaches. */
export function masker(secrets: Record<string, string> | null): (text: string) => string {
    const forms = Object.values(secrets ?? {}).filter(Boolean).flatMap((value) => [value, encodeURI(value), place(value)].filter(Boolean).map((form) => [form, value.length < SHOWN ? '…' : `${form.slice(0, 2)}…`] as const));
    // The longest first, so a url is hidden whole before a shorter secret inside it.
    forms.sort((a, b) => b[0].length - a[0].length);
    return (text) => forms.reduce((out, [form, shown]) => out.replaceAll(form, shown), text);
}

/* What a call reached and what came back about it, the name of a secret but never what it holds. */
function reached(module: string, name: string, args: EdgeValue[], value: unknown) {
    if (name === 'batch') return { scope: Array.isArray(args[0]) ? `${args[0].length} calls` : '' };
    if (module === 'secret') return { scope: typeof args[0] === 'string' ? args[0] : '' };
    if (module === 'fs') return { scope: typeof args[0] === 'string' ? args[0] : '.' };
    if (module === 'time') return { scope: name === 'zone' ? 'zone' : String(args[0] ?? 'wall') };
    if (name === 'request') return { scope: `${String(args[0])} ${place(args[1])}`, id: idOf(value) };
    if (name === 'connect') return { scope: place(args[0]), id: idOf(value) };
    const id = idOf(args[0]);
    if (name === 'response' && Array.isArray(value)) return { scope: '', id, status: Number(value[0]) };
    if (name === 'read') return { scope: '', id, bytes: value == null ? 0 : (value as { length: number }).length };
    return { scope: '', id };
}

/* A system call that reports itself to `emit` once it settles, and costs one check when nobody listens. */
export function traced(pkg: string, module: string, name: string, call: (...args: EdgeValue[]) => unknown, emit: () => ((event: TraceEvent) => void) | null, clock: () => number) {
    return (...args: EdgeValue[]): unknown => {
        const sink = emit();
        if (!sink) return call(...args);
        const at = clock();
        const done = (outcome: string, value?: unknown) => sink({ kind: 'call', at, ms: clock() - at, pkg, call: `${module}.${name}`, outcome, ...reached(module, name, args, value) });
        let result: unknown;
        try {
            result = call(...args);
        } catch (e) {
            done((e as Error)?.name ?? 'Error');
            throw e;
        }
        if (result instanceof Promise) result.then((value) => done('ok', value), (e) => done((e as Error)?.name ?? 'Error'));
        else done('ok', result);
        return result;
    };
}

/* A print as the trace holds it, cut short since the output keeps every line. */
export const printed = (at: number, text: string): TraceEvent => ({ kind: 'print', at, text: text.length > PRINT ? `${text.slice(0, PRINT)}…` : text });
