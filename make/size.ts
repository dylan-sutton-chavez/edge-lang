import { brotliCompressSync, constants } from 'node:zlib'

const [path] = Deno.args
const bytes = Deno.readFileSync(path)
// quality 11 and a 24 bit window, the defaults of the brotli CLI
const brotli = brotliCompressSync(bytes, { params: { [constants.BROTLI_PARAM_QUALITY]: 11, [constants.BROTLI_PARAM_LGWIN]: 24 } })
console.log(`brotli-11: ${Math.floor(brotli.length / 1024)} KB (raw ${Math.floor(bytes.length / 1024)} KB)`)
