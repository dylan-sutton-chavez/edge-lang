use crate::s;
use crate::util::hash::FxHashMap as HashMap;
use crate::value::{ExternFn, NativeFnId as F, hash_key};

use alloc::{boxed::Box, string::{String, ToString}, vec, vec::Vec};

pub(crate) const MAX_EXPR_DEPTH: usize = 200;
pub(crate) const MAX_INSTRUCTIONS: usize = 65_535;

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)] // <256 variants, guarantees a 1-byte tag for stable bytecode / transmute / jump-table dispatch
pub enum OpCode {
    LoadConst, LoadName, StoreName, Call, PopTop, ReturnValue, BuildString, CallPrint, CallLen, 
    FormatValue, CallAbs, Minus, CallStr, CallInt, CallRange, Phi, CallChr, CallType, MakeFunction, 
    Add, Sub, Mul, Div, Eq, CallFloat, CallBool, CallRound, CallMin, CallMax, CallSum, 
    CallEnumerate, CallZip, CallList, CallTuple, CallDict, CallIsInstance, CallSet, CallInput, 
    CallOrd, BuildDict, BuildList, NotEq, Lt, Gt, LtEq, GtEq, And, Or, Not, JumpIfFalse, Jump, 
    GetIter, ForIter, GetItem, Mod, Pow, FloorDiv, LoadTrue, LoadFalse, LoadNone, LoadAttr, StoreAttr, 
    BuildSlice, MakeClass, SetupExcept, PopExcept, Raise, BitAnd, BitOr, BitXor,
    BitNot, Shl, Shr, In, NotIn, Is, IsNot, UnpackSequence, BuildTuple, WithEnter, WithExit, Yield,
    /* Finally/with block stack where setup pushes a cleanup frame, BeginFinally marks a normal entry, EndFinally resumes the exit. */
    SetupFinally, BeginFinally, EndFinally,
    /* break/continue across N finally/with blocks before its jump, operand is N. */
    UnwindFinally,
    Del, Assert, Global, Nonlocal, UnpackArgs, ListAppend, SetAdd, MapAdd, BuildSet, RaiseFrom,
    UnpackEx, LoadEllipsis, Await, MakeCoroutine, StoreItem, Dup2,
    JumpIfFalseOrPop, JumpIfTrueOrPop, Dup, CallMethod, CallMethodArgs, CallAll, CallAny, CallBin,
    CallOct, CallHex, CallDivmod, CallPow, CallRepr, CallReversed, CallCallable,
    PopIter, DelItem, DelAttr, CallExtern,
    /* Constant-time lookup of chunk.imports[operand] from `vm.module_table`. */
    LoadModule,
    /* Read/write a `global`-declared name from/to `self.globals`, operand indexes the bare name in `chunk.names`. */
    LoadGlobal, StoreGlobal,
    /* Literal unpacking, pop a source value and merge it into the container left below it on the stack. `{**m}` / `{*s}` / `[*it]`. */
    DictUpdate, SetUpdate, ListExtend,
    /* Pop a value, push whether it matches a sequence pattern (list/tuple, str/bytes excluded). */
    MatchSeq,
    /* `name += rhs` extends list+list in place (so aliases observe it, matching Python's __iadd__), every other type behaves as Add. */
    InPlaceAdd,
    /* `name -= rhs` removes from a left set in place (alias-visible), every other type behaves as Sub. */
    InPlaceSub,
    /* Unary plus calls `__pos__`, coerces bool to int, else identity on numbers. */
    Pos,
    /* Pushes the value `yield from` produces, the exhausted subiterator's return / StopIteration value. */
    LoadYieldFrom,
    // Augmented set bitwise `|=` `&=` `^=`, mutating the left set in place.
    InPlaceBitOr, InPlaceBitAnd, InPlaceBitXor,
    // Push a fresh spread-delta frame.
    BeginArgs,
    /* Consume the staged `__exit__` result, truthy suppresses a pending re-raise. */
    WithJudge,
    // Swap the top two stack values.
    Swap,
    // Lift the third stack value to the top.
    Rot3,
    // `a @ b`, answered only by `__matmul__` or `__rmatmul__`.
    MatMul,
    // Wraps the value function on the stack in a type alias, operand indexes its name.
    MakeTypeAlias,
    // Pushes a type parameter, operand indexes its name.
    MakeTypeVar,
    /* `case C(p, k=q)`, pops subject, class and keyword names, pushes the matched attribute values or None. Operand is the positional count. */
    MatchClass,
    // Pop a value, push whether it matches a mapping pattern (dict).
    MatchMap,
    // `Call` whose args hold a `*` or `**`, closing the spread frame its first spread opened.
    CallSpread,
    /* Register forms only the VM lowering writes, each reads and writes frame slots. */
    Move, PushRegs,
    AddR, SubR, MulR, DivR, ModR, FloorDivR, InPlaceAddR, InPlaceSubR,
    BitAndR, BitOrR, BitXorR, ShlR, ShrR, PowR,
    EqR, NotEqR, LtR, LtEqR, GtR, GtEqR,
    MinusR, NotR, JumpIfFalseR, ReturnR,
    GetItemR, StoreItemR, GetAttrR, SetAttrR,
    LoadGlobalR, StoreGlobalR, LoadCellR, StoreCellR,
    InR, NotInR, IsR, IsNotR, LenR,
    // Adds registers to the comprehension accumulator on the stack top.
    ListAppendR, SetAddR, MapAddR,
    // Unpacks the stack top into the `x` locals named by a, b and c.
    UnpackR,
    // Pops the stack top into a slot, a module binding when `x` is set.
    StoreTopR,
    // Steps the innermost iterator into a register, jumping out once it ends.
    ForIterR,
    // Compare two registers and jump when the comparison is false.
    JumpUnlessEq, JumpUnlessNotEq, JumpUnlessLt, JumpUnlessLtEq, JumpUnlessGt, JumpUnlessGtEq,
    // Calls the function in register a with the `x` arguments in b and c, its result on the stack for the StoreTopR after it.
    CallR,
    // Calls method `a` of register b with the `x` arguments in c, its result on the stack for the StoreTopR after it.
    CallMethodR,
}

