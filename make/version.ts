function fail(message: string): never {
  console.error(message)
  Deno.exit(1)
}

const name = Deno.args[0] ?? ''
const tag = name.replace(/^v/, '')
// An interpreter floor names engine releases, so a release follows the rule every interpreter.json version does.
if (!/^(0|[1-9][0-9]?)\.(0|[1-9][0-9]?)\.(0|[1-9][0-9]?)$/.test(tag)) fail(`tag ${name} is not major.minor.patch with each part 0 to 99 and no leading zeros`)

const version = (manifest: string) => Deno.readTextFileSync(manifest).match(/^version = "([^"]*)"/m)?.[1]
const root = version('Cargo.toml')
const cli = version('cli/Cargo.toml')
if (root !== tag || cli !== tag) fail(`tag ${name} ships ${tag}, Cargo.toml says ${root} and cli/Cargo.toml says ${cli}`)
