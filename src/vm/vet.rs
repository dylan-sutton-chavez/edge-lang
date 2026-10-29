use alloc::{string::{String, ToString}, vec::Vec};
use core::hash::Hasher;

use crate::code::OpCode;
use crate::syntax::chunk::{Instruction, SSAChunk};
use crate::util::hash::FxHasher;
use crate::value::*;
use super::VM;
use super::coro::FINISHED;
use super::exc::RAW_HANDLER;
use super::frame::RESUME_RAW;
use super::lower::{self, Code};

/* the refusal of a blob whose parked state its program could not have left */
fn bad() -> String { "snapshot holds a malformed value".to_string() }

/* the refusal of a blob parked inside laid code this engine lays otherwise */
pub(crate) const LAID_OTHERWISE: &str = "snapshot parks inside laid code this engine lays otherwise";

/* hashes what a restored run reads of laid code as it resumes inside it */
pub(crate) fn code_print(code: &Code) -> u64 {
    let mut h = FxHasher::default();
    h.write_u64(code.frame as u64);
    for i in &code.ins { h.write_u64(u64::from(i.op as u8) | u64::from(i.x) << 8 | u64::from(i.a) << 16 | u64::from(i.b) << 32 | u64::from(i.c) << 48); }
    if let Some(b) = code.block.as_deref() {
        for s in b.sections.iter() { h.write_u64(u64::from(s.start) | u64::from(s.end) << 32); h.write_u64(u64::from(s.link) | (s.fi as u64) << 16); h.write_u64(s.value.get().0); }
        // a constant a restore makes again lies in another slot and reads alike
        for v in b.extra.iter() { h.write_u64(if v.is_heap() { u64::MAX } else { v.0 }); }
    }
    h.finish()
}

/* whether a run parked after `op` waits for the frame it called to return */
fn calls(op: OpCode) -> bool {
    use OpCode::*;
    matches!(op, Call | CallMethod | CallMethodArgs | CallSpread)
}

/* whether a park at compiler `ip` of `src` holding rows to `top` matches its depth */
fn rows_match(src: &[Instruction], chunk: &SSAChunk, depths: &[u16], ip: usize, top: u16, inner: bool, waits: bool) -> bool {
    let Some(&d) = depths.get(ip).filter(|&&d| d != u16::MAX) else { return false };
    let (d, rows) = (usize::from(d), super::rows::first_row(chunk.names.len()));
    let held = if top == 0 { 0 } else { match usize::from(top).checked_sub(rows) { Some(h) => h, None => return false } };
    let prev = ip.checked_sub(1).and_then(|k| src.get(k));
    // a frame under a callee lacks the result the callee pushes as it returns
    if !inner { return held + 1 == d && prev.is_some_and(|i| calls(i.opcode)); }
    // a loop's waiting generator resumes taking the top row which a statement's wait may drop
    if waits && src.get(ip).is_some_and(|i| i.opcode == OpCode::ForIter) { return held == d + 1; }
    held == d || held == d + 1 && prev.is_some_and(|i| i.opcode == OpCode::CallNative && native_void(chunk, i.operand))
}

/* whether lowered `code` resumes a park at compiler `ip` of `src` holding rows to `top` */
fn lowered_takes(code: &Code, src: &[Instruction], ip: usize, top: u16, inner: bool) -> bool {
    // an inner park holds no row or its wait's value one past its entry rows
    if inner { return top == 0 || usize::from(top) == code.slots as usize + code.entry_rows(ip) + 1; }
    // a frame under a callee holds no row and stores the result past its call
    let prev = ip.checked_sub(1).and_then(|k| src.get(k));
    top == 0 && prev.is_some_and(|i| calls(i.opcode)) && code.ins.get(code.lowered(ip)).is_some_and(|t| t.op == OpCode::StoreTopR && t.x == 0)
}

/* whether the native call site `a` of `chunk` keeps no value */
fn native_void(chunk: &SSAChunk, a: u16) -> bool {
    use crate::syntax::chunk::{SITE_TABLE, SITE_VOID, native_site};
    if a & SITE_TABLE == 0 { a & SITE_VOID != 0 } else { native_site(a, &chunk.native_sites).is_some_and(|s| s.void) }
}

