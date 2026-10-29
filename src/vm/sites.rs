use alloc::vec::Vec;

use crate::value::{NO_SHAPE, hash_str};

use super::VM;
use crate::value::*;
use crate::r#unsafe::peek;
use crate::lang::{Access, CallRule, GetRule, Key, Op, OpRule};

/* what an attribute site saw last so the next access skips the lookup */
#[derive(Clone, Copy, Default)]
pub(crate) enum Site {
    #[default]
    Empty,
    /* an own attribute at `idx` by name and hash or `shape` while no class changed */
    Field { key: Val, idx: u32, hash: u64, shape: u32, epoch: u32 },
    /* a store over an instance's own attribute safe while no class changed since `epoch` */
    SetField { class: Val, key: Val, idx: u32, epoch: u32 },
    /* a store adding the attribute moves a `class` instance from shape `from` to `to` */
    Append { class: u32, key: Val, hash: u64, from: u16, to: u16, epoch: u32 },
    /* the method func the class at slot class finds unless the instance shadows it */
    Method { class: u32, func: u32, hash: u64, epoch: u32 },
    /* a runtime's native method for receivers of `kind` found in the table of `owner` */
    Native { kind: crate::lang::KindId, owner: crate::lang::KindId, native: &'static crate::lang::Native },
    /* a module's attribute */
    Module { module: Val, value: Val },
    /* a plain function a call opened as a loop frame that reopens without its checks */
    Call { callee: Val, fi: u32 },
    /* a class whose init of body fi a call opened as a frame */
    Ctor { class: u32, init: Val, fi: u32, attrs: u16, epoch: u32 },
    /* a runtime's rule for keys `k1` and `k2` kept while no class changed since `epoch` */
    Rule { what: u8, id: u16, epoch: u32, k1: u64, k2: u64, v: Val },
}

// a cache holds one site per instruction so a site takes four words
const _: () = assert!(core::mem::size_of::<Site>() == 32);

impl Site {
    /* the values a site keeps which a collection must not sweep */
    pub(crate) fn roots(&self) -> [Val; 3] {
        let none = Val::none();
        match *self {
            Site::Field { key, .. } => [key, none, none],
            Site::SetField { class, key, .. } => [class, key, none],
            Site::Append { class, key, .. } => [Val::heap(class), key, none],
            Site::Method { class, func, .. } => [Val::heap(class), Val::heap(func), none],
            Site::Module { module, value } => [module, value, none],
            Site::Call { callee, .. } => [callee, none, none],
            Site::Ctor { class, init, .. } => [Val::heap(class), init, none],
            // a sweep bumps the epoch every kept rule holds so none needs its values kept
            Site::Empty | Site::Native { .. } | Site::Rule { .. } => [none, none, none],
        }
    }
}

/* `obj.name` through the site from the heap alone letting the loop keep its frame borrowed */
#[inline(always)]
pub(crate) fn site_value(heap: &HeapPool, layout_epoch: u32, site: &Site, obj: Val) -> Option<Val> {
    match *site {
        Site::Field { key, idx, hash, shape, epoch } if epoch == layout_epoch => match heap.try_get(obj) {
            // a class meets the field its own slots hold under that key
            Some(HeapObj::Object { slots: attrs, .. }) => {
                // nothing writes the attributes while one is read so the read takes no borrow
                let a = peek(attrs)?;
                // the shape names every key in place so its entry holds the attribute
                if a.shape() == shape { return Some(a.value_at(idx as usize)); }
                // a swept name may lend its slot to another whose hash tells them apart
                (a.key_at(idx as usize).0 == key.0 && a.hash_at(idx as usize) == hash).then(|| a.value_at(idx as usize))
            }
            _ => None,
        },
        Site::Module { module, value } if module.0 == obj.0 => Some(value),
        _ => None,
    }
}

/* the native method a site keeps for a receiver of `obj`'s kind */
#[inline(always)]
pub(crate) fn site_native(heap: &HeapPool, site: &Site, obj: Val) -> Option<(&'static crate::lang::Native, crate::lang::KindId)> {
    let Site::Native { kind, owner, native } = *site else { return None };
    (crate::lang::kind_of(heap, obj) == kind).then_some((native, owner))
}

/* a native method a site binds to its receiver */
#[derive(Clone, Copy)]
pub(crate) struct Bound(crate::lang::NativeId);

impl Bound {
    /* the bound method value of `recv` */
    pub(crate) fn of(self, recv: Val) -> HeapObj { HeapObj::Bound { recv, func: Val::native_ref(self.0) } }
}

/* the bound method a site gives `obj.name` as a value */
#[inline]
pub(crate) fn site_bound(heap: &HeapPool, site: &Site, obj: Val) -> Option<Bound> {
    site_native(heap, site, obj).map(|(n, _)| Bound(n.id))
}

/* the method a site keeps for `obj` off the heap alone as `site_method` finds it */
#[inline(always)]
pub(crate) fn site_method_func(heap: &HeapPool, layout_epoch: u32, shadowed: bool, site: &Site, obj: Val, name: &str) -> Option<Val> {
    let Site::Method { class, func, hash, epoch, .. } = *site else { return None };
    if epoch != layout_epoch { return None; }
    let Some(HeapObj::Object { meta: c, slots: attrs }) = heap.try_get(obj) else { return None };
    if c.0 != Val::heap(class).0 || shadowed && attrs.borrow().position_str(name, hash, heap).is_some() { return None; }
    Some(Val::heap(func))
}

/* `obj.name = value` through the site off the heap alone or false when it cannot */
#[inline(always)]
pub(crate) fn site_put(heap: &HeapPool, layout_epoch: u32, site: &Site, obj: Val, value: Val) -> bool {
    match *site {
        Site::SetField { class, key, idx, epoch } if epoch == layout_epoch => {
            let Some(HeapObj::Object { meta: c, slots: attrs }) = heap.try_get(obj) else { return false };
            let mut a = attrs.borrow_mut();
            if c.0 != class.0 || a.key_at(idx as usize).0 != key.0 { return false; }
            a.set_value_at(idx as usize, value);
            true
        }
        Site::Append { class, key, hash, from, to, epoch } if epoch == layout_epoch => {
            let Some(HeapObj::Object { meta: c, slots: attrs }) = heap.try_get(obj) else { return false };
            let mut a = attrs.borrow_mut();
            // an instance of the shape the site saw lacks the key so it goes last
            if c.0 != Val::heap(class).0 || a.shape() != from as u32 { return false; }
            a.push_shaped(key, value, hash, to as u32);
            true
        }
        _ => false,
    }
}

// shapes past this many leave new key orders unnamed so invented attributes stay bounded
const MAX_SHAPES: usize = 4096;
// past this many keys after one shape the dict goes unnamed so scans stay short
const MAX_EDGES: usize = 16;
// the shape of an instance holding no attribute yet
pub(crate) const ROOT_SHAPE: u32 = 1;
// a shape no dict takes so a field site learned without one checks its key
const UNSHAPED: u32 = u32::MAX;

impl<'a> VM<'a> {
    /* `obj.name` through the site or None when it cannot answer for `obj` */
    #[inline(always)]
    pub(crate) fn site_get(&self, site: Site, obj: Val) -> Option<Val> { site_value(&self.heap, self.layout_epoch, &site, obj) }

