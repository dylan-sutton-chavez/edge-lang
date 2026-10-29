use super::*;

impl Block {

    /* `at` itself or the section op whose head and stub lie past the end */
    #[inline]
    pub(crate) fn origin(&self, at: usize) -> usize {
        match self.grown.get().filter(|_| self.moved) { Some(g) => g.moved.iter().find(|m| (m.0 as usize..m.1 as usize).contains(&at)).map_or(at, |m| m.2 as usize), _ => at }
    }

    /* the section holding the op the moved head or stub at `at` was laid for */
    #[cold]
    #[inline(never)]
    pub(crate) fn moved_holding(&self, at: usize) -> Option<&Section> {
        let o = self.origin(at);
        (o != at).then(|| self.sections.iter().find(|s| (s.start as usize..s.end as usize).contains(&o))).flatten()
    }

    /* whether `at` lies in the code or a section body rather than past them */
    pub(crate) fn plain_at(&self, at: usize) -> bool { self.sections.first().is_none_or(|s| at < s.start as usize) || self.body_at(at).is_some() }

    /* the section whose body holds `at` and none for a head or stub */
    pub(crate) fn body_at(&self, at: usize) -> Option<&Section> {
        let k = self.sections.partition_point(|s| s.start as usize <= at).checked_sub(1)?;
        self.sections.get(k).filter(|s| at < s.end as usize)
    }

    /* the section holding `at` and the position in it that `at` stands for */
    pub(crate) fn holding(&self, at: usize) -> Option<(&Section, usize)> {
        // heads or stubs past the end lie outside every section so one inside is neither
        let find = |at: usize| { let k = self.sections.partition_point(|s| s.start as usize <= at).checked_sub(1)?; self.sections.get(k).filter(|s| at < s.end as usize).map(|s| (s, at)) };
        find(at).or_else(|| { let o = self.origin(at); (o != at).then(|| find(o)).flatten() })
    }

    /* what it keeps for its growths made when first asked */
    pub(crate) fn grown(&self) -> &Grown { self.grown.get_or_init(Default::default) }

    /* the heads laid past the end for calls of grown code and none for most */
    pub(crate) fn displaced(&self) -> &[(u32, u32)] { self.grown.get().map_or(&[], |g| &g.displaced[..]) }

}

#[derive(Clone)]
pub(crate) struct Block {
    pub sections: Box<[Section]>,
    /* constants the sections read and in `ranges` each growth's frame start and count of them */
    pub extra: Box<[Val]>,
    pub ranges: Box<[(usize, usize)]>,
    /* what each section was laid from by binding so a growth lays more beside them */
    pub callees: Vec<(u16, Callee<'static>)>,
    /* section layout past the end that a lineage shares as each code keeps earlier places */
    pub lineage: u32,
    /* what its growths keep made once a head first asks for one */
    pub grown: OnceCell<Box<Grown>>,
    /* the scope `REBOUND` value when every section's binding last held its function */
    pub binds: Cell<u32>,
    /* stub tables per register call meeting several functions slotting each function's section by heap index */
    pub alt_tab: Box<[(Val, u32)]>,
    /* the code's loops over generator sections with section and head so a stopped run resumes */
    pub loops: Box<[(u32, u32, u32, u32)]>,
    /* whether a head was moved past the end which section walks treat as its op */
    pub moved: bool,
    /* per method table slot the last class with the section it found and its version */
    pub by_class: Box<[Cell<(Val, u32, u32)>]>,
}

#[derive(Clone)]
/* a function laid once as a section held so a collection never lends its slot */
pub(crate) struct Section {
    pub start: u32,
    pub end: u32,
    pub link: u16,
    // the function it was laid from or for a `by_body` section the last one entered
    pub value: Cell<Val>,
    // the module its function lies in
    pub module: usize,
    // the binding holding the function and its module which for an import is the importer's
    pub global: u16,
    pub bound: usize,
    pub fi: usize,
    // per instruction the body's compiler instruction it came from where an error inside points
    pub orig: Box<[u32]>,
    // a class's init with the version it was laid for and its instance attribute room
    pub ctor: Option<(Cell<u32>, usize, Val)>,
    // for a `by_body` section its arity and cell registers which each call fills with defaults
    pub by_body: Option<(u8, Box<[u16]>)>,
    // for a body making functions the register of each slot their captures read else empty
    pub slots: alloc::rc::Rc<[u16]>,
    // last receiver class and version valid until a class changes or a slot is lent
    pub guard: Cell<(Val, u32, Val)>,
    // the instruction and head of the call whose return a generator's `yield` hands its loop
    pub tail: Option<(u32, u32)>,
    // for a generator's section the register its loop's step comes back by
    pub step_reg: u16,
    // first of two registers noting where stacks began in a generator with `try` or `with`
    pub held: u16,
    // per `yield` the compiler instruction a suspended object resumes at and the section index
    pub steps: Box<[(u32, u32, u32, u32)]>,
    // start in `alt_tab` of a method head's other methods and the size less one
    pub table: (u32, u32),
    pub arity: u8,
    // 1 for `*args` alone or 2 for `**kwargs` alone which a spreading call enters packed
    pub packs: u8,
    // as the callee's `on_class` for a method called on its class itself
    pub on_class: u8,
    // defaults of its last parameters which a call entering from another head's table leaves out
    pub defaults: Box<[Val]>,
}

/* a layable function whose frame is names then constants then `None` `True` `False` then temporaries */
#[derive(Clone)]
pub(crate) struct Callee<'p> {
    pub value: Val,
    pub fi: usize,
    // a closure's captured cells by slot which every call of this function shares
    pub cells: Vec<(usize, Val)>,
    // defaults of its trailing parameters and how many parameters a call must pass
    pub defaults: Vec<Val>,
    pub required: usize,
    pub body: Rc<Code>,
    pub arity: usize,
    pub names: usize,
    pub consts: Vec<Val>,
    pub module: usize,
    /* for an imported name the module whose binding a call reads it through */
    pub bound: u16,
    /* what few callees have kept apart so the many without it stay small */
    pub rare: Option<Box<Rare>>,
    // its parameters' names which a keyword call matches
    pub params: &'p [alloc::string::String],
    // how many are positional only which no keyword call matches
    pub posonly: usize,
    // its body's constants which name the parameters a keyword call in it passes
    pub literals: &'p [crate::syntax::chunk::Value],
    /* whether any function of its body runs the section each call supplying cells and defaults */
    pub by_body: bool,
    /* whether a `by_body` section follows this closure's own for siblings once the first is gone */
    pub twin: bool,
    /* the `*args` and `**kwargs` parameters past the positionals which a call packs the rest into */
    pub star: Option<usize>,
    pub dstar: Option<usize>,
    /* the slots its body keeps in cells which a call wraps as it enters */
    pub cellvars: Vec<usize>,
    /* a generator one `for` steps each `yield` setting its variable and the end leaving it */
    pub generator: bool,
    /* the parameters a call may pass by position before any lone `*` */
    pub positional: usize,
    /* 1 function on its class 4 held 7 bound method else 0 */
    pub on_class: u8,
    /* for a class deriving from an exception whose instance keeps its call's arguments as `args` */
    pub exc: bool,
}

/* what a callee has apart */
#[derive(Clone, Default)]
pub(crate) struct Rare {
    // for a method the site that found it which its call's stub starts with
    pub site: Option<crate::vm::sites::Site>,
    /* a class's init laid in its place with version and room handing back the instance */
    pub ctor: Option<(u32, usize, Val)>,
}

impl Callee<'_> {

    pub(crate) fn site(&self) -> Option<crate::vm::sites::Site> { self.rare.as_ref().and_then(|r| r.site) }

    pub(crate) fn ctor(&self) -> Option<(u32, usize, Val)> { self.rare.as_ref().and_then(|r| r.ctor) }

    /* what it has apart made when first written */
    pub(crate) fn rare_mut(&mut self) -> &mut Rare { self.rare.get_or_insert_with(Default::default) }

    pub(crate) fn set_site(&mut self, v: Option<crate::vm::sites::Site>) { if v.is_some() || self.rare.is_some() { self.rare_mut().site = v; } }

    pub(crate) fn set_ctor(&mut self, v: Option<(u32, usize, Val)>) { if v.is_some() || self.rare.is_some() { self.rare_mut().ctor = v; } }
}

/* whether temporary `r` is written or its block ends from `k` before a read */
fn dead_past(ins: &[Ins], k: usize, r: u16) -> bool {
    for i in ins.get(k..).unwrap_or_default() {
        if crate::vm::block::rows(i).is_some_and(|(_, s)| s.contains(&r)) { return false; }
        match crate::vm::block::role(i) {
            Some((dest, reads, jump)) => {
                if reads.iter().any(|&f| crate::vm::block::field(i, f) == r) { return false; }
                if dest && i.a == r || jump { return true; }
            }
            None => return matches!(i.op, OpCode::Jump | OpCode::ReturnValue | OpCode::Raise),
        }
        if matches!(i.op, OpCode::ReturnR) { return true; }
    }
    true
}

/* whether moving `regs` into the row from `b` last first overwrites none still to move */
pub(super) fn moves_fit(regs: &[u16], b: u16) -> bool {
    (0..regs.len()).all(|q| !regs[..q].contains(&(b + q as u16)))
}

/* whether row `r` is written or the frame ends from `k` before reads or jumps */
fn row_dead(ins: &[Ins], k: usize, r: u16) -> bool {
    for i in ins.get(k..).unwrap_or_default() {
        if crate::vm::block::rows(i).is_some_and(|(_, s)| s.contains(&r)) { return false; }
        match crate::vm::block::role(i) {
            Some((dest, reads, jump)) => {
                if reads.iter().any(|&f| crate::vm::block::field(i, f) == r) || jump { return false; }
                if dest && i.a == r || i.op == OpCode::ReturnR { return true; }
            }
            None => return false,
        }
    }
    false
}

/* whether `c` has a `try` whose handlers a recursion via a head's table would share */
fn guarded(c: &Callee) -> bool {
    c.body.ins.iter().any(|i| matches!(i.op, OpCode::SetupExcept | OpCode::SetupFinally))
}

/* whether the section wanted for w serves call c called the same way */
fn same_laid(w: &Callee, c: &Callee) -> bool {
    w.on_class == c.on_class && w.ctor().map(|c| c.2.0) == c.ctor().map(|c| c.2.0)
}

/* whether a spreading call may enter `c` when plain or taking `*args` or `**kwargs` alone */
pub(super) fn spreads(c: &Callee, kind: u8) -> bool {
    let fits = if kind == 2 { c.star.is_none() && c.dstar == Some(0) } else { c.dstar.is_none() && (c.star.is_none() && c.arity == c.positional && c.defaults.is_empty() || c.star == Some(0)) };
    c.ctor().is_none() && !c.by_body && fits && (c.star.is_none() && c.dstar.is_none() || c.arity == 1)
}

/* the function the register call at `j` of `code` found */
pub(crate) fn value_of(code: &Code, j: usize) -> Option<Val> {
    match code.cache.site(j) { crate::vm::sites::Site::Call { callee, .. } => Some(callee), _ => None }
}

/* the site of the method call at `j` of `code` when it found a method */
pub(crate) fn method_of(code: &Code, j: usize) -> Option<crate::vm::sites::Site> {
    let site = code.cache.site(j);
    matches!(site, crate::vm::sites::Site::Method { .. }).then_some(site)
}

/* an operand of a section call patched in as the sections are laid */
#[derive(Clone, Copy)]
pub(super) enum Prep { Param(usize, usize), Const(Val), GenReg(usize), GenVal(usize), GenResume(usize), GenEntry(usize) }

/* a callee to lay as a section found through a binding or a compiler instruction */
#[derive(Clone, Copy)]
pub(crate) enum Want { Global(usize, u16), Held(usize, u16), Spread(usize, u16), Method(u32), Value(u32), Nested(usize, u32, Option<crate::vm::sites::Site>, u16), NestedValue(usize, u32, Option<Val>), Alt(Option<usize>, u32, u32), MethodAlt(Option<usize>, u32, u32), Func(Val, bool), Celled(Val), Gen(usize, u16), GenFunc(Val), GenAt(u32), Body(u32), BodyOf(Val), Made(u32), MadeIn(usize, u16), GenBody(u32) }

impl Callee<'_> {

    /* the module of the binding a call reads the function through */
    pub(crate) fn bound_module(&self) -> usize { if self.bound == u16::MAX { self.module } else { self.bound as usize } }
}

/* what laying the sections carries from one section to the next */
struct LayState {
    next: usize,
    extra: Vec<Val>,
    uses: Vec<(usize, u8, usize)>,
    sections: Vec<Section>,
    gens: Vec<(u16, Option<Val>)>,
    slots: crate::util::hash::FxHashMap<u64, usize>,
    late: Vec<(usize, usize, usize, usize)>,
}

/* a `for` stepping its own generator section keeping the call as written for its stub */
pub(super) struct GenFor { pub(super) head: usize, pub(super) k: usize, pub(super) step: usize, pub(super) at: usize, pub(super) exit: usize, pub(super) target: u16, pub(super) global: Option<u16>, pub(super) call: Ins, pub(super) rest: Ins }

/* a `for` over a generator laid for it whose step the next `ForIter` lays */
pub(super) struct GenNext { pub(super) k: usize, pub(super) skip: usize, pub(super) list: Vec<u16>, pub(super) regs: Vec<u16>, pub(super) target: u16, pub(super) global: Option<u16>, pub(super) exit: usize }

