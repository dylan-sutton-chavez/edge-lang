use core::mem::Discriminant;

use crate::value::hash_str;

use super::VM;
use super::methods::BuiltinMethodId;
use super::types::*;

/* What an attribute site saw last, so the next access skips the lookup. */
#[derive(Clone, Copy, Default)]
pub(crate) enum Site {
    #[default]
    Empty,
    /* The own attribute of an instance at entry `idx`, under its interned name and its hash. */
    Field { key: Val, idx: u32, hash: u64 },
    /* A store over an instance's own attribute, safe while no class changed since `epoch`. */
    SetField { class: Val, key: Val, idx: u32, epoch: u32 },
    /* A store adding the attribute to an instance of `class` holding `len` attributes. */
    Append { class: Val, key: Val, hash: u64, len: u32, epoch: u32 },
    /* A method `class` finds on `owner`, unless the instance shadows the name. */
    Method { class: Val, func: Val, owner: Val, hash: u64, epoch: u32 },
    /* A builtin type's method, by the receiver's variant. */
    Builtin { kind: Discriminant<HeapObj>, id: BuiltinMethodId },
    /* A module's attribute. */
    Module { module: Val, value: Val },
    /* An operator dunder an instance of `class` finds on `owner`, taking `arity` operands. */
    Dunder { class: Val, func: Val, owner: Val, arity: u8, epoch: u32 },
    /* A plain function a call opened as a frame of the loop, which opens again without its checks. */
    Call { callee: Val, fi: u32 },
}

impl Site {
    /* The values a site keeps, which a collection must not sweep. */
    pub(crate) fn roots(&self) -> [Val; 3] {
        let none = Val::none();
        match *self {
            Site::Field { key, .. } => [key, none, none],
            Site::SetField { class, key, .. } | Site::Append { class, key, .. } => [class, key, none],
            Site::Method { class, func, owner, .. } | Site::Dunder { class, func, owner, .. } => [class, func, owner],
            Site::Module { module, value } => [module, value, none],
            Site::Call { callee, .. } => [callee, none, none],
            Site::Empty | Site::Builtin { .. } => [none, none, none],
        }
    }
}

impl<'a> VM<'a> {
    /* `obj.name` through the site, None when the site cannot answer for `obj`. */
    #[inline]
    pub(crate) fn site_get(&self, site: Site, obj: Val) -> Option<Val> {
        match site {
            Site::Field { key, idx, hash } => match self.heap.try_get(obj) {
                Some(HeapObj::Instance(_, attrs)) => {
                    let a = attrs.borrow();
                    // A swept name may lend its slot to another, whose hash tells them apart.
                    (a.key_at(idx as usize).0 == key.0 && a.hash_at(idx as usize) == hash).then(|| a.value_at(idx as usize))
                }
                _ => None,
            },
            Site::Module { module, value } if module.0 == obj.0 => Some(value),
            _ => None,
        }
    }

    /* What a site keeps after a slow `obj.name`, an own field or module attribute. */
    pub(crate) fn learn_get(&self, obj: Val, name: &str) -> Site {
        match self.heap.try_get(obj) {
            Some(&HeapObj::Instance(class, ref attrs)) => {
                if self.shadowed && self.class_property(class, name) { return Site::Empty; }
                let a = attrs.borrow();
                match a.position_str(name, hash_str(name), &self.heap) {
                    Some(i) => Site::Field { key: a.key_at(i), idx: i as u32, hash: a.hash_at(i) },
                    None => Site::Empty,
                }
            }
            Some(HeapObj::Module(_, attrs)) => attrs.iter().find(|(n, _)| n == name).map_or(Site::Empty, |&(_, value)| Site::Module { module: obj, value }),
            _ => Site::Empty,
        }
    }

