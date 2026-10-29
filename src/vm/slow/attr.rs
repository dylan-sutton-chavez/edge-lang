use crate::vm::VM;
use crate::value::{Val, VmErr, HeapObj, Host, cold_runtime};
use crate::code::OpCode;
use crate::syntax::chunk::SSAChunk;
use alloc::{string::String, vec::Vec};
use crate::lang::{same_text, Act, EngineError, ErrKind, KindId, Lock, Pattern};
use crate::vm::cache::OpcodeCache;

use crate::lang::{Cx, Native, Ret};

// the `resolve_attr` result with every shape `LoadAttr` and `CallMethod` dispatch on
pub(crate) enum AttrLookup {
    ModuleAttr(Val),
    ClassMember(Val),
    InstanceField(Val),
    // a class function bound to the instance it was read off
    InstanceMethod { recv: Val, func: Val },
    // a native method bound to its receiver or undef when read off the type
    Native(Val, &'static crate::lang::Native, KindId),
    // the name of a function type or class which `LoadAttr` materialises as a str
    Name(String),
    // a bound or step of a range which a read makes the int of
    Int(i64),
}

impl<'a> VM<'a> {
    // the cached order of a class or just itself for a builtin type
    pub(crate) fn mro_of(&self, c: Val) -> alloc::vec::Vec<Val> {
        match self.mro_cache.get(&c.0) {
            Some(r) => (**r).clone(),
            None => alloc::vec![c],
        }
    }

    /* the class then each base order depth first and left to right without repeats */
    pub(crate) fn order_of(&self, cls: Val, bases: &[Val]) -> alloc::vec::Vec<Val> {
        let mut out = alloc::vec![cls];
        for &b in bases {
            for c in self.mro_of(b) { if !out.iter().any(|o| o.0 == c.0) { out.push(c); } }
        }
        out
    }

    /* a method of the class that an instance attribute may hide */
    pub(crate) fn hides(&self, member: Val) -> bool { matches!(self.heap.try_get(member), Some(HeapObj::Func { .. })) }

    /* functions bind to the receiver while data members stay as they are */
    fn bind_member(&self, mv: Val, recv: Val) -> AttrLookup {
        match self.heap.try_get(mv) {
            // native class methods take self first as user functions do
            Some(HeapObj::Func { .. }) => AttrLookup::InstanceMethod { recv, func: mv },
            Some(&HeapObj::Native(id)) if id.host_at().is_some() => AttrLookup::InstanceMethod { recv, func: mv },
            _ => AttrLookup::ClassMember(mv),
        }
    }

    /* the first member by that name along the class order and the class holding it */
    pub(crate) fn lookup_class_member(&self, cls: Val, name: &str) -> Option<(Val, Val)> { class_member(&self.heap, &self.mro_cache, cls, name) }

    /* the builtin exception a class derives from or None for a plain class */
    pub(crate) fn exc_base(&self, cls: Val) -> Option<&str> {
        // the last class asked holds its answer until a class changes
        let (memo, epoch) = (self.exc_memo.get(), self.layout_epoch);
        let base = if memo.0 == cls.0 && memo.1 == epoch { memo.2 } else {
            let base = self.builtin_base(cls, &|t| self.heap.is_exc_type(t)).unwrap_or(Val::undef());
            self.exc_memo.set((cls.0, epoch, base));
            base
        };
        self.heap.type_object_name(base)
    }

    /* whether `v` is an instance of a raised class or of an engine error class */
    pub(crate) fn is_error(&self, v: Val) -> bool {
        let Some(c) = self.heap.class_of(v) else { return false };
        self.exc_base(c).is_some() || self.heap.type_object_name(c).is_some_and(|n| self.natives.names().error_named(n).is_some())
    }

