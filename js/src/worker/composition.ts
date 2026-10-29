import net from '../../../examples/net/net.ts';
import perms from '../system/perms.ts';
import type { Composition } from '../system/index.ts';

/* the JS host's own composition serving net under the grants of perms */
export default { modules: [net], access: perms } satisfies Composition;
