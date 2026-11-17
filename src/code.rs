pub use crate::syntax::chunk::{SSAChunk, Instruction, Value, ImportKind, ImportEntry, NativeSite, SITE_TABLE, native_site};
pub use crate::syntax::diag::{Diagnostic, Lines};
pub use crate::syntax::parser::{Parser, unpack_call, INPLACE, ITER_VALUE, ITER_AS_IS, YIELD_FROM, Arms, Builder, Parsed, Unit, build};
pub use crate::syntax::ssa::{SsaName, ssa_strip, param_base_name, COMP_ARG, SPREAD_BASES};
pub use fold::constant_fold;
pub use crate::vm::{Binding, BodyKind, FnScope, Kind, ParamKind};

internal! {
    /* the passes after SSA that run between parse and boot and touch no VM state */
    fold;
}

/* the columns an opcode table declares and the `ops!` macro that declares one */
pub mod ops;
mod table;
pub use table::OpCode;
pub(crate) use table::{attrs, folds, forms, simple};
pub use ops::{Arm, Class, Count, Edge, Fast, Flow, OpInfo, Operand, Operator, Rows, Spill};

// written here since a Debug derived inside the table reorders the built code
impl core::fmt::Debug for OpCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str(table::names!(self)) }
}

// `src` written in spelling `S` lexed parsed and folded into a chunk and its errors
pub fn compile_as<S: crate::syntax::lexer::Spelling>(src: &str) -> (SSAChunk, alloc::vec::Vec<Diagnostic>) {
    let (tokens, lex_errs) = crate::syntax::lexer::lex_as::<S>(src);
    let resolver = alloc::boxed::Box::new(crate::deps::NoopResolver);
    let mut parser = Parser::<_, S>::spelled(src, tokens.into_iter(), resolver);
    // the lexer's errors lead the report as they do through a front end
    parser.errors.extend(lex_errs.into_iter().map(|e| Diagnostic { start: e.start, end: e.end, msg: e.msg.into() }));
    let (mut chunk, errors) = parser.parse();
    constant_fold(&mut chunk, &S::NATIVES.policy);
    (chunk, errors)
}

// `src` through front end `front` parsed and folded into a chunk and its errors
pub fn compile(front: &crate::lang::Front, src: &str) -> (SSAChunk, alloc::vec::Vec<Diagnostic>) {
    match front.program(src, alloc::boxed::Box::new(crate::deps::NoopResolver)) {
        Ok(chunk) => {
            let mut chunk = alloc::rc::Rc::unwrap_or_clone(chunk);
            constant_fold(&mut chunk, front.policy);
            (chunk, alloc::vec::Vec::new())
        }
        Err(errors) => (SSAChunk::default(), errors),
    }
}

// each body's scope and parameters resolved as the VM resolves them without running
pub struct Program<'c> { vm: crate::vm::VM<'c>, bodies: alloc::vec::Vec<(&'c SSAChunk, BodyKind, alloc::vec::Vec<Binding>)> }

impl<'c> Program<'c> {
    pub fn new(chunk: &'c SSAChunk) -> Self {
        let mut vm = crate::vm::VM::new(chunk);
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
    pub(crate) fn of(i: crate::syntax::chunk::Instruction) -> Self { Self { op: i.opcode, x: 0, a: i.operand, b: 0, c: 0 } }
}