    /* the first builtin type `pick` takes in `cls`'s cached order so diamonds walk once */
    fn builtin_base(&self, cls: Val, pick: &impl Fn(Val) -> bool) -> Option<Val> {
        let is = |c: Val| self.heap.is_type(c) && pick(c);
        if self.heap.is_type(cls) { return is(cls).then_some(cls); }
        let bases = self.heap.bases(cls);
        if bases.is_empty() { return None; }
        match self.mro_cache.get(&cls.0) {
            Some(mro) => mro.iter().copied().find(|&c| is(c)),
            None => bases.iter().find_map(|&b| self.builtin_base(b, pick)),
        }
    }

    /* sets the `args` of exception `inst` to what its constructor and init received */
    pub(crate) fn set_exc_args(&mut self, inst: Val, args: Vec<Val>) -> Result<(), VmErr> { self.heap.set_args(inst, args) }

    /* whether `name` is the arguments field of error instance `obj` */
    #[inline(always)]
    fn holds_exc_args(&self, obj: Val, name: &str) -> bool {
        name.len() == self.natives.names().fields.args.len() && self.exc_args_named(obj, name)
    }

    /* `holds_exc_args` past the length its name shares with the field */
    #[cold]
    #[inline(never)]
    fn exc_args_named(&self, obj: Val, name: &str) -> bool {
        same_text(name, self.natives.names().fields.args) && self.heap.class_of(obj).is_some_and(|c| self.exc_base(c).is_some())
    }

    /* stores `value` as error `obj`'s arguments by the tuple of its items */
    #[cold]
    #[inline(never)]
    fn store_exc_args(&mut self, obj: Val, value: Val) -> Result<(), VmErr> {
        let items = self.extract_iter(value)?;
        self.set_exc_args(obj, items)
    }

    // `obj.<name>` for calls and stores with a miss worded as the error it raises
    pub(crate) fn resolve_attr(&self, obj: Val, name: &str) -> Result<AttrLookup, VmErr> {
        self.find_attr(obj, name)?.ok_or_else(|| self.no_attr(obj, name))
    }

    /* the error a miss of `name` on `obj` raises worded only once it does */
    #[cold]
    #[inline(never)]
    pub(crate) fn no_attr(&self, obj: Val, name: &str) -> VmErr {
        EngineError::NoAttribute { recv: obj, name: String::from(name), store: false }.into()
    }

