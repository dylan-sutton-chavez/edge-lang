import type { EdgeValue } from '../rt.ts';
import type { Held, HostModule, Opened } from './index.ts';

/* a call checked against what its package holds before the module sees it */
export function gated(module: HostModule, held: Held, name: string, call: (...args: EdgeValue[]) => unknown) {
    return (...args: EdgeValue[]) => {
        const reached = module.reaches(name, args);
        const covers = module.covers ?? ((scope: string, at: string) => scope === at);
        if (reached !== null && held.scopes !== null && !held.scopes.some((scope) => covers(scope, reached))) {
            held.need(module.asked?.(held.scopes, reached) ?? reached);
        }
        return call(...args);
    };
}

/* the calls a package opened each checked against its grant as the host serves them */
export function opened(module: HostModule, pkg: string, held: Held): Opened {
    const served = module.open(pkg, held);
    const calls = Object.fromEntries(Object.entries(served.calls).map(([name, call]) => [name, gated(module, held, name, call)]));
    return { calls, close: () => served.close() };
}
