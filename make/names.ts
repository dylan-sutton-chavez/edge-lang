const SECTIONS = ['any', 'code', 'string', 'allow'] as const
type Section = (typeof SECTIONS)[number]
type Deny = Exclude<Section, 'allow'>
type Kind = 'code' | 'string' | 'comment'
type Span = { kind: Kind; start: number; end: number }
type Hit = { at: number; text: string; section: Deny }

function fail(message: string): never {
  console.error(message)
  Deno.exit(1)
}

const [listPath, ...roots] = Deno.args
if (!listPath || roots.length === 0) fail('pass the list and the roots to scan')

const words: Record<Deny, string[]> = { any: [], code: [], string: [] }
const patterns: Record<Deny, RegExp[]> = { any: [], code: [], string: [] }
const allowed = new Map<string, Map<string, boolean>>()
let section: Section | undefined
for (const [n, raw] of Deno.readTextFileSync(listPath).split('\n').entries()) {
  const line = raw.trim()
  if (line === '' || line.startsWith('#')) continue
  const where = `${listPath}:${n + 1}`
  const head = line.match(/^\[(.*)\]$/)
  if (head) {
    if (!(SECTIONS as readonly string[]).includes(head[1])) fail(`${where} opens [${head[1]}], pick from ${SECTIONS.join(', ')}`)
    section = head[1] as Section
  } else if (!section) {
    fail(`${where} comes before the first section`)
  } else if (section === 'allow') {
    const entry = line.match(/^(\S+)((?: "(?:[^"\\]|\\.)*")+) \S/)
    if (!entry) fail(`${where} is not a file, its quoted texts and the reason they are allowed`)
    const texts = allowed.get(entry[1]) ?? new Map<string, boolean>()
    // a text is compared as written between its quotes with its escapes
    for (const [, text] of entry[2].matchAll(/"((?:[^"\\]|\\.)*)"/g)) texts.set(text, false)
    allowed.set(entry[1], texts)
  } else if (line.startsWith('/')) {
    const regex = line.match(/^\/(.+)\/([a-z]*)$/)
    if (!regex) fail(`${where} opens a pattern it never closes`)
    patterns[section].push(new RegExp(regex[1], `${regex[2]}g`))
  } else {
    words[section].push(...line.split(/\s+/))
  }
}

const IDENT = '[A-Za-z0-9_]'
const escape = (word: string) => word.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
const either = (list: string[]) => list.map(escape).join('|')
// an empty alternation would match everywhere so an empty list gives no rule
const rule = (list: string[], source: string) => (list.length ? [new RegExp(source, 'g')] : [])
const whole = (list: string[]) => rule(list, `(?<!${IDENT})(?:${either(list)})(?!${IDENT})`)
const ANY = [...whole(words.any), ...patterns.any]
const CODE = [...whole(words.code), ...patterns.code]
const STRING = [
  ...rule(words.string, `'(?:${either(words.string)})'`),
  ...rule(words.string, `(?<!${IDENT})(?:${either(words.string)})\\(\\)`),
  ...patterns.string
]
const EQUAL = new Set(words.string)

const RAW = /[bc]?r(#*)"/y
const CHAR = /'(?:\\u\{[0-9A-Fa-f]{1,6}\}|\\x[0-9A-Fa-f]{2}|\\.|[^\\'\n])'/uy

/* splits rust source into code and comments and the insides of string literals */
function spans(source: string): Span[] {
  const out: Span[] = []
  let code = 0
  let i = 0
  const take = (kind: Kind, open: number, start: number, end: number, resume: number) => {
    if (code < open) out.push({ kind: 'code', start: code, end: open })
    out.push({ kind, start, end })
    code = i = resume
  }
  while (i < source.length) {
    const c = source[i]
    RAW.lastIndex = i
    CHAR.lastIndex = i
    const raw = 'rbc'.includes(c) && !/[A-Za-z0-9_]/.test(source[i - 1] ?? '') ? RAW.exec(source) : null
    if (source.startsWith('//', i)) {
      const end = source.indexOf('\n', i)
      take('comment', i, i, end < 0 ? source.length : end, end < 0 ? source.length : end)
    } else if (source.startsWith('/*', i)) {
      // block comments nest in rust
      let depth = 1
      let j = i + 2
      while (depth > 0 && j < source.length) {
        const step = source.startsWith('/*', j) ? 1 : source.startsWith('*/', j) ? -1 : 0
        depth += step
        j += step === 0 ? 1 : 2
      }
      take('comment', i, i, j, j)
    } else if (raw) {
      const start = i + raw[0].length
      const close = source.indexOf(`"${raw[1]}`, start)
      const end = close < 0 ? source.length : close
      take('string', i, start, end, end + 1 + raw[1].length)
    } else if (c === '"') {
      let j = i + 1
      while (j < source.length && source[j] !== '"') j += source[j] === '\\' ? 2 : 1
      take('string', i, i + 1, j, j + 1)
    } else if (c === "'" && CHAR.exec(source)) {
      // a char literal holds no name and a lone quote opens a lifetime or label
      i = CHAR.lastIndex
    } else {
      i++
    }
  }
  if (code < source.length) out.push({ kind: 'code', start: code, end: source.length })
  return out
}

function scan(file: string, source: string): Hit[] {
  const hits: Hit[] = []
  const allows = allowed.get(file) ?? new Map<string, boolean>()
  for (const { kind, start, end } of spans(source)) {
    const text = source.slice(start, end)
    const found: Hit[] = []
    const look = (section: Deny, rules: RegExp[]) => {
      for (const regex of rules) for (const m of text.matchAll(regex)) found.push({ at: start + m.index, text: m[0], section })
    }
    look('any', ANY)
    if (kind === 'code') look('code', CODE)
    if (kind === 'string') {
      if (EQUAL.has(text)) found.push({ at: start, text: `"${text}"`, section: 'string' })
      look('string', STRING)
    }
    for (const hit of found) {
      // an allowed text spares a literal equal to it and any denied name it equals
      const spared = kind === 'string' && allows.has(text) ? text : allows.has(hit.text) ? hit.text : undefined
      if (spared === undefined) hits.push(hit)
      else allows.set(spared, true)
    }
  }
  return hits.sort((a, b) => a.at - b.at)
}

function* files(dir: string): Generator<string> {
  for (const entry of [...Deno.readDirSync(dir)].sort((a, b) => (a.name < b.name ? -1 : 1))) {
    const path = `${dir}/${entry.name}`
    if (entry.isDirectory) yield* files(path)
    else if (entry.name.endsWith('.rs')) yield path
  }
}

const report: string[] = []
for (const file of roots.flatMap((root) => [...files(root.replace(/\/+$/, ''))])) {
  const source = Deno.readTextFileSync(file)
  const hits = scan(file, source)
  if (hits.length === 0) continue
  report.push(`${file} holds ${hits.length}`)
  let line = 1
  let from = 0
  for (const hit of hits) {
    for (; from < hit.at; from++) if (source[from] === '\n') line++
    report.push(`  ${file}:${line} ${hit.text} [${hit.section}]`)
  }
}
for (const [file, texts] of allowed) for (const [text, used] of texts) if (!used) report.push(`${listPath} allows "${text}" in ${file}, where it spares no denied name`)
if (report.length) fail(`no scanned file may hold a name ${listPath} denies and these do\n${report.join('\n')}`)
