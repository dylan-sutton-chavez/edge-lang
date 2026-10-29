import { readdirSync } from 'node:fs'
import { extname, join, relative, sep } from 'node:path'
import { fileURLToPath } from 'node:url'

// The checkout the builds come out of, two folders above this file.
export const REPO_DIR = fileURLToPath(new URL('../../', import.meta.url))

const TYPES: Record<string, string> = {
  '.wasm': 'application/wasm',
  '.js': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.py': 'text/x-python; charset=utf-8',
  '.sh': 'text/x-shellscript; charset=utf-8',
  '.gz': 'application/gzip',
  '.ts': 'text/plain; charset=utf-8'
}

// Paths carry no version, so every object revalidates against its ETag.
export const CACHE = 'public, max-age=0, must-revalidate'

// edge.wasm and its compiler.wasm copy are the large payloads stored brotli encoded
const encoded = (key: string) => key === 'edge.wasm' || key === 'compiler.wasm'

export type CdnObject = { key: string; file: string; type: string; encoding: string | null }

/* Every file of a staged tree as the CDN stores it, keyed by its path and typed by its extension. */
export function cdn_objects(tree: string): CdnObject[] {
  const walk = (dir: string): string[] =>
    readdirSync(dir, { withFileTypes: true }).flatMap((entry) => (entry.isDirectory() ? walk(join(dir, entry.name)) : [join(dir, entry.name)]))

  return walk(tree)
    .map((file) => {
      const key = relative(tree, file).split(sep).join('/')
      return { key, file, type: TYPES[extname(key)] ?? 'application/octet-stream', encoding: encoded(key) ? 'br' : null }
    })
    .sort((a, b) => a.key.localeCompare(b.key))
}
