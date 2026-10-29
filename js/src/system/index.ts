import type { EdgeValue } from '../rt.ts';
import type { EdgeExports } from '../wasm.ts';

/* the calls a package opened of a host module and how a run closes them */
export interface Opened {
    calls: Record<string, (...args: EdgeValue[]) => unknown>
    close(): void
}

/* what a package holds of a host module where null scopes bound no call */
export interface Held {
    scopes: string[] | null
    // throws for a scope the package does not hold naming what it holds instead
    need(scope: string): void
}

/* what a trace shows of a call beside its name and how it ended */
export interface Shown {
    scope?: string
    id?: number
    status?: number
    bytes?: number
}

/* a module the host serves under a name no import may shadow */
export interface HostModule {
    name: string
    // whether a grant may give the module this scope after its name and a colon
    scope(scope: string): boolean
    // the scope a call reaches which the host checks first or null for none
    reaches(call: string, args: EdgeValue[]): string | null
    // whether a granted scope covers what a call reaches where none means equal text
    covers?(scope: string, reached: string): boolean
    // what a refusal names as asked for a reached scope no held one covers
    asked?(held: string[], reached: string): string
    open(pkg: string, held: Held): Opened
    // whether what a package holds lets it write `path` through a file channel
    writes?(held: Held, path: string): boolean
    // the origins a page lets its room reach for a scope the grants give
    origins?(scope: string): string[]
    // what a trace may show of a settled call but never a body or secret
    show?(call: string, args: EdgeValue[], value: unknown): Shown
}

/* every section from root to package with the key the next one comes under */
export type Chain = [unknown, string][];

/* what each package of a run holds of each host module the composition wired */
export interface Access {
    // the manifest field the walk carries per chain link
    field: string
    // the holder names the field gives meaning so no import may take them
    holders: readonly string[]
    // why the root section cannot be served or null when its modules take every entry
    check(e: EdgeExports, section: unknown, modules: readonly HostModule[]): string | null
    // what `chain` gives `pkg` of `module` or the text its import fails with
    held(e: EdgeExports, chain: Chain, pkg: string, module: string): Held | string
    // each module and scope a root section grants which bounds what a room reaches
    granted(section: unknown): [string, string][]
}

/* the host modules a bundle serves and the grants it holds every package to */
export interface Composition {
    modules: readonly HostModule[]
    // without one every package holds each module unbounded and a room reaches only its imports
    access?: Access | null
}