    /* `obj.name = value` through the site, false when the site cannot store for `obj`. */
    #[inline]
    pub(crate) fn site_store(&mut self, site: Site, obj: Val, value: Val) -> bool {
        match site {
            Site::SetField { class, key, idx, epoch } if epoch == self.class_epoch => {
                let Some(HeapObj::Instance(c, attrs)) = self.heap.try_get(obj) else { return false };
                let mut a = attrs.borrow_mut();
                if c.0 != class.0 || a.key_at(idx as usize).0 != key.0 { return false; }
                a.set_value_at(idx as usize, value);
                true
            }
            Site::Append { class, key, hash, len, epoch } if epoch == self.class_epoch => {
                let Some(HeapObj::Instance(c, attrs)) = self.heap.try_get(obj) else { return false };
                // The instance holds the attributes the site saw, so the new one goes last.
                if c.0 != class.0 { return false; }
                {
                    let a = attrs.borrow();
                    if a.len() != len as usize || a.entry_count() != len as usize || a.position_str(self.heap_str(key), hash, &self.heap).is_some() { return false; }
                }
                attrs.borrow_mut().push_hashed(key, value, hash);
                true
            }
            _ => false,
        }
    }

    /* What a site keeps after a slow store, `before` the counts before it. */
    pub(crate) fn learn_store(&self, obj: Val, name: &str, before: (usize, usize)) -> Site {
        let Some(&HeapObj::Instance(class, ref attrs)) = self.heap.try_get(obj) else { return Site::Empty };
        // A property of the name takes the store, which the site must never skip.
        if self.lookup_class_member(class, name).is_some_and(|(m, _)| matches!(self.heap.try_get(m), Some(HeapObj::Property(..)))) { return Site::Empty; }
        let a = attrs.borrow();
        let Some(i) = a.position_str(name, hash_str(name), &self.heap) else { return Site::Empty };
        let (key, epoch) = (a.key_at(i), self.class_epoch);
        if before.0 == before.1 && i == before.1 { Site::Append { class, key, hash: a.hash_at(i), len: i as u32, epoch } }
        else { Site::SetField { class, key, idx: i as u32, epoch } }
    }

    /* The function and owner a site's method call takes for `obj`. */
    #[inline]
    pub(crate) fn site_method(&self, site: Site, obj: Val, name: &str) -> Option<(Val, Val)> {
        let Site::Method { class, func, owner, hash, epoch } = site else { return None };
        if epoch != self.class_epoch { return None; }
        let Some(HeapObj::Instance(c, attrs)) = self.heap.try_get(obj) else { return None };
        // An attribute of the instance by the same name shadows the method.
        if c.0 != class.0 { return None; }
        if self.shadowed && attrs.borrow().position_str(name, hash, &self.heap).is_some() { return None; }
        Some((func, owner))
    }

    /* The builtin method a site keeps for a receiver of `obj`'s type. */
    #[inline]
    pub(crate) fn site_builtin(&self, site: Site, obj: Val) -> Option<BuiltinMethodId> {
        let Site::Builtin { kind, id } = site else { return None };
        (self.heap.plain(obj).map(core::mem::discriminant) == Some(kind)).then_some(id)
    }

    /* The site a method call keeps once the slow lookup bound `func`. */
    pub(crate) fn learn_method(&self, obj: Val, name: &str, func: Val, owner: Val) -> Site {
        let Some(&HeapObj::Instance(class, _)) = self.heap.try_get(obj) else { return Site::Empty };
        // Only a plain function binds the instance, a class or static method binds otherwise.
        let plain = self.lookup_class_member(class, name).is_some_and(|(m, _)| m.0 == func.0 && matches!(self.heap.try_get(m), Some(HeapObj::Func(..))));
        if !plain { return Site::Empty; }
        Site::Method { class, func, owner, hash: hash_str(name), epoch: self.class_epoch }
    }

    /* The site a builtin method call keeps, by the receiver's variant. */
    pub(crate) fn learn_builtin(&self, obj: Val, id: BuiltinMethodId) -> Site {
        match self.heap.try_get(obj) {
            // A type's method binds by the type itself, not by the variant.
            Some(HeapObj::Type(_) | HeapObj::Instance(..)) | None => Site::Empty,
            Some(o) => Site::Builtin { kind: core::mem::discriminant(o), id },
        }
    }

    fn heap_str(&self, key: Val) -> &str {
        match self.heap.try_get(key) { Some(HeapObj::Str(s)) => s, _ => "" }
    }
}
