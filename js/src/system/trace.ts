import type { EdgeValue } from '../rt.ts';
import type { HostModule, Shown } from './index.ts';

/* something a run did beyond its memory without any body or query */
export type TraceEvent =
    | { kind: 'run', at: number, epoch: number }
    | { kind: 'call', at: number, ms: number, pkg: string, call: string, scope: string, outcome: string, id?: number, status?: number, bytes?: number }
    | { kind: 'print', at: number, text: string };

// how much of a print the trace keeps since the output holds the full line
const PRINT = 120;

/* reports a call to `emit` once it settles and costs one check when nobody listens */
export function traced(pkg: string, module: HostModule, name: string, call: (...args: EdgeValue[]) => unknown, emit: () => ((event: TraceEvent) => void) | null, clock: () => number) {
    // only the module knows what is safe to show and its fault fails no call
    const shown = (args: EdgeValue[], value: unknown): Shown => {
        try { return module.show?.(name, args, value) ?? {}; } catch { return {}; }
    };
    return (...args: EdgeValue[]): unknown => {
        const sink = emit();
        if (!sink) return call(...args);
        const at = clock();
        const done = (outcome: string, value?: unknown) => sink({ kind: 'call', at, ms: clock() - at, pkg, call: `${module.name}.${name}`, scope: '', outcome, ...shown(args, value) });
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

export const printed = (at: number, text: string): TraceEvent => ({ kind: 'print', at, text: text.length > PRINT ? `${text.slice(0, PRINT)}…` : text });
