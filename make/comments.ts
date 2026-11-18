function fail(message: string): never {
  console.error(message)
  Deno.exit(1)
}

// the files git tracks or would track with the trees the repo does not write itself left out
const SKIPPED = [/^examples\/lox\/tests\/suite\//, /^bench\/cases\/book\//, /(^|\/)node_modules\//, /\.lock$/]
const SLASH = /\.(rs|ts|js|lox)$/
const HASH = /(\.(toml|ya?ml|sh)|(^|\/)(Makefile|Dockerfile|\.gitignore|\.dockerignore))$/
// the help text clap reads from a field's doc comment and the safety section clippy asks for
const DOCS = [/^src\/cli\//]
const DIRECTIVE = /^(deno-lint-ignore|deno-fmt-ignore|@ts-|eslint|shellcheck|:tombi|rustfmt::)/
// a word in capitals opens a comment as SAFETY or as an acronym such as AFL
const OPENER = /^([a-z0-9`'"$(\[{<@*_.-]|[A-Z0-9]{2,}\b|[A-Z][a-z]*[A-Z])/

const out = new Deno.Command('git', { args: ['ls-files', '--cached', '--others', '--exclude-standard'], stdout: 'piped' }).outputSync()
const files = new TextDecoder().decode(out.stdout).split('\n').filter((f) => f && !SKIPPED.some((s) => s.test(f)) && (SLASH.test(f) || HASH.test(f)))

type Line = { text: string; doc: boolean; open: boolean }

/* the comment a whole line holds with its marker taken off or none */
function comment(raw: string, slash: boolean, first: boolean): Line | undefined {
  const line = raw.trim()
  if (slash) {
    const doc = /^\/\/[/!]/.test(line)
    if (line.startsWith('//')) return { text: line.replace(/^\/\/[/!]?/, '').trim(), doc, open: false }
    if (line.startsWith('/*')) return { text: line.replace(/^\/\*+/, '').replace(/\*\/$/, '').trim(), doc: line.startsWith('/**'), open: !line.endsWith('*/') }
    return undefined
  }
  if (!line.startsWith('#') || (first && line.startsWith('#!'))) return undefined
  return { text: line.replace(/^#+/, '').trim(), doc: false, open: false }
}

const report: string[] = []
for (const file of files) {
  let source: string
  try {
    source = Deno.readTextFileSync(file)
  } catch {
    continue
  }
  const slash = SLASH.test(file)
  const lines = source.split('\n')
  let previous = false
  for (const [n, raw] of lines.entries()) {
    const at = `${file}:${n + 1}`
    const c = comment(raw, slash, n === 0)
    if (!c || DIRECTIVE.test(c.text)) {
      previous = false
      continue
    }
    // what a backtick or a link holds is code and not prose
    const prose = c.text.replace(/`[^`]*`/g, '').replace(/[a-z][a-z0-9+.-]*:\/\/\S+/g, '')
    if (previous) report.push(`${at} follows another comment, a block takes one`)
    if (c.open) report.push(`${at} runs past its line`)
    if (c.doc) {
      // a doc comment where one is due is the text a tool shows and not a comment
      if (!(DOCS.some((d) => d.test(file)) || c.text === '# Safety')) report.push(`${at} is a doc comment`)
      previous = false
      continue
    }
    if (n === 0 && (lines[1] ?? '').trim() === '') report.push(`${at} heads the file`)
    if (c.text && !OPENER.test(c.text)) report.push(`${at} opens with a capital`)
    if (/[:;–—]/.test(prose)) report.push(`${at} holds a colon, a semicolon or a dash`)
    if (/[^\t -~]/.test(prose)) report.push(`${at} holds a character past ascii`)
    previous = true
  }
}
if (report.length) fail(`every comment is one lowercase line with no colon, semicolon, dash or icon and these are not\n${report.join('\n')}`)
