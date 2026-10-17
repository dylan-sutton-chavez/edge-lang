use crate::s;
use alloc::{string::{String, ToString}, vec, vec::Vec};

use super::VM;
use super::types::*;

impl<'a> VM<'a> {

    // Interned keyword name behind a Val, or None when not a string.
    pub(crate) fn kw_name(&self, k: Val) -> Option<&str> {
        match self.heap.try_get(k) {
            Some(HeapObj::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    /* Byte offset of the last propagating error, or None on success / before `run()`. */
    pub fn error_pos(&self) -> Option<usize> { self.error_byte_pos.map(|p| p as usize) }

    /* `bad operand type for <what>: '<type>'`, what an operator or builtin of one operand says of a value it cannot take. */
    #[cold]
    #[inline(never)]
    pub(crate) fn bad_operand(&self, what: &str, v: Val) -> VmErr { VmErr::TypeMsg(crate::s!("bad operand type for ", str what, ": '", str self.type_name(v), "'")) }

    /* `C.__float__() must return a float, not int`, what a conversion dunder of `obj` that returned `got` raises. */
    #[cold]
    #[inline(never)]
    pub(crate) fn returned_wrong(&self, obj: Val, dunder: &str, want: &str, got: Val) -> VmErr {
        VmErr::TypeMsg(crate::s!(str self.type_name(obj), ".", str dunder, "() must return ", str want, ", not ", str self.type_name(got)))
    }

    /* The whole report of uncaught `e` in program `src`, its chain, then where it rose, in another text when it rose in one, then the calls it left. */
    pub fn render_error(&self, e: &VmErr, src: &str, path: Option<&str>) -> String {
        let mut out = self.render_chain(src, path);
        let mut head = e.render();
        if let Some(hint) = self.hint_for(e) { head.push_str(". "); head.push_str(&hint); }
        out.push_str(&match &self.error_inner {
            Some((s, p, pos)) => crate::value::err::render_text_at(head, s, Some(*pos as usize), Some(if p.is_empty() { "<string>" } else { p })),
            None => crate::value::err::render_text_at(head, src, self.error_pos(), path),
        });
        crate::value::err::render_calls(&mut out, &self.call_stack, &self.function_names, path, Some(self.chunk.source.as_str()));
        out
    }

    /* Intended process exit code when the last uncaught error is `SystemExit` with an integer (or absent/None) argument. `None` means "not a plain SystemExit", so the host renders a normal traceback. A non-int argument also yields `None` so its message surfaces as an error. */
    pub fn system_exit_code(&self) -> Option<i64> {
        let exc = self.pending.exc_val?;
        let HeapObj::ExcInstance(name, args, _) = self.heap.get(exc) else { return None; };
        if name != "SystemExit" { return None; }
        match args.first() {
            None => Some(0),
            Some(a) if a.is_none() => Some(0),
            Some(a) if a.is_int() => Some(a.as_int()),
            _ => None,
        }
    }

    pub fn call_stack_frames(&self) -> &[CallFrame] { &self.call_stack }
    pub fn function_names_ref(&self) -> &[String] { &self.function_names }

    /* Read-only heap access for embedders reading result values. */
    pub fn heap(&self) -> &HeapPool { &self.heap }

    /* Mutable heap access for embedders building argument values. */
    pub fn heap_mut(&mut self) -> &mut HeapPool { &mut self.heap }

    /* Faithful buffered output, lines rejoined with a trailing newline unless the last is open. */
    pub fn output_text(&self) -> String {
        let mut s = self.output.join("\n");
        if !self.output.is_empty() && !self.output_open { s.push('\n'); }
        s
    }

    /* Calls a top-level function by name with positional args, run() must have bound it first. */
    pub fn call_export(&mut self, name: &str, args: &[Val]) -> Result<Val, VmErr> {
        let _paying = self.paying()?;
        if args.len() > 255 {
            return Err(VmErr::TypeMsg(s!("call '", str name, "': too many arguments (max 255, got ", int args.len() as i64, ")")));
        }
        // A name read at run time may be a builtin the program never wrote, give it its slot first.
        self.register_builtin(name);
        let callee = self.global(name)
            .ok_or_else(|| VmErr::Name(name.into()))?;
        // Stack layout for a Call, callee at the bottom then positionals, exec_call pops them back.
        let chunk: &crate::parser::SSAChunk = unsafe { &*(self.chunk as *const _) };
        let before = self.stack.len();
        self.stack.push(callee);
        for &a in args { self.stack.push(a); }
        self.exec_call(args.len() as u16, chunk)?;
        if self.stack.len() != before + 1 {
            return Err(VmErr::Runtime("call_export: callable left no result"));
        }
        match self.stack.pop() { Some(v) => Ok(v), None => Err(VmErr::Runtime("call_export: stack drained")) }
    }


    // Stack helpers.

    #[inline] pub(crate) fn push(&mut self, v: Val) { self.stack.push(v); }

    #[inline] pub(crate) fn pop(&mut self) -> Result<Val, VmErr> {
        self.stack.pop().ok_or_else(|| cold_runtime("stack underflow"))
    }
    #[inline] pub(crate) fn pop2(&mut self) -> Result<(Val, Val), VmErr> {
        let b = self.pop()?; let a = self.pop()?; Ok((a, b))
    }
    #[inline] pub(crate) fn pop_n(&mut self, n: usize) -> Result<Vec<Val>, VmErr> {
        let at = self.stack.len().checked_sub(n).ok_or_else(|| cold_runtime("stack underflow"))?;
        Ok(self.stack.split_off(at))
    }