    // `obj.<name>` or None on a miss with other kinds falling to builtin methods
    pub(crate) fn find_attr(&self, obj: Val, name: &str) -> Result<Option<AttrLookup>, VmErr> {
        // the runtime's names load only on the paths that read them
        let names = || self.natives.names();
        match self.heap.try_get(obj) {
            // a module's attributes scan its table which holds a few dozen entries
            Some(HeapObj::Module(_, attrs)) => return Ok(attrs.iter().find(|(n, _)| n == name).map(|&(_, v)| AttrLookup::ModuleAttr(v))),
            // bound methods expose their receiver and user methods also their function
            Some(&HeapObj::Bound { recv, .. }) if same_text(name, names().fields.receiver) => return Ok(Some(AttrLookup::ClassMember(recv))),
            Some(&HeapObj::Bound { func, .. }) if func.is_heap() && same_text(name, names().fields.function) => return Ok(Some(AttrLookup::ClassMember(func))),
            Some(&HeapObj::Native(_) | &HeapObj::Bound { .. }) if same_text(name, names().name_field) && let Some((_, n, _)) = self.native_parts(obj) => return Ok(Some(AttrLookup::Name(n.name.into()))),
            // a function's stored attribute wins over its derived name
            Some(&HeapObj::Func { fi, .. }) => {
                if let Some(v) = self.heap.fn_field(obj, name) { return Ok(Some(AttrLookup::ClassMember(v))); }
                if same_text(name, names().name_field) && let Some(n) = self.function_names.get(fi) { return Ok(Some(AttrLookup::Name(if n.is_empty() { names().anonymous.into() } else { n.clone() }))); }
            }
            Some(HeapObj::Coroutine(_)) => if let Some(found) = self.coro_attr(obj, name) { return Ok(Some(found)); },
            // a class attribute like `MyClass.method` is the unbound function with no `self` prepended
            Some(HeapObj::Object { meta, .. }) if meta.is_class_meta() => {
                if same_text(name, names().name_field) { return Ok(Some(AttrLookup::Name(self.heap.class_name(obj).unwrap_or("").into()))); }
                if let Some((v, _)) = self.lookup_class_member(obj, name) { return Ok(Some(AttrLookup::ClassMember(v))); }
                // a class reads the init of object unbound
                if same_text(name, names().constructor) { return Ok(Some(self.init_lookup(Val::undef()))); }
                return Ok(None);
            }
            // a builtin type reads its name its kind's natives and the init of every class
            Some(HeapObj::Object { meta, .. }) if meta.is_type_meta() => {
                let n = self.heap.class_name(obj).unwrap_or("");
                if same_text(name, names().name_field) { return Ok(Some(AttrLookup::Name(n.into()))); }
                // a native method off a builtin type stays unbound unless it binds the type itself
                if let Some((m, owner)) = self.heap.type_kind(obj).and_then(|k| self.kind_method(k, name)) {
                    return Ok(Some(AttrLookup::Native(if m.on_class() { obj } else { Val::undef() }, m, owner)));
                }
                // a type's init method reaches the init every class ends in
                if same_text(name, names().constructor) && (same_text(n, names().kind(KindId::Instance)) || self.heap.is_exc_type(obj)) { return Ok(Some(self.init_lookup(Val::undef()))); }
            }
            // an instance reads its own fields first then its class chain
            Some(&HeapObj::Object { meta: cls_val, ref slots }) => {
                let found = slots.borrow().iter()
                    .find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == name))
                    .map(|(_, v)| v);
                if let Some(v) = found { return Ok(Some(AttrLookup::InstanceField(v))); }
                if let Some((mv, _)) = self.lookup_class_member(cls_val, name) { return Ok(Some(self.bind_member(mv, obj))); }
                // the arguments an instance was made with past any field or member of that name
                if same_text(name, names().fields.args) && let Some(t) = self.heap.args_tuple(obj) { return Ok(Some(AttrLookup::ClassMember(t))); }
                // the runtime's end of iteration carries its first argument as its value
                if same_text(name, names().fields.value) && self.heap.type_object_name(cls_val).is_some_and(|n| same_text(n, names().error(ErrKind::Stop))) {
                    return Ok(Some(AttrLookup::ClassMember(self.heap.error_args(obj).and_then(|a| a.first().copied()).unwrap_or(Val::none()))));
                }
                // last comes the init every class ends in bound to the instance
                if same_text(name, names().constructor) { return Ok(Some(self.init_lookup(obj))); }
                return Ok(None);
            }
            // the bounds and step of a range under the runtime's field names
            Some(&HeapObj::Range(s, e, st)) => if let Some(i) = names().fields.bound(name) { return Ok(Some(AttrLookup::Int([s, e, st][i]))); },
            Some(&HeapObj::BigRange(s, e, st)) => if let Some(i) = names().fields.bound(name) { return Ok(Some(AttrLookup::ClassMember([s, e, st][i]))); },
            _ => {}
        }
        // the runtime's native method of the receiver's kind
        Ok(self.native_method(obj, name).map(|(m, owner)| AttrLookup::Native(obj, m, owner)))
    }

    /* the init every class ends in bound to `recv` or unbound for undef */
    fn init_lookup(&self, recv: Val) -> AttrLookup { AttrLookup::Native(recv, &self.natives.init, KindId::Instance) }

    /* whether `obj` is an instance of `cls` as the runtime's isinstance answers */
    fn instance_of(&mut self, obj: Val, cls: Val) -> Result<bool, VmErr> {
        let Some(n) = self.natives.intrinsic(crate::lang::Intrinsic::IsA) else {
            // a runtime with no class test answers through its exception matcher
            return (self.rt.handles)(&self.heap, obj, cls);
        };
        if let Some(v) = n.fast2.and_then(|f| f(&mut self.heap, obj, cls)) { return Ok(self.truthy(v)); }
        match (n.run)(&mut crate::lang::Cx::new(self), crate::lang::Args { recv: Val::undef(), pos: &[obj, cls], kw: &[], named: &[] })? {
            crate::lang::Out::Val(v) => Ok(self.truthy(v)),
            _ => Err(cold_runtime("an isinstance that steps")),
        }
    }

    // the values a class pattern matches as a tuple or None on a miss
    pub(crate) fn match_class(&mut self, subj: Val, cls: Val, names: Val, npos: usize) -> Result<Val, VmErr> {
        if !self.instance_of(subj, cls)? { return Ok(Val::none()); }
        let mut attrs: Vec<String> = Vec::new();
        // the values found stay rooted while later reads may run user code
        let base = self.temp_roots.len();
        // a builtin type matches its one positional sub-pattern against the subject itself
        let held = self.natives.names();
        let self_match = matches!(self.heap.type_kind(cls), Some(KindId::Bool | KindId::Bytes | KindId::Dict | KindId::Float | KindId::Int | KindId::List | KindId::Set | KindId::Str | KindId::Tuple));
        if self_match && npos > 0 {
            if npos > 1 { return Err(EngineError::Pattern(Pattern::OnePositional(subj)).into()); }
            self.temp_roots.push(subj);
        } else if npos > 0 {
            let order = self.lookup_class_member(cls, held.fields.match_args).map(|(v, _)| v);
            let Some(HeapObj::Tuple(order)) = order.and_then(|v| self.heap.try_get(v)) else {
                return Err(EngineError::Pattern(Pattern::NoFields).into());
            };
            if npos > order.len() { return Err(EngineError::Pattern(Pattern::TooMany).into()); }
            let order = order[..npos].to_vec();
            for n in order { attrs.push(self.render(n, crate::lang::Mode::Str)?); }
        }
        let kw = match self.heap.get(names) { HeapObj::Tuple(kw) => kw.clone(), _ => Vec::new() };
        for n in kw { attrs.push(self.render(n, crate::lang::Mode::Str)?); }
        for a in &attrs {
            match self.attr_value(subj, a) {
                Ok(v) => self.temp_roots.push(v),
                // a missing attribute fails the pattern instead of raising
                Err(e) if self.absorb_attr_err(&e) => { self.temp_roots.truncate(base); return Ok(Val::none()); }
                Err(e) => { self.temp_roots.truncate(base); return Err(e); }
            }
        }
        let values = self.temp_roots.split_off(base);
        self.heap.alloc(HeapObj::Tuple(values))
    }

    // whether `v` fits a sequence pattern or with MatchMap a mapping one
    pub(crate) fn matches_shape(&self, op: OpCode, v: Val) -> Val {
        Val::bool(if op == OpCode::MatchSeq { v.is_heap() && matches!(self.heap.get(v), HeapObj::List(_) | HeapObj::Tuple(_)) } else { self.heap.dict_cell(v).is_some() })
    }

    /* true for any attribute error class which then counts as handled */
    pub(crate) fn absorb_attr_err(&mut self, e: &VmErr) -> bool {
        let names = self.natives.names();
        let user = self.pending.exc_val.and_then(|v| self.heap.class_of(v)).and_then(|c| self.exc_base(c));
        let hit = (matches!(e, VmErr::Raised(_)) || e.is_kind(ErrKind::Attribute)) && names.derives(user.unwrap_or(e.class(names)), names.error(ErrKind::Attribute));
        if hit { self.pending.exc_val = None; }
        hit
    }

    /* what `obj.name` reads */
    pub(crate) fn attr_value(&mut self, obj: Val, name: &str) -> Result<Val, VmErr> {
        let v = self.attr_found(obj, name)?;
        if v.is_undef() { self.attr_ruled(obj, name, None) } else { Ok(v) }
    }

    /* the engine's own `obj.name` or undef which no attribute holds for a rule to answer */
    pub(crate) fn attr_found(&mut self, obj: Val, name: &str) -> Result<Val, VmErr> {
        let Some(lookup) = self.find_attr(obj, name)? else {
            // a foreign kind's get and a map's key are the engine's own field rule
            if (obj.is_foreign() || self.natives.policy.map_fields) && let Some(v) = self.own_field(obj, name)? { return Ok(v); }
            return Ok(Val::undef());
        };
        self.lookup_value(lookup)
    }

    /* `obj.name` the engine missed as the rule kept at `at` gives it or the error */
    #[cold]
    #[inline(never)]
    pub(crate) fn attr_ruled(&mut self, obj: Val, name: &str, at: Option<(&OpcodeCache, usize)>) -> Result<Val, VmErr> {
        if let Some(v) = self.get_rule(obj, name, crate::lang::Access::Load, at)? { return Ok(v); }
        self.note_missing_attr(obj, name);
        Err(self.no_attr(obj, name))
    }

    /* what the lookup names */
    fn lookup_value(&mut self, lookup: AttrLookup) -> Result<Val, VmErr> {
        let made = match lookup {
            AttrLookup::ModuleAttr(v) | AttrLookup::ClassMember(v) | AttrLookup::InstanceField(v) => return Ok(v),
            AttrLookup::InstanceMethod { recv, func } => return self.heap.bind(recv, func),
            AttrLookup::Native(recv, n, _) if recv.is_undef() => return self.heap.native_value(n.id),
            AttrLookup::Native(recv, n, _) => return self.heap.bind_native(recv, n.id),
            AttrLookup::Name(s) => HeapObj::Str(s),
            AttrLookup::Int(n) => return self.heap.int(n as i128),
        };
        self.heap.alloc(made)
    }
}