/* whether `chunk`'s first code under `shape` resumes a park at `ip` holding rows to `top` */
pub(crate) fn rows_hold(chunk: &SSAChunk, shape: u8, ip: usize, top: u16, inner: bool) -> bool {
    let src = lower::fuse_method_calls(chunk, shape);
    super::depth::depths(&src, chunk).is_ok_and(|d| rows_match(&src, chunk, &d, ip, top, inner, false))
}

/* what link registers in `code` may hold from call returns and registers and generator steps */
fn links(code: &Code) -> Vec<i64> {
    let mut out = Vec::new();
    for (h, i) in code.ins.iter().enumerate() {
        if lower::section_call(i.op) && let Some(s) = code.ins.get(h + 1) { out.push(i64::from(s.b) << 16 | i64::from(s.a)); }
        if i.op == OpCode::GetIterR && i.x == lower::ADOPT && let Some(n) = code.ins.get(h + 1) { out.push(i64::from(i.a) << 16 | i64::from(n.b)); }
    }
    if let Some(b) = code.block.as_deref() {
        // a step's links lie among the constants its sections read
        out.extend(b.extra.iter().filter(|v| v.is_int()).map(|v| v.as_int()));
        for s in b.sections.iter() {
            for t in s.steps.iter() { out.extend([i64::from(t.2) << 16 | i64::from(s.step_reg), i64::from(t.3) << 16 | i64::from(s.step_reg)]); }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/* the registers laid code jumps back through leaving a section or stepping a generator's */
fn link_regs(code: &Code) -> Vec<u16> {
    let mut out: Vec<u16> = code.ins.iter().filter(|i| matches!(i.op, OpCode::ReturnSectionR | OpCode::ReturnBoundR | OpCode::ReturnTriedR)).map(|i| i.a).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/* each key's laid code and the levels met across the parks a restore walks */
#[derive(Default)]
struct Walked {
    codes: Vec<(i64, alloc::rc::Rc<Code>)>,
    levels: usize,
}

/* what one parked run holds that its resume trusts */
struct Park<'p> {
    ip: usize,
    top: u16,
    slots: &'p [Val],
    levels: &'p [Val],
    excs: &'p [ExceptionFrame],
    iters: &'p [IterFrame],
}

impl<'a> VM<'a> {

    /* refuses a restored state no run of its program could leave at a pause */
    pub(crate) fn vet(&mut self, booted: &[Vec<String>], prints: &[(i64, u64)]) -> Result<(), String> {
        // a pause parks frame state in coroutines except reasons and exceptions finally blocks hold
        if !self.levels.is_empty() || !self.iter_stack.is_empty() || !self.exception_stack.is_empty()
            || !self.pending_sync_frames.is_empty() || self.resume_ip != 0 || self.pending_exec_exc_base.is_some() || self.yielded || !self.temp_roots.is_empty() { return Err(bad()); }
        // the bindings each module had at boot keep their places
        if self.scopes.len() < booted.len() || self.scopes.iter().zip(booted).any(|(g, names)| g.entries().count() < names.len() || g.entries().zip(names).any(|((n, _), b)| n != b)) { return Err(bad()); }
        self.vet_heap()?;
        self.vet_logs()?;
        // what the levels of every parked run hold bounds how many a recursion counts
        let mut kept = 0usize;
        let coros: Vec<Val> = self.heap.snapshot_objs().enumerate().filter(|(_, o)| matches!(o, Some(HeapObj::Coroutine(_)))).map(|(k, _)| Val::heap(k as u32)).collect();
        for &co in &coros {
            let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { continue };
            kept += c.levels.len() + c.syncs.iter().map(|f| f.levels.len()).sum::<usize>();
        }
        if self.turned > kept { return Err(bad()); }
        let mut walked = Walked::default();
        for co in coros { self.vet_coro(co, prints, kept, &mut walked)?; }
        // the recursion levels counted are the levels every parked run keeps
        if walked.levels != self.turned { return Err(bad()); }
        Ok(())
    }

    /* refuses logs of what lowerings read whose entries no lowering writes */
    fn vet_logs(&self) -> Result<(), String> {
        let learned = self.learned.len() / 3;
        if !self.learned.len().is_multiple_of(3) || !self.grown.len().is_multiple_of(4) || self.learned.chunks(3).any(|t| !t[0].is_int() || !t[1].is_int() || t[1].as_int() < 0) { return Err(bad()); }
        // a key's growths follow its base lowering and entries read only what was learned then
        let mut keys: Vec<i64> = Vec::new();
        for e in self.grown.chunks(4) {
            if !e[0].is_int() || !e[1].is_int() || !e[2].is_int() { return Err(bad()); }
            let (key, kind) = (e[0].as_int(), e[2].as_int());
            // a function a lowering laid follows that lowering's entry
            if kind == super::registers::LAID_ENTRY && keys.contains(&key) && e[3].is_undef() { continue; }
            if !(0..=learned as i64).contains(&e[1].as_int()) { return Err(bad()); }
            let known = usize::try_from(key).map_or_else(|_| self.module_of_key(key).is_some(), |fi| fi < self.functions.len());
            let base = kind == -1 || kind == -2;
            let growth = kind >= 0 && ((kind & 0xFF) as usize) < lower::Grow::ALL.len();
            // only a module's lowering starts where its log says and a function's ignores it
            let from = key >= 0 || e[3].is_undef() || e[3].is_int() && usize::try_from(e[3].as_int()).is_ok_and(|f| f <= self.chunk_of_key(key).map_or(0, |c| c.instructions.len()));
            // a key's log holds one base lowering since a lowering again drops the earlier log
            if !known || !(base && from && !keys.contains(&key) || growth && keys.contains(&key)) { return Err(bad()); }
            if base { keys.push(key); }
        }
        Ok(())
    }

    /* the module chunk whose code key is `key` */
    fn module_of_key(&self, key: i64) -> Option<&'a SSAChunk> {
        let mut chunks: Vec<&'a SSAChunk> = Vec::new();
        super::snapshot::source_index(self.chunk, &mut chunks);
        chunks.into_iter().find(|c| Self::code_key_of(c, None) == key)
    }

    /* the chunk a code key names which is a function's body or a module's */
    fn chunk_of_key(&self, key: i64) -> Option<&'a SSAChunk> {
        match usize::try_from(key) { Ok(fi) => self.functions.get(fi).map(|f| &f.1), Err(_) => self.module_of_key(key) }
    }

    /* whether every iterator place of `iters` steps a value of the kind it names */
    fn iters_hold(&self, iters: &[IterFrame]) -> bool {
        iters.iter().all(|f| match f {
            IterFrame::Coroutine(v) => matches!(self.heap.try_get(*v), Some(HeapObj::Coroutine(_))),
            IterFrame::Held(_, h) => h.excs.iter().all(|e| e.handler_ip & RAW_HANDLER != 0 || e.kind == BlockKind::Except || e.kind == BlockKind::Finally),
            _ => true,
        })
    }

    /* refuses functions whose defaults or cells their body could not hold */
    fn vet_heap(&self) -> Result<(), String> {
        for obj in self.heap.snapshot_objs().flatten() {
            let HeapObj::Func { fi, defaults, cells } = obj else { continue };
            let Some(f) = self.functions.get(*fi) else { return Err(bad()) };
            let arity = self.call_facts.get(*fi).and_then(|c| c.arity).unwrap_or(f.0.len());
            if defaults.len() > arity.max(f.0.len()) || cells.iter().any(|&(slot, _)| slot >= f.1.names.len()) { return Err(bad()); }
        }
        Ok(())
    }

    /* refuses a coroutine whose parked frames would leave their code or rows */
    fn vet_coro(&mut self, co: Val, prints: &[(i64, u64)], kept: usize, walked: &mut Walked) -> Result<(), String> {
        let Some(HeapObj::Coroutine(c)) = self.heap.try_get(co) else { return Ok(()) };
        if c.ip == FINISHED { return Ok(()); }
        let (body, ip) = (c.body, c.ip);
        // a generator or async body makes each coroutine but the root's module body
        let made = match body { BodyRef::Fn(fi) => self.functions.get(fi).is_some_and(|f| f.1.is_generator) || self.is_async_fn(fi), BodyRef::Module => self.root.as_ref().is_some_and(|h| h.coro.0 == co.0) };
        if !made { return Err(bad()); }
        // a body not yet started holds its bound frame and nothing more
        if ip == 0 && (!c.syncs.is_empty() || !c.levels.is_empty() || !c.excs.is_empty() || !c.iters.is_empty() || c.top != 0) { return Err(bad()); }
        if ip == 0 { return Ok(()); }
        let fi = match body { BodyRef::Fn(fi) => Some(fi), BodyRef::Module => None };
        let syncs = c.syncs.clone();
        let (top, slots, levels, excs, iters) = (c.top, c.slots.clone(), c.levels.clone(), c.excs.clone(), c.iters.clone());
        let laid = self.vet_park(fi, Park { ip, top, slots: &slots, levels: &levels, excs: &excs, iters: &iters }, prints, kept, syncs.is_empty(), walked)?;
        let mut layouts = Vec::with_capacity(syncs.len());
        for (k, f) in syncs.iter().enumerate() {
            // helpers a body waits inside are plain functions holding an instance only as constructors
            let plain = self.functions.get(f.fi).is_some_and(|g| !g.1.is_generator) && !self.is_async_fn(f.fi);
            let instance = f.instance.is_undef() || self.heap.instance(f.instance).is_some();
            let func = f.func.is_undef() || matches!(self.heap.try_get(f.func), Some(HeapObj::Func { .. }));
            if !plain || !instance || !func || f.ip == FINISHED { return Err(bad()); }
            layouts.push(self.vet_park(Some(f.fi), Park { ip: f.ip, top: f.top, slots: &f.slots, levels: &f.levels, excs: &f.exception_delta, iters: &f.iter_delta }, prints, kept, k + 1 == syncs.len(), walked)?);
        }
        // a park inside laid code resumes in code made here which a later save names
        if let Some(HeapObj::Coroutine(c)) = self.heap.try_get_mut(co) {
            c.layout = laid;
            for (f, l) in c.syncs.iter_mut().zip(layouts) { f.layout = l; }
        }
        Ok(())
    }

    /* the chunk and pool a parked body `fi` or the module body runs in */
    fn park_body(&mut self, fi: Option<usize>) -> (&'a SSAChunk, usize) {
        match fi { Some(fi) => (&self.functions[fi].1, self.fn_pool[fi]), None => (self.chunk, self.pool_of(self.chunk)) }
    }

    /* refuses one parked frame whose place rows handlers or levels its code never holds */
    fn vet_park(&mut self, fi: Option<usize>, p: Park, prints: &[(i64, u64)], kept: usize, inner: bool, walked: &mut Walked) -> Result<u32, String> {
        let (chunk, pool) = self.park_body(fi);
        let key = self.code_key(chunk, fi);
        // the handlers a frame holds and those a generator's section set aside on its iterators
        let held: Vec<ExceptionFrame> = p.iters.iter().filter_map(|it| match it { IterFrame::Held(_, h) => Some(h.excs.clone()), _ => None }).flatten().collect();
        let all: Vec<&ExceptionFrame> = p.excs.iter().chain(held.iter()).collect();
        if !self.iters_hold(p.iters) { return Err(bad()); }
        if all.iter().any(|e| e.turned > kept) || !p.excs.iter().all(|e| e.levels <= p.levels.len() && e.iter_depth <= p.iters.len()) { return Err(bad()); }
        let raw = p.ip & RESUME_RAW != 0;
        // a loop over a generator that waits keeps the row what wakes it takes
        let waits = match p.iters.last() { Some(&IterFrame::Coroutine(g)) => match self.heap.try_get_mut(g) { Some(HeapObj::Coroutine(c)) => c.wait_slot().is_some_and(|v| v.is_undef()), _ => false }, _ => false };
        let laid_handlers = all.iter().any(|e| e.handler_ip & RAW_HANDLER != 0);
        if !raw && (p.ip > chunk.instructions.len() || !p.levels.is_empty()) { return Err(bad()); }
        if !raw && !laid_handlers {
            if !all.iter().all(|e| handler_of(chunk, e)) { return Err(bad()); }
            // first code fuses method calls as the pool's shape reads them
            let src = lower::fuse_method_calls(chunk, self.pools[pool].shape);
            let depths = super::depth::depths(&src, chunk).unwrap_or_default();
            if rows_match(&src, chunk, &depths, p.ip, p.top, inner, waits) { return Ok(0); }
            // a park off its first code's rows resumes in lowered code which keeps no top
            self.pools[pool].heat = self.pools[pool].heat.max(lower::HOT_LOOP);
            let code = self.lowered_code(chunk, pool, self.was_laid(chunk, fi));
            return if code.rows == 0 && usize::from(p.top) <= code.frame.max(p.slots.len()) && lowered_takes(&code, &src, p.ip, p.top, inner) { Ok(0) } else { Err(bad()) };
        }
        // a park tied to laid code resumes in it remade as saved or is refused
        (self.resume_ip, self.resume_layout) = (p.ip, 0);
        // a laid code remakes from its own log once whatever else its pool holds
        let (code, once) = match walked.codes.iter().find(|v| v.0 == key) {
            Some(v) => (v.1.clone(), false),
            None => match raw.then(|| self.replayed(chunk, pool)).flatten() {
                Some(code) => { walked.codes.push((key, code.clone())); (code, false) }
                None => self.frame_code(chunk, pool),
            },
        };
        (self.resume_ip, self.resume_layout) = (0, 0);
        if once { self.retired.push(code.clone()); }
        let print = code_print(&code);
        if raw && !prints.iter().any(|&(k, q)| k == key && q == print) { return Err(LAID_OTHERWISE.to_string()); }
        if code.rows != 0 || usize::from(p.top) > code.frame.max(p.slots.len()) { return Err(bad()); }
        let raw_handlers: Vec<(usize, bool)> = code.ins.iter().filter(|i| matches!(i.op, OpCode::SetupExcept | OpCode::SetupFinally) && i.x == 1).map(|i| (usize::from(i.a), i.op == OpCode::SetupExcept)).collect();
        let handler = |e: &ExceptionFrame| if e.handler_ip & RAW_HANDLER != 0 { raw_handlers.contains(&(e.handler_ip & !RAW_HANDLER, e.kind == BlockKind::Except)) } else { handler_of(chunk, e) };
        if !all.iter().all(|e| handler(e) && usize::from(e.slot) <= code.frame) { return Err(LAID_OTHERWISE.to_string()); }
        if !raw { return Ok(0); }
        let at = p.ip & !RESUME_RAW;
        if at >= code.ins.len() { return Err(bad()); }
        let legal = links(&code);
        let link = |v: Val| v.is_undef() || v.is_int() && legal.binary_search(&v.as_int()).is_ok();
        // every register laid code jumps back through holds one of its own links
        if !link_regs(&code).iter().all(|&r| p.slots.get(usize::from(r)).is_none_or(|&v| link(v))) { return Err(bad()); }
        let Some(met) = vet_levels(&code, at, p.slots, p.levels, &link) else { return Err(bad()) };
        walked.levels += met;
        Ok(code.layout)
    }
}

/* whether `e`'s handler starts a block of its kind at a compiler index of `chunk` */
fn handler_of(chunk: &SSAChunk, e: &ExceptionFrame) -> bool {
    let setup = if e.kind == BlockKind::Except { OpCode::SetupExcept } else { OpCode::SetupFinally };
    chunk.instructions.iter().any(|i| i.opcode == setup && usize::from(i.operand) == e.handler_ip)
}

/* the levels parked at `at` that match those `code`'s sections keep with its links */
fn vet_levels(code: &Code, at: usize, slots: &[Val], levels: &[Val], link: &dyn Fn(Val) -> bool) -> Option<usize> {
    let none = || levels.is_empty().then_some(0);
    let Some(block) = code.block.as_deref() else { return none() };
    let held = |at: usize| block.holding(at).map(|(s, _)| s).or_else(|| if block.moved { block.moved_holding(at) } else { None });
    let reg = |r: u16| slots.get(usize::from(r)).copied().unwrap_or_else(Val::undef);
    let Some(mut s) = held(at) else { return none() };
    let (mut sp, mut v, mut met) = (levels.len(), reg(s.link), 0);
    // a stub's call keeps its level before it opens and that level names the link
    if let Some(words) = super::level::pending_level(code, at) {
        let n = super::level::kept(words);
        let start = sp.checked_sub(n)?;
        (sp, v, met) = (start, levels[sp - 1], 1);
    }
    // each level is met once so the walk ends within as many steps
    for _ in 0..levels.len() + block.sections.len() + 2 {
        if v.is_undef() { return (sp == 0).then_some(met); }
        if !link(v) { return None; }
        let back = (v.as_int() as usize >> 16) & 0xFFFF;
        let site = super::sections::head_of(code, back)?;
        let Some(outer) = held(site) else { return (sp == 0).then_some(met) };
        match super::level::level_words(code, site) {
            Some(words) => {
                let n = super::level::kept(words);
                let start = sp.checked_sub(n)?;
                // the level keeps the link of the run below it last
                let &below = levels.get(sp.wrapping_sub(1)).filter(|_| n > 0)?;
                (sp, v, met) = (start, below, met + 1);
            }
            None => v = reg(outer.link),
        }
        s = outer;
    }
    let _ = s;
    None
}
