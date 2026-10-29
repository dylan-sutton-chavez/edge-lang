use crate::value::foreign::{BinOp, KindToken, UnOp};
use crate::value::{HeapPool, Val, VmErr, cold_runtime};
use super::{Cx, KindId, NativeId, kind_of};

/* what a site keys its rule by so it asks its language once per key */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    // a value of an engine kind whose rules the kind alone decides
    Kind(KindId),
    // an object whose meta decides its fields methods and operators
    Object(Val),
    // a value of a kind lib or the runtime registered keyed by its token
    Foreign(KindToken),
}

impl Key {
    /* the key of `v` with an instance keyed by its class */
    pub fn of(heap: &HeapPool, v: Val) -> Key {
        match kind_of(heap, v) {
            KindId::Foreign => heap.foreign_kind(v).and_then(|k| heap.foreign.token_of(k)).map_or(Key::Kind(KindId::Foreign), Key::Foreign),
            KindId::Instance => heap.class_of(v).map_or(Key::Kind(KindId::Instance), Key::Object),
            k => Key::Kind(k),
        }
    }
}

/* an interned text a site compares by its bits */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Name(pub Val);

/* what a site does with `recv.name` */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access { Load, Store, Delete, Method }

/* how a call passes its arguments so a rule may refuse it before any frame */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallShape { pub positional: u8, pub keywords: u8, pub spread: bool }

/* what an operator site asks of its operands */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Bin(BinOp),
    // the augmented form which a rule may answer by changing its left operand
    InPlace(BinOp),
    Cmp(Cmp),
    // whether the right operand holds the left
    In,
    Unary(UnOp),
    Truth,
    Len,
    Iter,
    Hash,
    GetItem,
    SetItem,
    DelItem,
    // opcode `n` of the runtime's own table
    Runtime(u8),
}

impl Op {
    /* one number per operator which a site keeping its rule compares */
    pub const fn code(self) -> u16 {
        match self {
            Op::Bin(b) => b as u16,
            Op::InPlace(b) => 0x10 | b as u16,
            Op::Cmp(c) => 0x20 | c as u16,
            Op::In => 0x30,
            Op::Unary(u) => 0x40 | u as u16,
            Op::Truth => 0x50,
            Op::Len => 0x51,
            Op::Iter => 0x52,
            Op::Hash => 0x53,
            Op::GetItem => 0x54,
            Op::SetItem => 0x55,
            Op::DelItem => 0x56,
            Op::Runtime(n) => 0x100 | n as u16,
        }
    }
}

/* the comparison operators */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cmp { Eq, Ne, Lt, Le, Gt, Ge }

/* how a site reads or writes `recv.name` for one key */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GetRule {
    // the object's own slot whose shape and index the engine keeps
    Slot,
    // a value fixed for the key such as a module global
    Const(Val),
    // a function the meta holds with the receiver bound first when `bind`
    Method { func: Val, bind: bool },
    // a native method of the receiver's kind
    Native(NativeId),
    // a map's key or a foreign kind's field of that name
    Field,
    // the attribute error which the runtime words
    Fail,
    // no rule so the engine's own path decides each time
    Ask,
}

/* how a site calls a callee of one key */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallRule {
    // a function of the program run as frames sections and calls as jumps
    Function,
    // a new object of `meta` whose `init` runs as a frame handing it back
    Construct { meta: Val, init: Option<Val>, slots: u16 },
    // `func` called with `recv` before the arguments
    Bound { func: Val, recv: Val },
    Native(NativeId),
    // the error of a value that cannot be called which the runtime words
    Fail,
    // no rule so the engine's own path decides each time
    Ask,
}

/* how a site answers an operator for one pair of keys */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpRule {
    // the engine's inline path for engine kinds under the policy
    Kernel,
    // a native of lib or of the runtime
    Native(NativeId),
    // a program function laid as a section with operands swapped or the answer negated
    Call { func: Val, swap: bool, negate: bool },
    // equality as identity
    Identity,
    // the unsupported operand error which the runtime words
    Fail,
}

/* the opcodes a runtime adds past the engine's own which `ops!` declares */
pub trait OpTable: 'static {
    const COUNT: u8 = 0;
    // a hash of its whole table so a blob of other opcodes never restores
    const HASH: u64 = 0;
    // the table erased to what the parser and the slow paths read
    const SET: &'static OpSet = &OpSet::NONE;

    /* op `op` over `args` answered in the loop or none for its slow path */
    #[inline(always)]
    fn fast(_op: u8, _args: &[Val], _heap: &mut HeapPool) -> Option<Val> { None }
}

impl OpTable for () {}

/* one op of a runtime's own table as the parser and slow paths read it */
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OwnOp {
    pub name: &'static str,
    // the values it takes off the stack where it leaves one
    pub pops: u8,
    // the native whose plain calls of `pops` arguments the parser emits as this op
    pub fuse: Option<&'static str>,
    // whether its constant operands fold at compile time
    pub folds: bool,
}

/* a runtime's own opcodes erased to tables and calls outside the loop */
#[derive(Clone, Copy)]
pub struct OpSet {
    pub ops: &'static [OwnOp],
    // the hash of the table the identity of a blob records
    pub hash: u64,
    // what op `op` gives of `args` once its fast path declined
    pub slow: fn(&mut Cx<'_, '_>, u8, &[Val]) -> Result<Val, VmErr>,
    // the constant op `op` folds `args` to or none
    pub fold: fn(u8, &[Val]) -> Option<Val>,
}

impl OpSet {
    /* the table of a runtime with no opcodes of its own */
    pub const NONE: OpSet = OpSet { ops: &[], hash: 0, slow: |_, _, _| Err(OpSet::lacks()), fold: |_, _| None };

    /* the error of an opcode the runtime's own table lacks */
    #[cold]
    pub fn lacks() -> VmErr { cold_runtime("an opcode the runtime's own table lacks") }

    /* an FNV-1a hash of each op's name pops fused native and fold in order */
    pub const fn hash_of(ops: &[OwnOp]) -> u64 {
        use crate::code::ops::{FNV, fnv};
        let (mut h, mut k) = (FNV, 0);
        while k < ops.len() {
            let o = ops[k];
            h = fnv(h, &[o.name.len() as u8]);
            h = fnv(h, o.name.as_bytes());
            let fuse = match o.fuse { Some(f) => f, None => "" };
            h = fnv(h, &[o.pops, fuse.len() as u8, o.folds as u8]);
            h = fnv(h, fuse.as_bytes());
            k += 1;
        }
        h
    }

    /* the op whose fused native `name` a plain call of `pos` arguments runs */
    pub fn fused(&self, name: &str, pos: u8) -> Option<u8> {
        self.ops.iter().position(|o| o.pops == pos && o.fuse == Some(name)).map(|k| k as u8)
    }
}