/* `lookup_class_member` on the heap and `mro_cache` alone */
pub(crate) fn class_member(heap: &crate::value::HeapPool, mro_cache: &crate::util::hash::FxHashMap<u64, alloc::rc::Rc<Vec<Val>>>, cls: Val, name: &str) -> Option<(Val, Val)> {
    let head = heap.class_head(heap.meta_of(cls)?)?;
    // a class heads its own order so one without bases looks only at its members
    if let Some(v) = head.member(name) { return Some((v, cls)); }
    let bases = &head.bases;
    if bases.is_empty() { return None; }
    if let Some(mro) = mro_cache.get(&cls.0) {
        for &c in mro.iter().skip(1) {
            if let Some(v) = heap.meta_of(c).and_then(|m| heap.class_head(m)).and_then(|h| h.member(name)) { return Some((v, c)); }
        }
        return None;
    }
    for &b in bases {
        if let Some(found) = class_member(heap, mro_cache, b, name) { return Some(found); }
    }
    None
}

/* the docstring a body opens with less the indent its later lines share */
pub(crate) fn docstring(body: &crate::syntax::chunk::SSAChunk) -> Option<String> {
    use crate::code::OpCode;
    use crate::syntax::chunk::Value;
    let [first, second, ..] = body.instructions.as_slice() else { return None };
    if first.opcode != OpCode::LoadConst || second.opcode != OpCode::PopTop { return None; }
    let Some(Value::Str(text)) = body.constants.get(first.operand as usize) else { return None };
    let mut lines = text.split('\n');
    let blank = [' ', '\t'];
    let head = lines.next().unwrap_or("").trim_start_matches(blank);
    let rest: Vec<&str> = lines.collect();
    let indent = rest.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start_matches(blank).len()).min().unwrap_or(0);
    let mut out = String::from(head);
    for l in rest {
        out.push('\n');
        out.push_str(if l.trim().is_empty() { "" } else { &l[indent.min(l.len())..] });
    }
    Some(out)
}

