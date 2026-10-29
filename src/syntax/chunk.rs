use crate::s;
use crate::code::OpCode;
use crate::util::hash::FxHashMap as HashMap;
use crate::value::{ExternFn, Val, hash_key};
use super::ssa::SsaName;

use alloc::{string::{String, ToString}, vec, vec::Vec};

pub(crate) const MAX_EXPR_DEPTH: usize = 200;
pub(crate) const MAX_INSTRUCTIONS: usize = 65_535;

// a constant literal of the bytecode pool
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Bytes(alloc::vec::Vec<u8>),
    Int(i64),
    // an int literal past i64 as a sign and digits in `base` most significant first
    Wide { neg: bool, base: u8, digits: alloc::boxed::Box<[u8]> },
    Float(f64),
    Bool(bool),
    None,
}

/* a runtime native's call with its positionals then keyword pairs stacked */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeSite {
    // the native's place in the runtime's table
    pub id: u16,
    pub pos: u8,
    pub kw: u8,
    // a `*` or `**` argument whose counts the run closes
    pub spread: bool,
    // a call the syntax makes which no binding of the native's name redirects
    pub pinned: bool,
    // bare positionals the native answers on the heap with no call
    pub fast: bool,
    // a statement's call whose value goes nowhere
    pub void: bool,
}

// the CallNative operand bit naming a site the chunk's table holds
pub const SITE_TABLE: u16 = 0x8000;
// the in place bit of a site no binding redirects
pub const SITE_PINNED: u16 = 0x4000;
// the in place bit of a site whose heap op answers bare positionals
pub const SITE_FAST: u16 = 0x2000;
// the in place bit of a statement's call that keeps no value
pub const SITE_VOID: u16 = 0x1000;
// the in place bit of a call with one keyword pair over its positionals
pub const SITE_KW1: u16 = 0x100;

impl NativeSite {
    /* the operand naming this plain site in place and none for the table */
    pub const fn in_place(&self) -> Option<u16> {
        if self.kw > 1 || self.spread || self.pos >= 8 || self.id >= 0x100 { return None; }
        Some(self.id | (self.pos as u16) << 9 | if self.kw == 1 { SITE_KW1 } else { 0 } | if self.void { SITE_VOID } else { 0 } | if self.fast { SITE_FAST } else { 0 } | if self.pinned { SITE_PINNED } else { 0 })
    }
}

/* the native and positional count an in place operand names */
#[inline(always)]
pub const fn site_in_place(operand: u16) -> (u16, usize) { (operand & 0xFF, (operand >> 9 & 7) as usize) }

/* the site a CallNative operand names in place or in `sites` */
#[inline]
pub fn native_site(operand: u16, sites: &[NativeSite]) -> Option<NativeSite> {
    if operand & SITE_TABLE == 0 {
        let (id, pos) = site_in_place(operand);
        let bit = |b: u16| operand & b != 0;
        return Some(NativeSite { id, pos: pos as u8, kw: u8::from(bit(SITE_KW1)), spread: false, pinned: bit(SITE_PINNED), fast: bit(SITE_FAST), void: bit(SITE_VOID) });
    }
    sites.get((operand & !SITE_TABLE) as usize).copied()
}

#[derive(Debug, Clone, Copy)]
pub struct Instruction {
    pub opcode: OpCode,
    pub operand: u16,
}

/* an import the VM dedupes by spec at `run()` so each Code module runs once */
#[derive(Clone)]
pub struct ImportEntry {
    pub spec: alloc::string::String,
    pub kind: ImportKind,
}

/* a native class by name and method externs that `init.rs` turns into a class object */
#[derive(Clone)]
pub struct NativeClassEntry {
    pub name: String,
    pub methods: Vec<crate::value::ExternFn>,
}

#[derive(Clone)]
pub enum ImportKind {
    Code(alloc::rc::Rc<SSAChunk>),
    Native { funcs: Vec<crate::value::ExternFn>, classes: Vec<NativeClassEntry>, consts: Vec<crate::value::ExternFn> },
}

