const bytes = Deno.readFileSync(Deno.args[0])

// hosts bind these by name so changing one breaks the js host and the cli
const EXPECTED = `
export EDGE_RUNTIME global i32 const
export bundle_index func (i32 i32) -> (i32)
export grants_check func (i32 i32 i32 i32 i32 i32) -> (i32)
export grants_held func (i32 i32 i32 i32 i32 i32) -> (i32)
export grants_scopes func (i32 i32) -> (i32)
export host_edge_decode func (i32 i32 i32 i32) -> (i32)
export host_edge_encode func (i32 i32 i32) -> (i32)
export host_edge_op func (i32 i32 i32 i32 i32 i32 i32) -> (i32)
export host_edge_release func (i32) -> ()
export host_edge_take_error func (i32 i32 i32) -> (i32)
export host_edge_throw func (i32 i32 i32) -> ()
export manifest_check func (i32 i32 i32 i32 i32 i32) -> (i32)
export manifest_fields func (i32 i32) -> (i32)
export memory memory
export memory_peak func () -> (i64)
export out_len func () -> (i32)
export out_ptr func () -> (i32)
export register_module_error func (i32 i32 i32 i32) -> ()
export register_native_module func (i32 i32 i32 i32 i32) -> ()
export repl_eval func (i32 i32) -> (i32)
export reset_modules func () -> ()
export restore_state func (i32 i32) -> (i32)
export run_push_event func (i32 i32) -> (i32)
export run_resume func () -> (i32)
export run_start func (i32 i32) -> (i32)
export runtime_descriptor func () -> (i32)
export save_state func () -> (i64)
export send_package func () -> (i32)
export set_entry func (i32 i32) -> ()
export set_host_error_by_id func (i32 i32 i32) -> (i32)
export set_host_result_by_id func (i32 i32) -> (i32)
export set_input func (i32 i32) -> ()
export set_limits func (i64 i64) -> ()
export set_preempt_interval func (i32) -> ()
export state_globals func () -> (i32)
export state_stack func () -> (i32)
export vm_create func () -> (i32)
export vm_drop func (i32) -> (i32)
export vm_select func (i32) -> (i32)
export walk_carry func (i32 i32) -> ()
export walk_fetched func (i32 i32 i32) -> (i32)
export walk_known func (i32 i32) -> (i32)
export walk_plugin func (i32 i32 i32) -> (i32)
export walk_plugin_bytes func () -> (i32)
export walk_served func (i32 i32) -> (i32)
export walk_start func (i32 i32 i32 i32) -> (i32)
export wasm_alloc func (i32) -> (i32)
export wasm_free func (i32 i32) -> ()
import env.host_call_native func (i32 i32 i32 i32 i32) -> (i32)
import env.host_fetch_bytes func (i32 i32 i32 i32) -> (i32)
import env.host_print func (i32 i32) -> ()
import env.host_send func (i32 i32 i32 i32) -> (i32)
`.trim()

const VALUES: Record<number, string> = { 0x7f: 'i32', 0x7e: 'i64', 0x7d: 'f32', 0x7c: 'f64', 0x7b: 'v128', 0x70: 'funcref', 0x6f: 'externref' }
const KINDS = ['func', 'table', 'memory', 'global', 'tag']
let at = 8
const byte = () => bytes[at++]
// signed and unsigned leb128 end alike so one reader skips both
const leb = () => {
  let value = 0
  let scale = 1
  let b: number
  do {
    b = byte()
    value += (b & 0x7f) * scale
    scale *= 128
  } while (b & 0x80)
  return value
}
const name = () => {
  const n = leb()
  at += n
  return new TextDecoder().decode(bytes.subarray(at - n, at))
}
const each = (item: () => void) => {
  for (let n = leb(); n > 0; n--) item()
}
const value = () => VALUES[byte()] ?? 'unknown'
const list = () => {
  const values: string[] = []
  each(() => values.push(value()))
  return `(${values.join(' ')})`
}
const limits = () => {
  const flags = byte()
  leb()
  if (flags & 1) leb()
}
const types: string[] = []
const funcs: string[] = []
const globals: string[] = []
const lines: string[] = []
const global = () => globals.push(`${value()} ${byte() ? 'mut' : 'const'}`)

