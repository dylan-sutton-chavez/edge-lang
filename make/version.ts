function fail(message: string): never {
  console.error(message)
  Deno.exit(1)
}

const name = Deno.args[0] ?? ''
const tag = name.replace(/^v/, '')
// an interpreter floor names engine releases, so a release follows the rule every interpreter.json version does
if (!/^(0|[1-9][0-9]?)\.(0|[1-9][0-9]?)\.(0|[1-9][0-9]?)$/.test(tag)) fail(`tag ${name} is not major.minor.patch with each part 0 to 99 and no leading zeros`)

// the engine and the cli and every workspace crate share the one version in Cargo.toml
const version = Deno.readTextFileSync('Cargo.toml').match(/^version = "([^"]*)"/m)?.[1]
if (version !== tag) fail(`tag ${name} ships ${tag} and Cargo.toml says ${version}`)
