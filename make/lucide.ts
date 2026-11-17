const REGISTRY = 'https://registry.npmjs.org/lucide-static/latest'

/* the first of `urls` to answer, each tried three times with a growing pause, so one CDN outage never fails a build */
async function json(...urls: string[]) {
  let failure: unknown
  for (let attempt = 1; attempt <= 3; attempt++) {
    for (const url of urls) {
      try {
        const response = await fetch(url)
        if (response.ok) return response.json()
        await response.body?.cancel()
        failure = new Error(`Fetching ${url} answered ${response.status}.`)
      } catch (error) {
        failure = error
      }
    }
    await new Promise((done) => setTimeout(done, attempt * 2000))
  }
  throw failure
}

// a release named on the command line, or the latest one Lucide published, so every build is fresh
const version: string = Deno.args[0] || (await json(REGISTRY)).version
const tags: Record<string, string[]> = await json(...['https://cdn.jsdelivr.net/npm', 'https://unpkg.com'].map((cdn) => `${cdn}/lucide-static@${version}/tags.json`))
const icons = Object.keys(tags).sort()
Deno.mkdirSync('target', { recursive: true })
Deno.writeTextFileSync('target/lucide.json', JSON.stringify({ version, icons }))
console.log(`Wrote ${icons.length} icons of Lucide ${version}.`)
