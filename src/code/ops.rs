use crate::syntax::chunk::{SSAChunk, native_site};
use crate::syntax::parser::ITER_VALUE;
use crate::syntax::ssa::SPREAD_BASES;

/* which writer makes an op */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    // the parser emits it or the lowering fuses it from what the parser emits
    Stack,
    // a register form the lowering writes
    Register,
    // a form past `ALL` that sections and calls lay
    Internal,
}

/* what an op pops or pushes as a constant or as its operand counts */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    N(u8),
    // the operand itself
    Items,
    // twice the operand for keys and values
    Pairs,
    // the targets before and after a star plus the list the star takes
    Rest,
    // a field and its spec when bit zero is set
    Spec,
    // the callee with its positionals and keyword pairs
    Args,
    // a binding's positionals and keyword pairs in two nibbles
    Packed,
    // the arguments of the native site the operand names
    Site,
    // one unless that site is a statement's call
    Gives,
    // the defaults of the function the operand names
    Defaults,
    // the bases of the class the operand names
    Bases,
    // one when the iterator stays a value
    Iter,
    // none for a bare raise of the handled exception
    Raised,
    // the values the high byte of its operand counts
    High,
    // only the run knows
    Run,
}

impl Count {
    /* the count for `operand` in `chunk` and none when only the run knows */
    pub fn of(self, operand: u16, chunk: &SSAChunk) -> Option<u16> {
        Some(match self {
            Count::N(n) => u16::from(n),
            Count::Items => operand,
            Count::Pairs => 2 * operand,
            Count::Rest => (operand >> 8) + 1 + (operand & 0xFF),
            Count::Spec => 1 + (operand & 1),
            Count::Args => 1 + (operand & 0xFF) + 2 * (operand >> 8),
            Count::Packed => (operand & 0xF) + 2 * (operand >> 4 & 0xF),
            Count::Site => { let s = native_site(operand, &chunk.native_sites)?; s.pos as u16 + 2 * s.kw as u16 }
            Count::Gives => u16::from(!native_site(operand, &chunk.native_sites)?.void),
            Count::Defaults => chunk.functions.get(operand as usize)?.2,
            Count::Bases => { let k = chunk.classes.get(operand as usize)?; if k.bases == SPREAD_BASES { 1 } else { k.bases } }
            Count::Iter => u16::from(operand == ITER_VALUE),
            Count::Raised => u16::from(operand != 1),
            Count::High => operand >> 8,
            Count::Run => return None,
        })
    }

    /* the least it counts and the most with none past any bound */
    pub const fn span(self) -> (u16, Option<u16>) {
        match self {
            Count::N(n) => (n as u16, Some(n as u16)),
            Count::Rest | Count::Args => (1, None),
            Count::Spec => (1, Some(2)),
            Count::Packed => (0, Some(45)),
            Count::Gives | Count::Iter | Count::Raised => (0, Some(1)),
            Count::High => (0, Some(255)),
            Count::Items | Count::Pairs | Count::Site | Count::Defaults | Count::Bases | Count::Run => (0, None),
        }
    }

    /* one byte per count for the table hash */
    pub const fn code(self) -> u8 {
        match self {
            Count::N(n) => n,
            Count::Items => 0x80,
            Count::Pairs => 0x81,
            Count::Rest => 0x82,
            Count::Spec => 0x83,
            Count::Args => 0x84,
            Count::Packed => 0x85,
            Count::Site => 0x86,
            Count::Gives => 0x87,
            Count::Defaults => 0x88,
            Count::Bases => 0x89,
            Count::Iter => 0x8A,
            Count::Raised => 0x8B,
            Count::High => 0x8C,
            Count::Run => 0xFF,
        }
    }
}

/* what an op's operand names */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand {
    None,
    // an index of the constant pool
    Constant,
    // a variable's slot in `names`
    Slot,
    // a bare name in `names`
    Name,
    // an attribute's name in `names`
    Attr,
    // the attribute a fused method call reads
    Method,
    // an instruction index that moving code retargets
    Jump,
    // how many values or blocks it takes
    Count,
    // a call's positionals low and keyword pairs high
    Call,
    // flags of its own
    Flags,
    // an index of `functions`
    Function,
    // an index of `classes`
    Class,
    // an index of `imports`
    Module,
    // a native site in place or in `native_sites`
    Native,
    // a native binding with its counts in the low byte
    Binding,
    // an opcode of the runtime's own table in the low byte
    Own,
}

/* the block of the register loop whose arm runs an op over rows */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm { None, Values, Containers, Calls, Natives, Flow }

