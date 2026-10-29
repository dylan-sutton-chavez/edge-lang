import { copyFileSync, mkdirSync } from 'node:fs'
import { join, relative, resolve, sep } from 'node:path'

const [target] = Deno.args
const repo = resolve(import.meta.dirname!, '..')

// The container sees the repo at /repo, so every path the build reads moves there too.
function inside(path: string) {
  const rel = relative(repo, resolve(path))
  if (rel.startsWith('..')) throw new Error(`${path} is outside ${repo}, the container cannot read it.`)
  return '/repo/' + rel.split(sep).join('/')
}

const env: string[] = []
const wasm = Deno.env.get('EDGE_WASM')
if (wasm) env.push('-e', `EDGE_WASM=${inside(wasm)}`)

// The container writes as root, so a host with users gets the target dir back.
const uid = Deno.uid()
const owner = uid === null ? '' : ` && chown -R ${uid}:${Deno.gid()} target`
const docker = new Deno.Command('docker', { args: ['run', '--rm', '-v', `${repo}:/repo`, '-w', '/repo/cli', ...env, 'rust:alpine', 'sh', '-c', `sh musl.sh${owner}`] })
const { code } = await docker.spawn().status
if (code !== 0) Deno.exit(code)

const out = join(repo, 'cli/target', target, 'release')
mkdirSync(out, { recursive: true })
copyFileSync(join(repo, 'cli/target/release/edge'), join(out, 'edge'))