    /* what a site keeps after a slow `obj.name` as an own field or module attribute */
    pub(crate) fn learn_get(&self, obj: Val, name: &str) -> Site {
        match self.heap.try_get(obj) {
            Some(HeapObj::Object { meta, slots: attrs }) if meta.is_heap() => {
                let a = attrs.borrow();
                if a.is_empty() { return Site::Empty; }
                match a.position_str(name, hash_str(name), &self.heap) {
                    Some(i) => Site::Field { key: a.key_at(i), idx: i as u32, hash: a.hash_at(i), shape: if a.shape() == NO_SHAPE { UNSHAPED } else { a.shape() }, epoch: self.layout_epoch },
                    None => Site::Empty,
                }
            }
            Some(HeapObj::Module(_, attrs)) => attrs.iter().find(|(n, _)| n == name).map_or(Site::Empty, |&(_, value)| Site::Module { module: obj, value }),
            _ => Site::Empty,
        }
    }

    /* `obj.name = value` through the site or false when it cannot store for `obj` */
    #[inline]
    pub(crate) fn site_store(&mut self, site: Site, obj: Val, value: Val) -> bool { site_put(&self.heap, self.layout_epoch, &site, obj, value) }

    /* what a site keeps after a slow store with `before` the shape the instance had */
    pub(crate) fn learn_store(&self, obj: Val, name: &str, before: u32) -> Site {
        let Some((class, attrs)) = self.heap.instance(obj) else { return Site::Empty };
        let a = attrs.borrow();
        let Some(i) = a.position_str(name, hash_str(name), &self.heap) else { return Site::Empty };
        let (key, epoch) = (a.key_at(i), self.layout_epoch);
        // a store that took the instance from one shape to the next appended its key
        if before != NO_SHAPE && a.shape() != NO_SHAPE && a.shape() != before { Site::Append { class: class.as_heap(), key, hash: a.hash_at(i), from: before as u16, to: a.shape() as u16, epoch } }
        else { Site::SetField { class, key, idx: i as u32, epoch } }
    }