/* where control goes past an op */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    // the next op
    Next,
    // its target alone
    Goto,
    // the next op or its target
    Fork,
    // the next op with a handler at its target
    Guard,
    // out of the frame
    Leave,
    // the next op or out through the finally blocks it unwinds
    Unwind,
}

/* what the way to a jump's target does to the stack the op started with */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    None,
    Keep,
    // drops the value it tests
    Pop,
    // adds the exception its handler takes
    Catch,
}

impl Edge {
    /* the pops and pushes on the way to a jump's target */
    pub const fn taken(self) -> Option<(u8, u8)> {
        match self { Edge::None => None, Edge::Keep => Some((0, 0)), Edge::Pop => Some((1, 0)), Edge::Catch => Some((0, 1)) }
    }
}

/* what lowering spills at an op so a run parked there resumes in any code */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spill {
    None,
    // the values under its operands
    Under,
    // every value
    Every,
}

/* the operator an op applies */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator { None, Binary, Augmented, Unary }

/* the rows a register form reads from `b` and writes from `a` */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rows {
    None,
    // what stack op `x` takes and gives with `c` as its operand
    Op,
    // `c` values into one
    Items,
    // one value into `c`
    Unpack,
    // what stack op `x` takes from `a` and leaves there
    Row,
}

/* the arm the loop and its slow path generate for a register form */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fast {
    // an arm written by hand where the loop is
    None,
    // two numbers through the int and float kernels of its stack op
    Arith,
    // two numbers ordered as its stack op orders them
    Order,
    // a jump unless two numbers keep the order of its stack op
    Test,
    // two ints through a bitwise op
    Bits,
    // a register copied into another
    Move,
}

/* every column the table declares for one op */
#[derive(Clone, Copy, Debug)]
pub struct OpInfo<Op> {
    pub name: &'static str,
    pub class: Class,
    pub pops: Count,
    pub pushes: Count,
    pub operand: Operand,
    pub arm: Arm,
    pub flow: Flow,
    pub edge: Edge,
    pub spill: Spill,
    pub operator: Operator,
    // whether constant operands fold at compile time
    pub fold: bool,
    // the register form the lowering writes for it
    pub form: Option<Op>,
    // the compare and branch form of a comparison
    pub branch: Option<Op>,
    // the stack op a register form falls back to
    pub stack: Option<Op>,
    pub rows: Rows,
    pub fast: Fast,
}

impl<Op> OpInfo<Op> {
    /* the values it pops and pushes falling through and none when only the run knows */
    pub fn effect(&self, operand: u16, chunk: &SSAChunk) -> Option<(u16, u16)> {
        Some((self.pops.of(operand, chunk)?, self.pushes.of(operand, chunk)?))
    }

    /* its pops and pushes when no operand or chunk sets them */
    pub const fn fixed(&self) -> Option<(u8, u8)> {
        match (self.pops, self.pushes) { (Count::N(p), Count::N(q)) => Some((p, q)), _ => None }
    }

    /* the pops and pushes on the way to its target */
    pub const fn taken(&self) -> Option<(u8, u8)> { self.edge.taken() }

    /* the most it adds to the stack for any operand and none past any bound */
    pub const fn rise(&self) -> Option<u8> {
        if matches!(self.pops, Count::Run) { return None; }
        let Some(most) = self.pushes.span().1 else { return None };
        let fell = most.saturating_sub(self.pops.span().0);
        let jumped: u16 = if matches!(self.edge, Edge::Catch) { 1 } else { 0 };
        Some((if fell > jumped { fell } else { jumped }) as u8)
    }

    /* whether control may reach the next op */
    pub const fn falls(&self) -> bool { !matches!(self.flow, Flow::Goto | Flow::Leave) }
}

/* the FNV-1a step of the table hash over `bytes` */
#[doc(hidden)]
pub const fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    let mut k = 0;
    while k < bytes.len() {
        h = (h ^ bytes[k] as u64).wrapping_mul(0x100_0000_01B3);
        k += 1;
    }
    h
}

/* the FNV-1a offset the table hash starts from */
#[doc(hidden)]
pub const FNV: u64 = 0xCBF2_9CE4_8422_2325;