// Each fused builtin opcode and the builtin it runs, with the name that alone picks the opcode.
macro_rules! fused {
    ( $( $op:ident => $id:ident $(, $name:literal)? ; )* ) => {
        // Builtin name -> its fused opcode.
        pub(super) fn builtin(name: &str) -> Option<OpCode> {
            match name { $( $( $name => Some(OpCode::$op), )? )* _ => None }
        }

        // The builtin a fused opcode runs, powers the rebind redirect and the shared arity guard.
        pub(crate) fn fused_native(op: OpCode) -> Option<F> {
            match op { $( OpCode::$op => Some(F::$id), )* _ => None }
        }

        // Every fused opcode, so a set of them as bits maps back to its builtins.
        pub(super) const FUSED: &[OpCode] = &[ $( OpCode::$op, )* ];
    };
}

fused! {
    CallLen => Len, "len"; CallAbs => Abs, "abs"; CallStr => Str, "str"; CallInt => Int, "int";
    CallType => Type, "type"; CallFloat => Float, "float"; CallBool => Bool, "bool"; CallRound => Round, "round";
    CallSum => Sum, "sum"; CallZip => Zip, "zip"; CallList => List, "list"; CallTuple => Tuple, "tuple";
    CallSet => Set, "set"; CallInput => Input, "input"; CallIsInstance => IsInstance, "isinstance"; CallChr => Chr, "chr";
    CallOrd => Ord, "ord"; CallAll => All, "all"; CallAny => Any, "any"; CallBin => Bin, "bin";
    CallOct => Oct, "oct"; CallHex => Hex, "hex"; CallDivmod => Divmod, "divmod"; CallPow => Pow, "pow";
    CallRepr => Repr, "repr"; CallReversed => Reversed, "reversed"; CallCallable => Callable, "callable";
    // dict, min, max, enumerate, print and range need the keyword-aware path in `call()`.
    CallPrint => Print; CallRange => Range; CallDict => Dict; CallMin => Min; CallMax => Max; CallEnumerate => Enumerate;
}

/* Builtins the language leaves out, each with the error its use compiles to. */
pub(super) const REMOVED_BUILTINS: [(&str, &str); 3] = [
    ("locals", "'locals' is not supported, pass the values a call needs"),
    ("open", "'open' is not supported, the sandbox holds no files"),
    ("__import__", "'__import__' is not supported, write an import statement"),
];