impl<'p> Lower<'_, 'p> {

    /* a `for` over a binding's generator call as a section the next `ForIter` head enters */
    pub(super) fn gen_for(&mut self, i: usize, op: u16) -> Option<usize> {
        let n = op as usize;
        if n > 5 || self.chunk.is_generator || self.pending.len() != n + 1 { return None; }
        let at = |k: usize| self.src.get(i + k).copied();
        let (get, step, store) = (at(1)?, at(2)?, at(3)?);
        if get.opcode != OpCode::GetIter || get.operand == crate::syntax::parser::ITER_VALUE || step.opcode != OpCode::ForIter || store.opcode != OpCode::StoreName
            || self.target(i + 1) || self.target(i + 3) || store.operand as usize >= self.names { return None; }
        // the loop binds a local or a module binding no builtin name redirects
        let global = match self.kind(store.operand) {
            Some(Kind::Local) => None,
            Some(Kind::Global(b)) if self.builtin_flag(store.operand) == 0 => Some(b as u16),
            _ => return None,
        };
        let regs: Vec<u16> = self.pending.iter().map(|e| e.r).collect();
        // a generator a module binding holds or the one a register call met
        let g = Self::global_in(&self.out, regs[0], self.out.len());
        let callee = match g { Some(g) => (self.sections)(Want::Gen(self.facts.module, g))?, None => (self.sections)(Want::GenAt(i as u32))? };
        let body = callee.body.clone();
        // positionals past `*args` go to its tuple made here
        let p = callee.star.unwrap_or(callee.arity);
        if !crate::vm::block::gen_pure(&body.ins) || n < callee.required || callee.star.is_some() && n < p || callee.star.is_none() && n > p || callee.dstar.is_some() { return None; }
        let tuple = callee.star.map(|_| self.temp());
        let mut list: Vec<u16> = regs[1..].to_vec();
        // the binding is checked like the sections' only for a generator of this module
        let bind = g.filter(|_| callee.module == self.facts.module).unwrap_or(u16::MAX);
        let k = self.want_callee(bind, callee, list.len().min(p) + tuple.is_some() as usize)?;
        self.bind_args(k, &body, &list[..p.min(n)], None, 0);
        if let Some(t) = tuple {
            let rest = list[p..].to_vec();
            self.build_into(OpCode::BuildTuple, t, &rest, None, &[t]);
            list.truncate(p);
            list.push(t);
        }
        self.pending.clear();
        self.settle();
        // a single `yield` always resumes past itself so its register is set from the start
        if body.ins.iter().filter(|i| i.op == OpCode::Yield).count() == 1 {
            let at = self.out.len();
            self.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: 0, c: 0 });
            self.prep.extend([(at, 0, Prep::GenReg(k)), (at, 1, Prep::GenResume(k))]);
        }
        // the loop is entered at its head past its step
        let skip = self.out.len();
        self.emit(Ins { op: OpCode::Jump, x: 1, a: 0, b: 0, c: 0 });
        self.gen_next = Some(GenNext { k, skip, list, regs, target: if global.is_some() { 0 } else { store.operand }, global, exit: step.operand as usize });
        Some(2)
    }

    /* what section `k` may assume of `args` that passed functions and generators run their sections */
    fn bind_args(&mut self, k: usize, body: &Code, args: &[u16], ctx: Option<&Code>, depth: usize) {
        let out = core::mem::take(&mut self.out);
        let code: &[Ins] = ctx.map_or(&out, |c| &c.ins);
        self.bind_in(k, body, args, code, ctx.is_none(), depth);
        self.out = out;
    }

    /* `bind_args` with `args` registers of `code` which is this code's own when `own` */
    fn bind_in(&mut self, k: usize, body: &Code, args: &[u16], code: &[Ins], own: bool, depth: usize) {
        let writes = |w: &Ins, r: u16| matches!(crate::vm::block::role(w), Some((true, _, _))) && w.a == r || w.op == OpCode::ForIterR && w.b == r
            || w.op == OpCode::UnpackR || w.op == OpCode::StoreTopR && w.x == 0 && w.a == r;
        let param = |r: u16| (r as usize) < args.len() && !body.ins.iter().any(|w| writes(w, r));
        for (j, c) in body.ins.iter().enumerate() {
            // a passed function the body calls through an unwritten parameter is laid as a section
            if c.op == OpCode::CallR && !wide(*c) && param(c.a) {
                let arg = Ins { op: OpCode::CallR, x: 0, a: args[c.a as usize], b: 0, c: 0 };
                let s = match Self::global_in(code, arg.a, code.len()) {
                    Some(f) => self.want(f, c.x as usize),
                    // a function just made here such as a lambda runs its body's section
                    None if own => self.made_at(code, arg.a).and_then(|body| (self.sections)(Want::Body(body))).and_then(|f| self.want_callee(u16::MAX, f, c.x as usize)),
                    None => None,
                };
                if let Some(s) = s { self.gen_args.push((k, j, s)); }
            }
            // a loop over a parameter given a generator call's result steps that generator's section
            if depth < 4 && let Some((src, _)) = Self::loop_step(&body.ins, j) && param(src)
                && let Some((callee, inner)) = self.gen_call_at(code, code.len(), args[src as usize], own)
                && let body2 = callee.body.clone()
                && callee.module == self.facts.module && crate::vm::block::gen_pure(&body2.ins) && callee.star.is_none() && inner.len() <= callee.arity && inner.len() >= callee.required
                && let Some(k2) = self.want_callee(u16::MAX, callee, inner.len()) {
                self.nested.push((k, j, k2));
                self.bind_in(k2, &body2, &inner, code, own, depth + 1);
            }
        }
    }

    /* the binding `r` still holds at `at` from `LoadGlobalR` or popped past a deep call */
    fn global_in(code: &[Ins], r: u16, at: usize) -> Option<u16> {
        let writes = |i: &Ins, r: u16| crate::vm::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r) || i.op == OpCode::StoreTopR && i.x == 0 && i.a == r;
        let (mut end, mut r) = (at, r);
        for _ in 0..4 {
            let w = code[..end].iter().rposition(|i| writes(i, r))?;
            match code[w] {
                Ins { op: OpCode::LoadGlobalR, b, .. } => return Some(b),
                // a value spilled into its row came from the register the spill read
                Ins { op: OpCode::Move, x: 0, b, .. } => (end, r) = (w, b),
                Ins { op: OpCode::StoreTopR, .. } => {
                    let pushes = |i: &Ins| i.op == OpCode::PushRegs && [i.a, i.b, i.c][..i.x.min(3) as usize].contains(&r);
                    let p = code[..w].iter().rposition(pushes)?;
                    if code[p + 1..w].iter().any(|i| writes(i, r)) { return None; }
                    end = p;
                }
                _ => return None,
            }
        }
        None
    }

    /* the register a loop at `j` of `ins` steps and its `ForIterR` past any `OwnResetR` */
    fn loop_step(ins: &[Ins], j: usize) -> Option<(u16, usize)> {
        let get = ins.get(j)?;
        if get.op != OpCode::GetIterR || get.x != 0 { return None; }
        let f = if ins.get(j + 1)?.op == OpCode::OwnResetR { j + 2 } else { j + 1 };
        // a loop whose target unpacks also steps into a register its unpacking reads
        ins.get(f).filter(|i| i.op == OpCode::ForIterR && i.x == 0).map(|_| (get.b, f))
    }

    /* the generator a call made into `r` unwritten since and the registers passed to it */
    fn gen_call_at(&self, code: &[Ins], at: usize, r: u16, own: bool) -> Option<(Callee<'p>, Vec<u16>)> {
        let k = code[..at].iter().rposition(|i| crate::vm::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r) || i.op == OpCode::StoreTopR && i.x == 0 && i.a == r)?;
        // a value spilled into its row is the one the spill read
        if code[k].op == OpCode::Move && code[k].x == 0 { return self.gen_call_at(code, k, code[k].b, own); }
        if code[k].op != OpCode::StoreTopR { return None; }
        let call = *code.get(k.checked_sub(1)?)?;
        if !matches!(call.op, OpCode::CallR | OpCode::CallGlobalR) || wide(call) { return None; }
        let callee = match if call.op == OpCode::CallGlobalR { Some(call.a) } else { Self::global_in(code, call.a, k - 1) } {
            Some(g) => (self.sections)(Want::Gen(self.facts.module, g))?,
            // a generator just made here such as a generator expression
            None if own => (self.sections)(Want::GenBody(self.made_at(code, call.a)?))?,
            None => return None,
        };
        let regs = [call.b, call.c];
        Some((callee, regs[..call.x as usize].to_vec()))
    }

    /* the body of the function a `MakeFunction` last wrote into `r` unwritten since */
    fn made_at(&self, code: &[Ins], r: u16) -> Option<u32> {
        let k = code.iter().rposition(|i| crate::vm::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r) || i.op == OpCode::StoreTopR && i.x == 0 && i.a == r)?;
        // a value spilled into its row is the one the spill read
        if code[k].op == OpCode::Move && code[k].x == 0 { return self.made_at(&code[..k], code[k].b); }
        let make = Some(code[k]).filter(|&m| made_by(m))?;
        self.chunk.functions.get(make.c as usize).map(|f| f.1.id.get())
    }

    /* calls a layable function by keyword as a section matching names and filling defaults */
    pub(super) fn kw_call(&mut self, i: usize, op: u16) -> Option<usize> {
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let total = np + 2 * nk;
        if np + nk > 5 || self.pending.len() <= total || i + 1 >= self.src.len() { return None; }
        let under = self.pending.len() - total - 1;
        let f = self.pending[under].r;
        let g = crate::vm::block::global_of(&self.out, self.out.len(), Ins { op: OpCode::CallR, x: 0, a: f, b: 0, c: 0 })?;
        let callee = (self.sections)(Want::Global(self.facts.module, g))?;
        let regs: Vec<u16> = self.pending[under..].iter().map(|e| e.r).collect();
        let (m, slots, first) = Self::kw_slots(&regs, op, &callee, (&self.chunk.constants, self.cbase))?;
        self.keyed = true;
        let section = self.want_callee(g, callee, m)?;
        let join = self.target(i + 1);
        self.pending.truncate(under);
        let t = self.temp();
        let below = self.pending.clone();
        self.kwrow = Some((self.free_home(&regs), false));
        self.kw_lay(regs, op, m, &slots, first, t, section, &below);
        self.pending.push(Entry::reg(t));
        self.dest = Some(self.out.len() - 3);
        if !join { self.resume = Some(self.out.len() - 3); }
        Some(1)
    }

    /* calls a layable function taking `*args` or `**kwargs` as a section packing the rest */
    pub(super) fn spread_call(&mut self, i: usize, op: u16) -> Option<usize> {
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let total = np + 2 * nk;
        if self.pending.len() <= total || i + 1 >= self.src.len() { return None; }
        let under = self.pending.len() - total - 1;
        let f = self.pending[under].r;
        // a module binding's function or one the call found before wherever it reads it from
        let g = crate::vm::block::global_of(&self.out, self.out.len(), Ins { op: OpCode::CallR, x: 0, a: f, b: 0, c: 0 });
        let callee = match g { Some(g) => (self.sections)(Want::Spread(self.facts.module, g))?, None => (self.sections)(Want::Value(i as u32))? };
        let g = g.unwrap_or(u16::MAX);
        let regs: Vec<u16> = self.pending[under..].iter().map(|e| e.r).collect();
        let packed = Self::packed(&regs, op, &callee, (&self.chunk.constants, self.cbase))?;
        self.keyed = true;
        let section = self.want_callee(g, callee.clone(), callee.arity)?;
        let join = self.target(i + 1);
        // the tuple and dict registers avoid every register the call reads
        let tuple = callee.star.map(|_| self.temp());
        let held = self.pending.len();
        self.pending.push(Entry::reg(tuple.unwrap_or(0)));
        let dict = callee.dstar.map(|_| self.temp());
        self.pending.truncate(held);
        self.pending.truncate(under);
        let (list, gaps) = self.pack(&regs, &packed, tuple.zip(callee.star), dict.zip(callee.dstar), None);
        self.gaps = gaps;
        let r = |k: usize| list.get(k).copied().unwrap_or(0);
        let call = Ins { op: OpCode::CallR, x: list.len() as u8, a: f, b: r(0), c: r(1) };
        let rest = Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) };
        let t = self.temp();
        // its stub moves the operands into temporaries the values under the call leave free
        self.kwrow = Some((self.free_home(&regs), false));
        self.kwcall = Some((regs, op));
        self.head_take(call, rest, t, section, join);
        Some(1)
    }

    /* `f(*a)` after the `BeginArgs` at `i` with its sequence loaded as a spreading register call */
    pub(super) fn spread(&mut self, i: usize) -> Option<usize> {
        use OpCode::*;
        let src = self.src;
        let simple = |k: usize| src.get(k).is_some_and(|s| matches!(s.opcode, LoadName | LoadConst | LoadGlobal | LoadNone | LoadTrue | LoadFalse | BuildList | BuildTuple | BuildDict)) && !self.target(k);
        let mut end = i + 1;
        while simple(end) { end += 1; }
        let at = |k: usize| src.get(k).filter(|_| !self.target(k)).copied();
        let (unpack, call) = (at(end)?, at(end + 1)?);
        let held = self.pending.len();
        // 0 spreads the items of a sequence and 2 the pairs of a mapping
        let kind = if unpack.operand == 2 { 2 } else { 0 };
        if end == i + 1 || unpack.opcode != UnpackArgs || !matches!(unpack.operand, 1 | 2) || call.opcode != CallSpread || call.operand != 1 || held == 0 || held > 1 + MAX_UNDER
            || end + 2 >= src.len() || held + end - i > MAX_PENDING { return None; }
        // the loads and builds must leave exactly one value over the callee
        let mut depth = 0usize;
        for s in &src[i + 1..end] {
            depth = match s.opcode { BuildList | BuildTuple => (depth + 1).checked_sub(s.operand as usize)?, BuildDict => (depth + 1).checked_sub(2 * s.operand as usize)?, _ => depth + 1 };
        }
        if depth != 1 { return None; }
        // the binding the callee was read from found before the sequence's loads and builds
        let f = self.pending[held - 1];
        let g = crate::vm::block::global_of(&self.out, self.out.len(), Ins { op: OpCode::CallR, x: 0, a: f.r, b: 0, c: 0 });
        for (k, &s) in src.iter().enumerate().take(end).skip(i + 1) {
            self.cur = k as u32;
            match s.opcode {
                BuildList | BuildTuple | BuildDict => { self.build(s, build_len(s), true); }
                _ => { self.one(k); }
            }
        }
        if self.pending.len() != held + 1 { return None; }
        let under = self.pending.len() - 2;
        let seq = self.pending[under + 1];
        // the `CallSpread` where an error in the call points
        let (c, steps) = (end + 1, end + 2 - i);
        let join = self.target(c + 1);
        // a module binding's function or one found before becomes a section the items go to
        let callee = match g { Some(g) => (self.sections)(Want::Global(self.facts.module, g)), None => (self.sections)(Want::Value(c as u32)) };
        self.cur = c as u32;
        if let Some(callee) = callee.filter(|c| spreads(c, kind)) && let Some(section) = { self.keyed = true; self.want_callee(g.unwrap_or(u16::MAX), callee.clone(), callee.arity) } {
            self.pending.truncate(under);
            let t = self.temp();
            self.head_take(Ins { op: OpCode::CallSpreadR, x: kind, a: f.r, b: seq.r, c: 0 }, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, t, section, join);
            return Some(steps);
        }
        // what lies under the call waits in rows and a spill may move operands
        self.settle_rows_with(under, &mut [], true);
        let (f, seq) = (self.pending[under], self.pending[under + 1]);
        self.pending.truncate(under);
        self.quiet = self.cur;
        self.emit(Ins { op: OpCode::CallSpreadR, x: kind, a: f.r, b: seq.r, c: 0 });
        self.take_at(join, 1);
        Some(steps)
    }

    /* calls with over five positionals as a section moving those past the second into parameters */
    pub(super) fn many_call(&mut self, i: usize, n: usize) -> Option<usize> {
        if self.pending.len() <= n || i + 1 >= self.src.len() { return None; }
        let under = self.pending.len() - n - 1;
        let f = self.pending[under];
        let g = crate::vm::block::global_of(&self.out, self.out.len(), Ins { op: OpCode::CallR, x: 0, a: f.r, b: 0, c: 0 });
        let callee = match g { Some(g) => (self.sections)(Want::Global(self.facts.module, g))?, None => (self.sections)(Want::Value(i as u32))? };
        if callee.star.is_some() || callee.dstar.is_some() || callee.ctor().is_some() || callee.on_class != 0 { return None; }
        self.wide = true;
        let section = self.want_callee(g.unwrap_or(u16::MAX), callee, n)?;
        let regs: Vec<u16> = self.pending[under..].iter().map(|e| e.r).collect();
        let a = |k: usize| regs.get(1 + k).copied().unwrap_or(0);
        let call = Ins { op: OpCode::CallR, x: n as u8, a: f.r, b: a(0), c: a(1) };
        let rest = Ins { op: OpCode::ArgsR, x: 0, a: a(2), b: a(3), c: a(4) };
        let join = self.target(i + 1);
        self.pending.truncate(under);
        let t = self.temp();
        self.more_args = regs[6..].to_vec();
        // the stub calls with the stack as the call was written
        self.kwrow = Some((self.free_home(&regs), false));
        self.kwcall = Some((regs, n as u16));
        self.head_take(call, rest, t, section, join);
        Some(1)
    }

    /* the section a call of `g` with `n` arguments enters with its body's calls wanted */
    pub(super) fn want(&mut self, g: u16, n: usize) -> Option<usize> { self.want_in(self.facts.module, g, n) }

    /* wants other functions a register call met checked after section `k` each taking `n` plainly */
    pub(super) fn want_alts(&mut self, fi: Option<usize>, at: u32, k: usize, n: usize) {
        let alt = |c: &Callee| !guarded(c) && fills(c, n, false) && c.positional == c.arity && c.ctor().is_none() && !c.twin;
        let plain = |c: &Callee| alt(c) && !c.by_body;
        // a `by_body` head's check yields to the lookup and a growth adds the met function
        let (grows, only) = (self.old.is_some(), self.only.take());
        // a growth's head may serve a closure's whole body with its stub looking others up
        let head_fits = |c: &Callee| alt(c) || grows && alt(&Callee { twin: false, ..c.clone() });
        if !head_fits(&self.wanted[k].1) || !grows && self.alts.iter().any(|a| a.0 == k || a.1 == k) { return; }
        for j in 0..ALTS {
            let Some(mut c) = (match only { Some(f) if j == 0 => (self.sections)(Want::Func(f, false)), Some(_) => None, None => (self.sections)(Want::Alt(fi, at, j)) }) else { break };
            // a growth meeting another function of a laid body lays one `by_body` section for all
            if grows && (self.wanted.iter().any(|w| w.1.fi == c.fi && (w.1.value.0 != c.value.0 || w.1.by_body))) { (c.by_body, c.twin) = (true, false); }
            if !(plain(&c) || grows && c.by_body && alt(&c)) { continue; }
            // a call of over five arguments names the rest apart as its head does
            self.wide = n > 5;
            let Some(a) = self.want_callee(u16::MAX, c, n) else { continue };
            let free = if grows { !self.alts.iter().any(|x| x.0 == k && x.1 == a) } else { !self.alts.iter().any(|x| x.0 == a || x.1 == a) };
            if a != k && free && (plain(&self.wanted[a].1) || grows && self.wanted[a].1.by_body) { self.alts.push((k, a)); }
        }
    }

    /* wants other methods a method call met for section `k`'s stub each taking `n` plainly */
    pub(super) fn want_method_alts(&mut self, fi: Option<usize>, at: u32, k: usize, n: usize) {
        let on = self.wanted[k].1.on_class;
        // methods take the receiver too and held functions only the arguments yet share head tables
        let takes = |c: &Callee| n - (c.on_class == 4) as usize;
        let kinds = |c: &Callee| c.on_class == on && on == 0 || matches!((on, c.on_class), (0, 4) | (4, 0) | (4, 4)) && c.arity == takes(c);
        let plain = |c: &Callee| !guarded(c) && fills(c, takes(c), false) && c.positional == c.arity && kinds(c) && c.ctor().is_none() && !c.by_body && !c.twin;
        let (grows, only) = (self.old.is_some(), self.only.take());
        if !plain(&self.wanted[k].1) || !grows && self.alts.iter().any(|a| a.0 == k || a.1 == k) { return; }
        // methods the call met and for a method or held head the functions receivers held
        let held = matches!(on, 0 | 4);
        for at in core::iter::once(at).chain(held.then_some(at | FIELD)) {
            for j in 0..ALTS {
                let Some(c) = (match only { Some(f) if j == 0 && at & FIELD == 0 => (self.sections)(Want::Func(f, true)), Some(_) => None, None => (self.sections)(Want::MethodAlt(fi, at, j)) }) else { break };
                if !plain(&c) { continue; }
                let m = takes(&c);
                let Some(a) = self.want_callee(u16::MAX, c, m) else { continue };
                let free = if grows { !self.alts.iter().any(|x| x.0 == k && x.1 == a) } else { !self.alts.iter().any(|x| x.0 == a || x.1 == a || x.0 == k && x.1 == a) };
                if a != k && free && plain(&self.wanted[a].1) { self.alts.push((k, a)); }
            }
        }
    }

    /* `want` of binding `g` of module `m` which a body laid from that module calls */
    fn want_in(&mut self, m: usize, g: u16, n: usize) -> Option<usize> {
        // a class call adds its instance and over five arguments need the rest named apart
        if let Some(k) = self.wanted_at(m, g) { let c = &self.wanted[k].1; let n = n + c.ctor().is_some() as usize; return ((n <= 5 || self.wide) && fits(c, n)).then_some(k); }
        let callee = (self.sections)(Want::Global(m, g))?;
        // a call packs extra positionals into an `*args` tuple and only `spread_call` lays `**kwargs` ones
        if packs(&callee, n).is_some() { self.keyed = true; let arity = callee.arity; return self.want_callee(g, callee, arity); }
        if callee.star.is_some() || callee.dstar.is_some() { return None; }
        let n = n + callee.ctor().is_some() as usize;
        self.want_callee(g, callee, n)
    }

    /* the section wanted for binding `g` of module `m` */
    fn wanted_at(&self, m: usize, g: u16) -> Option<usize> { self.wanted.iter().position(|w| w.0 == g && w.1.bound_module() == m && !w.1.generator) }

    /* `want` of `callee` called through binding `g` or the u16 max for a method */
    pub(super) fn want_callee(&mut self, g: u16, callee: Callee<'p>, n: usize) -> Option<usize> {
        // a generator's section serves one loop and only keyword calls fill past a lone `*`
        let keyed = core::mem::take(&mut self.keyed);
        // a head reads five arguments including instance or receiver and more only if named apart
        let named = core::mem::take(&mut self.wide);
        if n > 5 && !named { return None; }
        if let Some(k) = self.wanted.iter().position(|w| !w.1.generator && !callee.generator && (w.1.value.0 == callee.value.0 && !callee.value.is_undef() || w.1.by_body && callee.by_body && w.1.fi == callee.fi) && same_laid(&w.1, &callee)) { return fills(&self.wanted[k].1, n, keyed).then_some(k); }
        // a call run once lays none unless a generator loop or recursion reruns the body
        if self.once && !callee.generator && !recurs(&callee.body.ins, g) { return None; }
        if !fills(&callee, n, keyed) || callee.arity > MAX_ARGS || !callee.value.is_undef() && Val::heap((callee.value.0 >> 4) as u32).0 != callee.value.0 { return None; }
        let body = callee.body.clone();
        // its callees run as often as it does like a generator's body at each step
        self.once = false;
        // wanted before its callees so a recursion calling back into it finds it
        let k = self.wanted.len();
        let twin = callee.twin.then(|| Callee { by_body: true, twin: false, ..callee.clone() });
        // few callees are usually wanted and each is large so room grows only as needed
        if self.wanted.len() == self.wanted.capacity() { self.wanted.reserve_exact(self.wanted.len().max(1)); }
        self.wanted.push((g, callee));
        if let Some(twin) = twin { self.wanted.push((g, twin)); }
        {
            let ins = &body.ins;
            // the body instructions each look below needs found in one pass
            let marked: Vec<(OpCode, usize)> = ins.iter().enumerate().filter(|(_, i)| matches!(i.op, OpCode::CallGlobalR | OpCode::CallR
                | OpCode::CallMethodR | OpCode::CallSpreadR | OpCode::MakeFunction | OpCode::MakeCoroutine | OpCode::OpR | OpCode::StoreName | OpCode::RowsR)).map(|(j, i)| (i.op, j)).collect();
            let of = |op: OpCode| marked.iter().filter(move |m| m.0 == op).map(|m| m.1);
            // a body storing by name or making functions over cells is refused before the walk
            let makes = marked.iter().any(|m| crate::vm::block::makes(&ins[m.1]));
            if of(OpCode::StoreName).any(|j| !self.wanted[k].1.cellvars.contains(&(ins[j].a as usize))) { self.wanted.truncate(k); return None; }
            if makes && !self.wanted[k].1.cells.is_empty() && !self.wanted[k].1.by_body { self.wanted.truncate(k); return None; }
            // calls of layable functions jump to their sections and others call as outside them
            let mut calls: Vec<(u16, usize, bool)> = marked.iter().filter(|m| matches!(m.0, OpCode::CallGlobalR | OpCode::CallR))
                .filter_map(|&(_, j)| Some((crate::vm::block::global_of(ins, j, ins[j])?, ins[j].x as usize, false))).collect();
            // so is the function a row call of positionals calls through its binding
            calls.extend(of(OpCode::RowsR).filter(|&j| ins[j].x == OpCode::Call as u8 && ins[j].b >> 8 == 0 && ins[j].b as usize <= MAX_ARGS).filter_map(|j| {
                Some((crate::vm::block::global_of(ins, j, Ins { op: OpCode::CallR, x: 0, a: ins[j].a, b: 0, c: 0 })?, ins[j].b as usize, ins[j].b > 5))
            }));
            let module = self.wanted[k].1.module;
            for &(h, m, wide) in &calls { self.wide = wide; self.want_in(module, h, m); self.wide = false; }
            // and one with keywords fills the parameters its names match
            for j in (0..ins.len()).filter(|&j| ins[j].op == OpCode::RowsR && ins[j].x == OpCode::Call as u8 && ins[j].b >> 8 != 0) {
                let op = ins[j].b;
                let Some((g, kw)) = self.row_kw(k, j) else { continue };
                let Some(c) = (self.sections)(Want::Global(module, g)).filter(|c| c.ctor().is_none()) else { continue };
                let lits = (self.wanted[k].1.literals, self.wanted[k].1.body.cbase as usize);
                // one to an `*args` or `**kwargs` function passes all parameters packed
                let m = if c.star.is_some() || c.dstar.is_some() { Self::packed(&kw, op, &c, lits).map(|_| c.arity) } else { Self::kw_slots(&kw, op, &c, lits).map(|s| s.0) };
                let Some(m) = m else { continue };
                self.keyed = true;
                self.want_callee(g, c, m);
            }
            // a method a call inside found now or when laid before is laid too
            let fi = self.wanted[k].1.fi;
            for j in of(OpCode::CallMethodR) {
                let i = ins[j];
                if let Some(callee) = (self.sections)(Want::Nested(fi, body.orig(j), method_of(&body, j), i.a)) && let Some(m) = { let n = i.x as usize + !matches!(callee.on_class, 1 | 4) as usize; self.want_callee(u16::MAX, callee, n) } {
                    self.want_method_alts(Some(fi), body.orig(j), m, i.x as usize + 1);
                }
            }
            // so is what register or unlaid binding calls found like closures bound methods or instances
            let valued: Vec<(usize, Ins)> = of(OpCode::CallR).chain(of(OpCode::CallGlobalR)).map(|j| (j, ins[j])).filter(|&(j, i)| self.valued(ins, j, i, module)).collect();
            for (j, i) in valued {
                // a comprehension the body makes and calls at once runs the section of its body
                if i.op == OpCode::CallR && let Some(callee) = made_in(ins, j, i.a).and_then(|m| (self.sections)(Want::MadeIn(fi, m))) { self.want_callee(u16::MAX, callee, i.x as usize); continue; }
                let Some(callee) = self.met(k, j, i) else { continue };
                // an `*args` function takes what the call passes past its positionals packed into its tuple
                if packs(&callee, i.x as usize).is_some() { self.keyed = true; self.want_callee(u16::MAX, callee.clone(), callee.arity); continue; }
                if let Some(v) = { let n = i.x as usize + (callee.on_class & 3 == 3) as usize; self.want_callee(u16::MAX, callee, n) } {
                    self.want_alts(Some(fi), body.orig(j), v, i.x as usize);
                }
            }
            // so is a function a spreading call calls with the items filling its parameters
            for (j, i) in of(OpCode::CallSpreadR).map(|j| (j, ins[j])) {
                let g = crate::vm::block::global_of(ins, j, Ins { op: OpCode::CallR, ..i });
                let callee = match g { Some(h) => (self.sections)(Want::Global(module, h)), None => self.celled(k, j, i.a).or_else(|| (self.sections)(Want::NestedValue(fi, body.orig(j), value_of(&body, j)))) };
                if let Some(c) = callee.filter(|c| spreads(c, i.x)) { self.keyed = true; self.want_callee(g.unwrap_or(u16::MAX), c.clone(), c.arity); }
            }
            // a loop over a called generator steps its section while `try` bodies lay no recursion
            for j in 2..ins.len() {
                let (call, store) = (ins[j - 2], ins[j - 1]);
                let Some((src, _)) = Self::loop_step(ins, j) else { continue };
                if store.op != OpCode::StoreTopR || store.x != 0 || store.a != src { continue; }
                if module != self.facts.module { break; }
                let Some((callee, args)) = self.gen_call_at(ins, j, src, false) else { continue };
                let body2 = callee.body.clone();
                if callee.module != module || !crate::vm::block::gen_pure(&body2.ins) || callee.star.is_some() || args.len() > callee.arity || args.len() < callee.required { continue; }
                // an already stepped generator would lay sections endlessly so its loop runs a frame
                if wide(call) || self.wanted.iter().any(|w| w.1.generator && w.1.fi == callee.fi) { continue; }
                if let Some(k2) = self.want_callee(u16::MAX, callee, args.len()) {
                    self.nested.push((k, j, k2));
                    self.bind_args(k2, &body2, &args, Some(&body), 1);
                }
            }
        }
        Some(k)
    }

    /* the section row call `c` at `j` of section `k` calls and its keyword passing */
    fn laid_row(&self, k: usize, j: usize, c: Ins) -> Option<(usize, Option<Result<Passed, Packed>>)> {
        if c.op != OpCode::RowsR || c.x != OpCode::Call as u8 { return None; }
        // a row call names its operand in `b` and reads its operands from its row
        let c = Ins { a: c.b, ..c };
        let w = &self.wanted[k].1;
        let ins = &w.body.ins;
        let (g, kw) = if c.a >> 8 != 0 { self.row_kw(k, j)? } else {
            if c.a as usize > MAX_ARGS { return None; }
            (crate::vm::block::global_of(ins, j, Ins { op: OpCode::CallR, x: 0, a: ins[j].a, b: 0, c: 0 })?, Vec::new())
        };
        let to = self.wanted_at(w.module, g)?;
        let t = &self.wanted[to].1;
        if t.ctor().is_some() || t.twin || t.by_body { return None; }
        let spreads = t.star.is_some() || t.dstar.is_some();
        if kw.is_empty() { return fills(t, c.a as usize, false).then_some((to, None)); }
        if spreads { return Some((to, Some(Err(Self::packed(&kw, c.a, t, (w.literals, w.body.cbase as usize))?)))); }
        let (m, slots, _) = Self::kw_slots(&kw, c.a, t, (w.literals, w.body.cbase as usize))?;
        Some((to, Some(Ok((m, slots)))))
    }

    /* the binding keyword row call `j` of section `k` calls and its `kw_slots` operands */
    fn row_kw(&self, k: usize, j: usize) -> Option<(u16, Vec<u16>)> {
        let body = &self.wanted[k].1.body;
        let (ins, w) = (&body.ins, body.ins[j]);
        let (np, nk) = ((w.b & 0xFF) as usize, (w.b >> 8) as usize);
        let n = 1 + np + 2 * nk;
        let (cb, tail) = (body.cbase as usize, body.cbase as usize + self.wanted[k].1.consts.len());
        if usize::from(row_op_counts(&w).0) != n || n > 64 { return None; }
        let mut kw: Vec<u16> = (0..n).map(|q| q as u16).collect();
        let (mut q, mut seen, names) = (j, 0u64, (0..nk).fold(0u64, |m, e| m | 1 << (1 + np + 2 * e)));
        while q > 0 && ins[q - 1].op == OpCode::Move && ins[q - 1].x == 0 {
            let m = ins[q - 1];
            if let Some(e) = usize::from(m.a).checked_sub(usize::from(w.a)).filter(|&e| e < n && seen >> e & 1 == 0) {
                seen |= 1 << e;
                if names >> e & 1 != 0 && (cb..tail).contains(&usize::from(m.b)) { kw[e] = m.b; }
            }
            q -= 1;
        }
        // each name must be a constant the spill moved into its row
        if (0..nk).any(|e| kw[1 + np + 2 * e] as usize == 1 + np + 2 * e) { return None; }
        Some((crate::vm::block::global_of(ins, j, Ins { op: OpCode::CallR, x: 0, a: w.a, b: 0, c: 0 })?, kw))
    }

    fn laid_call(&self, ins: &[Ins], j: usize, c: Ins, m: usize) -> bool { self.laid_call_to(matches!(c.op, OpCode::CallGlobalR | OpCode::CallR).then(|| crate::vm::block::global_of(ins, j, c)).flatten(), c, m).is_some() }

    /* the section of binding `g` the register call `c` jumps to */
    fn laid_call_to(&self, g: Option<u16>, c: Ins, m: usize) -> Option<usize> {
        if !matches!(c.op, OpCode::CallGlobalR | OpCode::CallR) { return None; }
        // a call of another arity is not laid so it raises as cold code does
        g.and_then(|g| self.wanted_at(m, g)).filter(|&k| { let w = &self.wanted[k].1; fits(w, c.x as usize + w.ctor().is_some() as usize) && (c.x < 5 || w.ctor().is_none()) })
    }

    /* the section laid for the method the call `c` at `j` of section `k` found */
    fn laid_method(&self, k: usize, j: usize, c: Ins) -> Option<usize> {
        let body = &self.wanted[k].1.body;
        if c.op != OpCode::CallMethodR { return None; }
        let callee = (self.sections)(Want::Nested(self.wanted[k].1.fi, body.orig(j), method_of(body, j), c.a))?;
        let n = c.x as usize + !matches!(callee.on_class, 1 | 4) as usize;
        self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && same_laid(&w.1, &callee) && fills(&w.1, n, false))
    }

    /* the section laid for the function register call `c` at `j` of section `k` found */
    fn laid_value(&self, k: usize, j: usize, c: Ins) -> Option<usize> {
        if let Some(g) = self.gen_args.iter().find(|g| g.0 == k && g.1 == j) { return Some(g.2); }
        let body = &self.wanted[k].1.body;
        if !self.valued(&body.ins, j, c, self.wanted[k].1.module) { return None; }
        if c.op == OpCode::CallR && let Some(callee) = made_in(&body.ins, j, c.a).and_then(|m| (self.sections)(Want::MadeIn(self.wanted[k].1.fi, m))) {
            return self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.by_body && w.1.fi == callee.fi);
        }
        let callee = self.met(k, j, c)?;
        if packs(&callee, c.x as usize).is_some() { return self.wanted.iter().position(|w| w.0 == u16::MAX && w.1.value.0 == callee.value.0 && packs(&w.1, c.x as usize).is_some()); }
        let n = c.x as usize + (callee.on_class & 3 == 3) as usize;
        self.wanted.iter().position(|w| w.0 == u16::MAX && (w.1.value.0 == callee.value.0 || w.1.by_body && callee.by_body && w.1.fi == callee.fi) && w.1.on_class == callee.on_class && fills(&w.1, n, false))
    }

    /* what `c` calls via a closure cell or its binding's value or what it met */
    fn met(&self, k: usize, j: usize, c: Ins) -> Option<Callee<'p>> {
        let (w, body) = (&self.wanted[k].1, &self.wanted[k].1.body);
        if c.op == OpCode::CallR && let Some(c) = self.celled(k, j, c.a) { return Some(c); }
        crate::vm::block::global_of(&body.ins, j, c).and_then(|g| (self.sections)(Want::Held(w.module, g))).or_else(|| (self.sections)(Want::NestedValue(w.fi, body.orig(j), value_of(body, j))))
    }

    /* whether a call calls what it met through a register or an unlaid binding */
    fn valued(&self, ins: &[Ins], j: usize, c: Ins, m: usize) -> bool {
        matches!(c.op, OpCode::CallR | OpCode::CallGlobalR) && crate::vm::block::global_of(ins, j, c).map_or(c.op == OpCode::CallR, |g| self.wanted_at(m, g).is_none())
    }

    /* whether section k's body lays as is without calls loops handlers or stack work */
    fn plain_lay(&self, k: usize, body: &Code) -> bool {
        use OpCode::*;
        let c = &self.wanted[k].1;
        if c.generator || c.exc || !c.cellvars.is_empty() || c.names - c.arity > 64 || self.genfors.iter().any(|g| g.k == k) { return false; }
        body.ins.iter().enumerate().all(|(j, i)| match i.op {
            Move | MinusR | NotR | UnaryR | LenR | GetAttrR | SetAttrR | StoreItemR | LoadGlobalR | StoreGlobalR | IsR | IsNotR | InR | NotInR | PowR
            | BitAndR | BitOrR | BitXorR | ShlR | ShrR | JumpUnlessLt | JumpUnlessLtEq | JumpUnlessGt | JumpUnlessGtEq
            | JumpUnlessEq | JumpUnlessNotEq | ReturnR | JumpIfFalseR | AddR | SubR | MulR | DivR | ModR | FloorDivR
            | InPlaceAddR | InPlaceSubR | EqR | NotEqR | LtR | LtEqR | GtR | GtEqR | GetItemR => true,
            Jump => i.x == 0,
            // a positional binding section call storing its result that never comes back into this one
            CallGlobalR | CallR => self.plain_call(k, &body.ins, j).is_some(),
            StoreTopR => j > 0 && i.x == 0 && self.plain_call(k, &body.ins, j - 1).is_some(),
            _ => false,
        })
    }

    /* the section a stored binding call of at most two arguments enters under `plain_lay` */
    fn plain_call(&self, k: usize, ins: &[Ins], j: usize) -> Option<usize> {
        let i = ins[j];
        if !matches!(i.op, OpCode::CallGlobalR | OpCode::CallR) || wide(i) || j > 0 && ins[j - 1].op == OpCode::PushRegs
            || !ins.get(j + 1).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0) { return None; }
        let to = self.wanted_at(self.wanted[k].1.module, crate::vm::block::global_of(ins, j, i)?)?;
        let w = &self.wanted[to].1;
        // a plain body packs nothing so its call must fill the parameters as they stand
        let n = i.x as usize + w.ctor().is_some() as usize;
        (!w.twin && w.on_class & 3 != 3 && w.star.is_none() && w.dstar.is_none() && fills(w, n, false) && !self.reaches(to, k)).then_some(to)
    }

    /* the section a call `c` at `j` of section `k` spreading a sequence jumps to */
    fn laid_spread(&self, k: usize, j: usize, c: Ins) -> Option<usize> {
        if c.op != OpCode::CallSpreadR { return None; }
        let w = &self.wanted[k].1;
        let body = &w.body;
        let callee = match crate::vm::block::global_of(&body.ins, j, Ins { op: OpCode::CallR, ..c }) {
            Some(h) => (self.sections)(Want::Global(w.module, h))?,
            None => self.celled(k, j, c.a).or_else(|| (self.sections)(Want::NestedValue(w.fi, body.orig(j), value_of(body, j))))?,
        };
        if !spreads(&callee, c.x) { return None; }
        self.wanted.iter().position(|w| w.1.value.0 == callee.value.0 && spreads(&w.1, c.x))
    }

    /* the function in `r` at call `j` loaded from a closure cell every call shares */
    fn celled(&self, k: usize, j: usize, r: u16) -> Option<Callee<'p>> {
        let w = &self.wanted[k].1;
        if w.by_body || w.cells.is_empty() { return None; }
        let ins = &w.body.ins;
        let at = ins[..j].iter().rposition(|i| crate::vm::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r) || i.op == OpCode::LoadCellR && i.a == r)?;
        if ins[at].op != OpCode::LoadCellR { return None; }
        let cell = w.cells.iter().find(|c| c.0 == ins[at].b as usize).map(|c| c.1).filter(|c| !c.is_undef())?;
        (self.sections)(Want::Celled(cell))
    }

    /* whether a call from `to` into `from` may come back into `to` */
    pub(super) fn reaches(&self, from: usize, to: usize) -> bool {
        if from == to || self.twin(from) == Some(to) { return true; }
        // a section calling none with no table reaches nothing so the walk is skipped
        if self.called(from).is_empty() && !self.alts.iter().any(|a| a.0 == from) { return false; }
        let mut seen = vec![false; self.wanted.len()];
        // a head may enter any section its table names for other functions its call met
        let alts = |t: usize| self.alts.iter().filter(move |a| a.0 == t).map(|a| a.1);
        let mut todo: Vec<usize> = core::iter::once(from).chain(alts(from)).collect();
        while let Some(k) = todo.pop() {
            if k == to || self.twin(k) == Some(to) { return true; }
            if core::mem::replace(&mut seen[k], true) { continue; }
            // a call of a closure's section may run its twin instead
            for &(_, t) in self.called(k).iter() { todo.push(t); todo.extend(self.twin(t)); todo.extend(alts(t)); }
        }
        false
    }

    /* the section paired with `k` where one serves any closure of the same body */
    fn twin(&self, k: usize) -> Option<usize> {
        let w = &self.wanted;
        if w[k].1.twin { return Some(k + 1).filter(|&t| t < w.len()); }
        (w[k].1.by_body && k > 0 && w[k - 1].1.twin && w[k - 1].1.fi == w[k].1.fi).then(|| k - 1)
    }

    /* the calls in section `k` jumping to sections with their targets found once per lay */
    fn called(&self, k: usize) -> Calls {
        self.calls.get_or_init(|| (0..self.wanted.len()).map(|_| OnceCell::new()).collect())[k].get_or_init(|| self.calls_of(k).into()).clone()
    }

    fn calls_of(&self, k: usize) -> Vec<(usize, usize)> {
        let body = &self.wanted[k].1.body;
        let m = self.wanted[k].1.module;
        body.ins.iter().enumerate().filter_map(|(j, &i)| match i.op {
            OpCode::CallGlobalR | OpCode::CallR if let Some(to) = self.laid_call_to(crate::vm::block::global_of(&body.ins, j, i), i, m) => Some((j, to)),
            OpCode::CallMethodR => Some((j, self.laid_method(k, j, i)?)),
            OpCode::CallR | OpCode::CallGlobalR => Some((j, self.laid_value(k, j, i)?)),
            OpCode::CallSpreadR => Some((j, self.laid_spread(k, j, i)?)),
            OpCode::RowsR => Some((j, self.laid_row(k, j, i)?.0)),
            _ => None,
        }).collect()
    }

    /* emits section `k`'s call into `dest` with `outer` outside sections and `below` under it */
    pub(super) fn head(&mut self, call: Ins, rest: Ins, dest: u16, k: usize, outer: bool, below: &[Entry]) {
        // each parameter takes an argument or default and those past the second move in first
        let callee = &self.wanted[k].1;
        let (arity, ctor, by_body, twin) = (callee.arity, callee.ctor().is_some(), callee.by_body, callee.twin);
        // a spreading call fills every parameter as it enters apart in the handler
        let spread = call.op == OpCode::CallSpreadR;
        // a function called on its class or held by an instance takes the arguments alone
        let stat = matches!(callee.on_class, 1 | 4);
        let n = if spread { arity } else { call.x as usize + (call.op == OpCode::CallMethodR && !stat || ctor) as usize };
        // a class call takes the instance first then arguments with the second from `b`
        let args = if ctor { [0, call.b, call.c, rest.a, rest.b] } else if stat { [call.c, rest.a, rest.b, rest.c, 0] } else { [call.b, call.c, rest.a, rest.b, rest.c] };
        // a call of over five arguments names those past the fifth apart
        let more = core::mem::take(&mut self.more_args);
        let arg_at = |p: usize| if p < 5 { args[p] } else { more.get(p - 5).copied().unwrap_or(0) };
        // defaults belong to parameters marked with `=` in their order
        let default = |l: &Self, p: usize| { let c = &l.wanted[k].1; c.defaults[c.params.iter().take(p).filter(|m| m.ends_with('=')).count()] };
        // a parameter past the arguments or one a keyword call skipped takes its default
        let gaps = core::mem::take(&mut self.gaps);
        let given = |p: usize| p < n && gaps & 1 << p == 0;
        for p in (if stat { 0 } else { 2 })..arity {
            let arg = arg_at(p);
            if spread { break; }
            let at = self.out.len();
            // a `by_body` section takes the defaults of the function the call makes
            if by_body && !given(p) { continue; }
            self.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: if given(p) { arg } else { 0 }, c: 0 });
            self.prep.push((at, 0, Prep::Param(k, p)));
            if !given(p) { let v = default(self, p); self.prep.push((at, 1, Prep::Const(v))); }
        }
        let at = self.out.len();
        // a parameter left to its default copies itself with the default written first
        for p in (0..if stat { 0 } else { arity.min(2) }).filter(|&p| !given(p)) { let v = if by_body { Prep::Param(k, p) } else { Prep::Const(default(self, p)) }; self.prep.push((at, if ctor { p as u8 } else { 1 + p as u8 }, v)); }
        let row = self.kwrow.take();
        if let Some((regs, op)) = self.kwcall.take() { self.kwstubs.push((at, regs, op, row)); }
        let ret = self.ret_to.take().unwrap_or(self.out.len() as u16 + HEADER);
        if let Some(site) = self.wanted[k].1.site() { self.seeds.push((self.out.len(), site)); }
        self.heads.push((self.out.len(), k, rest));
        self.emit(Ins { op: OpCode::CallSectionR, ..call });
        // a resume from other code takes temporaries back from their rows
        let at = self.out.len() - 1;
        for (k, e) in below.iter().enumerate() { if !e.name && self.is_temp(e.r as usize) { let r = self.row_reg(k); self.resumes.push((at, e.r, r)); } }
        let (held, method) = (call.op == OpCode::CallR, call.op == OpCode::CallMethodR || spread);
        // where a call its section's check turns down goes instead laid after the sections
        self.emit_all(&[
            Ins { op: OpCode::StoreTopR, x: 0, a: dest, b: ret, c: 0 },
            Ins { op: OpCode::SectionR, x: outer as u8 | (held as u8) << 2 | (method as u8) << 3 | (ctor as u8) << 4 | ((by_body || twin) as u8) << 5 | (spread as u8) << 6, a: 0, b: 0, c: k as u16 },
            Ins { op: OpCode::SectionR, x: STUB, a: 0, b: 0, c: 0 },
        ]);
    }

    /* lays keyword head `h` again past the end for `callee` or false when names miss */
    pub(super) fn rekeyed(&mut self, h: usize, callee: Callee<'p>, g: u16) -> bool {
        let (store, sect, stub) = (self.out[h + 1], self.out[h + 2], self.out[h + 3]);
        if sect.x & 1 == 0 { return false; }
        let (start, mut j) = (stub.a as usize, stub.a as usize);
        while self.out.get(j).is_some_and(|m| m.op == OpCode::Move && m.x == 0) { j += 1; }
        // a stub's operands lie in its row or move there from the call's registers
        let Some(&Ins { op: OpCode::RowsR, a: row, b: op, c: n, .. }) = self.out.get(j).filter(|w| w.x == OpCode::Call as u8) else { return false };
        let mut regs: Vec<u16> = (0..n).map(|q| row + q).collect();
        for m in &self.out[start..j] { if let Some(q) = m.a.checked_sub(row).filter(|&q| q < n) { regs[q as usize] = m.b; } }
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let Some(under) = regs.len().checked_sub(1 + np + 2 * nk) else { return false };
        // what was pending under the call taken back past its store with names dropped
        let below: Option<Vec<Entry>> = (0..under).map(|e| match self.out.get(j + 2 + under - 1 - e) {
            Some(t) if t.op == OpCode::PopTop => Some(Entry { r: regs[e], name: true }),
            Some(t) if t.op == OpCode::StoreTopR && t.a == regs[e] => Some(Entry { r: regs[e], name: false }),
            _ => None,
        }).collect();
        let (Some(below), true) = (below, self.out.get(j + 1).is_some_and(|t| matches!(t.op, OpCode::StoreTopR | OpCode::RowsBackR))) else { return false };
        let kw = regs[under..].to_vec();
        let Some((m, slots, first)) = Self::kw_slots(&kw, op, &callee, (&self.chunk.constants, self.cbase)) else { return false };
        self.keyed = true;
        let Some(to) = self.want_callee(g, callee, m) else { return false };
        (self.cur, self.quiet) = (u32::from(self.orig[h]), u32::from(self.back[h]));
        let from = self.out.len();
        self.kw_lay(kw, op, m, &slots, first, store.a, to, &below);
        // its section and stub come back past the head then past the old one
        self.emit(Ins { op: OpCode::JumpR, x: 0, a: (h + HEADER as usize) as u16, b: 0, c: 0 });
        self.displaced_to(h, from);
        true
    }

    /* how a keyword call fills `callee`'s parameters or none when a name or default misses */
    pub(super) fn kw_slots(kw: &[u16], op: u16, callee: &Callee, (lits, base): (&[crate::syntax::chunk::Value], usize)) -> Option<(usize, [Option<u16>; 5], usize)> {
        let (np, nk) = ((op & 0xFF) as usize, (op >> 8) as usize);
        let first = callee.ctor().is_some() as usize;
        if kw.len() != 1 + np + 2 * nk || callee.star.is_some() || callee.dstar.is_some() || first + np > callee.positional { return None; }
        let mut slots: [Option<u16>; 5] = [None; 5];
        for p in 0..np { *slots.get_mut(first + p)? = Some(kw[1 + p]); }
        for q in 0..nk {
            let (name, value) = (kw[1 + np + 2 * q], kw[2 + np + 2 * q]);
            let crate::syntax::chunk::Value::Str(key) = lits.get((name as usize).checked_sub(base)?)? else { return None };
            let p = callee.params.iter().position(|q| crate::syntax::ssa::param_base_name(q) == key).filter(|&p| p >= first + np && p >= callee.posonly && p < callee.arity && p < 5)?;
            if slots[p].is_some() { return None; }
            slots[p] = Some(value);
        }
        let m = slots.iter().rposition(|s| s.is_some())? + 1;
        if (first..m).any(|p| p >= first && slots[p].is_none() && p < callee.arity - callee.defaults.len()) { return None; }
        Some((m, slots, first))
    }

    /* where each operand of a call to an `*args` or `**kwargs` function goes */
    pub(super) fn packed(kw: &[u16], op: u16, callee: &Callee, (lits, base): (&[crate::syntax::chunk::Value], usize)) -> Option<Packed> {
        let (np, nk, n) = ((op & 0xFF) as usize, (op >> 8) as usize, callee.positional);
        let apart = |p: usize| callee.star == Some(p) || callee.dstar == Some(p);
        if callee.star.is_none() && callee.dstar.is_none() || np > n && callee.star.is_none() || callee.arity > 5 || kw.len() != 1 + np + 2 * nk { return None; }
        let mut slots: Vec<Option<usize>> = alloc::vec![None; callee.arity];
        for (k, s) in slots.iter_mut().enumerate().take(np.min(n)) { *s = Some(1 + k); }
        let mut pairs = Vec::new();
        for q in 0..nk {
            let name = 1 + np + 2 * q;
            let crate::syntax::chunk::Value::Str(key) = lits.get((kw[name] as usize).checked_sub(base)?)? else { return None };
            match (callee.posonly..callee.arity).find(|&p| !apart(p) && crate::syntax::ssa::param_base_name(&callee.params[p]) == key) {
                Some(p) if slots[p].is_none() => slots[p] = Some(name + 1),
                Some(_) => return None,
                None if callee.dstar.is_some() => pairs.extend([name, name + 1]),
                None => return None,
            }
        }
        // a parameter no operand passes must have a default
        if (0..callee.arity).any(|p| slots[p].is_none() && !apart(p) && !callee.params[p].ends_with('=')) { return None; }
        Some((slots, 1 + n.min(np)..1 + np, pairs))
    }

    /* builds a packed call's tuple and dict returning its registers and bits of defaulted parameters */
    fn pack(&mut self, regs: &[u16], (slots, rest, pairs): &Packed, tuple: Option<(u16, usize)>, dict: Option<(u16, usize)>, mut next: Option<&mut usize>) -> (Vec<u16>, u8) {
        let mut list: Vec<u16> = slots.iter().map(|s| s.map_or(0, |s| regs[s])).collect();
        let gaps = slots.iter().enumerate().filter(|s| s.1.is_none()).fold(0u8, |g, (p, _)| g | 1 << p);
        let packed = [tuple, dict].iter().flatten().fold(0u8, |g, t| g | 1 << t.1);
        // the moves into a row keep off every operand and both packs
        let avoid: Vec<u16> = regs.iter().copied().chain([tuple, dict].into_iter().flatten().map(|t| t.0)).collect();
        for (into, from, build) in [(tuple, rest.clone().collect::<Vec<_>>(), OpCode::BuildTuple), (dict, pairs.clone(), OpCode::BuildDict)] {
            let Some((t, p)) = into else { continue };
            let from: Vec<u16> = from.iter().map(|&q| regs[q]).collect();
            self.build_into(build, t, &from, next.as_deref_mut(), &avoid);
            list[p] = t;
        }
        (list, gaps & !packed)
    }

    /* builds `kind` of `items` into `t` from their row or moves into a fresh one */
    fn build_into(&mut self, kind: OpCode, t: u16, items: &[u16], next: Option<&mut usize>, avoid: &[u16]) {
        let n = items.len();
        let b = if n == 0 || items.windows(2).all(|w| w[1] == w[0] + 1) { items.first().copied().unwrap_or(0) } else {
            let b = match next {
                Some(next) => { *next += n; (*next - n) as u16 }
                None => self.free_block(n, avoid),
            };
            for (q, &r) in items.iter().enumerate() { self.emit(Ins { op: OpCode::Move, x: 0, a: b + q as u16, b: r, c: 0 }); }
            b
        };
        self.emit(Ins { op: OpCode::BuildR, x: kind as u8, a: t, b, c: n as u16 });
    }

    /* lays a packed call's head building its tuple and dict straight into section `to`'s parameters */
    #[allow(clippy::too_many_arguments)]
    pub(super) fn pack_lay(&mut self, kw: &[u16], op: u16, packed: &Packed, store: u16, to: usize, below: &[Entry], row: Option<u16>) {
        let w = &self.wanted[to].1;
        let (arity, apart) = (w.arity, [w.star, w.dstar]);
        let (slots, rest, pairs) = packed;
        // the rest pack into the parameter from the call's row or one at a time
        for (p, from, dict) in [(apart[0], rest.clone().collect::<Vec<_>>(), false), (apart[1], pairs.clone(), true)] {
            let Some(p) = p else { continue };
            if let Some(r) = row.filter(|_| from.windows(2).all(|w| w[1] == w[0] + 1)) {
                self.prep.push((self.out.len(), 0, Prep::Param(to, p)));
                let kind = if dict { OpCode::BuildDict } else { OpCode::BuildTuple };
                self.emit(Ins { op: OpCode::BuildR, x: kind as u8, a: 0, b: r + from.first().map_or(0, |&q| q as u16), c: from.len() as u16 });
                continue;
            }
            let made = if dict { OpCode::BuildDict } else { OpCode::BuildList };
            self.prep.push((self.out.len(), 0, Prep::Param(to, p)));
            self.emit(Ins { op: OpCode::BuildR, x: made as u8, a: 0, b: 0, c: 0 });
            for g in from.chunks(if dict { 2 } else { 1 }) {
                self.prep.push((self.out.len(), 0, Prep::Param(to, p)));
                let (k, v) = (kw[g[0]], kw[*g.last().unwrap_or(&g[0])]);
                self.emit(if dict { Ins { op: OpCode::MapAddR, x: 1, a: 0, b: k, c: v } } else { Ins { op: OpCode::ListAppendR, x: 1, a: 0, b: v, c: v } });
            }
            if !dict {
                for f in [0, 1] { self.prep.push((self.out.len(), f, Prep::Param(to, p))); }
                self.emit(Ins { op: OpCode::ListToTupleR, x: 0, a: 0, b: 0, c: 0 });
            }
        }
        let r = |p: usize| slots.get(p).copied().flatten().map_or(0, |q| kw[q]);
        self.gaps = (0..arity).filter(|&p| slots[p].is_none() && !apart.contains(&Some(p))).fold(0, |g, p| g | 1 << p);
        self.kwcall = Some((kw.to_vec(), op));
        let moves = self.out.len();
        self.head(Ins { op: OpCode::CallR, x: arity as u8, a: kw[0], b: r(0), c: r(1) }, Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) }, store, to, true, below);
        // the tuple and dict already in their parameters are copied onto themselves by the head
        let call = self.out.len() - HEADER as usize;
        for p in apart.into_iter().flatten() { self.prep.push(if p >= 2 { (moves + p - 2, 1, Prep::Param(to, p)) } else { (call, 1 + p as u8, Prep::Param(to, p)) }); }
    }

    /* lays keyword call `op` to section `to` as `kw_slots` matched with a stub as written */
    #[allow(clippy::too_many_arguments)]
    pub(super) fn kw_lay(&mut self, kw: Vec<u16>, op: u16, m: usize, slots: &[Option<u16>; 5], first: usize, store: u16, to: usize, below: &[Entry]) {
        let r = |p: usize| slots.get(p).copied().flatten().unwrap_or(0);
        let f = kw[0];
        let (call, rest) = if first == 1 {
            (Ins { op: OpCode::CallR, x: (m - 1) as u8, a: f, b: r(1), c: r(2) }, Ins { op: OpCode::ArgsR, x: 0, a: r(3), b: r(4), c: 0 })
        } else {
            (Ins { op: OpCode::CallR, x: m as u8, a: f, b: r(0), c: r(1) }, Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) })
        };
        self.gaps = (first..m).filter(|&p| p >= first && slots[p].is_none()).fold(0, |g, p| g | 1 << p);
        self.kwcall = Some((kw, op));
        self.head(call, rest, store, to, true, below);
    }

    /* lays wanted sections past the code behind a jump using registers from `frame` then constants */
    pub(super) fn lay(&mut self, frame: usize) -> (Option<Box<Block>>, usize) {
        if self.wanted.is_empty() { return (None, frame); }
        let (over, outside) = (self.out.len(), self.heads.len());
        // room for each section's body plus a few instructions so the code grows once
        let room = self.wanted.iter().map(|w| w.1.body.ins.len() + 4).sum::<usize>() + 12 * self.heads.len();
        self.out.reserve_exact(room);
        self.orig.reserve_exact(room);
        self.back.reserve_exact(room);
        // a growth adds no jump as the grown code's own moves to the new end
        if self.old.is_none() { self.emit(Ins { op: OpCode::Jump, x: 0, a: 0, b: 0, c: 0 }); }
        let (next, extra, uses, mut sections) = (frame, Vec::<Val>::new(), Vec::new(), Vec::with_capacity(self.wanted.len()));
        // per section a generator's step register and its fixed value when yielding at one place
        let mut gens: Vec<(u16, Option<Val>)> = Vec::new();
        if let Some(old) = self.old {
            sections.extend(old.sections.iter().cloned());
            gens.extend(old.sections.iter().map(|s| (s.step_reg, None)));
        }
        let slots = crate::util::hash::FxHashMap::<u64, usize>::default();
        let main: Vec<(usize, u16, Option<u16>, u16, bool)> = self.genfors.iter().map(|g| (g.k, self.at[g.exit] as u16, g.global.is_none().then_some(g.target), g.head as u16 + HEADER, true)).collect();
        self.gen_loops.extend(main);
        // each loop a section lays over a generator section with its intake entry and step
        let late: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut st = LayState { next, extra, uses, sections, gens, slots, late };
        self.calls = OnceCell::new();
        for k in self.first..self.wanted.len() { let plain = self.lays_plain(k); self.place(k, &mut st, plain); }
        self.calls = OnceCell::new();
        let LayState { mut next, mut extra, mut uses, mut sections, gens, mut slots, late, .. } = st;
        for &(at, k, _) in &self.heads { (self.out[at + 2].a, self.out[at + 2].b) = (sections[k].start as u16, sections[k].link + 1); }
        // a loop steps a generator section by its register the entry sets back to itself
        for &(k2, entry, mv, step) in &late {
            let r = gens[k2].0;
            (self.out[step].a, self.out[step].b, self.out[mv].a) = (r, r, r);
            let v = Val::int((entry as i64) << 16 | r as i64);
            let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 });
            uses.push((mv, 1, slot));
        }
        for (at, f, prep) in core::mem::take(&mut self.prep) {
            *crate::vm::block::field_mut(&mut self.out[at], f) = match prep {
                Prep::Param(k, p) => sections[k].link + 1 + p as u16,
                Prep::Const(v) => { let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 }); uses.push((at, f, slot)); 0 }
                Prep::GenReg(k) => gens[k].0,
                Prep::GenVal(k) => gens[k].0 + 1,
                Prep::GenResume(k) | Prep::GenEntry(k) => {
                    let entry = self.genfors.iter().find(|g| g.k == k).map_or(0, |g| g.step as i64 - 3);
                    let v = if matches!(prep, Prep::GenEntry(_)) { Val::int(entry << 16 | gens[k].0 as i64) } else { gens[k].1.unwrap_or_else(Val::none) };
                    let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 });
                    uses.push((at, f, slot));
                    0
                }
            };
        }
        // the stubs of the calls from outside every section
        for h in 0..outside {
            let (at, k, rest) = self.heads[h];
            match self.genfors.iter().position(|g| g.head == at) {
                Some(g) => self.gen_stub(g, gens[k].0 + 1),
                None => self.stub(at, k, rest, &mut next),
            }
        }
        // a resume past a head takes the temporaries under it back from their rows
        let resumes = core::mem::take(&mut self.resumes);
        let (mut k, mut moved) = (0, Vec::new());
        while k < resumes.len() {
            let (at, entry) = (resumes[k].0, self.out.len());
            self.cur = u32::from(self.orig[at]);
            while k < resumes.len() && resumes[k].0 == at { self.emit(Ins { op: OpCode::Move, x: 0, a: resumes[k].1, b: resumes[k].2, c: 0 }); k += 1; }
            self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: self.out[at + 1].a, b: 0, c: 0 });
            self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER, b: 0, c: 0 });
            moved.push((at as u32 + 1, entry as u32));
        }
        // each resume entering a head's store enters its stub instead
        if !moved.is_empty() { for a in self.at.iter_mut() { if let Ok(m) = moved.binary_search_by_key(&(*a & 0xFFFF), |x| x.0) { *a = *a & !0xFFFF | moved[m].1; } } }
        // a stub's call reads the constants its head reads
        let copies: Vec<(usize, u8, usize)> = uses.iter().filter(|u| self.out[u.0].op == OpCode::CallSectionR && self.out.get(u.0 + 3).is_some_and(|t| t.op == OpCode::SectionR && t.x == STUB) && !self.kwstubs.iter().any(|s| s.0 == u.0))
            .map(|&(at, f, slot)| (self.out[at + 3].b as usize, f, slot)).collect();
        uses.extend(copies);
        // the heads a growth chained jump to the sections laid for them
        for &(at, k) in &self.chain_heads { (self.out[at + 2].a, self.out[at + 2].b) = (sections[k].start as u16, sections[k].link + 1); }
        if self.old.is_none() { self.out[over].a = self.out.len() as u16; }
        for &(at, f, slot) in &uses { *crate::vm::block::field_mut(&mut self.out[at], f) = (next + slot) as u16; }
        let frame = next + extra.len();
        let callees = core::mem::take(&mut self.wanted);
        let callees = crate::r#unsafe::kept_callees(callees);
        let Some(old) = self.old else {
            let binds = Cell::new(crate::vm::scope::REBOUND.load(core::sync::atomic::Ordering::Relaxed));
            let (alts, alt_tab) = alts_of(&self.alts, &sections);
            // a method's head looks its table up off its section
            for &(k, start, mask) in &alts { sections[k as usize].table = (start, mask); }
            // each lookup names where its table starts and its size less one
            for &(at, k) in &self.lookups {
                if let Some(&(_, start, mask)) = alts.iter().find(|a| a.0 == k as u32) { (self.out[at].b, self.out[at].c) = (start as u16, mask as u16); }
            }
            let loops = self.genfors.iter().map(|g| (g.step as u32 - 3, self.at[g.exit] & 0xFFFF, g.k as u32, g.head as u32)).collect();
            let ranges = alloc::vec![(next, extra.len())].into();
            return (Some(Box::new(Block { sections: sections.into(), extra: extra.into(), ranges, callees, lineage: 0, grown: { let d = core::mem::take(&mut self.displaced); if d.is_empty() { OnceCell::new() } else { OnceCell::from(Box::new(Grown { displaced: d, ..Default::default() })) } }, binds, by_class: alt_tab.iter().map(|_| Cell::new((Val::undef(), u32::MAX, 0))).collect(), alt_tab, loops, moved: false })), frame);
        };
        // a growth keeps old tables in place laying grown heads' tables anew past them
        let (mut tab, mut by_class) = (old.alt_tab.to_vec(), old.by_class.to_vec());
        let mut grown: Vec<usize> = self.alts[self.alts_first..].iter().map(|a| a.0).collect();
        grown.sort_unstable();
        grown.dedup();
        let mut index = Vec::new();
        for k in grown {
            let pairs: Vec<(usize, usize)> = self.alts.iter().filter(|a| a.0 == k).copied().collect();
            let (mut i, t) = alts_of(&pairs, &sections);
            let Some(&(_, start, mask)) = i.first() else { continue };
            let at = tab.len() as u32;
            tab.extend_from_slice(&t);
            by_class.extend(t.iter().map(|_| Cell::new((Val::undef(), u32::MAX, 0))));
            i[0] = (k as u32, start + at, mask);
            sections[k].table = (start + at, mask);
            index.push(i[0]);
        }
        // each lookup names a grown head's new table or the one its section kept
        let len = self.out.len();
        for at in 0..len {
            let ins = self.out[at];
            if ins.op != OpCode::CallValueR { continue; }
            let k = if self.lookups.iter().any(|l| l.0 == at) { self.lookups.iter().find(|l| l.0 == at).map(|l| l.1 as u32) } else { self.out.get(ins.a as usize + 2).map(|s| s.c as u32) };
            let table = k.and_then(|k| index.iter().find(|a| a.0 == k).map(|a| (a.1, a.2)).or_else(|| sections.get(k as usize).map(|s| s.table).filter(|t| t.1 != u32::MAX)));
            if let Some((start, mask)) = table { (self.out[at].b, self.out[at].c) = (start as u16, mask as u16); }
        }
        let mut ranges = old.ranges.to_vec();
        if !extra.is_empty() { ranges.push((next, extra.len())); }
        let all: Vec<Val> = old.extra.iter().copied().chain(extra).collect();
        (Some(Box::new(Block { sections: sections.into(), extra: all.into(), ranges: ranges.into(), callees, lineage: old.lineage, grown: OnceCell::from(Box::new({
            let g = old.grown();
            let mut displaced = g.displaced.clone();
            displaced.append(&mut self.displaced);
            displaced.sort_unstable();
            Grown { src: g.src.clone(), facts: g.facts.clone(), displaced, moved: g.moved.clone(), tried: core::cell::RefCell::new(g.tried.borrow().clone()), turned: Cell::new(g.turned.get()), last: Cell::new(g.last.get()), pending: Default::default(), refused: Cell::new(g.refused.get()), refused_at: Cell::new(g.refused_at.get()) }
        })), binds: Cell::new(old.binds.get()), by_class: by_class.into(), alt_tab: tab.into(), loops: old.loops.clone(), moved: old.moved })), frame)
    }

    /* whether section `k` is laid one instruction per body instruction as `plain_lay` takes */
    fn lays_plain(&self, k: usize) -> bool {
        let c = &self.wanted[k].1;
        !c.generator && c.cells.is_empty() && !c.by_body && c.module == self.facts.module && self.plain_lay(k, &c.body)
    }

    /* lays section `k` past the code taking registers from `next` and constants as extras */
    fn place(&mut self, k: usize, st: &mut LayState, plain: bool) {
        let mut next = st.next;
        let LayState { extra, uses, sections, gens, slots, late, .. } = st;
        let (global, callee) = (self.wanted[k].0, &self.wanted[k].1);
        let (value, body, arity, names, consts, module, fi) = (callee.value, callee.body.clone(), callee.arity, callee.names, callee.consts.clone(), callee.module, callee.fi);
        // the callee's constants lie past its names and the rows its code keeps
        let cb = body.cbase as usize;
        let (cells, ctor, by_body) = (callee.cells.clone(), callee.ctor(), callee.by_body);
        let mut slot_map: Vec<u16> = Vec::new();
        let mut cell_regs = Vec::new();
        let mut orig = Vec::new();
        let recurs = !plain && self.called(k).iter().any(|&(_, t)| self.reaches(t, k));
        let (link, start, inner) = (next as u16, self.out.len(), self.heads.len());
        // at least two parameters so a call copies both argument registers whatever it passes
        next += 1 + arity.max(2);
        // a generator's step return point and a binding loop's value take two registers
        let shared = self.gen_steps.iter().find(|g| g.0 == k).map(|g| g.1);
        let stepped = self.wanted[k].1.generator.then(|| shared.unwrap_or_else(|| { next += 2; (next - 2) as u16 }));
        let lp = self.gen_loops.iter().find(|g| g.0 == k).copied();
        let exit = lp.map_or(0, |g| g.1);
        // each `yield`'s step register move and the body index past it
        let mut yielded: Vec<(usize, usize)> = Vec::new();
        // stubs of yields marking the iterators they hold by entry in `yielded` and stub position
        let mut unmarks: Vec<(usize, usize)> = Vec::new();
        let (mut resume, mut fused_at): (Option<Val>, Option<(u32, u32)>) = (None, None);
        let mut steps: Vec<(u32, u32, u32, u32)> = Vec::new();
        let mut held_at = u16::MAX;
        // loop variable and body start kept only for the code's own loop taking call returns
        let body_of_loop = lp.filter(|g| g.4).map(|g| (g.2.unwrap_or(stepped.unwrap_or(0) + 1), g.3));
        // this body's loops over generator sections whose back jumps go to their steps
        let mut loops_in: Vec<(usize, usize, usize, usize, usize, usize, u16)> = Vec::new();
        if plain {
            // plain ops lay as is with registers remapped by kind and jumps offset to match
            let first = if cb > names { crate::vm::rows::first_row(names) } else { names };
            let (tail, locals, rows) = (cb + consts.len(), names - arity, cb - first);
            let base = next as u16;
            next += locals + rows + body.frame.saturating_sub(tail + 3);
            let specials = (self.cbase + self.consts) as u16;
            let reg = |r: u16| -> u16 {
                let r = r as usize;
                if r < arity { link + 1 + r as u16 }
                else if r < names { base + (r - arity) as u16 }
                else if r < cb { base + (locals + r.saturating_sub(first)) as u16 }
                else if r < tail { r as u16 }
                else if r < tail + 3 { specials + (r - tail) as u16 }
                else { base + (locals + rows + r - tail - 3) as u16 }
            };
            let ins = &body.ins;
            // each body instruction lays one or more so room is reserved up front
            orig.reserve(ins.len() + 1);
            for v in [&mut self.orig, &mut self.back] { v.reserve(ins.len() + 8); }
            self.out.reserve(ins.len() + 8);
            // a checked or returned local may be unbound so each call unbinds it first
            let mut checked = 0u64;
            for i in ins.iter().filter(|i| i.op == OpCode::Move && i.x == 1 || i.op == OpCode::ReturnR) {
                if (arity..names).contains(&(i.b as usize)) { checked |= 1 << (i.b as usize - arity); }
            }
            for r in (0..locals).filter(|r| checked & 1 << r != 0) { self.emit(Ins { op: OpCode::UnbindR, x: 0, a: base + r as u16, b: 0, c: 0 }); }
            let at = self.out.len();
            orig.reserve(at - start + ins.len() + 1);
            orig.extend(core::iter::repeat_n(body.orig(0), at - start));
            // a call lays several instructions so jumps land where their targets were laid
            let calls = ins.iter().any(|i| matches!(i.op, OpCode::CallGlobalR | OpCode::CallR));
            let (mut starts, mut jumps, mut off) = (Vec::new(), Vec::new(), false);
            for (j, &i) in ins.iter().enumerate() {
                if calls { starts.push(self.out.len()); }
                let here = self.out.len();
                let mut out = i;
                if i.op == OpCode::StoreTopR { continue; }
                // a spill into a row only the body's own park reads is left out
                if i.op == OpCode::Move && (names..cb).contains(&(i.a as usize)) && row_dead(ins, j + 1, i.a) { continue; }
                if let Some(to) = self.plain_call(k, ins, j).filter(|_| matches!(i.op, OpCode::CallGlobalR | OpCode::CallR)) {
                    let mut call = Ins { a: if i.op == OpCode::CallR { reg(i.a) } else { i.a }, b: reg(i.b), c: reg(i.c), ..i };
                    // a class call moves a constant second argument into a register of its own first
                    let moved = self.wanted[to].1.ctor().is_some() && i.x >= 2 && (cb..tail).contains(&(i.c as usize));
                    if moved {
                        let v = consts[i.c as usize - cb];
                        uses.push((self.out.len(), 1, *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 })));
                        call.c = next as u16;
                        next += 1;
                        self.emit(Ins { op: OpCode::Move, x: 0, a: call.c, b: 0, c: 0 });
                    }
                    self.head(call, Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 }, reg(ins[j + 1].a), to, false, &[]);
                    // the constants it passes first and second are read by the call itself
                    let h = self.out.len() - HEADER as usize;
                    for (f, r) in [(1u8, i.b), (2, i.c)].into_iter().take(i.x as usize).take(if moved { 1 } else { 2 }) {
                        if (cb..tail).contains(&(r as usize)) { let v = consts[r as usize - cb]; uses.push((h, f, *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 }))); }
                    }
                    orig.extend(core::iter::repeat_n(body.orig(j), self.out.len() - here));
                    continue;
                }
                if i.op == OpCode::ReturnR {
                    // a parameter nothing deletes or a constant is bound which a `c` of 1 marks
                    let bound = (cb..tail + 3).contains(&(i.b as usize)) || (i.b as usize) < arity;
                    out = Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: reg(i.b), c: bound as u16 };
                    if (cb..tail).contains(&(i.b as usize)) { let v = consts[i.b as usize - cb]; uses.push((here, 1, *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 }))); }
                } else {
                    let (dest, reads, jump) = crate::vm::block::role(&i).unwrap_or((false, &[], false));
                    for &f in reads {
                        let r = crate::vm::block::field(&i, f);
                        *crate::vm::block::field_mut(&mut out, f) = reg(r);
                        if (cb..tail).contains(&(r as usize)) { let v = consts[r as usize - cb]; uses.push((here, f, *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 }))); }
                    }
                    if dest { out.a = reg(i.a); }
                    off |= jump && i.a as usize >= ins.len();
                    if jump && calls { jumps.push(here); } else if jump { out.a = (at + i.a as usize) as u16; }
                }
                self.emit(out);
                orig.push(body.orig(j));
            }
            // running off the end returns `None` as does a jump past the last instruction
            starts.push(self.out.len());
            if runs_off(ins, off) {
                self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: specials, c: 1 });
                orig.push(body.orig(ins.len().saturating_sub(1)));
            }
            for p in jumps { self.out[p].a = starts[self.out[p].a as usize] as u16; }
            // its calls' stubs lie inside it too naming what they name in its function's chunk
            for h in inner..self.heads.len() {
                let (at, hk, rest) = self.heads[h];
                let from = self.out.len();
                self.stub(at, hk, rest, &mut next);
                for _ in from..self.out.len() { orig.push(orig[at - start]); }
            }
            // an init returns nil so the call gives back its instance
            if ctor.is_some() { for r in &mut self.out[start..] { if r.op == OpCode::ReturnSectionR && r.a == link { (r.b, r.c) = (link + 1, 1); } } }
        } else {
            // callee registers become parameters past the link or extra slots or shared singletons or fresh
            let tail = cb + consts.len();
            let first = if cb > names { crate::vm::rows::first_row(names) } else { names };
            let map: Vec<Option<u16>> = (0..body.frame).map(|r| match r {
                r if r < arity => Some(link + 1 + r as u16),
                // the registers a body of under two names lays its rows past hold nothing
                r if (names..first).contains(&r) => None,
                // a `by_body` section's cell is a register the call fills
                r if by_body && cells.iter().any(|c| c.0 == r) => { next += 1; Some((next - 1) as u16) }
                r if (cb..tail).contains(&r) || cells.iter().any(|c| c.0 == r) => None,
                r if (tail..tail + 3).contains(&r) => Some((self.cbase + self.consts + r - tail) as u16),
                _ => { next += 1; Some((next - 1) as u16) }
            }).collect();
            if by_body { cell_regs = cells.iter().map(|c| map.get(c.0).copied().flatten().unwrap_or(0)).collect(); }
            let ins = &body.ins;
            // each body instruction lays one or more so room is reserved up front
            orig.reserve(ins.len() + 1);
            for v in [&mut self.orig, &mut self.back] { v.reserve(ins.len() + 8); }
            self.out.reserve(ins.len() + 8);
            // a body from another module reads its bindings and calls them through a dedicated register
            let cross = (module != self.facts.module).then(|| { next += 1; (next - 1) as u16 });
            // a checked or returned local may be unbound so each call unbinds it first
            let mut checked: Vec<u16> = ins.iter().filter(|i| i.op == OpCode::Move && i.x == 1 || i.op == OpCode::ReturnR).map(|i| i.b)
                .filter(|&r| (arity..names).contains(&(r as usize))).filter_map(|r| map.get(r as usize).copied().flatten()).collect();
            checked.sort_unstable();
            checked.dedup();
            for &r in &checked { self.emit(Ins { op: OpCode::UnbindR, x: 0, a: r, b: 0, c: 0 }); }
            // cells are made on entry and an exception instance keeps its call's arguments as `args`
            if self.wanted[k].1.exc { self.emit(Ins { op: OpCode::ExcArgsR, x: 0, a: link + 1, b: link + 2, c: arity as u16 - 1 }); }
            let cellvars = self.wanted[k].1.cellvars.clone();
            for &c in &cellvars {
                let Some(Some(r)) = map.get(c).copied() else { continue };
                if c >= arity { self.emit(Ins { op: OpCode::UnbindR, x: 0, a: r, b: 0, c: 0 }); }
                self.emit(Ins { op: OpCode::WrapCellR, x: 0, a: r, b: 0, c: 0 });
            }
            // what the body holds read in one pass
            let marks = body_marks(ins);
            // generators keep slots too so one made before lowering hands the section its state
            if stepped.is_some() || marks.makes { slot_map = map.iter().map(|m| m.unwrap_or(u16::MAX)).collect(); }
            // a return inside its `for` loops drops their iterators as a frame's end would
            let loops = crate::vm::block::loops_around(ins);
            let deletes = by_body || marks.deletes;
            // bodies that build drop their last returned container on entry so collection can free it
            let builds = marks.builds;
            let tried = marks.tried;
            // a body returning what it left on the stack returns it from its own register
            let guard = marks.stacked.then(|| { next += 1; (next - 1) as u16 });
            if builds && let Some(t) = guard { self.emit(Ins { op: OpCode::UnbindR, x: 0, a: t, b: 0, c: 0 }); }
            // registers live across recursion found only when recursing using each `try` and handler start
            let tries: Vec<(usize, usize)> = ins.iter().enumerate().filter(|(_, i)| matches!(i.op, OpCode::SetupExcept | OpCode::SetupFinally)).map(|(j, i)| (j, body.lowered(i.a as usize))).collect();
            let live = if recurs { crate::vm::block::live_across(ins, body.frame, &tries) } else { Vec::new() };
            // each call of this very section with the parameters it passes on unchanged
            let turning = recurs && !by_body && ctor.is_none() && self.wanted[k].1.on_class == 0;
            let targets = crate::vm::block::targets(ins, &tries);
            let selfs: Vec<(usize, u64)> = if !turning { Vec::new() } else {
                self.called(k).iter().filter(|&&(j, to)| to == k && matches!(ins[j].op, OpCode::CallGlobalR | OpCode::CallR | OpCode::CallMethodR) && packs(&self.wanted[k].1, ins[j].x as usize).is_none())
                    .map(|&(j, _)| (j, crate::vm::block::passed(ins, j, arity))).collect()
            };
            // parameters each recursion passes unchanged and reads after which a level keeps for its caller
            let bits = |r: &[u16]| r.iter().filter(|&&r| r < 64).fold(0u64, |m, &r| m | 1 << r);
            let fixed = crate::vm::block::params_written(ins, arity) | cellvars.iter().filter(|&&c| c < 64).fold(0u64, |m, &c| m | 1 << c);
            let steady = selfs.iter().fold(0u64, |m, &(j, same)| m | same & bits(&live[j])) & !fixed;
            // a level keeps registers read after recursion plus steady parameters less constants and `here`
            let kept = |live: &[u16], here: u64| -> Vec<u16> {
                live.iter().copied().chain((0..arity.min(64) as u16).filter(|&q| steady >> q & 1 != 0 && !live.contains(&q)))
                    .filter(|&r| !(cb..tail + 3).contains(&(r as usize)) && !(r < 64 && here >> r & 1 != 0)).filter_map(|r| map.get(r as usize).copied().flatten()).collect()
            };
            let (mut starts, mut jumps, mut skip) = (Vec::with_capacity(ins.len() + 1), Vec::new(), 0);
            // a shared step register holds the other's value so each yield here sets it too
            let several = shared.is_some() || marks.yields > 1;
            // `try` or `with` in a generator saves start stack depths in two registers for yields
            let guard_at = (stepped.is_some() && crate::vm::block::guarded(ins)).then(|| { next += 2; (next - 2) as u16 });
            if let Some(g) = guard_at { self.emit(Ins { op: OpCode::PopIter, x: 3, a: g, b: 0, c: 0 }); held_at = g; }
            // a register for the value a `yield` takes off the stack
            let held = if stepped.is_some() && marks.yields_stack { next += 1; (next - 1) as u16 } else { 0 };
            // `Jump`s to a return left out so jumps landing on them land on that return
            let mut gone = Vec::new();
            // a value pushed before an ordinary call and taken back after its result
            let under = |j: usize| j > 0 && ins[j - 1].op == OpCode::PushRegs && ins[j - 1].x == 1 && ins.get(j + 2 + wide(ins[j]) as usize).is_some_and(|t| t.op == OpCode::PopTop || t.op == OpCode::StoreTopR && t.x == 0 && t.a == ins[j - 1].a);
            // callee constants live in extra frame slots whose registers are patched once all are known
            let konst = |at: usize, f: u8, r: u16, extra: &mut Vec<Val>, slots: &mut crate::util::hash::FxHashMap<u64, usize>, uses: &mut Vec<(usize, u8, usize)>| {
                if let Some(None) = map.get(r as usize) {
                    // a constant or a cell the closure captured read from an extra slot
                    let v = cells.iter().find(|c| c.0 == r as usize).map_or_else(|| consts[r as usize - cb], |c| c.1);
                    let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 });
                    uses.push((at, f, slot));
                }
            };
            // a binding loaded just for one call is called through directly and the load dropped
            let folds: Vec<(usize, usize)> = if cross.is_some() { Vec::new() } else {
                (0..ins.len()).filter(|&j| ins[j].op == OpCode::CallR && self.laid_call(ins, j, ins[j], module)).filter_map(|j| {
                    let (c, w) = (ins[j], wide(ins[j]) as usize);
                    let d = (0..j).rev().find(|&d| crate::vm::block::role(&ins[d]).is_some_and(|r| r.0) && ins[d].a == c.a)?;
                    let alone = ins[d].op == OpCode::LoadGlobalR && !targets[d + 1..=j].iter().any(|&t| t)
                        && !ins[d + 1..j].iter().any(|i| crate::vm::block::role(i).is_none_or(|r| r.1.iter().any(|&f| crate::vm::block::field(i, f) == c.a)))
                        && ![c.b, c.c].iter().take(c.x as usize).any(|&r| r == c.a) && (w == 0 || ![ins[j + 1].a, ins[j + 1].b, ins[j + 1].c].contains(&c.a))
                        && ins.get(j + 1 + w).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0 && (t.a == c.a || c.a as usize >= tail + 3 && dead_past(ins, j + 2 + w, c.a)))
                        && self.wanted_at(module, ins[d].b).is_some_and(|t| { let w = &self.wanted[t].1; !w.twin && !w.by_body && w.ctor().is_none() && w.star.is_none() && w.dstar.is_none() });
                    alone.then_some((d, j))
                }).collect()
            };
            for (j, &i) in ins.iter().enumerate() {
                starts.push(self.out.len());
                if skip > 0 { skip -= 1; continue; }
                if folds.iter().any(|f| f.0 == j) { continue; }
                // a spill into a row only the body's own park reads is left out
                if i.op == OpCode::Move && (names..cb).contains(&(i.a as usize)) && row_dead(ins, j + 1, i.a) { continue; }
                let reg = |r: u16| map.get(r as usize).copied().flatten().unwrap_or(r);
                let leave = |l: &mut Self, loops: usize| {
                    // a return from a handler drops what the body left on the stacks
                    if let Some(g) = guard_at { l.emit(Ins { op: OpCode::PopIter, x: 4, a: g, b: 0, c: 0 }); }
                    for _ in 0..loops { l.emit(Ins { op: OpCode::PopIter, x: 0, a: 0, b: 0, c: 0 }); }
                    l.emit(Ins { op: OpCode::Jump, x: 1, a: exit, b: 0, c: 0 });
                };
                match i.op {
                    // a loop over a passed generator steps its section each `yield` resuming the loop body
                    OpCode::GetIterR if let Some(&(_, _, k2)) = self.nested.iter().find(|n| n.0 == k && n.1 == j) && let Some((src, fj)) = Self::loop_step(ins, j) => {
                        let f = Ins { b: reg(ins[fj].b), ..ins[fj] };
                        if fj == j + 2 { self.emit(Ins { a: reg(ins[j + 1].a), ..ins[j + 1] }); }
                        let adopt = self.out.len();
                        self.emit(Ins { op: OpCode::GetIterR, x: ADOPT, a: 0, b: reg(src), c: k2 as u16 });
                        let entry = self.out.len();
                        jumps.push(entry);
                        self.emit(f);
                        self.emit(Ins { op: OpCode::Move, x: 0, a: 0, b: 0, c: 0 });
                        let into = self.out.len();
                        self.emit(Ins { op: OpCode::Jump, x: 1, a: 0, b: 0, c: 0 });
                        let step = self.out.len();
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: 0, b: 0, c: 0 });
                        loops_in.push((k2, fj, adopt, entry, into, step, f.b));
                        skip = fj - j;
                    }
                    // yielding a stack call's value goes through its own register past the `PopTop` after it
                    OpCode::Yield if let Some(g) = stepped => {
                        let v = if i.x == 0 { self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: held, b: 0, c: 0 }); skip = 1; held } else { reg(i.a) };
                        // a yield not in exactly one loop marks held iterators and unmarks them on resume
                        let held_iters = loops.get(j).copied().unwrap_or(0);
                        let marks = held_iters != 1 || guard_at.is_some();
                        if marks { self.emit(Ins { op: OpCode::PopIter, x: 2, a: held_iters as u16, b: guard_at.unwrap_or(0), c: guard_at.is_some() as u16 }); }
                        yielded.push((if several { self.out.len() } else { usize::MAX }, j + 1 + skip));
                        if several { self.emit(Ins { op: OpCode::Move, x: 0, a: g, b: 0, c: 0 }); }
                        // a constant it yields is read from its slot by the return itself
                        if i.x != 0 { konst(self.out.len(), 1, i.a, &mut *extra, &mut *slots, &mut *uses); }
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: v, c: 0 });
                        if marks {
                            unmarks.push((yielded.len() - 1, self.out.len()));
                            self.emit(Ins { op: OpCode::PopIter, x: 1, a: 0, b: 0, c: 0 });
                            self.emit(Ins { op: OpCode::Jump, x: 1, a: 0, b: 0, c: 0 });
                        }
                    }
                    // its return ends the loop dropping the iterators of the loops around it
                    OpCode::PushRegs if stepped.is_some() && crate::vm::block::returns(ins, j) => {
                        leave(self, loops[j]);
                        skip = ins.get(j + 1).is_some_and(|n| n.op == OpCode::Jump) as usize;
                    }
                    OpCode::ReturnR if stepped.is_some() => leave(self, loops[j]),
                    // a join no instruction reads per `pure` and `gen_pure` names its slot and is dropped
                    OpCode::Phi => {}
                    OpCode::ReturnValue if stepped.is_some() => {
                        self.emit(Ins { op: OpCode::PopTop, x: 0, a: 0, b: 0, c: 0 });
                        leave(self, loops[j]);
                    }
                    // a pushed value a `ReturnValue` returns comes from its register
                    OpCode::PushRegs if crate::vm::block::returns(ins, j) => {
                        konst(self.out.len(), 1, i.a, &mut *extra, &mut *slots, &mut *uses);
                        for _ in 0..loops.get(j).copied().unwrap_or(0) { self.emit(Ins { op: OpCode::PopIter, x: 0, a: 0, b: 0, c: 0 }); }
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: reg(i.a), c: 0 });
                        // the next `Jump` goes but a `ReturnValue` other paths reach with a stacked value stays
                        skip = ins.get(j + 1).is_some_and(|n| n.op == OpCode::Jump) as usize;
                        if skip == 1 { gone.push((j + 1, ins[j + 1].a as usize)); }
                    }
                    // a value a handler leaves on the stack is returned from its own register
                    OpCode::ReturnValue => if let Some(t) = guard {
                        self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                        for _ in 0..loops.get(j).copied().unwrap_or(0) { self.emit(Ins { op: OpCode::PopIter, x: 0, a: 0, b: 0, c: 0 }); }
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: tried as u8, a: link, b: t, c: 0 });
                    },
                    // a value under a call stays put since the called section never writes its register
                    OpCode::PushRegs if ins.get(j + 1).is_some_and(|&c| self.laid_call(ins, j + 1, c, module) || self.laid_method(k, j + 1, c).is_some() || self.laid_value(k, j + 1, c).is_some() || self.laid_spread(k, j + 1, c).is_some()) && under(j + 1) => {}
                    // a value under a call taken back into a constant's register has nowhere to go
                    OpCode::StoreTopR if i.x == 0 && matches!(map.get(i.a as usize), Some(None)) => self.emit(Ins { op: OpCode::PopTop, x: 0, a: 0, b: 0, c: 0 }),
                    OpCode::ReturnR => {
                        konst(self.out.len(), 1, i.b, &mut *extra, &mut *slots, &mut *uses);
                        for _ in 0..loops.get(j).copied().unwrap_or(0) { self.emit(Ins { op: OpCode::PopIter, x: 0, a: 0, b: 0, c: 0 }); }
                        // a parameter nothing deletes or a constant is bound which a `c` of 1 marks
                        let bound = (cb..tail + 3).contains(&(i.b as usize)) || (i.b as usize) < arity && !deletes && !cellvars.contains(&(i.b as usize));
                        self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: reg(i.b), c: bound as u16 });
                    }
                    // calls in a section enter their callee's section and the following `StoreTopR` names the destination
                    OpCode::CallGlobalR | OpCode::CallR | OpCode::CallMethodR | OpCode::CallSpreadR if let g = crate::vm::block::global_of(ins, j, i)
                        && let Some(to) = self.laid_method(k, j, i).or_else(|| self.laid_value(k, j, i)).or_else(|| self.laid_spread(k, j, i)).or_else(|| self.laid_call_to(g, i, module)) => {
                        // one whose load was left out calls through the binding
                        let i = match g { Some(g) if folds.iter().any(|f| f.1 == j) => Ins { op: OpCode::CallGlobalR, a: g, ..i }, _ => i };
                        let a = if matches!(i.op, OpCode::CallR | OpCode::CallSpreadR) { reg(i.a) } else { i.a };
                        // another module's binding is read into a register the call checks
                        let (i, a) = match cross {
                            Some(t) if i.op == OpCode::CallGlobalR => { self.emit(Ins { op: OpCode::LoadGlobalAtR, x: 0, a: t, b: i.a, c: module as u16 }); (Ins { op: OpCode::CallR, ..i }, t) }
                            // so is a binding holding one body's functions in turn checked by their body
                            _ if i.op == OpCode::CallGlobalR && (self.wanted[to].1.twin || self.valued(ins, j, i, module)) => {
                                let t = next as u16;
                                next += 1;
                                self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: t, b: i.a, c: 0 });
                                (Ins { op: OpCode::CallR, ..i }, t)
                            }
                            _ => (i, a),
                        };
                        // a call that may recurse keeps a level of later reads minus parameters passed unchanged
                        let back = self.reaches(to, k);
                        let here = selfs.iter().find(|s| s.0 == j && back && to == k).map_or(0, |s| s.1) & steady;
                        // wide call operands past the second move into parameters with constants via their own registers
                        let w = wide(i) as usize;
                        let mut rest = Ins { op: OpCode::ArgsR, x: 0, a: 0, b: 0, c: 0 };
                        let more = if w == 0 { 0 } else { i.x as usize - 1 - (i.op != OpCode::CallMethodR) as usize };
                        for f in 0..more.min(3) {
                            let r = crate::vm::block::field(&ins[j + 1], f as u8);
                            *crate::vm::block::field_mut(&mut rest, f as u8) = match map.get(r as usize) {
                                Some(None) => {
                                    let t = next as u16;
                                    next += 1;
                                    konst(self.out.len(), 1, r, &mut *extra, &mut *slots, &mut *uses);
                                    self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: 0, c: 0 });
                                    t
                                }
                                _ => reg(r),
                            };
                        }
                        // yielding a call's result uses its return to set the loop variable and resume
                        let fused = stepped.filter(|_| w == 0 && !back && !under(j) && fused_at.is_none() && ins.get(j + 1).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0)
                            && ins.get(j + 2).is_some_and(|y| y.op == OpCode::Yield && y.a == ins[j + 1].a)).and(body_of_loop);
                        if let Some(g) = stepped.filter(|_| fused.is_some()) {
                            yielded.push((if several { self.out.len() } else { usize::MAX }, j + 3));
                            if several { self.emit(Ins { op: OpCode::Move, x: 0, a: g, b: 0, c: 0 }); }
                        }
                        let (mut call, mut rest) = (Ins { a, b: reg(i.b), c: reg(i.c), ..i }, rest);
                        // extra arguments of an `*args` function fill its tuple while the stub calls as written
                        let order = if matches!(i.op, OpCode::CallR | OpCode::CallGlobalR) { packs(&self.wanted[to].1, i.x as usize) } else { None };
                        let packed = order.is_some();
                        if let Some(order) = order {
                            let n = i.x as usize;
                            // a constant passed first or second gets its own register as later ones did
                            for (f, r) in [(1u8, i.b), (2, i.c)].into_iter().take(n) {
                                if !matches!(map.get(r as usize), Some(None)) { continue; }
                                let t = next as u16;
                                next += 1;
                                konst(self.out.len(), 1, r, &mut *extra, &mut *slots, &mut *uses);
                                self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: 0, c: 0 });
                                if f == 1 { call.b = t } else { call.c = t }
                            }
                            let all = [call.b, call.c, rest.a, rest.b, rest.c];
                            // the binding's function goes into a register the call checks and its stub calls
                            if call.op == OpCode::CallGlobalR {
                                let f = next as u16;
                                next += 1;
                                self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: f, b: call.a, c: 0 });
                                call = Ins { op: OpCode::CallR, a: f, ..call };
                            }
                            let regs: Vec<u16> = core::iter::once(call.a).chain(all[..n].iter().copied()).collect();
                            let w = &self.wanted[to].1;
                            let (star, dstar) = (w.star.map(|p| { next += 1; ((next - 1) as u16, p) }), w.dstar.map(|p| { next += 1; ((next - 1) as u16, p) }));
                            let (list, gaps) = self.pack(&regs, &order, star, dstar, Some(&mut next));
                            self.gaps = gaps;
                            self.kwcall = Some((regs, n as u16));
                            let r = |q: usize| list.get(q).copied().unwrap_or(0);
                            (call, rest) = (Ins { x: list.len() as u8, b: r(0), c: r(1), ..call }, Ins { op: OpCode::ArgsR, x: 0, a: r(2), b: r(3), c: r(4) });
                        }
                        // a recursive call saves arguments its parameter moves would clobber as a parallel move would
                        if to == k && self.wanted[k].1.ctor().is_none() {
                            let written = |r: u16, q: usize| (r as usize).checked_sub(link as usize + 3).is_some_and(|p| p + 2 < arity && p + 2 != q)
                                || q == 1 && r == link + 1;
                            let mut args = [call.b, call.c, rest.a, rest.b, rest.c];
                            let n = (call.x as usize + (call.op == OpCode::CallMethodR) as usize).min(5);
                            for (q, arg) in args.iter_mut().enumerate().take(n) {
                                if !written(*arg, q) { continue; }
                                let t = next as u16;
                                next += 1;
                                self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: *arg, c: 0 });
                                *arg = t;
                            }
                            (call.b, call.c, rest.a, rest.b, rest.c) = (args[0], args[1], args[2], args[3], args[4]);
                        }
                        // a class call moves a constant second argument into a register of its own first
                        let moved = !packed && self.wanted[to].1.ctor().is_some() && i.x >= 2 && matches!(map.get(i.c as usize), Some(None));
                        if moved {
                            let t = next as u16;
                            next += 1;
                            konst(self.out.len(), 1, i.c, &mut *extra, &mut *slots, &mut *uses);
                            self.emit(Ins { op: OpCode::Move, x: 0, a: t, b: 0, c: 0 });
                            call.c = t;
                        }
                        let (call, rest) = if self.wanted[to].1.on_class & 3 == 3 { called_instance(call, rest) } else { (call, rest) };
                        self.ret_to = fused.map(|f| f.1);
                        // a receiver just loaded from a binding is read by the head as outside sections
                        let global = (i.op == OpCode::CallMethodR && cross.is_none() && w == 0 && j > 0 && ins[j - 1].op == OpCode::LoadGlobalR).then(|| self.out.last().copied()).flatten()
                            .filter(|l| l.op == OpCode::LoadGlobalR && l.a == call.b && l.b < u16::MAX && (call.x == 0 || call.c != l.a)).map(|l| l.b);
                        if global.is_some() {
                            self.out.pop();
                            self.orig.pop();
                            self.back.pop();
                            // the call starts where the load did so its instructions point at the call
                            if let Some(s) = starts.last_mut() { *s = self.out.len(); }
                        }
                        let level = back.then(|| (self.turn(), kept(&live[j], here)));
                        // a kept result a handler reads comes back past the level through its own register
                        let dest = fused.map_or(reg(ins[j + 1 + w].a), |f| f.0);
                        let into = level.as_ref().filter(|l| l.1.contains(&dest)).map(|_| { next += 1; (next - 1) as u16 });
                        self.head(call, rest, into.unwrap_or(dest), to, false, &[]);
                        // the constants it passes first and second are read by the call itself
                        let at = self.out.len() - HEADER as usize;
                        if let Some(g) = global { self.out[at + 3].c = g + 1; }
                        // a method call's receiver lies in `b` and its first argument in `c`
                        let given = if packed { 0 } else { i.x as usize + matches!(i.op, OpCode::CallMethodR | OpCode::CallSpreadR) as usize };
                        if given > 0 { konst(at, 1, i.b, &mut *extra, &mut *slots, &mut *uses); }
                        if given > 1 && !moved { konst(at, 2, i.c, &mut *extra, &mut *slots, &mut *uses); }
                        if let Some((t, keep)) = level { self.unturn(t, link, &keep); }
                        if let Some(t) = into { self.emit(Ins { op: OpCode::Move, x: 0, a: dest, b: t, c: 0 }); }
                        skip = 1 + w + under(j) as usize;
                        // a turned down call hands its result to the loop variable and body here
                        if let Some((target, _)) = fused {
                            fused_at = Some((body.orig(j), (self.out.len() - HEADER as usize) as u32));
                            self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: target, c: 0 });
                            skip = 2;
                        }
                    }
                    // a row call of a binding's section reads its row and leaves its value there
                    OpCode::RowsR if let Some((to, kw)) = self.laid_row(k, j, i) => {
                        let op = i.b;
                        let n = (op & 0xFF) as usize + 2 * (op >> 8) as usize;
                        let regs: Vec<u16> = (0..=n as u16).map(|q| reg(i.a + q)).collect();
                        // parameters take operands or defaults packing `*args` and `**kwargs` while stubs call as written
                        let (m, r): (usize, [u16; 5]) = match &kw {
                            None => (n, core::array::from_fn(|p| regs.get(1 + p).copied().unwrap_or(0))),
                            Some(Ok((m, slots))) => {
                                self.gaps = (0..*m).filter(|&p| slots[p].is_none()).fold(0, |g, p| g | 1 << p);
                                (*m, slots.map(|s| s.map_or(0, |q| regs[q as usize])))
                            }
                            Some(Err(packed)) => {
                                let w = &self.wanted[to].1;
                                let (star, dstar) = (w.star.map(|p| { next += 1; ((next - 1) as u16, p) }), w.dstar.map(|p| { next += 1; ((next - 1) as u16, p) }));
                                let (list, gaps) = self.pack(&regs, packed, star, dstar, Some(&mut next));
                                self.gaps = gaps;
                                (list.len(), core::array::from_fn(|p| list.get(p).copied().unwrap_or(0)))
                            }
                        };
                        if n > 5 || kw.is_some() { (self.more_args, self.kwcall) = (regs.get(6..).filter(|_| kw.is_none()).unwrap_or(&[]).to_vec(), Some((regs.clone(), op))); }
                        self.kwrow = regs.windows(2).all(|w| w[1] == w[0] + 1).then_some((regs[0], true));
                        let (call, rest) = (Ins { op: OpCode::CallR, x: m as u8, a: regs[0], b: r[0], c: r[1] }, Ins { op: OpCode::ArgsR, x: 0, a: r[2], b: r[3], c: r[4] });
                        let level = self.reaches(to, k).then(|| self.turn());
                        self.head(call, rest, regs[0], to, false, &[]);
                        if let Some(at) = level { self.unturn(at, link, &kept(&live[j], 0)); }
                        skip = 1;
                    }
                    // a store into a body's cell moves the stack top into the cell's register
                    OpCode::StoreName => if let Some(t) = guard {
                        self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: t, b: 0, c: 0 });
                        self.emit(Ins { op: OpCode::StoreCellR, x: 0, a: reg(i.a), b: t, c: 0 });
                    },
                    // a module binding's delete names binding and module keeping the name for its error
                    OpCode::Del if let Some(&Kind::Global(g)) = body.kinds.get(i.a as usize) => self.emit(Ins { op: OpCode::Del, x: 1, a: i.a, b: g as u16, c: module as u16 }),
                    // a pair writes both its targets and reads its source which may be a constant
                    OpCode::UnpackR if i.x != 0 => {
                        konst(self.out.len(), 2, i.c, &mut *extra, &mut *slots, &mut *uses);
                        self.emit(Ins { a: reg(i.a), b: reg(i.b), c: reg(i.c), ..i });
                    }
                    // a try's handler maps as a jump does and its slot as a register does
                    OpCode::SetupExcept | OpCode::SetupFinally => {
                        jumps.push(self.out.len());
                        self.emit(Ins { x: 1, a: body.lowered(i.a as usize) as u16, b: if i.b == 0 { 0 } else { reg(i.b - 1) + 1 }, ..i });
                    }
                    // a row the section lays apart moves into fresh registers in a row first
                    OpCode::BuildR | OpCode::RuntimeR if i.c >= 2 && !(0..i.c).all(|q| map.get((i.b + q) as usize).copied().flatten().is_some_and(|m| Some(m) == map.get(i.b as usize).copied().flatten().map(|s| s + q))) => {
                        let t = next as u16;
                        next += i.c as usize;
                        for q in 0..i.c {
                            konst(self.out.len(), 1, i.b + q, &mut *extra, &mut *slots, &mut *uses);
                            self.emit(Ins { op: OpCode::Move, x: 0, a: t + q, b: reg(i.b + q), c: 0 });
                        }
                        self.emit(Ins { a: reg(i.a), b: t, ..i });
                    }
                    _ => {
                        let (dest, reads, jump) = crate::vm::block::role(&i).unwrap_or((false, &[], false));
                        let mut out = i;
                        if let Some(t) = cross {
                            match i.op {
                                OpCode::LoadGlobalR => out = Ins { op: OpCode::LoadGlobalAtR, c: module as u16, ..i },
                                OpCode::StoreGlobalR => out = Ins { op: OpCode::StoreGlobalAtR, c: module as u16, ..i },
                                OpCode::CallGlobalR => {
                                    self.emit(Ins { op: OpCode::LoadGlobalAtR, x: 0, a: t, b: i.a, c: module as u16 });
                                    out = Ins { op: OpCode::CallR, a: t, ..i };
                                }
                                _ => {}
                            }
                        }
                        for &f in reads {
                            let r = crate::vm::block::field(&i, f);
                            if let Some(Some(m)) = map.get(r as usize) { *crate::vm::block::field_mut(&mut out, f) = *m; }
                            konst(self.out.len(), f, r, &mut *extra, &mut *slots, &mut *uses);
                        }
                        if dest { out.a = reg(i.a); }
                        if jump { jumps.push(self.out.len()); }
                        self.emit(out);
                    }
                }
            }
            // running off the end returns `None` as does a jump past the last instruction
            starts.push(self.out.len());
            if stepped.is_some() { self.emit(Ins { op: OpCode::Jump, x: 1, a: exit, b: 0, c: 0 }); }
            else if runs_off(ins, jumps.iter().any(|&p| self.out[p].a as usize >= ins.len())) {
                self.emit(Ins { op: OpCode::ReturnSectionR, x: 0, a: link, b: (self.cbase + self.consts) as u16, c: 1 });
            }
            // each step resumes past its last `yield` or wherever the jump after it goes
            for (y, &(at, after)) in yielded.iter().enumerate() {
                let back = match ins.get(after) { Some(n) if n.op == OpCode::Jump && n.x == 0 => n.a as usize, _ => after };
                // resumes land on a generator section loop's step and adopted objects on its entry
                let to = loops_in.iter().find(|n| n.1 == back).map_or(starts[back], |n| n.5);
                // a yield that marked its iterators resumes through its stub which unmarks them first
                let to = match unmarks.iter().find(|u| u.0 == y) { Some(&(_, stub)) => { self.out[stub + 1].a = to as u16; stub } None => to };
                let taken = loops_in.iter().find(|n| n.1 == back).map_or(starts[back], |n| n.3);
                steps.push((body.resume(back) as u32, body.orig(after), taken as u32, to as u32));
                let v = Val::int((to as i64) << 16 | stepped.unwrap_or(0) as i64);
                if at == usize::MAX { resume = Some(v); continue; }
                let slot = *slots.entry(v.0).or_insert_with(|| { extra.push(v); extra.len() - 1 });
                uses.push((at, 1, slot));
            }
            // a return from a body with `try` leaves through its handlers unlike a generator's yields
            if tried && stepped.is_none() {
                for r in &mut self.out[start..] { if r.op == OpCode::ReturnSectionR && r.a == link { r.x = 1; } }
            }
            // an init returns nil so the call gives back its instance
            if ctor.is_some() { for r in &mut self.out[start..] { if r.op == OpCode::ReturnSectionR && r.a == link { (r.b, r.c) = (link + 1, 1); } } }
            // each instruction points at its source body instruction before jumps to dropped returns are retargeted
            let mut j = 0;
            for e in start..self.out.len() {
                while j + 1 < ins.len() && starts[j + 1] <= e { j += 1; }
                orig.push(body.orig(j));
            }
            for (k, to) in gone { starts[k] = starts[to]; }
            // a jump back to a loop over a generator's section goes to its step
            for at in jumps {
                let t = self.out[at].a as usize;
                self.out[at].a = loops_in.iter().find(|n| n.1 == t && n.3 != at).map_or(starts[t], |n| n.5) as u16;
            }
            for &(k2, fj, adopt, entry, into, step, target) in &loops_in {
                let (body_at, leave) = (starts[fj + 1] as u16, starts[ins[fj].a as usize] as u16);
                (self.out[adopt].a, self.out[into].a) = (body_at, body_at);
                self.gen_loops.push((k2, leave, Some(target), body_at, false));
                late.push((k2, entry, entry + 1, step));
            }
            // its calls' stubs lie inside it too naming what they name in its function's chunk
            for h in inner..self.heads.len() {
                let (at, hk, rest) = self.heads[h];
                let from = self.out.len();
                self.stub(at, hk, rest, &mut next);
                for _ in from..self.out.len() { orig.push(orig[at - start]); }
            }
        }
        let ctor = ctor.map(|(epoch, attrs, init)| (Cell::new(epoch), attrs, init));
        // a `by_body` section holds no function since each call brings its cells and defaults
        let value = Cell::new(if by_body { Val::undef() } else { value });
        let by_body = by_body.then(|| (arity as u8, cell_regs.into_boxed_slice()));
        gens.push((stepped.unwrap_or(0), resume));
        let step_reg = stepped.unwrap_or(0);
        // a section a call checks by its body holds no binding the block checks
        let global = if by_body.is_some() || self.wanted[k].1.twin { u16::MAX } else { global };
        sections.push(Section { start: start as u32, end: self.out.len() as u32, link, value, module, global, bound: self.wanted[k].1.bound_module(), fi, orig: orig.into(), ctor, by_body, slots: slot_map.into(), guard: Cell::new((Val::undef(), u32::MAX, Val::undef())), table: (0, u32::MAX), arity: arity as u8, packs: if self.wanted[k].1.dstar == Some(0) { 2 } else { (self.wanted[k].1.star == Some(0)) as u8 }, on_class: self.wanted[k].1.on_class, defaults: if self.wanted[k].1.by_body { Box::default() } else { self.wanted[k].1.defaults.clone().into() }, tail: fused_at, steps: steps.into(), step_reg, held: held_at });
        st.next = next;
    }

    /* lays the stub of `genfors[g]`'s head which calls as written and loops over the result */
    fn gen_stub(&mut self, g: usize, val: u16) {
        let GenFor { head, step, target, global, call, rest, .. } = self.genfors[g];
        let target = if global.is_some() { val } else { target };
        (self.out[head + 3].a, self.out[head + 3].b) = (self.out.len() as u16, self.out.len() as u16);
        self.cur = u32::from(self.orig[head]);
        self.emit(call);
        if wide(call) { self.emit(rest); }
        self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: target, b: 0, c: 0 });
        self.emit(Ins { op: OpCode::GetIterR, x: 0, a: 0, b: target, c: 0 });
        self.emit(Ins { op: OpCode::Jump, x: 1, a: step as u16 - 3, b: 0, c: 0 });
    }

    /* lays the stub where a turned down call at `at` runs plainly then carries on */
    fn stub(&mut self, at: usize, k: usize, rest: Ins, next: &mut usize) {
        let (call, store, sect) = (self.out[at], self.out[at + 1], self.out[at + 2]);
        self.out[at + 3].a = self.out.len() as u16;
        self.cur = u32::from(self.orig[at]);
        // a receiver the head read from a binding is loaded into its register first
        if sect.x & 8 != 0 && self.out[at + 3].c != 0 { self.emit(Ins { op: OpCode::LoadGlobalR, x: 0, a: call.b, b: self.out[at + 3].c - 1, c: 0 }); }
        // a register call that met other functions looks its callee up among their sections
        if sect.x & 8 == 0 && !self.kwstubs.iter().any(|s| s.0 == at) && self.alts.iter().any(|a| a.0 == k) {
            self.lookups.push((self.out.len(), k));
            self.emit(Ins { op: OpCode::CallValueR, x: 0, a: at as u16, b: 0, c: 0 });
        }
        // a turned down keyword call calls with its keywords as written
        if let Some(k) = self.kwstubs.iter().position(|s| s.0 == at) {
            let (_, regs, op, home) = self.kwstubs[k].clone();
            // the call's own row takes its operands when no move into it overwrites another
            let row = match home {
                Some((r, true)) => r,
                Some((r, false)) if moves_fit(&regs, r) => r,
                // stubs run one at a time so they share one row the longest needs
                _ if self.stub_row.1 >= regs.len() => self.stub_row.0,
                _ => { self.stub_row = (*next as u16, regs.len()); *next += regs.len(); self.stub_row.0 }
            };
            if !matches!(home, Some((_, true))) { for (q, &r) in regs.iter().enumerate().rev().filter(|&(q, &r)| r != row + q as u16) { self.emit(Ins { op: OpCode::Move, x: 0, a: row + q as u16, b: r, c: 0 }); } }
            self.out[at + 3].b = self.out.len() as u16;
            self.emit(Ins { op: OpCode::RowsR, x: OpCode::Call as u8, a: row, b: op, c: regs.len() as u16 });
            self.emit(Ins { op: OpCode::RowsBackR, x: 0, a: row, b: STUB_STORE, c: 0 });
            if store.a != row { self.emit(Ins { op: OpCode::Move, x: 0, a: store.a, b: row, c: 0 }); }
            self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER, b: 0, c: 0 });
            return;
        }
        // a called instance's stub calls it through its register again with the arguments in place
        if self.wanted[k].1.on_class & 3 == 3 {
            self.out[at + 3].b = self.out.len() as u16;
            let call = Ins { op: OpCode::CallR, x: call.x, a: call.b, b: call.c, c: rest.a };
            self.emit(call);
            if wide(call) { self.emit(Ins { op: OpCode::ArgsR, x: 0, a: rest.b, b: rest.c, c: 0 }); }
            self.emit(Ins { op: OpCode::StoreTopR, x: 0, a: store.a, b: STUB_STORE, c: 0 });
            self.emit(Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER, b: 0, c: 0 });
            return;
        }
        let spread = sect.x & 72 == 72;
        let op = if spread { OpCode::CallSpreadR } else if sect.x & 8 != 0 { OpCode::CallMethodR } else if sect.x & 4 != 0 { OpCode::CallR } else { OpCode::CallGlobalR };
        // a method check reads the stub call's site seeded from the one that found it
        self.out[at + 3].b = self.out.len() as u16;
        for s in self.seeds.iter_mut().filter(|s| s.0 == at) { s.0 = self.out.len() | SEEDED; }
        let store = Ins { op: OpCode::StoreTopR, x: 0, a: store.a, b: STUB_STORE, c: 0 };
        let jump = Ins { op: OpCode::Jump, x: 1, a: at as u16 + HEADER, b: 0, c: 0 };
        let call = Ins { op, ..call };
        if wide(call) { self.emit_all(&[call, rest, store, jump]); } else { self.emit_all(&[call, store, jump]); }
    }

    /* emits the `TurnR` of a recursion level before a section call */
    fn turn(&mut self) -> usize {
        let at = self.out.len();
        self.emit(Ins { op: OpCode::TurnR, x: 0, a: 0, b: 0, c: 0 });
        at
    }

    /* emits `UnturnR` words where the `TurnR` at `at` resumes naming `link` and kept registers */
    fn unturn(&mut self, at: usize, link: u16, keep: &[u16]) {
        let first = self.out.len();
        let r = |g: &[u16], n: usize| g.get(n).copied().unwrap_or(0);
        let (head, rest) = keep.split_at(keep.len().min(2));
        let lone = if rest.is_empty() { LONE } else { 0 };
        self.emit(Ins { op: OpCode::UnturnR, x: LINK | lone | head.len() as u8, a: link, b: r(head, 0), c: r(head, 1) });
        for g in rest.chunks(3) { self.emit(Ins { op: OpCode::UnturnR, x: g.len() as u8, a: r(g, 0), b: r(g, 1), c: r(g, 2) }); }
        let len = self.out.len() - first;
        let t = &mut self.out[at];
        (t.a, t.b, t.x) = (first as u16, len as u16, t.x | if lone != 0 { LONE_TURN } else { 0 });
    }
}