#[derive(Default, Clone)]
pub struct SSAChunk {
    pub instructions: Vec<Instruction>,
    pub constants: Vec<Value>,
    pub names: Vec<String>,
    pub functions: Vec<(Vec<String>, SSAChunk, u16, u16)>,
    pub phi_sources: Vec<(u16, u16)>,
    pub classes: Vec<SSAChunk>,
    /* how many bases the MakeClass of a class body pops and zero for other chunks */
    pub bases: u16,
    /* the name of a function or class body with the classes and functions around it */
    pub qualname: String,
    /* how many parameters come before a `/` and pass only by position */
    pub posonly: u16,
    /* the line a function definition starts on counting its first decorator */
    pub first_line: u32,
    pub is_generator: bool,
    /* whether it holds an `a if c else b` that its lowering keeps in registers */
    pub ternaries: bool,
    pub overflow: bool,
    pub prev_slots: Vec<Option<u16>>,
    /* the slot at the root of the version chain of each name */
    pub canonical: Vec<u16>,
    pub phi_map: Vec<usize>,
    pub nonlocals: Vec<String>,
    /* each slot of `names` by the hash of its text */
    pub(super) name_index: hashbrown::HashTable<u16>,
    /* the length and ends marks of its names as bits while no table holds them */
    pub(super) name_marks: u32,
    /* the byte offset of each statement ip searched only on the error path */
    pub stmt_pos: Vec<(u32, u32)>,
    /* the byte offset of each call ip so a traceback caret lands under the call */
    pub call_byte_pos: Vec<(u32, u32)>,
    /* the source text shared across sub-chunks and empty for chunks built by hand */
    pub source: alloc::sync::Arc<alloc::string::String>,
    /* the traceback path where an empty one drops the `file:` prefix */
    pub path: alloc::sync::Arc<alloc::string::String>,
    /* the native calls CallNative operands index each with its native and counts */
    pub native_sites: Vec<NativeSite>,
    /* the native bindings `from <pkg> import` makes which CallExtern indexes as `(idx<<8)|argc` */
    pub extern_table: Vec<ExternFn>,
    pub(crate) extern_index: HashMap<String, u16>,
    /* the imports LoadModule operands index with one module value per spec at init */
    pub imports: Vec<ImportEntry>,
    /* one past its place among the bodies the machine running it registered and zero before */
    pub(crate) id: core::cell::Cell<u32>,
    /* each op of constants by place and pops with its folded value and its name */
    pub(crate) folds: Vec<(u32, u8, Val, &'static str)>,
    /* the natives its calls index by place and none for a chunk built by hand */
    #[cfg(not(all(target_arch = "wasm32", feature = "runtime")))]
    natives: Option<&'static crate::lang::Natives>,
}

/* how many names a chunk scans before it keeps a hash table of them */
const SCANNED_NAMES: usize = 16;

/* the bit of `name_marks` a name sets by its length and first and last bytes */
#[inline]
fn name_mark(n: &str) -> usize {
    let b = n.as_bytes();
    (b.len() * 7 + b.last().map_or(0, |&c| c as usize) * 3 + b.first().map_or(0, |&c| c as usize)) & 31
}

/* the use an op's operand marks on its name by the op's byte */
const NAME_USE: [u8; 256] = {
    let mut marks = [0u8; 256];
    let mut k = 0;
    while k < OpCode::EVERY.len() {
        let op = OpCode::EVERY[k];
        // one marks an attribute and two a slot or name while other operands mark nothing
        marks[op as usize] = if matches!(op, crate::code::attrs!()) { 1 } else if matches!(op.info().operand, crate::code::Operand::Slot | crate::code::Operand::Name) { 2 } else { 0 };
        k += 1;
    }
    marks
};

impl SSAChunk {
    /* an empty chunk over texts it is given which allocates nothing of its own */
    pub(crate) fn over(source: alloc::sync::Arc<String>, path: alloc::sync::Arc<String>, natives: Option<&'static crate::lang::Natives>) -> Self {
        // the wasm runtime boots only what its own front compiles so keeps no natives
        #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
        let _ = natives;
        Self {
            instructions: Vec::new(), constants: Vec::new(), names: Vec::new(), functions: Vec::new(), phi_sources: Vec::new(), classes: Vec::new(),
            bases: 0, qualname: String::new(), posonly: 0, first_line: 0, is_generator: false, ternaries: false, overflow: false,
            prev_slots: Vec::new(), canonical: Vec::new(), phi_map: Vec::new(), nonlocals: Vec::new(),
            name_index: hashbrown::HashTable::new(), name_marks: 0, stmt_pos: Vec::new(), call_byte_pos: Vec::new(),
            source, path, native_sites: Vec::new(), extern_table: Vec::new(), extern_index: HashMap::default(), imports: Vec::new(),
            id: core::cell::Cell::new(0), folds: Vec::new(),
            #[cfg(not(all(target_arch = "wasm32", feature = "runtime")))]
            natives,
        }
    }

