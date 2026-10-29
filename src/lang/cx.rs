use alloc::{rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use crate::value::{DictMap, DictPart, HeapObj, HeapPool, Host, IterFrame, Val, ValSet, VmErr};
use crate::value::foreign::BinOp;
use crate::vm::VM;
use super::{KindId, kind_of};

/* the engine ops a native may use and none of them runs user code */
pub struct Cx<'c, 'v> { vm: &'c mut VM<'v> }

impl<'c, 'v> Cx<'c, 'v> {
    pub(crate) fn new(vm: &'c mut VM<'v>) -> Self { Self { vm } }
    /* the machine under an engine native which a runtime's natives never reach */
    pub(crate) fn vm(&mut self) -> &mut VM<'v> { self.vm }
    /* the runtime the machine booted with */
    pub fn runtime(&self) -> &'static super::Runtime { self.vm.rt }

    pub fn heap(&self) -> &HeapPool { &self.vm.heap }
    pub fn heap_mut(&mut self) -> &mut HeapPool { &mut self.vm.heap }

    pub fn kind(&self, v: Val) -> KindId { kind_of(&self.vm.heap, v) }
    /* what the runtime calls the engine's kinds errors and conventions */
    pub fn names(&self) -> &'static super::Names { self.vm.natives.names() }
    /* the natives of the runtime running the native */
    pub fn natives(&self) -> &'static super::Natives { self.vm.natives }
    /* the rules of the runtime running the native */
    pub fn policy(&self) -> &'static super::Policy { &self.vm.natives.policy }
    /* the name of the type of `v` as the runtime's `type_name` gives it */
    pub fn type_name(&self, v: Val) -> &str { (self.vm.rt.type_name)(self, v) }
    /* the type `NAMES` gives `v` which a runtime's own `type_name` may read */
    pub fn plain_type(&self, v: Val) -> &str { self.vm.type_name(v) }
    pub fn truthy(&self, v: Val) -> bool { self.vm.heap.truthy(v) }
    /* an int or wide int as an i128 and a bool where bools are ints */
    pub fn int(&self, v: Val) -> Option<i128> { crate::value::as_i128(v, &self.vm.heap) }
    /* a number as an f64 and None for anything else */
    pub fn float(&self, v: Val) -> Option<Result<f64, VmErr>> { crate::value::eq::num_f64(v, &self.vm.heap) }
    pub fn text(&self, v: Val) -> Option<&str> { Host::text(self.vm, v) }
    /* the text of a str charged as native work for its length */
    pub fn str_of(&self, v: Val) -> Option<&str> {
        let s = Host::text(self.vm, v)?;
        self.vm.heap.spend(s.len());
        Some(s)
    }
    pub fn bytes_of(&self, v: Val) -> Option<&[u8]> { self.vm.heap.bytes_of(v) }
    pub fn kw_name(&self, k: Val) -> Option<&str> { self.vm.kw_name(k) }
    pub fn is_ascii(&mut self, v: Val) -> bool { self.vm.heap.str_is_ascii(v) }

    /* equality as containers compare members with identity first and a cycle past the cap raising */
    pub fn eq(&self, a: Val, b: Val) -> Result<bool, VmErr> { self.vm.member_eq(a, b) }
    pub fn lt(&self, a: Val, b: Val) -> Result<bool, VmErr> { self.vm.lt_vals(a, b) }
    pub fn add(&mut self, a: Val, b: Val) -> Result<Val, VmErr> { self.vm.add_vals(a, b) }
    pub fn floor_div(&mut self, a: Val, b: Val) -> Result<Val, VmErr> { self.vm.floor_op(BinOp::FloorDiv, a, b) }
    pub fn modulo(&mut self, a: Val, b: Val) -> Result<Val, VmErr> { self.vm.floor_op(BinOp::Mod, a, b) }
    /* the power of `a` and `b` */
    pub fn pow(&mut self, a: Val, b: Val) -> Result<Val, VmErr> { self.vm.pow_vals(a, b) }
    pub fn len(&mut self, v: Val) -> Result<i128, VmErr> { self.vm.builtin_len(v) }
    pub fn get_item(&mut self, o: Val, i: Val) -> Result<Val, VmErr> { self.vm.getitem_val(o, i) }
    pub fn slice(&mut self, o: Val, start: Val, stop: Val, step: Val) -> Result<Val, VmErr> { self.vm.slice_val(o, start, stop, step) }
    pub fn contains(&mut self, c: Val, x: Val) -> Result<bool, VmErr> { self.vm.contains(c, x) }
    pub fn require_key(&self, v: Val) -> Result<(), VmErr> { self.vm.require_key(v) }
    pub fn require_element(&self, v: Val) -> Result<(), VmErr> { self.vm.require_element(v) }
    pub fn callable(&self, v: Val) -> bool { self.vm.is_callable(v) }
    pub fn is_generator(&self, v: Val) -> bool { self.vm.coro_kind(v) == Some(crate::vm::coro::CoroKind::Generator) }
    pub fn is_coroutine(&self, v: Val) -> bool { self.vm.coro_kind(v) == Some(crate::vm::coro::CoroKind::Coroutine) }

    pub fn new_int(&mut self, i: i128) -> Result<Val, VmErr> { self.vm.heap.int(i) }
    pub fn new_str(&mut self, s: String) -> Result<Val, VmErr> { self.vm.heap.alloc(HeapObj::Str(s)) }
    pub fn new_list(&mut self, items: Vec<Val>) -> Result<Val, VmErr> { self.vm.heap.alloc(HeapObj::List(Rc::new(RefCell::new(items)))) }
    pub fn new_tuple(&mut self, items: Vec<Val>) -> Result<Val, VmErr> { self.vm.heap.alloc(HeapObj::Tuple(items)) }
    pub fn new_dict(&mut self, d: DictMap) -> Result<Val, VmErr> { self.vm.heap.alloc(HeapObj::Dict(Rc::new(RefCell::new(d)))) }
    pub fn new_set(&mut self, s: ValSet) -> Result<Val, VmErr> { self.vm.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s)))) }
    pub fn new_bytes(&mut self, b: Vec<u8>, mutable: bool) -> Result<Val, VmErr> { self.vm.heap.alloc(if mutable { HeapObj::ByteArray(b) } else { HeapObj::Bytes(b) }) }
    /* the interned type object of `name` */
    pub fn type_object(&mut self, name: &str) -> Result<Val, VmErr> { self.vm.heap.type_object(name) }
    /* the type object of `v` whose kind's name it keeps with no copy */
    pub fn type_of(&mut self, v: Val) -> Result<Val, VmErr> {
        let found = {
            let name = self.type_name(v);
            let kinds = &self.vm.natives.names().kinds;
            (self.vm.heap.listed_type(name), kinds.iter().copied().find(|k| core::ptr::eq(k.as_ptr(), name.as_ptr()) && k.len() == name.len()))
        };
        match found {
            (Some(t), _) => Ok(Val::heap(t)),
            (None, Some(kind)) => self.vm.heap.type_object_static(kind),
            (None, None) => { let name = String::from(self.type_name(v)); self.vm.heap.type_object(&name) }
        }
    }

    /* every item of `v` or None when stepping it may run user code */
    pub fn items(&mut self, v: Val) -> Result<Option<Vec<Val>>, VmErr> {
        if self.vm.steps_user(v) { return Ok(None); }
        self.vm.extract_iter(v).map(Some)
    }
    /* the engine iterator over `v` or `v` itself when it already iterates */
    pub fn iter_of(&mut self, v: Val) -> Result<Val, VmErr> { self.vm.iter_of(v) }
    /* a builtin iterator named `name` over `frame` */
    pub fn new_iter(&mut self, frame: IterFrame, name: &'static str) -> Result<Val, VmErr> { self.vm.alloc_iterator(frame, name) }
    pub fn dict_walk(&self, d: Val, part: DictPart) -> IterFrame { self.vm.dict_walk(d, part) }
    /* the end of iteration that carries `v` as a generator's return */
    pub fn stop_iteration(&mut self, v: Val) -> VmErr { self.vm.stop_iteration(v) }
    /* the exception `name` holding `args` for its handler with `text` as its message */
    pub fn raised(&mut self, name: &'static str, args: Vec<Val>, text: &str) -> VmErr {
        match self.vm.heap.new_error_static(name, args) {
            Ok(exc) => { self.vm.pending.exc_val = Some(exc); VmErr::Raised(crate::s!(str name, ": ", str text)) }
            Err(e) => e,
        }
    }

    pub fn get_attr(&mut self, o: Val, name: &str) -> Result<Val, VmErr> { self.vm.attr_value(o, name) }
    pub fn set_attr(&mut self, o: Val, name: &str, v: Val) -> Result<(), VmErr> { self.vm.store_attr(o, name, v) }

    /* native work proportional to input charged at the next step */
    pub fn spend(&self, n: usize) { self.vm.heap.spend(n); }
    /* refuses a result of `bytes` before it is built */
    pub fn reserve(&self, bytes: usize) -> Result<(), VmErr> { self.vm.heap.reserve(bytes) }
    pub fn room(&self) -> usize { self.vm.heap.room() }
    pub fn charge(&mut self, n: usize) -> Result<(), VmErr> { self.vm.charge_steps(n) }

    /* hands the exact text to the host's output */
    pub fn write(&mut self, text: &str) { self.vm.write_out(text); }
    /* the runtime's kept line emptied so an output builds its text in held room */
    pub fn line(&mut self) -> String {
        let mut line = core::mem::take(&mut self.vm.out_line);
        line.clear();
        line
    }
    /* hands `line` to the output as `write` does and keeps its room for the next */
    pub fn write_line(&mut self, line: String) {
        self.vm.write_out(&line);
        self.vm.out_line = line;
    }
    /* the next line of input and None once it ended */
    pub fn read_line(&mut self) -> Option<String> {
        if let Some(line) = self.vm.input_buffer.get_mut(self.vm.input_at) {
            let line = core::mem::take(line);
            self.vm.input_at += 1;
            // a buffer read to its end clears so the next one starts at its front
            if self.vm.input_at == self.vm.input_buffer.len() { self.vm.input_buffer.clear(); self.vm.input_at = 0; }
            return Some(line);
        }
        if self.vm.strict_input { return None; }
        #[cfg(feature = "std")]
        {
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 { return None; }
            while line.ends_with('\n') || line.ends_with('\r') { line.pop(); }
            Some(line)
        }
        #[cfg(not(feature = "std"))]
        {
            None
        }
    }
    /* the oldest event the host pushed */
    pub fn take_event(&mut self) -> Option<Val> { self.vm.take_event() }
    /* whether the host's scheduler took the message */
    pub fn send(&mut self, group: &str, body: &str) -> bool { self.vm.send_hook.is_some_and(|hook| hook(group, body)) }
    /* whether the host took the message or the error it refused the message with */
    pub fn sent(&mut self, group: &str, body: &str) -> Result<bool, VmErr> {
        // the wasm runtime hands a send straight to its host which may refuse it
        #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
        {
            crate::targets::web::exports::sent(group, body)
        }
        #[cfg(not(all(target_arch = "wasm32", feature = "runtime")))]
        {
            Ok(self.vm.send_hook.is_some_and(|hook| hook(group, body)))
        }
    }


    /* refuses a value no table may hold naming only the unhashable type */
    pub fn require_hashable(&self, v: Val) -> Result<(), VmErr> { self.vm.require_hashable(v) }


    /* the qualified name a function class bound method or generator shows in its text */
    pub fn fn_name(&self, v: Val) -> Option<&str> {
        let vm = &*self.vm;
        let qual = |fi: usize| match vm.function_qualnames.get(fi) {
            Some(q) if !q.is_empty() => q.as_str(),
            _ => vm.function_names.get(fi).map(String::as_str).filter(|n| !n.is_empty()).unwrap_or(vm.natives.names().anonymous),
        };
        Some(match vm.heap.try_get(v)? {
            &HeapObj::Func { fi, .. } => qual(fi),
            HeapObj::Object { meta, .. } if meta.is_class_meta() => vm.class_qualnames.get(&v.0).map(String::as_str).or_else(|| vm.heap.class_name(v))?,
            &HeapObj::Bound { func: f, .. } if f.is_heap() => match vm.heap.try_get(f) { Some(&HeapObj::Func { fi, .. }) => qual(fi), _ => "?" },
            HeapObj::Coroutine(c) => match c.body { crate::value::BodyRef::Fn(fi) => qual(fi), crate::value::BodyRef::Module => vm.natives.names().module_body },
            _ => return None,
        })
    }
    /* the builtin exception class `cls` derives from and None when it is no exception */
    pub fn exc_base(&self, cls: Val) -> Option<&str> { self.vm.exc_base(cls) }
    /* removes attribute `name` of `o` as `del o.name` does */
    pub fn del_attr(&mut self, o: Val, name: &str) -> Result<(), VmErr> { self.vm.delete_attr_named(o, name) }
    /* the next item of `it` when stepping it runs no user code and None otherwise */
    pub fn next_plain(&mut self, it: Val) -> Option<Result<Option<Val>, VmErr>> {
        let vm = &mut *self.vm;
        let plain = match vm.heap.try_get(it) {
            Some(HeapObj::Coroutine(c)) if !c.pending.is_undef() => return Some(Ok(Some(vm.take_pending(it)))),
            // a callable iterator over a builtin calls it with no frame
            Some(HeapObj::Iter(f, _)) => match f.try_borrow().as_deref() {
                Ok(&IterFrame::Call { f, .. }) => !(matches!(vm.heap.try_get(f), Some(HeapObj::Func { .. })) || vm.heap.bound_of(f).is_some_and(|(_, g)| g.is_heap()) || vm.heap.is_class(f)) && !vm.steps_whole(f),
                _ => !vm.steps_user(it),
            },
            _ => false,
        };
        plain.then(|| vm.iter_step(it))
    }
    /* the token of `kind` once registered so its owner reads payloads off a shared heap */
    pub fn kind_token(heap: &HeapPool, kind: &'static crate::value::foreign::Kind) -> Option<crate::value::foreign::KindToken> { heap.foreign.token_of(kind) }
    /* whether `g` is a generator that ran to its end with no item held back */
    #[inline]
    pub fn spent(&self, g: Val) -> bool {
        matches!(self.vm.heap.try_get(g), Some(HeapObj::Coroutine(c)) if c.ip == crate::vm::coro::FINISHED && c.pending.is_undef()) && self.is_generator(g)
    }
    /* `f` on each of `items` and None when a call could run user code */
    pub fn call_each(&mut self, f: Val, items: &[Val]) -> Result<Option<Vec<Val>>, VmErr> {
        let heap = &self.vm.heap;
        let user = match heap.try_get(f) {
            Some(HeapObj::Func { .. }) => true,
            Some(&HeapObj::Bound { func, .. }) if func.is_heap() => true,
            Some(&HeapObj::Native(id)) if id.host_at().is_some() => true,
            Some(HeapObj::Object { meta, .. }) if meta.is_class_meta() => true,
            Some(&HeapObj::Bound { recv, .. }) => matches!(heap.try_get(recv), Some(HeapObj::Coroutine(_))),
            _ => false,
        };
        if user || self.vm.steps_whole(f) && items.iter().any(|&v| self.vm.steps_user(v)) { return Ok(None); }
        let chunk = self.vm.chunk;
        self.vm.call_rows(f, core::slice::from_ref(&items.to_vec()), items.len(), chunk).map(Some)
    }
    /* whether `g` is a generator holding no item so its next step resumes its frame */
    #[inline]
    pub fn resumable(&self, g: Val) -> bool {
        matches!(self.vm.heap.try_get(g), Some(HeapObj::Coroutine(c)) if c.pending.is_undef() && matches!(c.body, crate::value::BodyRef::Fn(fi) if !self.vm.is_async_fn(fi)))
    }
    /* `v` as text in `mode` the way the runtime's render hook writes it */
    pub fn render(&mut self, v: Val, mode: super::Mode) -> Result<String, VmErr> { self.vm.render(v, mode) }
    /* the index of `x` in wide range `r` and None for any other range */
    pub fn range_index(&mut self, r: Val, x: Val) -> Option<Result<Option<Val>, VmErr>> { crate::value::wide::range_index(&mut self.vm.heap, r, x) }
    /* wide range `r` walked back as its last item its end and its negated step */
    pub fn range_reversed(&mut self, r: Val) -> Option<Result<(Val, Val, Val), VmErr>> { crate::value::wide::range_reversed(&mut self.vm.heap, r) }
}