/* The index of `name` in `REMOVED_BUILTINS`, the length test keeping every other name to one compare. */
#[inline]
pub(super) fn removed_builtin(name: &str) -> Option<usize> {
    if !matches!(name.len(), 4 | 6 | 10) { return None; }
    REMOVED_BUILTINS.iter().position(|&(n, _)| n == name)
}

/* Whether a class body may bind dunder `name`, every other one compiles to an error. */
pub(super) fn class_dunder(name: &str) -> bool {
    matches!(name,
        "__init__" | "__repr__" | "__str__" | "__format__" | "__bool__" | "__len__" | "__iter__" | "__next__" |
        "__contains__" | "__getitem__" | "__setitem__" | "__delitem__" | "__missing__" | "__getattr__" | "__call__" | "__enter__" |
        "__exit__" | "__eq__" | "__ne__" | "__lt__" | "__le__" | "__gt__" | "__ge__" | "__int__" | "__float__" |
        "__index__" | "__abs__" | "__neg__" | "__pos__" | "__invert__" | "__class_getitem__" | "__match_args__" |
        "__add__" | "__sub__" | "__mul__" | "__truediv__" | "__floordiv__" | "__matmul__" | "__mod__" | "__pow__" |
        "__and__" | "__or__" | "__xor__" | "__lshift__" | "__rshift__" | "__radd__" | "__rsub__" | "__rmul__" |
        "__rtruediv__" | "__rfloordiv__" | "__rmatmul__" | "__rmod__" | "__rpow__" | "__rand__" | "__ror__" |
        "__rxor__" | "__rlshift__" | "__rrshift__" | "__iadd__" | "__isub__" | "__imul__" | "__itruediv__" |
        "__ifloordiv__" | "__imatmul__" | "__imod__" | "__ipow__" | "__iand__" | "__ior__" | "__ixor__" |
        "__ilshift__" | "__irshift__"
    )
}

// Constant literals stored in the bytecode constants pool.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Bytes(alloc::vec::Vec<u8>),
    Int(i64),
    BigInt(crate::frontend::bigint::BigInt), // An int literal past i64, boxed when the constant pool is built.
    Float(f64),
    Bool(bool),
    None,
}

impl OpCode {
    /* Its operand is an instruction index, so moving code retargets it. */
    pub const fn is_jump(self) -> bool {
        matches!(self, Self::Jump | Self::JumpIfFalse | Self::JumpIfFalseOrPop | Self::JumpIfTrueOrPop | Self::ForIter | Self::SetupExcept | Self::SetupFinally)
    }

    /* One of the register forms only the VM lowering writes. */
    pub const fn is_register(self) -> bool { self as u8 >= Self::Move as u8 }
}

// One bytecode instruction, opcode + 16-bit operand.
#[derive(Debug, Clone, Copy)]
pub struct Instruction {
    pub opcode: OpCode,
    pub operand: u16,
}

/* Parse-time import entry. VM dedupes by spec at `run()` start, runs Code modules once, LoadModule is then O(1). Native skips execution, Module Val built from bindings. */
#[derive(Clone)]
pub struct ImportEntry {
    pub spec: alloc::string::String,
    pub kind: ImportKind,
}

/* Synthesised class spec of name + method externs, init.rs builds a HeapObj::Class from this. */
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

// SSA chunk holding instructions, constant/name pools, Phi metadata, nested functions/classes.
#[derive(Default, Clone)]
pub struct SSAChunk {
    pub instructions: Vec<Instruction>,
    pub constants: Vec<Value>,
    pub names: Vec<String>,
    pub functions: Vec<(Vec<String>, SSAChunk, u16, u16)>,
    pub phi_sources: Vec<(u16, u16)>,
    pub classes: Vec<SSAChunk>,
    pub is_pure: bool,
    pub is_generator: bool,
    pub overflow: bool,
    pub prev_slots: Vec<Option<u16>>,
    pub alias_groups: Vec<Vec<u16>>,
    pub phi_map: Vec<usize>,
    pub nonlocals: Vec<String>,
    /* Each slot of `names` by the hash of its text. */
    pub(super) name_index: hashbrown::HashTable<u16>,
    /* stmt ip->byte_offset map, binary-searched on error path, hot dispatch never touches it. */
    pub stmt_pos: Vec<(u32, u32)>,
    /* Call ip->byte_offset map, finer than stmt_pos, traceback caret lands under the call. */
    pub call_byte_pos: Vec<(u32, u32)>,
    /* Source text, shared via Arc across sub-chunks. Empty for manually constructed chunks. */
    pub source: alloc::sync::Arc<alloc::string::String>,
    /* Display path for tracebacks, empty string suppresses the `file:` prefix. */
    pub path: alloc::sync::Arc<alloc::string::String>,
    /* Native bindings from `from <pkg> import`. CallExtern `operand=(idx<<8)|argc`, per-chunk. */
    pub extern_table: Vec<ExternFn>,
    pub(crate) extern_index: HashMap<String, u16>,
    /* Chunk's import list, LoadModule operands index here, each spec becomes one Module Val at init. */
    pub imports: Vec<ImportEntry>,
}