    /* the natives it was compiled over where a machine may meet a chunk of another */
    #[inline(always)]
    pub(crate) fn natives(&self) -> Option<&'static crate::lang::Natives> {
        #[cfg(not(all(target_arch = "wasm32", feature = "runtime")))]
        return self.natives;
        #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
        None
    }

    #[inline]
    pub fn ternaries(&self) -> bool { self.ternaries }

    /* marks a chunk holding a ternary so its lowering looks for the join */
    pub(crate) fn set_ternaries(&mut self, on: bool) { if on { self.ternaries = true; } }

    /* the byte offset of the statement holding `ip` */
    pub fn resolve(&self, ip: u32) -> Option<u32> {
        let i = self.stmt_pos.partition_point(|&(s, _)| s <= ip).checked_sub(1)?;
        Some(self.stmt_pos[i].1)
    }

    /* the byte offset of the call at `ip` with `resolve` as the coarser fallback */
    pub fn resolve_call(&self, ip: u32) -> Option<u32> {
        let i = self.call_byte_pos.partition_point(|&(s, _)| s < ip);
        let (recorded_ip, byte) = *self.call_byte_pos.get(i)?;
        if recorded_ip == ip { Some(byte) } else { None }
    }

    // a runtime crate builds the parser so only a marked helper inlines there
    #[inline]
    pub(super) fn emit(&mut self, op: OpCode, operand: u16) {
        // a full chunk sets a flag the parser reports later instead of panicking
        if self.instructions.len() >= MAX_INSTRUCTIONS {
            self.overflow = true;
            return;
        }
        self.instructions.push(Instruction { opcode: op, operand });
    }

    /* records `byte_pos` for the last emitted call so a traceback caret lands on it */
    pub(super) fn record_call_pos(&mut self, byte_pos: u32) {
        if self.instructions.is_empty() { return; }
        let ip = (self.instructions.len() - 1) as u32;
        self.call_byte_pos.push((ip, byte_pos));
    }

    #[inline]
    pub(super) fn push_const(&mut self, v: Value) -> u16 {
        if self.constants.len() >= u16::MAX as usize {
            self.overflow = true;
            return 0;
        }
        self.constants.push(v);
        (self.constants.len() - 1) as u16
    }

    /* the slot of `n` by scan while the names are few and by hash after */
    #[inline(always)]
    fn find_name(&self, n: &str) -> Option<u16> {
        // before the table a name matching no mark is new without any scan
        if self.name_index.is_empty() {
            if self.name_marks & 1 << name_mark(n) == 0 { return None; }
            let (b, last) = (n.as_bytes(), n.len().wrapping_sub(1));
            return self.names.iter().position(|m| m.len() == b.len() && m.as_bytes().get(last) == b.get(last) && m.as_bytes() == b).map(|i| i as u16);
        }
        self.name_index.find(hash_key(n), |&i| self.names[i as usize] == n).copied()
    }

    /* the sizes of the chunk tables now which `rewind` takes it back to */
    pub(super) fn sizes(&self) -> [usize; 7] {
        [self.instructions.len(), self.call_byte_pos.len(), self.names.len(), self.constants.len(), self.functions.len(), self.classes.len(), self.native_sites.len()]
    }

    /* drops everything emitted past `at` with the names leaving their index too */
    pub(super) fn rewind(&mut self, at: [usize; 7]) {
        // a cut to few names drops the table as `push_name` keeps none for them
        if at[2] <= SCANNED_NAMES { self.name_index.clear(); }
        for j in at[2]..self.names.len() {
            if let Ok(e) = self.name_index.find_entry(hash_key(&self.names[j]), |&k| k as usize == j) { e.remove(); }
        }
        self.instructions.truncate(at[0]);
        self.call_byte_pos.truncate(at[1]);
        self.names.truncate(at[2]);
        self.constants.truncate(at[3]);
        self.functions.truncate(at[4]);
        self.classes.truncate(at[5]);
        self.native_sites.truncate(at[6]);
    }

