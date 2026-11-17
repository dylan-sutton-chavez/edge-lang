import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { convertV4MiniflareOptions, Miniflare } from 'miniflare'
import { CACHE, cdn_objects } from './tree'

// serves bucket objects like the public R2 domain, encoded only for clients that accept it
const WORKER = `
const CORS = { 'access-control-allow-origin': '*', 'access-control-allow-methods': 'GET, HEAD', 'access-control-max-age': '86400' }
export default {
  async fetch(request, env) {
    if (request.method === 'OPTIONS') return new Response(null, { status: 204, headers: CORS })
    const object = await env.CDN.get(decodeURIComponent(new URL(request.url).pathname.slice(1)))
    if (!object) return new Response('Not found', { status: 404, headers: CORS })
    const headers = new Headers(CORS)
    object.writeHttpMetadata(headers)
    headers.set('etag', object.httpEtag)
    const encoding = object.httpMetadata?.contentEncoding
    if (encoding && !(request.headers.get('accept-encoding') ?? '').includes(encoding)) headers.delete('content-encoding')
    return new Response(request.method === 'HEAD' ? null : object.body, { headers })
  }
}
`

const [tree, port = '8788'] = process.argv.slice(2)
if (!tree) throw new Error('Pass a staged tree, for example "make serve".')

const mf = new Miniflare(convertV4MiniflareOptions({ modules: true, script: WORKER, compatibilityDate: '2026-09-01', r2Buckets: ['CDN'], host: '127.0.0.1', port: Number(port) }))
// miniflare types the proxy loosely, only put is needed here
const bucket = (await mf.getR2Bucket('CDN')) as unknown as { put(key: string, value: Uint8Array, options: { httpMetadata: Record<string, string> }): Promise<unknown> }
const objects = cdn_objects(resolve(tree))

for (const object of objects) {
  const httpMetadata = { contentType: object.type, cacheControl: CACHE, ...(object.encoding ? { contentEncoding: object.encoding } : {}) }
  // stored plain, the runtime applies the stored encoding per response the way the edge negotiates it
  await bucket.put(object.key, readFileSync(object.file), { httpMetadata })
}

const url = await mf.ready
console.log(`Serving ${objects.length} object(s), export INTERPRETER_CDN_BASE=${url.origin}`)

process.on('SIGINT', () => mf.dispose().then(() => process.exit(0)))
process.on('SIGTERM', () => mf.dispose().then(() => process.exit(0)))