    /* the shape a `shape` dict takes appending `key` or `NO_SHAPE` once the table fills */
    pub(crate) fn shape_after(&mut self, shape: u32, key: Val) -> u32 {
        // the table holds the unused `NO_SHAPE` and the root before any shape
        if self.shapes.is_empty() { self.shapes.extend([Vec::new(), Vec::new()]); }
        let Some(edges) = self.shapes.get(shape as usize).filter(|_| shape != NO_SHAPE) else { return NO_SHAPE };
        if let Some(&(_, next)) = edges.iter().find(|(k, _)| k.0 == key.0) { return next; }
        // a shape many key orders leave is not worth naming further
        if edges.len() >= MAX_EDGES || self.shapes.len() >= MAX_SHAPES { return NO_SHAPE; }
        let next = self.shapes.len() as u32;
        self.shapes.push(Vec::new());
        self.shapes[shape as usize].push((key, next));
        next
    }

    /* the function a site's method call takes for obj */
    #[inline]
    pub(crate) fn site_method(&self, site: Site, obj: Val, name: &str) -> Option<Val> {
        let Site::Method { class, func, hash, epoch } = site else { return None };
        if epoch != self.layout_epoch { return None; }
        let Some(HeapObj::Object { meta: c, slots: attrs }) = self.heap.try_get(obj) else { return None };
        // an attribute of the instance by the same name shadows the method
        if c.0 != Val::heap(class).0 { return None; }
        if self.shadowed && attrs.borrow().position_str(name, hash, &self.heap).is_some() { return None; }
        Some(Val::heap(func))
    }