impl SSAChunk {
    /* Binary-searches stmt_pos to map ip->byte offset, statement-level precision. */
    pub fn resolve(&self, ip: u32) -> Option<u32> {
        let i = self.stmt_pos.partition_point(|&(s, _)| s <= ip).checked_sub(1)?;
        Some(self.stmt_pos[i].1)
    }

    /* Finer than `resolve()`, returns call-site byte offset or None (caller falls back to resolve). */
    pub fn resolve_call(&self, ip: u32) -> Option<u32> {
        let i = self.call_byte_pos.partition_point(|&(s, _)| s < ip);
        let (recorded_ip, byte) = *self.call_byte_pos.get(i)?;
        if recorded_ip == ip { Some(byte) } else { None }
    }

    pub(super) fn emit(&mut self, op: OpCode, operand: u16) {
        // Overflow, set flag for post-parse diagnostic rather than panic.
        if self.instructions.len() >= MAX_INSTRUCTIONS {
            self.overflow = true;
            return;
        }
        self.instructions.push(Instruction { opcode: op, operand });
    }

    /* Records (ip, `byte_pos`) for the last emitted call so traceback caret lands on it. */
    pub(super) fn record_call_pos(&mut self, byte_pos: u32) {
        if self.instructions.is_empty() { return; }
        let ip = (self.instructions.len() - 1) as u32;
        self.call_byte_pos.push((ip, byte_pos));
    }

    pub(super) fn push_const(&mut self, v: Value) -> u16 {
        if self.constants.len() >= u16::MAX as usize {
            self.overflow = true;
            return 0;
        }
        self.constants.push(v);
        (self.constants.len() - 1) as u16
    }

    fn find_name(&self, n: &str, hash: u64) -> Option<u16> {
        self.name_index.find(hash, |&i| self.names[i as usize] == n).copied()
    }

    pub(super) fn push_name(&mut self, n: &str) -> u16 {
        let hash = hash_key(n);
        if let Some(i) = self.find_name(n, hash) { return i; }
        if self.names.len() >= u16::MAX as usize {
            self.overflow = true;
            return 0;
        }
        let i = self.names.len() as u16;
        self.names.push(n.to_string());
        let names = &self.names;
        self.name_index.insert_unique(hash, i, |&j| hash_key(&names[j as usize]));
        i
    }

    /* Builds `prev_slots`, coalesces SSA versions to canonical root, rewrites operands, builds `phi_map`. */
    pub fn finalize_prev_slots(&mut self) {
        let n = self.names.len();

        // `prev_slots[i]` is the slot of name i at version-1, if any.
        let mut ps: Vec<Option<u16>> = vec![None; n];
        for (i, name) in self.names.iter().enumerate() {
            if let Some(parsed) = SsaName::parse(name)
                && parsed.version > 0
            {
                let prev = s!(str parsed.bare, "_", int parsed.version as i64 - 1);
                if let Some(j) = self.find_name(&prev, hash_key(&prev)) {
                    ps[i] = Some(j);
                }
            }
        }

        // Coalesce each version chain to its root, an earlier slot already holds its own.
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
            match ins.opcode {
                OpCode::LoadName | OpCode::StoreName | OpCode::Del | OpCode::Phi => {
                    // Malformed input can leave an out-of-range operand, keep it as-is then.
                    if let Some(&c) = canonical.get(ins.operand as usize) { ins.operand = c; }
                }
                _ => {}
            }
        }
        for (a, b) in &mut self.phi_sources {
            if let Some(&c) = canonical.get(*a as usize) { *a = c; }
            if let Some(&c) = canonical.get(*b as usize) { *b = c; }
        }