impl<'a> VM<'a> {

    /* `obj.name = value` at a site that keeps where an instance's attribute went */
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store_attr_at(&mut self, obj: Val, name_idx: u16, value: Val, rip: usize, cache: &OpcodeCache, chunk: &SSAChunk) -> Result<(), VmErr> {
        if self.site_store(cache.site(rip), obj, value) { return Ok(()); }
        let name = chunk.names.get(name_idx as usize).ok_or_else(|| cold_runtime("StoreAttr: bad name index"))?;
        let before = self.heap.fields(obj).map_or(crate::value::NO_SHAPE, |a| a.borrow().shape());
        self.store_attr_in(obj, name, value, Some((cache, rip)))?;
        crate::vm::sites::keep_site(cache, rip, self.learn_store(obj, name, before));
        Ok(())
    }

    /* a store error on a value keeping no attributes worded by what `name` already is */
    #[cold]
    fn no_store(&mut self, obj: Val, name: &str) -> VmErr {
        // a field a foreign kind reads is as fixed as a range's bounds
        if self.heap.foreign_kind(obj).is_some_and(|k| k.get.is_some()) && matches!(self.heap.foreign_get(obj, name), Ok(Some(_))) {
            return EngineError::ReadOnly { recv: obj, name: String::from(name), lock: Lock::Fixed }.into();
        }
        let name = String::from(name);
        let lock = match self.heap.try_get(obj) {
            _ if self.heap.is_type(obj) => Lock::Type,
            _ => match self.resolve_attr(obj, &name) {
                Ok(AttrLookup::Native(..)) => Lock::Native,
                Ok(_) if matches!(self.heap.try_get(obj), Some(HeapObj::Range(..) | HeapObj::BigRange(..))) || self.heap.foreign_kind(obj).is_some_and(|k| k.get.is_some()) => Lock::Fixed,
                Ok(_) => Lock::Plain,
                Err(_) => return EngineError::NoAttribute { recv: obj, name, store: true }.into(),
            },
        };
        EngineError::ReadOnly { recv: obj, name, lock }.into()
    }