/* a head's table by its section with the table start and size less one */
type Lookup = (u32, u32, u32);

// a keyword call's parameter count up to the last and which operand passes each
type Passed = (usize, [Option<u16>; 5]);

// each parameter's operand or none and the tuple's operands and the dict's names and values
type Packed = (Vec<Option<usize>>, core::ops::Range<usize>, Vec<usize>);

/* per head section a stub table's start in `alt_tab` and its power of two mask */
fn alts_of(alts: &[(usize, usize)], sections: &[Section]) -> (Vec<Lookup>, Box<[(Val, u32)]>) {
    let mut heads: Vec<usize> = alts.iter().map(|a| a.0).collect();
    heads.sort_unstable();
    heads.dedup();
    let (mut index, mut tab) = (Vec::new(), Vec::new());
    for k in heads {
        // a `by_body` section is found by its body
        let funcs: Vec<(Val, u32)> = alts.iter().filter(|a| a.0 == k).map(|&(_, a)| (if sections[a].by_body.is_some() { body_key(sections[a].fi) } else { sections[a].value.get() }, a as u32)).collect();
        let size = (2 * funcs.len()).next_power_of_two();
        let start = tab.len();
        tab.resize(start + size, (Val::undef(), u32::MAX));
        for (v, a) in funcs {
            // defaulted sections are keyed and marked apart so only the stub's lookup fills them in
            let defaults = !sections[a as usize].defaults.is_empty() && sections[a as usize].by_body.is_none();
            let v = if defaults { with_defaults(v) } else { v };
            let mut at = alt_slot(v, size - 1);
            while tab[start + at].1 != u32::MAX { at = (at + 1) & (size - 1); }
            tab[start + at] = (v, a | if defaults { DEFAULTS } else { 0 });
        }
        index.push((k as u32, start as u32, (size - 1) as u32));
    }
    (index, tab.into())
}