        self.prev_slots = ps;
        self.alias_groups = (0..n).map(|i| vec![canonical[i]]).collect();

        for (_, body, _, _) in &mut self.functions {
            body.finalize_prev_slots();
        }
        for body in &mut self.classes {
            body.finalize_prev_slots();
        }

        self.index_phis();
    }

    /* Names only attribute ops use, no variable to capture or read from a caller. */
    pub(crate) fn attr_only_names(&self) -> Vec<bool> {
        // Bit 1 marks an attribute use, bit 2 any other.
        let mut uses = vec![0u8; self.names.len()];
        for ins in &self.instructions {
            let attr = matches!(ins.opcode, OpCode::LoadAttr | OpCode::StoreAttr | OpCode::DelAttr);
            if let Some(u) = uses.get_mut(ins.operand as usize) { *u |= if attr { 1 } else { 2 }; }
        }
        for &(a, b) in &self.phi_sources {
            for i in [a, b] { if let Some(u) = uses.get_mut(i as usize) { *u |= 2; } }
        }
        uses.into_iter().map(|u| u == 1).collect()
    }

    /* Maps each Phi to its `phi_sources` entry by order, empty when no Phi is left. */
    pub(crate) fn index_phis(&mut self) {
        self.phi_map.clear();
        if !self.instructions.iter().any(|i| i.opcode == OpCode::Phi) { return; }
        let mut next = 0;
        self.phi_map = self.instructions.iter().map(|i| if i.opcode == OpCode::Phi { next += 1; next - 1 } else { 0 }).collect();
    }
}

/* An open loop, its `continue` target, pending `break` jumps, kind and finally depth at entry. */
pub(crate) struct LoopCtx {
    pub(super) start: u16,
    pub(super) breaks: Vec<usize>,
    pub(super) is_for: bool,
    pub(super) cleanup_base: usize,
}

// A branch join as trail positions, where its block opened and where its last branch began.
pub(crate) struct JoinNode {
    pub(super) start: usize,
    pub(super) mid: Option<usize>,
}

/* The current version of each name a chunk binds, its text allocated once and found by hash. */
#[derive(Default, Clone)]
pub(crate) struct Versions {
    index: hashbrown::HashTable<u32>,
    entries: Vec<(Box<str>, u32)>,
}

impl Versions {
    fn find(&self, name: &str, hash: u64) -> Option<usize> {
        self.index.find(hash, |&i| &*self.entries[i as usize].0 == name).map(|&i| i as usize)
    }

    pub(super) fn get(&self, name: &str) -> Option<u32> {
        self.find(name, hash_key(name)).map(|i| self.entries[i].1)
    }

    pub(super) fn name(&self, slot: u32) -> &str { &self.entries[slot as usize].0 }

    pub(super) fn at(&self, slot: u32) -> u32 { self.entries[slot as usize].1 }

    pub(super) fn set_at(&mut self, slot: u32, ver: u32) { self.entries[slot as usize].1 = ver; }

    pub(super) fn contains(&self, name: &str) -> bool { self.get(name).is_some() }

    /* Sets the version of `name`, returning its slot and the version it replaces, zero for a new name. */
    pub(super) fn set(&mut self, name: &str, ver: u32) -> (u32, u32) {
        let hash = hash_key(name);
        if let Some(i) = self.find(name, hash) { return (i as u32, core::mem::replace(&mut self.entries[i].1, ver)); }
        let i = self.entries.len() as u32;
        self.entries.push((name.into(), ver));
        let entries = &self.entries;
        self.index.insert_unique(hash, i, |&j| hash_key(&entries[j as usize].0));
        (i, 0)
    }

    /* Moves `name` one version up, returning its slot and the version it had. */
    pub(super) fn bump(&mut self, name: &str) -> (u32, u32) {
        match self.find(name, hash_key(name)) {
            Some(i) => { self.entries[i].1 += 1; (i as u32, self.entries[i].1 - 1) }
            None => self.set(name, 1),
        }
    }
}

/* Synthetic SSA temps for multi-step desugarings. Leading `#` hides them from `globals()`, centralised so a typo becomes a compile error, not a misnamed slot. */
pub const SSA_TMP_CMP: &str = "#cmp";
pub const SSA_TMP_MATCH: &str = "#match";
pub const SSA_TMP_MATCH_ITEM: &str = "#match_item";