    /* the native method a site keeps for a receiver of `obj`'s kind */
    #[inline(always)]
    pub(crate) fn site_native(&self, site: Site, obj: Val) -> Option<(&'static crate::lang::Native, crate::lang::KindId)> { site_native(&self.heap, &site, obj) }

    /* the site a native method call keeps but none where methods may open frames */
    #[inline(never)]
    pub(crate) fn learn_native(&self, obj: Val, native: &'static crate::lang::Native, owner: crate::lang::KindId) -> Site {
        match crate::lang::kind_of(&self.heap, obj) {
            // a foreign value keeps no site since its kind picks the table
            crate::lang::KindId::Coro | crate::lang::KindId::Type | crate::lang::KindId::Instance | crate::lang::KindId::Foreign => Site::Empty,
            kind => Site::Native { kind, owner, native },
        }
    }

    /* the site a read of `obj.name` keeps once it gave `v` */
    #[inline(never)]
    pub(crate) fn learn_loaded(&self, obj: Val, name: &str, v: Val) -> Site {
        // only a native bound to the receiver keeps a native site
        let native = self.heap.bound_native(v).filter(|&(recv, _)| recv.0 == obj.0).and_then(|(_, id)| self.natives.native(id));
        match native {
            Some(n) if let Some(owner) = n.owner => self.learn_native(obj, n, owner),
            _ => self.learn_get(obj, name),
        }
    }

    /* the site a method call keeps once the slow lookup bound `func` */
    pub(crate) fn learn_method(&self, obj: Val, name: &str, func: Val) -> Site {
        let Some(class) = self.heap.class_of(obj) else { return Site::Empty };
        // only a plain function binds the instance
        let plain = self.lookup_class_member(class, name).is_some_and(|(m, _)| m.0 == func.0 && matches!(self.heap.try_get(m), Some(HeapObj::Func { .. })));
        // a site keeps the slots of what it names which must be heap objects
        if !plain || !func.is_heap() { return Site::Empty; }
        Site::Method { class: class.as_heap(), func: func.as_heap(), hash: hash_str(name), epoch: self.layout_epoch }
    }
}

/* a learned site replacing the kept one unless it learned nothing over a rule */
pub(crate) fn keep_site(cache: &super::cache::OpcodeCache, i: usize, site: Site) {
    if matches!(site, Site::Empty) && matches!(cache.site(i), Site::Rule { .. }) { return; }
    cache.set_site(i, site);
}

// the family of a kept rule in the high bits of its `what`
const RULE_GET: u8 = 0x40;
const RULE_CALL: u8 = 0x80;
const RULE_OP: u8 = 0xC0;
const FAMILY: u8 = 0xC0;
// the key bits of a foreign kind past every engine kind and below every value
const FOREIGN_KEY: u64 = 1 << 32;

/* the bits a kept rule compares a key by */
pub(crate) fn key_bits(k: Key) -> u64 {
    match k { Key::Kind(k) => k as u64, Key::Object(v) => v.0, Key::Foreign(t) => FOREIGN_KEY | u64::from(t.index()) }
}

/* the site keeping get rule `rule` for `key` and name `name` read as `how` */
pub(crate) fn kept_get(rule: GetRule, key: u64, name: Val, how: Access, epoch: u32) -> Site {
    let (code, id, v) = match rule {
        GetRule::Slot => (0, 0, Val::none()),
        GetRule::Const(v) => (1, 0, v),
        GetRule::Method { func, bind } => (2 + u8::from(bind), 0, func),
        GetRule::Native(n) => (4, n.0, Val::none()),
        GetRule::Field => (5, 0, Val::none()),
        GetRule::Fail => (6, 0, Val::none()),
        GetRule::Ask => (7, 0, Val::none()),
    };
    Site::Rule { what: RULE_GET | (how as u8) << 3 | code, id, epoch, k1: key, k2: name.0, v }
}

/* the get rule a site keeps for `key` and `name` read as `how` */
pub(crate) fn get_kept(site: Site, key: u64, name: Val, how: Access, epoch: u32) -> Option<GetRule> {
    let Site::Rule { what, id, epoch: at, k1, k2, v } = site else { return None };
    if what & !7 != RULE_GET | (how as u8) << 3 || at != epoch || k1 != key || k2 != name.0 { return None; }
    Some(match what & 7 {
        0 => GetRule::Slot,
        1 => GetRule::Const(v),
        2 => GetRule::Method { func: v, bind: false },
        3 => GetRule::Method { func: v, bind: true },
        4 => GetRule::Native(crate::lang::NativeId(id)),
        5 => GetRule::Field,
        6 => GetRule::Fail,
        _ => GetRule::Ask,
    })
}

/* the site keeping call rule `rule` for callee `callee` */
pub(crate) fn kept_call(rule: CallRule, callee: Val, epoch: u32) -> Site {
    let (code, id, k2, v) = match rule {
        CallRule::Function => (0, 0, Val::none(), Val::none()),
        CallRule::Construct { meta, init, slots } => (1, slots, init.unwrap_or(Val::undef()), meta),
        CallRule::Bound { func, recv } => (2, 0, recv, func),
        CallRule::Native(n) => (3, n.0, Val::none(), Val::none()),
        CallRule::Fail => (4, 0, Val::none(), Val::none()),
        CallRule::Ask => (5, 0, Val::none(), Val::none()),
    };
    Site::Rule { what: RULE_CALL | code, id, epoch, k1: callee.0, k2: k2.0, v }
}

/* the call rule a site keeps for callee `callee` */
pub(crate) fn call_kept(site: Site, callee: Val, epoch: u32) -> Option<CallRule> {
    let Site::Rule { what, id, epoch: at, k1, k2, v } = site else { return None };
    if what & FAMILY != RULE_CALL || at != epoch || k1 != callee.0 { return None; }
    Some(match what & 7 {
        0 => CallRule::Function,
        1 => CallRule::Construct { meta: v, init: Some(Val(k2)).filter(|i| !i.is_undef()), slots: id },
        2 => CallRule::Bound { func: v, recv: Val(k2) },
        3 => CallRule::Native(crate::lang::NativeId(id)),
        4 => CallRule::Fail,
        _ => CallRule::Ask,
    })
}

/* the site keeping op rule `rule` for `op` over keys `a` and `b` */
pub(crate) fn kept_op(rule: OpRule, op: Op, a: u64, b: u64, epoch: u32) -> Site {
    let (code, v) = match rule {
        OpRule::Kernel => (0, Val::none()),
        OpRule::Native(n) => (1, Val::int(i64::from(n.0))),
        OpRule::Call { func, swap, negate } => (2 | u8::from(swap) << 3 | u8::from(negate) << 4, func),
        OpRule::Identity => (3, Val::none()),
        OpRule::Fail => (4, Val::none()),
    };
    Site::Rule { what: RULE_OP | code, id: op.code(), epoch, k1: a, k2: b, v }
}

/* the op rule a site keeps for `op` over keys `a` and `b` */
pub(crate) fn op_kept(site: Site, op: Op, a: u64, b: u64, epoch: u32) -> Option<OpRule> {
    let Site::Rule { what, id, epoch: at, k1, k2, v } = site else { return None };
    if what & FAMILY != RULE_OP || id != op.code() || at != epoch || k1 != a || k2 != b { return None; }
    Some(match what & 7 {
        0 => OpRule::Kernel,
        1 => OpRule::Native(crate::lang::NativeId(v.as_int() as u16)),
        2 => OpRule::Call { func: v, swap: what & 8 != 0, negate: what & 16 != 0 },
        3 => OpRule::Identity,
        _ => OpRule::Fail,
    })
}