    /* stores obj.name for StoreAttr and setattr */
    #[inline]
    pub(crate) fn store_attr(&mut self, obj: Val, name: &str, value: Val) -> Result<(), VmErr> { self.store_attr_in(obj, name, value, None) }

    /* stores obj.name where site `at` keeps the rule a refused store asks */
    pub(crate) fn store_attr_in(&mut self, obj: Val, name: &str, value: Val, at: Option<(&OpcodeCache, usize)>) -> Result<(), VmErr> {
        if !obj.is_heap() { return if self.put_rule(obj, name, value, at)? { Ok(()) } else { Err(self.no_store(obj, name)) }; }
        let member = self.instance_class(obj).and_then(|c| self.lookup_class_member(c, name));
        // a class attribute inserts or replaces in the mutable members store
        if self.heap.is_class(obj) {
            // a method set on a class may share a name with an instance attribute
            self.shadowed |= self.hides(value);
            self.heap.set_member(obj, name, value)?;
            self.layout_epoch = self.layout_epoch.wrapping_add(1);
            return Ok(());
        }
        if matches!(self.heap.get(obj), HeapObj::Func { .. }) {
            self.heap.set_fn_field(obj, name, value);
            return Ok(());
        }
        // an error's arguments take the items of what is stored as one tuple
        if self.holds_exc_args(obj, name) { return self.store_exc_args(obj, value); }
        let key = self.heap.intern_str(name)?;
        let Some(attrs) = self.heap.own_fields(obj) else { return if self.put_rule(obj, name, value, at)? { Ok(()) } else { Err(self.no_store(obj, name)) } };
        if member.is_some_and(|(m, _)| self.hides(m)) && !core::mem::replace(&mut self.shadowed, true) { self.layout_epoch = self.layout_epoch.wrapping_add(1); }
        let mut a = attrs.borrow_mut();
        let (shape, count) = (a.shape(), a.entry_count());
        a.insert(key, value, &self.heap);
        // a new key moves the dict along its shapes but a replaced value does not
        if shape != crate::value::NO_SHAPE && a.entry_count() > count {
            drop(a);
            let next = self.shape_after(shape, key);
            if let Some(attrs) = self.heap.fields(obj) { attrs.borrow_mut().set_shape(next); }
        }
        Ok(())
    }
}