/* a `StoreTopR` `b` marking a stub's call which running the store never reads */
pub(crate) const STUB_STORE: u16 = u16::MAX;

/* marks a table's section whose function has defaults that a shorter call moves in */
pub(crate) const DEFAULTS: u32 = 1 << 30;

/* the table key for function `v` with defaults which no real value can equal */
#[inline]
pub(crate) fn with_defaults(v: Val) -> Val { Val(v.0 ^ 1 << 62) }

/* the table key for any function of body `fi` which no function value equals */
#[inline]
pub(crate) fn body_key(fi: usize) -> Val { Val::int(fi as i64) }

/* the first slot in a table of `mask + 1` where function `v` may lie */
#[inline(always)]
pub(crate) fn alt_slot(v: Val, mask: usize) -> usize { (v.0 >> 4) as usize & mask }

/* the body function index a comprehension's `MakeFunction` made into `r` just before call `j` */
fn made_in(ins: &[Ins], j: usize, r: u16) -> Option<u16> {
    let k = ins[..j].iter().rposition(|i| crate::vm::block::role(i).is_some_and(|(dest, _, _)| dest && i.a == r))?;
    made_by(ins[k]).then(|| ins[k].c)
}

/* whether `i` is the `OpR` of a `MakeFunction` */
fn made_by(i: Ins) -> bool { i.op == OpCode::OpR && i.x == OpCode::MakeFunction as u8 }

