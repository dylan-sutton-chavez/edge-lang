pub use crate::engine::syntax::chunk::{SSAChunk, Value, ImportKind, ImportEntry, native_site, Notes, Node, NodeKind, Annotation, Annotated, Decl, DeclKind};
#[cfg(feature = "unstable-internals")]
pub use crate::engine::syntax::chunk::{Instruction, NativeSite};
pub use crate::engine::syntax::diag::Diagnostic;
pub use crate::engine::syntax::parser::{ITER_VALUE, Builder, Parsed, Recorded, Unit, build};
#[cfg(feature = "unstable-internals")]
pub use crate::engine::syntax::parser::Parser;
pub use crate::engine::syntax::ssa::SPREAD_BASES;
pub use fold::constant_fold;
#[cfg(feature = "unstable-internals")]
use crate::engine::vm::{Binding, BodyKind, FnScope, ParamKind};

/* the passes after SSA that run between parse and boot and touch no VM state */
pub(crate) mod fold;

/* the columns an opcode table declares and the `ops!` macro that declares one */
pub mod ops;
mod table;
pub use table::OpCode;
pub(crate) use table::{attrs, folds, forms, simple};
pub use ops::Rows;
pub use ops::Operand;
pub use ops::Operator;
pub(crate) use ops::{Arm, Flow, Spill};

// written here since a Debug derived inside the table reorders the built code
impl core::fmt::Debug for OpCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str(table::names!(self)) }
}

/* the opcode a row or a builder emits for operator `op` or none for one it cannot apply */
pub(crate) const fn opcode_of(op: crate::engine::lang::Op) -> Option<OpCode> {
    use crate::engine::lang::{Cmp, Op};
    use crate::engine::value::foreign::{BinOp, UnOp};
    Some(match op {
        Op::Bin(b) => match b {
            BinOp::Add => OpCode::Add, BinOp::Sub => OpCode::Sub, BinOp::Mul => OpCode::Mul, BinOp::TrueDiv => OpCode::Div,
            BinOp::FloorDiv => OpCode::FloorDiv, BinOp::Mod => OpCode::Mod, BinOp::Pow => OpCode::Pow, BinOp::MatMul => OpCode::MatMul,
            BinOp::And => OpCode::BitAnd, BinOp::Or => OpCode::BitOr, BinOp::Xor => OpCode::BitXor, BinOp::Shl => OpCode::Shl, BinOp::Shr => OpCode::Shr,
        },
        Op::Cmp(c) => match c { Cmp::Eq => OpCode::Eq, Cmp::Ne => OpCode::NotEq, Cmp::Lt => OpCode::Lt, Cmp::Le => OpCode::LtEq, Cmp::Gt => OpCode::Gt, Cmp::Ge => OpCode::GtEq },
        Op::In => OpCode::In,
        Op::NotIn => OpCode::NotIn,
        Op::Is => OpCode::Is,
        Op::IsNot => OpCode::IsNot,
        Op::Not => OpCode::Not,
        Op::Unary(UnOp::Neg) => OpCode::Minus,
        Op::Unary(UnOp::Pos) => OpCode::Pos,
        Op::Unary(UnOp::Invert) => OpCode::BitNot,
        _ => return None,
    })
}

// `src` written in spelling `S` lexed parsed and folded into a chunk and its errors
#[cfg(feature = "unstable-internals")]
pub fn compile_as<S: crate::engine::syntax::lexer::Spelling>(src: &str) -> (SSAChunk, alloc::vec::Vec<Diagnostic>) {
    let (tokens, lex_errs) = crate::engine::syntax::lexer::lex_as::<S>(src);
    let resolver = alloc::boxed::Box::new(crate::engine::deps::NoopResolver);
    let mut parser = Parser::<_, S>::spelled(src, tokens.into_iter(), resolver);
    // the lexer's errors lead the report as they do through a front end
    parser.errors.extend(lex_errs.into_iter().map(|e| Diagnostic { start: e.start, end: e.end, msg: e.msg.into() }));
    let (mut chunk, errors) = parser.parse();
    constant_fold(&mut chunk, &S::NATIVES.policy);
    (chunk, errors)
}

// `compile_as` again keeping each instruction's span in every body and the outline and annotations on the root
#[cfg(feature = "unstable-internals")]
pub fn read_as<S: crate::engine::syntax::lexer::Spelling>(src: &str) -> (SSAChunk, alloc::vec::Vec<Diagnostic>) { compile_as::<Recorded<S>>(src) }

// `src` through front end `front` parsed and folded into a chunk and its errors
#[cfg(feature = "unstable-internals")]
pub fn compile(front: &crate::engine::lang::Front, src: &str) -> (SSAChunk, alloc::vec::Vec<Diagnostic>) {
    match front.compile(src) {
        Ok(chunk) => (chunk, alloc::vec::Vec::new()),
        Err(errors) => (SSAChunk::default(), errors),
    }
}

// each body's scope and parameters resolved as the VM resolves them without running
#[cfg(feature = "unstable-internals")]
pub struct Program<'c> { vm: crate::engine::vm::VM<'c>, bodies: alloc::vec::Vec<(&'c SSAChunk, BodyKind, alloc::vec::Vec<Binding>)> }

#[cfg(feature = "unstable-internals")]
impl<'c> Program<'c> {
    pub fn new(chunk: &'c SSAChunk) -> Self {
        let mut vm = crate::engine::vm::VM::new(chunk);
        let mut found = vm.body_bindings(chunk);
        // each body in tree order found by the id the machine gave it
        let mut bodies = alloc::vec::Vec::with_capacity(found.len());
        each_body(chunk, &mut |c| if let Some((k, b)) = found.remove(&c.id.get()) { bodies.push((c, k, b)); });
        Self { vm, bodies }
    }
    // none for the module body
    pub fn scope_of(&self, body: &SSAChunk) -> Option<(usize, &FnScope)> {
        match self.body(body)?.1 { BodyKind::Function(fi) => Some((fi, &self.vm.fn_scope[fi])), _ => None }
    }
    // which body `body` is with each name's binding or none for another program's chunk
    pub fn bindings_of(&self, body: &SSAChunk) -> Option<(BodyKind, &[Binding])> { self.body(body).map(|b| (b.1, b.2.as_slice())) }
    // the entry of `body` found by the body itself
    fn body(&self, body: &SSAChunk) -> Option<&(&'c SSAChunk, BodyKind, alloc::vec::Vec<Binding>)> { self.bodies.iter().find(|b| core::ptr::eq(b.0, body)) }
    // the kind and slot of each parameter in order
    pub fn params_of(&self, fi: usize) -> &[(ParamKind, usize)] { self.vm.params_of(fi) }
}

/* `chunk` and every body under it in the order the tree holds them */
#[cfg(feature = "unstable-internals")]
fn each_body<'c>(chunk: &'c SSAChunk, f: &mut impl FnMut(&'c SSAChunk)) {
    f(chunk);
    for k in &chunk.classes { each_body(k, f); }
    for d in &chunk.functions { each_body(&d.1, f); }
    for e in &chunk.imports { if let ImportKind::Code(sub) = &e.kind { each_body(sub, f); } }
}

/* one instruction as the VM runs it with an opcode and up to three operands */
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ins {
    pub op: OpCode,
    pub x: u8,
    pub a: u16,
    pub b: u16,
    pub c: u16,
}

impl Ins {
    pub(crate) fn of(i: crate::engine::syntax::chunk::Instruction) -> Self { Self { op: i.opcode, x: 0, a: i.operand, b: 0, c: 0 } }
}