    /* Moves the live stacks above the bases into a suspended frame, handler depths made relative to it. */
    pub(crate) fn save_frames(&mut self, sb: usize, ib: usize, eb: usize, stack: &mut Vec<Val>, iters: &mut Vec<IterFrame>, excs: &mut Vec<ExceptionFrame>) {
        stack.extend(self.stack.drain(sb.min(self.stack.len())..));
        iters.extend(self.iter_stack.drain(ib.min(self.iter_stack.len())..));
        let at = excs.len();
        excs.extend(self.exception_stack.drain(eb.min(self.exception_stack.len())..));
        for f in &mut excs[at..] {
            f.stack_depth = f.stack_depth.saturating_sub(sb);
            f.iter_depth = f.iter_depth.saturating_sub(ib);
        }
    }

    /* Puts a suspended frame back on the live stacks and returns their bases, its buffers left empty to reuse. */
    pub(crate) fn restore_into(&mut self, stack: &mut Vec<Val>, iters: &mut Vec<IterFrame>, excs: &mut Vec<ExceptionFrame>) -> (usize, usize, usize) {
        let bases = (self.stack.len(), self.iter_stack.len(), self.exception_stack.len());
        self.stack.append(stack);
        self.iter_stack.append(iters);
        for f in excs.iter_mut() {
            f.stack_depth += bases.0;
            f.iter_depth += bases.1;
        }
        self.exception_stack.append(excs);
        bases
    }

    /* Runs `f` with `vals` as GC roots, since user code inside it can run a collection. */
    pub(crate) fn with_roots<R>(&mut self, vals: impl IntoIterator<Item = Val>, f: impl FnOnce(&mut Self) -> R) -> R {
        let base = self.temp_roots.len();
        self.temp_roots.extend(vals);
        let r = f(self);
        self.temp_roots.truncate(base);
        r
    }