/* how a call of `n` positionals packs them for an `*args` or `**kwargs` function `c` */
pub(super) fn packs(c: &Callee, n: usize) -> Option<Packed> {
    if c.on_class != 0 || c.ctor().is_some() || n > 5 || c.star.is_none() && c.dstar.is_none() { return None; }
    Lower::packed(&(0..=n as u16).collect::<Vec<_>>(), n as u16, c, (&[], 0))
}

/* whether a call filling `n` parameters of `c` passes each it must and no more */
pub(super) fn fills(c: &Callee, n: usize, keyed: bool) -> bool {
    // only keyword or packing calls fill packed parameters or those past a lone star
    if keyed { (c.required..=c.arity).contains(&n) } else { c.star.is_none() && c.dstar.is_none() && (c.required..=c.positional).contains(&n) }
}

/* whether `n` positionals with a class's instance counted enter the section laid for `c` */
pub(super) fn fits(c: &Callee, n: usize) -> bool { fills(c, n, false) || packs(c, n).is_some() }

/* whether a body runs off its end by falling through or by a jump there */
fn runs_off(ins: &[Ins], jumps_off: bool) -> bool {
    jumps_off || !ins.last().is_some_and(|i| matches!(i.op, OpCode::ReturnR | OpCode::ReturnValue))
}