// The parameter a comprehension receives its iterator through.
pub const COMP_ARG: &str = "#arg";

// Param name without `*`/`**`/`~` marker prefixes.
pub fn param_base_name(p: &str) -> &str {
    p.trim_start_matches(['*', '~']).trim_end_matches('=')
}

/* Parsed view of a `<bare>_<digits>` SSA-suffixed name, avoids re-inlining the rfind('_') + ascii-digit + parse dance at every call site. */
pub struct SsaName<'a> {
    pub bare: &'a str,
    pub version: u32,
}

impl<'a> SsaName<'a> {
    // Some when `name` matches `<bare>_<digits>`, None for synthetic temps and non-SSA names.
    pub fn parse(name: &'a str) -> Option<Self> {
        let pos = name.rfind('_')?;
        if pos + 1 >= name.len() { return None; }
        let suffix = &name[pos + 1..];
        if !suffix.bytes().all(|b| b.is_ascii_digit()) { return None; }
        let version = suffix.parse().ok()?;
        Some(Self { bare: &name[..pos], version })
    }

    // (bare, version) for any name, defaulting to (name, 0) when no SSA suffix is present.
    pub fn parse_or_bare(name: &'a str) -> (&'a str, u32) {
        Self::parse(name)
            .map(|s| (s.bare, s.version))
            .unwrap_or((name, 0))
    }
}

/* Strips `_<digits>` SSA suffix for user-facing diagnostics, returns input unchanged if absent. */
pub fn ssa_strip(name: &str) -> &str {
    SsaName::parse(name).map(|s| s.bare).unwrap_or(name)
}

/* Diagnostic with byte offsets, line/col computed at render time (UTF-8 safe). */
pub struct Diagnostic {
    pub start: usize,
    pub end: usize,
    pub msg: String,
}

/* UAX#11 display width, 0=combining, 2=CJK/emoji, 1=other, keeps caret aligned in diagnostics. */
const fn char_width(c: char) -> usize {
    let cp = c as u32;
    if matches!(cp,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF
        | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7 | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8 | 0x06EA..=0x06ED | 0x0711 | 0x0730..=0x074A
        | 0x07A6..=0x07B0 | 0x07EB..=0x07F3 | 0x200B..=0x200F | 0x202A..=0x202E
        | 0x2060..=0x206F | 0xFE00..=0xFE0F | 0xFEFF | 0xE0100..=0xE01EF)
    {
        0
    } else if matches!(cp,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD)
    {
        2
    } else {
        1
    }
}

#[inline]
fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

// A long line shows SHOWN chars, LEAD before its error, and a report shows MAX_SHOWN errors.
const SHOWN: usize = 80;
const LEAD: usize = 40;
const MAX_SHOWN: usize = 20;

impl Diagnostic {
    /* Byte offset -> (line, col), 1-indexed, col counts display cells for wide-char alignment. */
    fn line_col(src: &str, byte: usize) -> (usize, usize) {
        let byte = byte.min(src.len());
        let line = src[..byte].matches('\n').count() + 1;
        let line_start = src[..byte].rfind('\n').map_or(0, |p| p + 1);
        let col = display_width(&src[line_start..byte]) + 1;
        (line, col)
    }

    /* rustc-style render, error+arrow+source line+caret, path defaults to `<input>`. */
    pub fn render(&self, src: &str, path: Option<&str>) -> alloc::string::String {
        let path = path.unwrap_or("<input>");
        let s_off = self.start.min(src.len());
        let e_off = self.end.min(src.len()).max(s_off);
        let (line_no, col) = Self::line_col(src, s_off);
        let line_start = src[..s_off].rfind('\n').map_or(0, |p| p + 1);
        let line_end = src[s_off..].find('\n').map_or(src.len(), |p| s_off + p);
        let line_txt = &src[line_start..line_end];
        let at = s_off - line_start;
        let (from, to) = if line_txt.len() <= SHOWN { (0, line_txt.len()) } else {
            let from = line_txt[..at].char_indices().rev().nth(LEAD - 1).map_or(0, |(i, _)| i);
            (from, line_txt[from..].char_indices().nth(SHOWN).map_or(line_txt.len(), |(i, _)| from + i))
        };
        let (head, tail) = (if from > 0 { "..." } else { "" }, if to < line_txt.len() { "..." } else { "" });
        let mark = display_width(&src[s_off..e_off.min(line_start + to)]).max(1);
        let mut buf = itoa::Buffer::new();
        let pad_len = buf.format(line_no).len();
        let pad: String = " ".repeat(pad_len);
        let mut o = alloc::string::String::with_capacity(to - from + 96);
        o.push_str("error: "); o.push_str(&self.msg); o.push('\n');
        o.push_str(&pad); o.push_str(" --> ");
        o.push_str(path); o.push(':');
        o.push_str(buf.format(line_no)); o.push(':'); o.push_str(buf.format(col)); o.push('\n');
        o.push_str(&pad); o.push_str(" |\n");
        o.push_str(buf.format(line_no)); o.push_str(" | "); o.push_str(head); o.push_str(&line_txt[from..to]); o.push_str(tail); o.push('\n');
        o.push_str(&pad); o.push_str(" | ");
        let caret = if from == 0 { col - 1 } else { head.len() + display_width(&line_txt[from..at]) };
        for _ in 0..caret { o.push(' '); }
        for _ in 0..mark { o.push('^'); }
        o.push('\n');
        o
    }

