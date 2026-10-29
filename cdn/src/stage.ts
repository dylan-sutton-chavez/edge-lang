import { createHash } from 'node:crypto'
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { REPO_DIR } from './tree'

const PARTS = ['wasm', 'js', 'cli'] as const
export type Part = (typeof PARTS)[number]

function need(path: string, hint: string) {
  if (!existsSync(path)) throw new Error(`${path} is missing, ${hint} first.`)
  return path
}

function copy(from: string, to: string) {
  mkdirSync(dirname(to), { recursive: true })
  cpSync(from, to, { recursive: true })
}

// Each part lays one build's outputs out in the CDN layout, the tree a deploy ships.
const STAGERS: Record<Part, (out: string) => void> = {
  wasm(out) {
    const target = join(REPO_DIR, 'target/wasm32-unknown-unknown')
    const wasm = need(join(target, 'release/edge.wasm'), 'run make wasm')
    copy(wasm, join(out, 'edge.wasm'))
    // hosts deployed before the rename still fetch the engine as compiler.wasm
    copy(wasm, join(out, 'compiler.wasm'))
    // The CLI embeds the speed build, later jobs read it from the staged tree and a deploy skips it.
    if (existsSync(join(target, 'cli/edge-cli.wasm'))) copy(join(target, 'cli/edge-cli.wasm'), join(out, '_build/edge-cli.wasm'))
  },
  js(out) {
    copy(need(join(REPO_DIR, 'js/dist'), 'run make js'), join(out, 'js/src'))
  },
  cli(out) {
    for (const script of ['install.sh', 'uninstall.sh']) copy(join(REPO_DIR, 'cli/setup', script), join(out, 'cli', script))
    for (const file of readdirSync(join(REPO_DIR, 'cli')).filter((name) => /^edge-.+\.tar\.gz$/.test(name))) {
      copy(join(REPO_DIR, 'cli', file), join(out, 'cli', file))
      // the installer checks the archive against this digest before it extracts anything
      const digest = createHash('sha256').update(readFileSync(join(REPO_DIR, 'cli', file))).digest('hex')
      writeFileSync(join(out, 'cli', `${file}.sha256`), `${digest}  ${file}\n`)
    }
  }
}

export function stage(out: string, parts: readonly Part[] = PARTS) {
  rmSync(out, { recursive: true, force: true })
  mkdirSync(out, { recursive: true })
  for (const part of parts) STAGERS[part](out)
  return out
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  const [out, ...parts] = process.argv.slice(2)
  if (!out) throw new Error('Pass the output directory and optional parts, for example "make stage PARTS=js".')

  const unknown = parts.find((part) => !(PARTS as readonly string[]).includes(part))
  if (unknown) throw new Error(`Unknown part "${unknown}", pick from ${PARTS.join(', ')}.`)

  stage(resolve(out), parts.length ? (parts as Part[]) : PARTS)
  console.log(`Staged ${parts.length ? parts.join(', ') : 'every part'} into ${resolve(out)}.`)
}