/* the engine's own native method `name` of kind `k` which a snapshot names */
pub(crate) fn own(natives: &'static crate::lang::Natives, k: crate::lang::KindId, name: &str) -> Option<&'static Native> {
    (k == crate::lang::KindId::Instance && name == natives.init.name).then_some(&natives.init)
}

/* the init every class ends in which keeps an exception's arguments as its args */
pub(crate) fn init(cx: &mut Cx<'_, '_>, a: crate::lang::Args<'_>) -> Ret {
    let exc = cx.heap().class_of(a.recv).is_some_and(|c| cx.exc_base(c).is_some());
    if exc { cx.vm().set_exc_args(a.recv, a.pos.to_vec())?; }
    else if !a.pos.is_empty() {
        return Err(EngineError::Call(crate::lang::CallFault::InitArgs).into());
    }
    Ok(Val::none().into())
}

impl<'a> VM<'a> {

    // `del obj.attr` removing the attribute named `op` in place
    pub fn exec_del_attr(&mut self, obj: Val, op: u16, chunk: &crate::syntax::chunk::SSAChunk) -> Result<(), VmErr> { self.del_attr_at(obj, op, chunk, None) }

    /* `exec_del_attr` where site `at` keeps the rule a missing attribute asks */
    pub(crate) fn del_attr_at(&mut self, obj: Val, op: u16, chunk: &crate::syntax::chunk::SSAChunk, at: Option<(&OpcodeCache, usize)>) -> Result<(), VmErr> {
        let name = chunk.names.get(op as usize).ok_or_else(|| cold_runtime("DelAttr: bad name index"))?.clone();
        self.delete_attr_in(obj, &name, at)
    }

    /* removes attribute `name` of `obj` for natives and `del` and raises when it is absent */
    #[inline]
    pub(crate) fn delete_attr_named(&mut self, obj: Val, name: &str) -> Result<(), VmErr> { self.delete_attr_in(obj, name, None) }

    /* `delete_attr_named` where site `at` keeps the rule a missing attribute asks */
    pub(crate) fn delete_attr_in(&mut self, obj: Val, name: &str, at: Option<(&OpcodeCache, usize)>) -> Result<(), VmErr> {
        if self.heap.is_class(obj) {
            self.layout_epoch = self.layout_epoch.wrapping_add(1);
            if self.heap.drop_member(obj, name) { return Ok(()); }
            if self.drop_rule(obj, name, at)? { return Ok(()); }
            return Err(self.no_attr(obj, name));
        }
        let attrs = match self.heap.try_get(obj) {
            Some(HeapObj::Func { .. }) => {
                if self.heap.drop_fn_field(obj, name) { return Ok(()); }
                None
            }
            Some(HeapObj::Object { meta, slots }) if meta.is_heap() => Some(slots.clone()),
            _ => return if self.drop_rule(obj, name, at)? { Ok(()) } else { Err(EngineError::cannot(Act::DeleteAttr, None)) },
        };
        // the instance gives up its own attribute
        if let Some(attrs) = attrs {
            let key = attrs.borrow().iter().find(|(k, _)| matches!(self.heap.try_get(*k), Some(HeapObj::Str(s)) if s == name)).map(|(k, _)| k);
            if key.is_some_and(|k| attrs.borrow_mut().remove(&k, &self.heap).is_some()) { return Ok(()); }
            // an error's arguments stay with it as the tuple they are
            if self.holds_exc_args(obj, name) { return Err(EngineError::cannot(Act::DropArgs, Some(obj))); }
        }
        if self.drop_rule(obj, name, at)? { return Ok(()); }
        Err(self.no_attr(obj, name))
    }
}