while (at < bytes.length) {
  const id = byte()
  const end = leb() + at
  if (id === 1) {
    each(() => {
      byte()
      const params = list()
      types.push(`${params} -> ${list()}`)
    })
  }
  if (id === 2) {
    each(() => {
      const where = `${name()}.${name()}`
      const kind = byte()
      if (kind === 0) funcs.push(types[leb()])
      if (kind === 1) byte()
      if (kind === 1 || kind === 2) limits()
      if (kind === 3) global()
      lines.push(`import ${where} ${KINDS[kind]}${kind === 0 ? ` ${funcs.at(-1)}` : ''}`)
    })
  }
  if (id === 3) each(() => funcs.push(types[leb()]))
  // an initializer is one constant or global.get and its end as rustc writes it
  if (id === 6) {
    each(() => {
      global()
      byte()
      leb()
      byte()
    })
  }
  if (id === 7) {
    each(() => {
      const what = name()
      const kind = byte()
      const index = leb()
      const type = kind === 0 ? ` ${funcs[index]}` : kind === 3 ? ` ${globals[index]}` : ''
      lines.push(`export ${what} ${KINDS[kind]}${type}`)
    })
  }
  at = end
}

const found = lines.sort().join('\n')
if (found !== EXPECTED) {
  console.error(`edge.wasm no longer offers its hosts what they bind\n\nexpected\n${EXPECTED}\n\nfound\n${found}`)
  Deno.exit(1)
}

// plugins compile these values in so each stays as it was added
const CONSTANTS = `
EDGE_ABI_VERSION: u32 = 1
MAX_WIRE_DEPTH: u32 = 32
TAG_INVALID: u32 = u32::MAX
error_kind::ATTRIBUTE: u32 = 3
error_kind::CUSTOM: u32 = 6
error_kind::INDEX: u32 = 4
error_kind::KEY: u32 = 5
error_kind::RUNTIME: u32 = 2
error_kind::TYPE: u32 = 0
error_kind::VALUE: u32 = 1
op::CALL: u32 = 0
op::CALL_SELF: &str = "__call__"
op::GET_ATTR: u32 = 1
op::GET_ITEM: u32 = 3
op::ITER: u32 = 6
op::ITER_END: &str = "end of iteration"
op::ITER_NEXT: u32 = 7
op::LEN: u32 = 5
op::NEW_DICT: u32 = 8
op::NEW_FROZENSET: u32 = 13
op::NEW_LIST: u32 = 9
op::NEW_SET: u32 = 12
op::NEW_TUPLE: u32 = 11
op::SET_ATTR: u32 = 2
op::SET_ITEM: u32 = 4
op::SYS: u32 = 14
op::TEXT: &str = "str"
op::TYPE_OF: u32 = 10
tag::BOOL: u32 = 1
tag::BYTES: u32 = 4
tag::DICT: u32 = 7
tag::FLOAT: u32 = 3
tag::INT: u32 = 2
tag::LIST: u32 = 6
tag::NONE: u32 = 0
tag::RAW: u32 = 5
`.trim()

const held: string[] = []
let within = ''
for (const line of Deno.readTextFileSync(new URL('../abi/src/lib.rs', import.meta.url)).split('\n')) {
  within = line.match(/^pub mod (\w+) \{/)?.[1] ?? (line.startsWith('}') ? '' : within)
  const constant = line.match(/^\s*pub const (\w+): (.+?) = (.+);$/)
  // the value layout stays the engine's own since a plugin sees handles only
  if (constant && within !== 'nan_box') held.push(`${within ? `${within}::` : ''}${constant[1]}: ${constant[2]} = ${constant[3]}`)
}
const pinned = held.sort().join('\n')
if (pinned !== CONSTANTS) {
  console.error(`abi/src/lib.rs no longer holds the values plugins compile in\n\nexpected\n${CONSTANTS}\n\nfound\n${pinned}`)
  Deno.exit(1)
}
