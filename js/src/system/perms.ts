import { denied } from './error.ts';
import type { Access, Held, HostModule } from './index.ts';
import type { EdgeExports } from '../wasm.ts';
import { writeBytes } from '../util.ts';

/* each holder in an edge.json section mapped to the entries it is granted */
export type Permissions = Record<string, string[]>;

/* every section from root to package with the key the next one comes under */
export type Chain = [Permissions, string][];

const TE = new TextEncoder();
const TD = new TextDecoder();

/* stages `texts` as pointer and length pairs for a grant export and reads its answer */
function call(e: EdgeExports, texts: string[], run: (at: number[]) => number): string {
    const bytes = texts.map((text) => TE.encode(text));
    const at = bytes.map((b) => writeBytes(e, b));
    let len: number;
    try {
        len = run(at.flatMap((ptr, i) => [ptr, bytes[i]!.length]));
    } finally {
        at.forEach((ptr, i) => e.wasm_free(ptr, Math.max(1, bytes[i]!.length)));
    }
    return TD.decode(new Uint8Array(e.memory.buffer, e.out_ptr(), len));
}

/* the grant exports of the runtime or a refusal naming the build that lacks them */
function exported(e: EdgeExports) {
    const { grants_check: check, grants_scopes: scopes, grants_held: held } = e;
    if (!check || !scopes || !held) throw new Error('this edge.wasm exports no grant logic, build its runtime with the perms feature');
    return { check, scopes, held };
}

/* why a permissions section is malformed or null when its modules take every entry */
export function check(e: EdgeExports, section: unknown, modules: readonly Pick<HostModule, 'name' | 'scope'>[]): string | null {
    if (section === undefined) return null;
    const grants = exported(e);
    const text = JSON.stringify(section);
    // the runtime lists each scoped entry and only its module can judge the scope
    const asked = JSON.parse(call(e, [text], ([s, n]) => grants.scopes(s!, n!))) as [string, string][];
    const taken = asked.filter(([name, scope]) => modules.some((m) => m.name === name && m.scope(scope))).map(([name, scope]) => `${name}:${scope}`);
    const names = JSON.stringify(modules.map((m) => m.name));
    return call(e, [text, names, JSON.stringify(taken)], ([s, sn, m, mn, t, tn]) => grants.check(s!, sn!, m!, mn!, t!, tn!)) || null;
}

/* what `chain` gives `pkg` of `module` or the text its import fails with */
export function held(e: EdgeExports, chain: Chain, pkg: string, module: string): Held | string {
    const grants = exported(e);
    const got = JSON.parse(call(e, [JSON.stringify(chain), pkg, module], ([c, cn, p, pn, m, mn]) => grants.held(c!, cn!, p!, pn!, m!, mn!))) as { refused: string } | { scopes: string[], refusal: [string, string] };
    if ('refused' in got) return got.refused;
    const { scopes, refusal: [before, after] } = got;
    return { scopes, need: (scope) => { if (!scopes.includes(scope)) throw denied(`${before}${scope}${after}`); } };
}

/* each module and scope the holders of a section list in their entries */
export function granted(section: unknown): [string, string][] {
    const holders = section !== null && typeof section === 'object' && !Array.isArray(section) ? Object.values(section) : [];
    return holders.flatMap((entries) => (Array.isArray(entries) ? entries : [])).flatMap((entry): [string, string][] => {
        const at = typeof entry === 'string' ? entry.indexOf(':') : -1;
        return at > 0 ? [[entry.slice(0, at), entry.slice(at + 1)]] : [];
    });
}

/* the grants each edge.json writes held from the root down as the opt-in perms component */
export default { field: 'permissions', holders: ['all', 'main', 'eval'], check, held, granted } satisfies Access;