/* declares each op of a table once and generates its enum facts and hash */
#[macro_export]
macro_rules! ops {
    (@len $($x:ident)*) => { <[()]>::len(&[$($crate::ops!(@unit $x)),*]) };
    (@unit $x:ident) => { () };
    // a muncher's ops as one pattern or as whether `op` is one of them
    (@hits pattern $ty:ident [$($hit:ident)*]) => { $($ty::$hit)|* };
    (@hits (is $op:tt) $ty:ident []) => { false };
    (@hits (is $op:tt) $ty:ident [$($hit:ident)+]) => { matches!($op, $($ty::$hit)|+) };
    // each muncher keeps the ops whose column holds one value and hands them to `@hits`
    (@jumps $how:tt $ty:ident [$($hit:ident)*]) => { $crate::ops!(@hits $how $ty [$($hit)*]) };
    (@jumps $how:tt $ty:ident [$($hit:ident)*] $n:ident jump $($rest:tt)*) => { $crate::ops!(@jumps $how $ty [$($hit)* $n] $($rest)*) };
    (@jumps $how:tt $ty:ident [$($hit:ident)*] $n:ident $k:ident $($rest:tt)*) => { $crate::ops!(@jumps $how $ty [$($hit)*] $($rest)*) };
    (@slots $how:tt $ty:ident [$($hit:ident)*]) => { $crate::ops!(@hits $how $ty [$($hit)*]) };
    (@slots $how:tt $ty:ident [$($hit:ident)*] $n:ident slot $($rest:tt)*) => { $crate::ops!(@slots $how $ty [$($hit)* $n] $($rest)*) };
    (@slots $how:tt $ty:ident [$($hit:ident)*] $n:ident $k:ident $($rest:tt)*) => { $crate::ops!(@slots $how $ty [$($hit)*] $($rest)*) };
    (@attrs $how:tt $ty:ident [$($hit:ident)*]) => { $crate::ops!(@hits $how $ty [$($hit)*]) };
    (@attrs $how:tt $ty:ident [$($hit:ident)*] $n:ident attr $($rest:tt)*) => { $crate::ops!(@attrs $how $ty [$($hit)* $n] $($rest)*) };
    (@attrs $how:tt $ty:ident [$($hit:ident)*] $n:ident $k:ident $($rest:tt)*) => { $crate::ops!(@attrs $how $ty [$($hit)*] $($rest)*) };
    (@binary $how:tt $ty:ident [$($hit:ident)*]) => { $crate::ops!(@hits $how $ty [$($hit)*]) };
    (@binary $how:tt $ty:ident [$($hit:ident)*] $n:ident binary $($rest:tt)*) => { $crate::ops!(@binary $how $ty [$($hit)* $n] $($rest)*) };
    (@binary $how:tt $ty:ident [$($hit:ident)*] $n:ident $k:tt $($rest:tt)*) => { $crate::ops!(@binary $how $ty [$($hit)*] $($rest)*) };
    (@folds $how:tt $ty:ident [$($hit:ident)*]) => { $crate::ops!(@hits $how $ty [$($hit)*]) };
    (@folds $how:tt $ty:ident [$($hit:ident)*] $n:ident binary fold $($rest:tt)*) => { $crate::ops!(@folds $how $ty [$($hit)* $n] $($rest)*) };
    (@folds $how:tt $ty:ident [$($hit:ident)*] $n:ident $k:tt $f:tt $($rest:tt)*) => { $crate::ops!(@folds $how $ty [$($hit)*] $($rest)*) };
    (@ends $how:tt $ty:ident [$($hit:ident)*]) => { $crate::ops!(@hits $how $ty [$($hit)*]) };
    (@ends $how:tt $ty:ident [$($hit:ident)*] $n:ident goto $($rest:tt)*) => { $crate::ops!(@ends $how $ty [$($hit)* $n] $($rest)*) };
    (@ends $how:tt $ty:ident [$($hit:ident)*] $n:ident leave $($rest:tt)*) => { $crate::ops!(@ends $how $ty [$($hit)* $n] $($rest)*) };
    (@ends $how:tt $ty:ident [$($hit:ident)*] $n:ident $k:tt $($rest:tt)*) => { $crate::ops!(@ends $how $ty [$($hit)*] $($rest)*) };
    (@still $how:tt $ty:ident [$($hit:ident)*]) => { $crate::ops!(@hits $how $ty [$($hit)*]) };
    (@still $how:tt $ty:ident [$($hit:ident)*] $n:ident 0 0 $($rest:tt)*) => { $crate::ops!(@still $how $ty [$($hit)* $n] $($rest)*) };
    (@still $how:tt $ty:ident [$($hit:ident)*] $n:ident $p:tt $q:tt $($rest:tt)*) => { $crate::ops!(@still $how $ty [$($hit)*] $($rest)*) };
    // a count as the value an op's effect reads off `$o` and the chunk `$c`
    (@of ? $o:ident $c:ident) => { None::<u16> };
    (@of items $o:ident $c:ident) => { Some($o) };
    (@of pairs $o:ident $c:ident) => { Some(2 * $o) };
    (@of rest $o:ident $c:ident) => { Some(($o >> 8) + 1 + ($o & 0xFF)) };
    (@of spec $o:ident $c:ident) => { Some(1 + ($o & 1)) };
    (@of args $o:ident $c:ident) => { Some(1 + ($o & 0xFF) + 2 * ($o >> 8)) };
    (@of packed $o:ident $c:ident) => { Some(($o & 0xF) + 2 * ($o >> 4 & 0xF)) };
    (@of site $o:ident $c:ident) => { $crate::code::native_site($o, &$c.native_sites).map(|s| s.pos as u16 + 2 * s.kw as u16) };
    (@of gives $o:ident $c:ident) => { $crate::code::native_site($o, &$c.native_sites).map(|s| u16::from(!s.void)) };
    (@of defaults $o:ident $c:ident) => { $c.functions.get($o as usize).map(|f| f.2) };
    (@of bases $o:ident $c:ident) => { $c.classes.get($o as usize).map(|k| if k.bases == $crate::code::SPREAD_BASES { 1 } else { k.bases }) };
    (@of iter $o:ident $c:ident) => { Some(u16::from($o == $crate::code::ITER_VALUE)) };
    (@of raised $o:ident $c:ident) => { Some(u16::from($o != 1)) };
    (@of high $o:ident $c:ident) => { Some($o >> 8) };
    (@of $n:literal $o:ident $c:ident) => { Some($n) };
    // a count as its operand alone sets it and none where a chunk must
    (@plain site $o:ident) => { None::<u16> };
    (@plain gives $o:ident) => { None::<u16> };
    (@plain defaults $o:ident) => { None::<u16> };
    (@plain bases $o:ident) => { None::<u16> };
    (@plain $k:tt $o:ident) => { $crate::ops!(@of $k $o chunk) };
    (@count ?) => { $crate::code::ops::Count::Run };
    (@count items) => { $crate::code::ops::Count::Items };
    (@count pairs) => { $crate::code::ops::Count::Pairs };
    (@count rest) => { $crate::code::ops::Count::Rest };
    (@count spec) => { $crate::code::ops::Count::Spec };
    (@count args) => { $crate::code::ops::Count::Args };
    (@count packed) => { $crate::code::ops::Count::Packed };
    (@count site) => { $crate::code::ops::Count::Site };
    (@count gives) => { $crate::code::ops::Count::Gives };
    (@count defaults) => { $crate::code::ops::Count::Defaults };
    (@count bases) => { $crate::code::ops::Count::Bases };
    (@count iter) => { $crate::code::ops::Count::Iter };
    (@count raised) => { $crate::code::ops::Count::Raised };
    (@count high) => { $crate::code::ops::Count::High };
    (@count $n:literal) => { $crate::code::ops::Count::N($n) };
    (@operand none) => { $crate::code::ops::Operand::None };
    (@operand constant) => { $crate::code::ops::Operand::Constant };
    (@operand slot) => { $crate::code::ops::Operand::Slot };
    (@operand name) => { $crate::code::ops::Operand::Name };
    (@operand attr) => { $crate::code::ops::Operand::Attr };
    (@operand method) => { $crate::code::ops::Operand::Method };
    (@operand jump) => { $crate::code::ops::Operand::Jump };
    (@operand count) => { $crate::code::ops::Operand::Count };
    (@operand call) => { $crate::code::ops::Operand::Call };
    (@operand flags) => { $crate::code::ops::Operand::Flags };
    (@operand function) => { $crate::code::ops::Operand::Function };
    (@operand class) => { $crate::code::ops::Operand::Class };
    (@operand module) => { $crate::code::ops::Operand::Module };
    (@operand native) => { $crate::code::ops::Operand::Native };
    (@operand binding) => { $crate::code::ops::Operand::Binding };
    (@operand own) => { $crate::code::ops::Operand::Own };
    (@arm -) => { $crate::code::ops::Arm::None };
    (@arm values) => { $crate::code::ops::Arm::Values };
    (@arm containers) => { $crate::code::ops::Arm::Containers };
    (@arm calls) => { $crate::code::ops::Arm::Calls };
    (@arm natives) => { $crate::code::ops::Arm::Natives };
    (@arm flow) => { $crate::code::ops::Arm::Flow };
    (@flow -) => { $crate::code::ops::Flow::Next };
    (@flow goto) => { $crate::code::ops::Flow::Goto };
    (@flow fork) => { $crate::code::ops::Flow::Fork };
    (@flow guard) => { $crate::code::ops::Flow::Guard };
    (@flow leave) => { $crate::code::ops::Flow::Leave };
    (@flow unwind) => { $crate::code::ops::Flow::Unwind };
    (@edge -) => { $crate::code::ops::Edge::None };
    (@edge keep) => { $crate::code::ops::Edge::Keep };
    (@edge pop) => { $crate::code::ops::Edge::Pop };
    (@edge catch) => { $crate::code::ops::Edge::Catch };
    (@spill -) => { $crate::code::ops::Spill::None };
    (@spill under) => { $crate::code::ops::Spill::Under };
    (@spill every) => { $crate::code::ops::Spill::Every };
    (@operator -) => { $crate::code::ops::Operator::None };
    (@operator binary) => { $crate::code::ops::Operator::Binary };
    (@operator augmented) => { $crate::code::ops::Operator::Augmented };
    (@operator unary) => { $crate::code::ops::Operator::Unary };
    (@fold -) => { false };
    (@fold fold) => { true };
    (@rows -) => { $crate::code::ops::Rows::None };
    (@rows op) => { $crate::code::ops::Rows::Op };
    (@rows items) => { $crate::code::ops::Rows::Items };
    (@rows unpack) => { $crate::code::ops::Rows::Unpack };
    (@rows row) => { $crate::code::ops::Rows::Row };
    (@fast -) => { $crate::code::ops::Fast::None };
    (@fast arith) => { $crate::code::ops::Fast::Arith };
    (@fast order) => { $crate::code::ops::Fast::Order };
    (@fast test) => { $crate::code::ops::Fast::Test };
    (@fast bits) => { $crate::code::ops::Fast::Bits };
    (@fast move) => { $crate::code::ops::Fast::Move };
    // the register forms of one generated arm as one pattern
    (@forms $f:ident $how:tt $ty:ident [$($hit:ident)*]) => { $crate::ops!(@hits $how $ty [$($hit)*]) };
    (@forms arith $how:tt $ty:ident [$($hit:ident)*] $n:ident arith $($rest:tt)*) => { $crate::ops!(@forms arith $how $ty [$($hit)* $n] $($rest)*) };
    (@forms order $how:tt $ty:ident [$($hit:ident)*] $n:ident order $($rest:tt)*) => { $crate::ops!(@forms order $how $ty [$($hit)* $n] $($rest)*) };
    (@forms test $how:tt $ty:ident [$($hit:ident)*] $n:ident test $($rest:tt)*) => { $crate::ops!(@forms test $how $ty [$($hit)* $n] $($rest)*) };
    (@forms bits $how:tt $ty:ident [$($hit:ident)*] $n:ident bits $($rest:tt)*) => { $crate::ops!(@forms bits $how $ty [$($hit)* $n] $($rest)*) };
    (@forms move $how:tt $ty:ident [$($hit:ident)*] $n:ident move $($rest:tt)*) => { $crate::ops!(@forms move $how $ty [$($hit)* $n] $($rest)*) };
    (@forms $f:ident $how:tt $ty:ident [$($hit:ident)*] $n:ident $k:tt $($rest:tt)*) => { $crate::ops!(@forms $f $how $ty [$($hit)*] $($rest)*) };
    // the register forms of each generated arm gathered row by row then handed to `$m`
    (@simple $m:ident $pass:tt $a:tt $o:tt $t:tt $b:tt $v:tt ;) => { $m! { arith $a order $o test $t bits $b move $v $pass } };
    (@simple $m:ident $pass:tt [$($a:tt)*] $o:tt $t:tt $b:tt $v:tt ; $r:ident $s:tt arith $($rest:tt)*) => { $crate::ops!(@simple $m $pass [$($a)* ($r $s)] $o $t $b $v ; $($rest)*) };
    (@simple $m:ident $pass:tt $a:tt [$($o:tt)*] $t:tt $b:tt $v:tt ; $r:ident $s:tt order $($rest:tt)*) => { $crate::ops!(@simple $m $pass $a [$($o)* ($r $s)] $t $b $v ; $($rest)*) };
    (@simple $m:ident $pass:tt $a:tt $o:tt [$($t:tt)*] $b:tt $v:tt ; $r:ident $s:tt test $($rest:tt)*) => { $crate::ops!(@simple $m $pass $a $o [$($t)* ($r $s)] $b $v ; $($rest)*) };
    (@simple $m:ident $pass:tt $a:tt $o:tt $t:tt [$($b:tt)*] $v:tt ; $r:ident $s:tt bits $($rest:tt)*) => { $crate::ops!(@simple $m $pass $a $o $t [$($b)* ($r $s)] $v ; $($rest)*) };
    (@simple $m:ident $pass:tt $a:tt $o:tt $t:tt $b:tt [$($v:tt)*] ; $r:ident $s:tt move $($rest:tt)*) => { $crate::ops!(@simple $m $pass $a $o $t $b [$($v)* ($r $s)] ; $($rest)*) };
    (@simple $m:ident $pass:tt $a:tt $o:tt $t:tt $b:tt $v:tt ; $r:ident $s:tt $f:tt $($rest:tt)*) => { $crate::ops!(@simple $m $pass $a $o $t $b $v ; $($rest)*) };
    (@op $ty:ident -) => { None };
    (@op $ty:ident $f:ident) => { Some($ty::$f) };
    // a runtime op's fused native and the paths a row may leave out
    (@fuse -) => { None };
    (@fuse $n:literal) => { Some($n) };
    (@has -) => { false };
    (@has $f:ident) => { true };
    (@ownfast - $h:ident $a:ident) => { None };
    (@ownfast $f:ident $h:ident $a:ident) => { $f($h, $a) };
    (@ownfold - $a:ident) => { None };
    (@ownfold $f:ident $a:ident) => { $f($a) };
    // a register or internal form declares its stack op its rows and its arm
    (@info $ty:ident $class:ident $name:ident $stack:tt $rows:tt $fast:tt) => {
        $crate::code::ops::OpInfo {
            name: stringify!($name), class: $crate::code::ops::Class::$class, pops: $crate::code::ops::Count::Run, pushes: $crate::code::ops::Count::Run,
            operand: $crate::code::ops::Operand::None, arm: $crate::code::ops::Arm::None, flow: $crate::code::ops::Flow::Next, edge: $crate::code::ops::Edge::None,
            spill: $crate::code::ops::Spill::None, operator: $crate::code::ops::Operator::None, fold: false, form: None, branch: None,
            stack: $crate::ops!(@op $ty $stack), rows: $crate::ops!(@rows $rows), fast: $crate::ops!(@fast $fast),
        }
    };
    (
        table $ty:ident $(: $($derive:ident),+)?;
        stack { $( $s:ident $pops:tt $pushes:tt $operand:ident $arm:tt $flow:tt $edge:tt $spill:tt $operator:tt $fold:tt $form:tt $branch:tt ; )* }
        register { $r0:ident $r0stack:tt $r0rows:tt $r0fast:tt ; $( $r:ident $rstack:tt $rrows:tt $rfast:tt ; )* }
        internal { $( $i:ident $istack:tt $irows:tt ; )* }
    ) => {
        // one byte per op so bytecode stays stable
        #[derive(Clone, Copy, PartialEq $($(, $derive)+)?)]
        #[repr(u8)]
        pub enum $ty { $($s,)* $r0, $($r,)* $($i,)* }

        impl $ty {
            /* every op but the internal forms at the index of the byte it encodes to */
            pub const ALL: [$ty; $crate::ops!(@len $($s)* $r0 $($r)*)] = [$($ty::$s,)* $ty::$r0, $($ty::$r,)*];

            /* every op of the table with the internal forms past `ALL` */
            pub const EVERY: [$ty; $crate::ops!(@len $($s)* $r0 $($r)* $($i)*)] = [$($ty::$s,)* $ty::$r0, $($ty::$r,)* $($ty::$i,)*];

            /* the columns of each op at the index of its byte */
            pub const INFO: [$crate::code::ops::OpInfo<$ty>; $crate::ops!(@len $($s)* $r0 $($r)* $($i)*)] = [
                $( $crate::code::ops::OpInfo {
                    name: stringify!($s), class: $crate::code::ops::Class::Stack, pops: $crate::ops!(@count $pops), pushes: $crate::ops!(@count $pushes),
                    operand: $crate::ops!(@operand $operand), arm: $crate::ops!(@arm $arm), flow: $crate::ops!(@flow $flow), edge: $crate::ops!(@edge $edge),
                    spill: $crate::ops!(@spill $spill), operator: $crate::ops!(@operator $operator), fold: $crate::ops!(@fold $fold),
                    form: $crate::ops!(@op $ty $form), branch: $crate::ops!(@op $ty $branch), stack: None, rows: $crate::code::ops::Rows::None,
                    fast: $crate::code::ops::Fast::None,
                }, )*
                $crate::ops!(@info $ty Register $r0 $r0stack $r0rows $r0fast),
                $( $crate::ops!(@info $ty Register $r $rstack $rrows $rfast), )*
                $( $crate::ops!(@info $ty Internal $i $istack $irows -), )*
            ];

            /* a hash of every column in order so a stale blob never restores */
            pub const HASH: u64 = {
                let (mut h, mut k) = ($crate::code::ops::FNV, 0);
                while k < Self::INFO.len() {
                    let e = Self::INFO[k];
                    h = $crate::code::ops::fnv(h, &[e.name.len() as u8]);
                    h = $crate::code::ops::fnv(h, e.name.as_bytes());
                    h = $crate::code::ops::fnv(h, &[e.class as u8, e.pops.code(), e.pushes.code(), e.operand as u8, e.arm as u8, e.flow as u8, e.edge as u8,
                        e.spill as u8, e.operator as u8, e.fold as u8, Self::byte(e.form), Self::byte(e.branch), Self::byte(e.stack), e.rows as u8, e.fast as u8]);
                    k += 1;
                }
                h
            };

            /* the byte of an op a column names for the hash */
            const fn byte(op: Option<$ty>) -> u8 { match op { Some(op) => op as u8, None => u8::MAX } }

            /* its columns */
            #[inline]
            pub const fn info(self) -> $crate::code::ops::OpInfo<$ty> { Self::INFO[self as usize] }

            /* the values it pops and pushes falling through and none when only the run knows */
            pub fn effect(self, operand: u16, chunk: &$crate::code::SSAChunk) -> Option<(u16, u16)> {
                let _ = (operand, chunk);
                // one arm per op as its columns count so each reads its operand once
                match self {
                    $( $ty::$s => match ($crate::ops!(@of $pops operand chunk), $crate::ops!(@of $pushes operand chunk)) { (Some(p), Some(q)) => Some((p, q)), _ => None }, )*
                    _ => None,
                }
            }

            /* the values it pops and pushes when its operand alone sets them */
            pub fn plain_effect(self, operand: u16) -> Option<(u16, u16)> {
                let _ = operand;
                match self {
                    $( $ty::$s => match ($crate::ops!(@plain $pops operand), $crate::ops!(@plain $pushes operand)) { (Some(p), Some(q)) => Some((p, q)), _ => None }, )*
                    _ => None,
                }
            }

            /* its register form read alone so a run reads no whole row */
            #[inline]
            pub const fn form(self) -> Option<$ty> {
                const T: [Option<$ty>; 256] = { let mut t = [None; 256]; let mut k = 0; while k < $ty::INFO.len() { t[k] = $ty::INFO[k].form; k += 1; } t };
                T[self as usize]
            }

            /* its compare and branch form read alone */
            #[inline]
            pub const fn branch(self) -> Option<$ty> {
                const T: [Option<$ty>; 256] = { let mut t = [None; 256]; let mut k = 0; while k < $ty::INFO.len() { t[k] = $ty::INFO[k].branch; k += 1; } t };
                T[self as usize]
            }

            /* the operator it applies read alone */
            #[inline]
            pub const fn operator(self) -> $crate::code::ops::Operator {
                const T: [$crate::code::ops::Operator; 256] = { let mut t = [$crate::code::ops::Operator::None; 256]; let mut k = 0; while k < $ty::INFO.len() { t[k] = $ty::INFO[k].operator; k += 1; } t };
                T[self as usize]
            }

            /* the way to its target read alone */
            #[inline]
            pub const fn edge(self) -> $crate::code::ops::Edge {
                const T: [$crate::code::ops::Edge; 256] = { let mut t = [$crate::code::ops::Edge::None; 256]; let mut k = 0; while k < $ty::INFO.len() { t[k] = $ty::INFO[k].edge; k += 1; } t };
                T[self as usize]
            }

            /* the rows a register form reads and writes read alone */
            #[inline]
            pub const fn rows(self) -> $crate::code::ops::Rows {
                const T: [$crate::code::ops::Rows; 256] = { let mut t = [$crate::code::ops::Rows::None; 256]; let mut k = 0; while k < $ty::INFO.len() { t[k] = $ty::INFO[k].rows; k += 1; } t };
                T[self as usize]
            }

            /* its operand is an instruction index so moving code must retarget it */
            pub const fn is_jump(self) -> bool { $crate::ops!(@jumps (is self) $ty [] $($s $operand)*) }

            /* a register form only the lowering writes */
            pub const fn is_register(self) -> bool { self as u8 >= Self::$r0 as u8 }

            /* its operand names a variable's slot */
            pub const fn names_slot(self) -> bool { $crate::ops!(@slots (is self) $ty [] $($s $operand)*) }

            /* its operand names an attribute */
            pub const fn names_attr(self) -> bool { $crate::ops!(@attrs (is self) $ty [] $($s $operand)*) }

            /* a binary operator over the two values on top */
            pub const fn is_binary(self) -> bool { $crate::ops!(@binary (is self) $ty [] $($s $operator)*) }

            /* a binary operator that folds constant operands at compile time */
            pub const fn folds_binary(self) -> bool { $crate::ops!(@folds (is self) $ty [] $($s $operator $fold)*) }

            /* control never reaches the next op as it jumps away or leaves the frame */
            pub const fn ends_flow(self) -> bool { $crate::ops!(@ends (is self) $ty [] $($s $flow)*) }

            /* a stack op that pops and pushes nothing */
            pub const fn unstacked(self) -> bool { $crate::ops!(@still (is self) $ty [] $($s $pops $pushes)*) }
        }

        // the name of each op for a Debug the table's crate writes apart
        #[allow(unused_macros)]
        macro_rules! names { ($e:expr) => { match $e { $($ty::$s => stringify!($s),)* $ty::$r0 => stringify!($r0), $($ty::$r => stringify!($r),)* $($ty::$i => stringify!($i),)* } }; }
        #[allow(unused_imports)]
        pub(crate) use names;

        // the binary ops that fold as one pattern a consumer's match takes as written
        #[allow(unused_macros)]
        macro_rules! folds { () => { $crate::ops!(@folds pattern $ty [] $($s $operator $fold)*) }; }
        #[allow(unused_imports)]
        pub(crate) use folds;

        // the ops naming an attribute as one pattern for the same reason
        #[allow(unused_macros)]
        macro_rules! attrs { () => { $crate::ops!(@attrs pattern $ty [] $($s $operand)*) }; }
        #[allow(unused_imports)]
        pub(crate) use attrs;

        // each generated arm's register forms and stack ops handed to a consumer's match
        #[allow(unused_macros)]
        macro_rules! simple { ($m:ident $pass:tt) => { $crate::ops!(@simple $m $pass [] [] [] [] [] ; $r0 $r0stack $r0fast $($r $rstack $rfast)*) }; }
        #[allow(unused_imports)]
        pub(crate) use simple;

        // the register forms of one generated arm as one pattern a slow path's match takes
        #[allow(unused_macros)]
        macro_rules! forms { ($f:ident) => { $crate::ops!(@forms $f pattern $ty [] $r0 $r0fast $($r $rfast)*) }; }
        #[allow(unused_imports)]
        pub(crate) use forms;
    };
    // a runtime's own table whose ops the loop runs past the engine's
    (
        table $ty:ident $(: $($derive:ident),+)?;
        own { $( $o:ident $pops:literal $fuse:tt $fast:tt $slow:ident $fold:tt ; )* }
    ) => {
        // one byte per op which a RuntimeOp carries in the low byte of its operand
        #[derive(Clone, Copy, PartialEq $($(, $derive)+)?)]
        #[repr(u8)]
        pub enum $ty { $($o,)* }

        impl $ty {
            /* every op at the index of its byte */
            pub const ALL: [$ty; $crate::ops!(@len $($o)*)] = [$($ty::$o,)*];

            /* the columns of each op at the index of its byte */
            pub const OPS: [$crate::lang::OwnOp; $crate::ops!(@len $($o)*)] = [$(
                $crate::lang::OwnOp { name: stringify!($o), pops: $pops, fuse: $crate::ops!(@fuse $fuse), folds: $crate::ops!(@has $fold) },
            )*];

            /* the operand of the RuntimeOp that runs it with its pops in the high byte */
            pub const fn operand(self) -> u16 { self as u16 | (Self::OPS[self as usize].pops as u16) << 8 }

            /* what op `op` gives of `args` once its fast path declined */
            #[doc(hidden)]
            pub fn slow_of(cx: &mut $crate::lang::Cx<'_, '_>, op: u8, args: &[$crate::value::Val]) -> Result<$crate::value::Val, $crate::value::VmErr> {
                let _ = (&cx, args);
                $( if op == $ty::$o as u8 { return $slow(cx, args); } )*
                Err($crate::lang::OpSet::lacks())
            }

            /* the constant op `op` folds `args` to */
            #[doc(hidden)]
            pub fn fold_of(op: u8, args: &[$crate::value::Val]) -> Option<$crate::value::Val> {
                let _ = args;
                $( if op == $ty::$o as u8 { return $crate::ops!(@ownfold $fold args); } )*
                None
            }
        }

        impl $crate::lang::OpTable for $ty {
            const COUNT: u8 = $crate::ops!(@len $($o)*) as u8;
            const HASH: u64 = $crate::lang::OpSet::hash_of(&$ty::OPS);
            const SET: &'static $crate::lang::OpSet = &$crate::lang::OpSet { ops: &$ty::OPS, hash: $crate::lang::OpSet::hash_of(&$ty::OPS), slow: $ty::slow_of, fold: $ty::fold_of };

            #[inline(always)]
            fn fast(op: u8, args: &[$crate::value::Val], heap: &mut $crate::value::HeapPool) -> Option<$crate::value::Val> {
                let _ = (args, &heap);
                $( if op == $ty::$o as u8 { return $crate::ops!(@ownfast $fast heap args); } )*
                None
            }
        }
    };
}