    /* `f(*row)` for each of the first `n` rows of `cols`, args and results rooted while `f` runs. */
    pub(crate) fn call_rows(&mut self, f: Val, cols: &[Vec<Val>], n: usize, chunk: &crate::parser::SSAChunk) -> Result<Vec<Val>, VmErr> {
        self.with_roots(core::iter::once(f).chain(cols.iter().flatten().copied()), |vm| {
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                vm.push(f);
                for c in cols { vm.push(c[i]); }
                vm.exec_call(cols.len() as u16, chunk)?;
                let r = vm.pop()?;
                vm.temp_roots.push(r);
                out.push(r);
            }
            Ok(out)
        })
    }

    /* Items of any iterable, a user `__iter__` included, for `*` spreads and unpacking. */
    pub(crate) fn iterable_items(&mut self, v: Val, chunk: &crate::parser::SSAChunk) -> Result<Vec<Val>, VmErr> {
        match self.iter_to_vec_op(v, chunk)? {
            Some(items) => Ok(items),
            None => self.extract_iter(v),
        }
    }

    /* Materialise a mapping into (key_str, value) pairs for `**kwargs` spread. */
    pub(crate) fn mapping_to_kw_pairs(&self, v: Val) -> Result<Vec<(Val, Val)>, VmErr> {
        let not_mapping = || VmErr::TypeMsg(crate::s!("Value after ** must be a mapping, not ", str self.type_name(v)));
        if !v.is_heap() { return Err(not_mapping()); }
        match self.heap.get(v) {
            HeapObj::Dict(rc) | HeapObj::FrozenDict(rc) => {
                let entries: Vec<(Val, Val)> = rc.borrow().iter().collect();
                for (k, _) in &entries {
                    if !k.is_heap() || !matches!(self.heap.get(*k), HeapObj::Str(_)) {
                        return Err(VmErr::Type("keywords must be strings"));
                    }
                }
                Ok(entries)
            }
            _ => Err(not_mapping()),
        }
    }

    /* Seed slots with `undef()` so LoadName can detect unbound names via a u64 compare. */
    pub(crate) fn fill_builtins(&self, names: &[String]) -> Vec<Val> {
        let mut slots = vec![Val::undef(); names.len()];
        for (i, name) in names.iter().enumerate() {
            if let Some(v) = self.global_slot(name) {
                slots[i] = v;
            }
        }
        slots
    }

    /* An entry module binding by name, the builtin under it otherwise. */
    pub(crate) fn global(&self, bare: &str) -> Option<Val> {
        self.scopes[0].get(bare).or_else(|| self.builtins.get(bare).copied())
    }

    /* The global a slot starts from, a builtin also answering to its version-0 name. */
    pub(crate) fn global_slot(&self, name: &str) -> Option<Val> {
        self.global(name).or_else(|| name.strip_suffix("_0").and_then(|bare| self.builtins.get(bare).copied()))
    }

    #[inline]
    pub(crate) fn checked_jump(&mut self, target: usize, limit: usize) -> Result<usize, VmErr> {
        self.charge_step()?;
        if target > limit { return Err(cold_runtime("jump target out of bounds")); }
        Ok(target)
    }

    pub(crate) fn str_to_char_vals(&mut self, s: &str) -> Result<Vec<Val>, VmErr> {
        // Per-char heap allocs scale with input, charge the budget so loops over this stay bounded.
        self.charge_steps(s.len())?;
        s.chars().map(|c| self.heap.alloc(HeapObj::Str(c.to_string()))).collect()
    }

    /* A live walk over dict `d` showing the part `kind` names. */
    pub(crate) fn dict_walk(&self, d: Val, kind: View) -> IterFrame {
        let len = self.heap.dict_cell(d).map_or(0, |m| m.borrow().len());
        IterFrame::DictWalk { of: d, idx: 0, left: len, len, kind }
    }

    /* A frame over `items` read from `o`, watching a dict, dict view or set for a change of size. */
    pub(crate) fn watched(&self, o: Val, items: Vec<Val>) -> IterFrame {
        let of = match self.heap.try_get(o) {
            Some(HeapObj::Dict(_) | HeapObj::FrozenDict(_) | HeapObj::Set(_)) => o,
            Some(&HeapObj::DictView(d, _)) => d,
            _ => return IterFrame::Seq { items: items.into(), idx: 0 },
        };
        IterFrame::Watched { items: items.into(), idx: 0, of, len: crate::value::watched_len(&self.heap, of) }
    }

    pub(crate) fn make_iter_frame(&mut self, obj: Val, chunk: &crate::parser::SSAChunk) -> Result<IterFrame, VmErr> {
        if !obj.is_heap() {
            return Err(VmErr::TypeMsg(s!("'", str self.type_name(obj), "' object is not iterable")));
        }
        // Instance `__iter__` gives a user iterator stepped by `__next__`, or a generator or builtin iterator looped as itself.
        if self.instance_class(obj).is_some()
            && let Some(iter) = self.try_call_dunder(obj, "__iter__", &[], chunk)? {
            if matches!(self.heap.try_get(iter), Some(HeapObj::Instance(..))) { return Ok(IterFrame::UserDefined(iter)); }
            return self.make_iter_frame(iter, chunk);
        }
        Ok(match self.heap.get(obj) {
            HeapObj::Range(s, e, st) => IterFrame::Range { cur: *s, end: *e, step: *st },
            &HeapObj::BigRange(cur, end, step) => IterFrame::BigRange { cur, end, step },
            HeapObj::List(v) => IterFrame::List { rc: v.clone(), idx: 0 },
            HeapObj::Tuple(v) => IterFrame::Seq { items: v.as_slice().into(), idx: 0 },
            HeapObj::Dict(_) | HeapObj::FrozenDict(_) => self.dict_walk(obj, View::Keys),
            &HeapObj::DictView(d, kind) => self.dict_walk(d, kind),
            HeapObj::Set(s) => { let items = s.borrow().iter().cloned().collect(); self.watched(obj, items) }
            HeapObj::FrozenSet(s) => {
                let items: Vec<Val> = s.iter().cloned().collect();
                IterFrame::Seq { items: items.into(), idx: 0 }
            },
            HeapObj::Str(s) => {
                let s = s.clone();
                let items = self.str_to_char_vals(&s)?;
                IterFrame::Seq { items: items.into(), idx: 0 }
            },
            HeapObj::MemoryView(_) => {
                let items: Vec<Val> = self.heap.octets(obj).unwrap_or_default().iter().map(|&byte| Val::int(byte as i64)).collect();
                IterFrame::Seq { items: items.into(), idx: 0 }
            },
            HeapObj::Bytes(b) | HeapObj::ByteArray(b) => {
                // Bytes iteration yields ints, matching `iter()` and indexing.
                let items: Vec<Val> = b.iter().map(|&byte| Val::int(byte as i64)).collect();
                IterFrame::Seq { items: items.into(), idx: 0 }
            },
            // A coroutine or async generator steps only under `async for`.
            HeapObj::Coroutine(..) if !self.is_async_object(obj) => return Ok(IterFrame::Coroutine(obj)),
            // A builtin iterator advances in place, so the loop spends it.
            HeapObj::Iter(..) => return Ok(IterFrame::UserDefined(obj)),
            _ => return Err(self.not_iterable(obj)),
        })
    }

    /* `a, b = it`, or `a, *b, c = it` with `after` targets past the star. */
    pub(crate) fn unpack_iterable(&mut self, n: usize, after: Option<usize>, chunk: &crate::parser::SSAChunk) -> Result<(), VmErr> {
        let obj = self.pop()?;
        let (items, sized) = match self.heap.try_get(obj) {
            Some(HeapObj::Tuple(t)) => (t.clone(), true),
            Some(HeapObj::List(l)) => (l.borrow().clone(), true),
            _ => (self.iterable_items(obj, chunk)?, false),
        };
        let tail = after.unwrap_or(0);
        if items.len() < n + tail {
            let least = if after.is_some() { "at least " } else { "" };
            return Err(VmErr::Raised(s!("ValueError: not enough values to unpack (expected ", str least, int (n + tail) as i64, ", got ", int items.len() as i64, ")")));
        }
        // Only a list or tuple tells how many it had, like Python.
        if after.is_none() && items.len() > n {
            let got = if sized { s!(", got ", int items.len() as i64) } else { String::new() };
            return Err(VmErr::Raised(s!("ValueError: too many values to unpack (expected ", int n as i64, str &got, ")")));
        }
        let mid = items.len() - tail;
        for &v in items[mid..].iter().rev() { self.push(v); }
        if after.is_some() {
            let star = self.alloc_list(items[n..mid].to_vec())?;
            self.push(star);
        }
        for &v in items[..n].iter().rev() { self.push(v); }
        Ok(())
    }

    /* Pick the first defined Phi source, the slot stays unbound when neither is. */
    pub(crate) fn exec_phi(&mut self, op: u16, rip: usize, phi_map: &[usize], phi_sources: &[(u16, u16)]) {
        // Parse recovery can leave a Phi indexing past the frame (sized to names.len()), index defensively.
        let Some(&(ia, ib)) = phi_map.get(rip).and_then(|&pi| phi_sources.get(pi)) else { return };
        let a = self.regs.get(self.base + ia as usize).copied().unwrap_or_else(Val::undef);
        let val = if !a.is_undef() { a }
        else { self.regs.get(self.base + ib as usize).copied().unwrap_or_else(Val::undef) };
        if let Some(dst) = self.regs.get_mut(self.base + op as usize) { *dst = val; }
    }
}