pub(super) fn called_instance(call: Ins, rest: Ins) -> (Ins, Ins) {
    (Ins { op: OpCode::CallMethodR, x: call.x, a: 0, b: call.a, c: call.b }, Ins { op: OpCode::ArgsR, x: 0, a: call.c, b: rest.a, c: rest.b })
}

/* a `GetIterR` `x` moving the generator in `b` into section `c` resumed at `a` */
pub(crate) const ADOPT: u8 = 7;

/* marks a compiler instruction with the function a method call found held by the instance */
pub(crate) const FIELD: u32 = 1 << 30;

/* marks a compiler instruction with the plain function a method call found on the class */
pub(crate) const STATIC: u32 = 1 << 26;

/* marks a compiler instruction with the function of a bound method a register call called */
pub(crate) const BOUND: u32 = 1 << 29;

/* marks a call with its generator function whose section a loop over the result steps */
pub(crate) const GEN: u32 = 1 << 27;

/* a section call's call and store then section and where a turned down call goes */
pub(crate) const HEADER: u16 = 4;

// marks a seed laid at its stub's call
pub(super) const SEEDED: usize = 1 << 31;

// the `x` of the `SectionR` holding in `a` where a turned down call goes
pub(crate) const STUB: u8 = 0x80;

/* whether `op` calls a section from its head */
#[inline]
pub(crate) fn section_call(op: OpCode) -> bool { matches!(op, OpCode::CallSectionR | OpCode::CallGlobalSectionR | OpCode::CallMethodSectionR | OpCode::CallValueSectionR | OpCode::CallCtorSectionR | OpCode::CallMethodGlobalSectionR) }

// flags of an `UnturnR` word whose first names the link and two registers
pub(crate) const LINK: u8 = 0x40;

pub(crate) const LONE: u8 = 0x20;

pub(crate) const LONE_TURN: u8 = 0x40;