    /* A whole report, its first MAX_SHOWN diagnostics and then how many more there are. */
    pub fn render_all(errs: &[Diagnostic], src: &str, path: Option<&str>) -> String {
        let mut out = String::new();
        for (i, e) in errs.iter().take(MAX_SHOWN).enumerate() {
            if i > 0 { out.push('\n'); }
            out.push_str(&e.render(src, path));
        }
        let more = errs.len().saturating_sub(MAX_SHOWN);
        if more > 0 { out.push_str(&s!("\nand ", int more, str if more == 1 { " more error\n" } else { " more errors\n" })); }
        out
    }
}


/* Scan only the prefix chars before the opening quote, the body itself may legally contain 'r'/'R'. */
pub(super) fn has_raw_prefix(s: &str) -> bool {
    s.bytes()
        .take_while(|b| !matches!(b, b'"' | b'\''))
        .any(|b| matches!(b, b'r' | b'R'))
}

// Strip prefix + quotes and unescape (skipped for raw strings).
pub(super) fn parse_string(s: &str) -> String {
    let is_raw = has_raw_prefix(s);
    let s = s.trim_start_matches(|c: char| "bBrRuU".contains(c));
    let inner = if s.starts_with("\"\"\"") || s.starts_with("'''") {
        s.get(3..s.len().saturating_sub(3)).unwrap_or("")
    } else {
        s.get(1..s.len().saturating_sub(1)).unwrap_or("")
    };
    // Python normalizes source CR and CRLF to LF before building the literal.
    let owned;
    let inner: &str = if inner.contains('\r') {
        owned = inner.replace("\r\n", "\n").replace('\r', "\n");
        &owned
    } else { inner };
    if is_raw { inner.to_string() } else { unescape(inner) }
}