    pub(super) fn push_name(&mut self, n: &str) -> u16 {
        if let Some(i) = self.find_name(n) { return i; }
        if self.names.len() >= u16::MAX as usize {
            self.overflow = true;
            return 0;
        }
        let i = self.names.len() as u16;
        self.name_marks |= 1 << name_mark(n);
        self.names.push(n.to_string());
        // few names keep no table and the first name past them hashes them all once
        if self.names.len() > SCANNED_NAMES {
            let names = &self.names;
            let at = if self.name_index.is_empty() { 0 } else { i as usize };
            for j in at..names.len() { self.name_index.insert_unique(hash_key(&names[j]), j as u16, |&k| hash_key(&names[k as usize])); }
        }
        i
    }

    /* builds `prev_slots` and `phi_map` and rewrites operands to the root slot of their version chain */
    pub fn finalize_prev_slots(&mut self) {
        let n = self.names.len();
        // names all at a first version are their own roots which empty tables mean
        if let Some(first) = self.names.iter().position(|name| SsaName::parse(name).is_some_and(|p| p.version > 0)) {
            // `prev_slots[i]` is the slot of name `i` one version earlier if any
            let mut ps: Vec<Option<u16>> = vec![None; n];
            for (i, name) in self.names.iter().enumerate().skip(first) {
                if let Some(parsed) = SsaName::parse(name)
                    && parsed.version > 0
                {
                    let prev = s!(str parsed.bare, "_", int parsed.version as i64 - 1);
                    if let Some(j) = self.find_name(&prev) {
                        ps[i] = Some(j);
                    }
                }
            }

            // each chain coalesces to its root since every earlier slot already holds its own
            let mut canonical: Vec<u16> = (0..n as u16).collect();
            for i in 0..n {
                let mut root = i;
                while let Some(&Some(p)) = ps.get(root) {
                    let p = p as usize;
                    if p == root { break; }
                    if p < i { root = canonical[p] as usize; break; }
                    root = p;
                }
                canonical[i] = root as u16;
            }

            for ins in &mut self.instructions {
                // malformed input can leave an operand out of range which stays as it is
                if ins.opcode.names_slot() && let Some(&c) = canonical.get(ins.operand as usize) { ins.operand = c; }
            }
            for (a, b) in &mut self.phi_sources {
                if let Some(&c) = canonical.get(*a as usize) { *a = c; }
                if let Some(&c) = canonical.get(*b as usize) { *b = c; }
            }

            self.prev_slots = ps;
            self.canonical = canonical;
        } else {
            self.prev_slots.clear();
            self.canonical.clear();
        }

        for (_, body, _, _) in &mut self.functions {
            body.finalize_prev_slots();
        }
        for body in &mut self.classes {
            body.finalize_prev_slots();
        }

        self.index_phis();
    }

    /* the names only attribute ops use which are no variable to capture or read */
    pub(crate) fn attr_only_names(&self) -> Vec<bool> {
        let mut uses = vec![0u8; self.names.len()];
        for ins in &self.instructions {
            if let Some(u) = uses.get_mut(ins.operand as usize) { *u |= NAME_USE[ins.opcode as usize]; }
        }
        for &(a, b) in &self.phi_sources {
            for i in [a, b] { if let Some(u) = uses.get_mut(i as usize) { *u |= 2; } }
        }
        uses.into_iter().map(|u| u == 1).collect()
    }

    /* maps each Phi to its `phi_sources` entry by order and stays empty without one */
    pub(crate) fn index_phis(&mut self) {
        self.phi_map.clear();
        if !self.instructions.iter().any(|i| i.opcode == OpCode::Phi) { return; }
        let mut next = 0;
        self.phi_map = self.instructions.iter().map(|i| if i.opcode == OpCode::Phi { next += 1; next - 1 } else { 0 }).collect();
    }
}