/* Parses b"..." to raw bytes, non-ASCII pass through, \xHH and up-to-3-digit \ooo decode to one byte, \u/\U/\N pass through verbatim. */
pub(super) fn parse_bytes_literal(s: &str) -> alloc::vec::Vec<u8> {
    let bytes = s.as_bytes();
    let is_raw = has_raw_prefix(s);
    // Skip b/B/r/R prefix chars.
    let mut i = 0;
    while i < bytes.len() && matches!(bytes[i], b'b' | b'B' | b'r' | b'R') {
        i += 1;
    }
    // Strip triple or single quotes.
    let body: &[u8] = if bytes.len() >= i + 6
        && (bytes[i..i + 3] == *b"\"\"\"" || bytes[i..i + 3] == *b"'''")
    {
        &bytes[i + 3..bytes.len() - 3]
    } else {
        bytes.get(i + 1..bytes.len().saturating_sub(1)).unwrap_or(&[])
    };
    // Source CR and CRLF read as LF, like a str literal.
    let owned;
    let body: &[u8] = if body.contains(&b'\r') {
        owned = String::from_utf8_lossy(body).replace("\r\n", "\n").replace('\r', "\n").into_bytes();
        &owned
    } else { body };
    if is_raw { return body.to_vec(); }

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(body.len());
    let mut j = 0;
    while j < body.len() {
        if body[j] != b'\\' { out.push(body[j]); j += 1; continue; }
        if j + 1 >= body.len() { out.push(b'\\'); break; }
        if let Some(b) = simple_escape(body[j + 1]) { out.push(b); j += 2; continue; }
        match body[j + 1] {
            // A backslash before a newline joins the lines.
            b'\n' => j += 2,
            b'0'..=b'7' => {
                // Octal takes up to 3 digits, mirroring push_escape, truncated to one byte.
                let mut v = (body[j + 1] - b'0') as u32;
                let mut k = j + 2;
                while k < body.len() && k < j + 4 && matches!(body[k], b'0'..=b'7') {
                    v = v * 8 + (body[k] - b'0') as u32;
                    k += 1;
                }
                out.push(v as u8);
                j = k;
            }
            b'x' => {
                // \xHH takes exactly two hex digits.
                if j + 3 < body.len() {
                    let hi = (body[j + 2] as char).to_digit(16);
                    let lo = (body[j + 3] as char).to_digit(16);
                    if let (Some(hi), Some(lo)) = (hi, lo) {
                        out.push((hi as u8) * 16 + lo as u8);
                        j += 4;
                        continue;
                    }
                }
                // Malformed \x emits verbatim.
                out.push(b'\\'); out.push(b'x'); j += 2;
            }
            other => { out.push(b'\\'); out.push(other); j += 2; }
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' { push_escape(&mut out, &mut chars); } else { out.push(c); }
    }
    out
}

/* The byte a one-character escape such as `\n` stands for, shared by str and bytes literals. */
fn simple_escape(c: u8) -> Option<u8> {
    Some(match c {
        b'n' => b'\n', b't' => b'\t', b'r' => b'\r', b'a' => 0x07, b'b' => 0x08, b'f' => 0x0C, b'v' => 0x0B,
        b'\\' | b'\'' | b'"' => c,
        _ => return None,
    })
}

/* Decodes one backslash escape (cursor already past the `\`) into `out`, unknown escapes keep the backslash. */
pub(super) fn push_escape(out: &mut String, chars: &mut core::iter::Peekable<core::str::Chars>) {
    let take_hex = |chars: &mut core::iter::Peekable<core::str::Chars>, n: usize| -> char {
        let hex: String = chars.by_ref().take(n).collect();
        u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}')
    };
    let next = chars.next();
    if let Some(b) = next.and_then(|c| u8::try_from(c).ok()).and_then(simple_escape) { out.push(b as char); return; }
    match next {
        // A backslash before a newline joins the lines.
        Some('\n') => {}
        Some('x') => out.push(take_hex(chars, 2)),
        Some('u') => out.push(take_hex(chars, 4)),
        Some('U') => out.push(take_hex(chars, 8)),
        // Octal takes up to 3 digits.
        Some(c @ '0'..='7') => {
            let mut digits = String::from(c);
            while digits.len() < 3 && matches!(chars.peek(), Some('0'..='7')) {
                digits.push(chars.next().unwrap());
            }
            let code = u32::from_str_radix(&digits, 8).unwrap_or(0);
            out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
        }
        Some(c) => { out.push('\\'); out.push(c); }
        None => out.push('\\'),
    }
}

// Builtin types registered as Type heap objects at VM init, a match so a user name misses without a scan.
macro_rules! builtin_types {
    ($($name:literal),* $(,)?) => {
        pub fn builtin_type(name: &str) -> Option<&'static str> {
            match name { $($name => Some($name),)* _ => None }
        }
        // Every builtin type name, in table order.
        pub const BUILTIN_TYPES: &[&str] = &[$($name),*];
    };
}
builtin_types! {
    "int", "float", "str", "bytes", "bool", "list",
    "tuple", "dict", "set", "frozenset", "range", "slice", "type", "NoneType", "object",
    "Exception", "BaseException",
    "ValueError", "TypeError", "NameError", "UnboundLocalError", "KeyError",
    "UnicodeError", "UnicodeEncodeError", "UnicodeDecodeError",
    "IndexError", "AttributeError", "RuntimeError",
    "ZeroDivisionError", "OverflowError", "MemoryError",
    "RecursionError", "StopIteration", "NotImplementedError",
    "OSError", "PermissionError", "IOError", "ImportError", "ModuleNotFoundError", "SyntaxError",
    "AssertionError", "ArithmeticError", "LookupError",
    "CancelledError", "TimeoutError", "SystemExit", "GeneratorExit",
}
