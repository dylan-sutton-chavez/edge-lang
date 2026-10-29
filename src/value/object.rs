use alloc::{boxed::Box, rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use super::{DictMap, HeapObj, HeapPool, Val, VmErr, hash_str};
use crate::abi::nan_box::QNAN;

/* the fields of functions a restore read by the slots that hold them */
pub(crate) type FnFields = Vec<(u32, Vec<(Box<str>, Val)>)>;

// the metas of classes and type objects carry their head's place past these tags
const CLASS_TAG: u64 = QNAN | 5;
const TYPE_TAG: u64 = QNAN | 6;
// the bits a form test reads since a meta is a heap value or tag
const FORM: u64 = 0xF;


/* what the heap keeps for objects apart from their slots made on the first use */
#[derive(Default)]
pub(crate) struct Tables {
    // the fields functions were given at the places their slots name and the free places
    pub(crate) fn_fields: Vec<Vec<(Box<str>, Val)>>,
    pub(crate) free_fn_fields: Vec<u32>,
    // the places of the heads a sweep freed
    pub(crate) free_heads: Vec<u32>,
    // the slot of each native method's value by its place
    pub(crate) natives: crate::util::hash::FxHashMap<u16, u32>,
    // the bindings hosts gave which a native's place past the table names
    pub(crate) externs: Vec<super::ExternFn>,
}

/* a head's name kept with no copy when the runtime's tables hold it */
#[derive(Clone, Debug)]
pub enum Text { Static(&'static str), Owned(Box<str>) }

impl Default for Text { fn default() -> Self { Text::Static("") } }

impl core::ops::Deref for Text {
    type Target = str;
    #[inline(always)]
    fn deref(&self) -> &str { match self { Text::Static(s) => s, Text::Owned(s) => s } }
}

impl core::borrow::Borrow<str> for Text { fn borrow(&self) -> &str { self } }
impl PartialEq for Text { fn eq(&self, o: &Self) -> bool { **self == **o } }
impl Eq for Text {}
impl core::hash::Hash for Text { fn hash<H: core::hash::Hasher>(&self, h: &mut H) { (**self).hash(h) } }

/* the name bases and members a class or a type object keeps beside its slots */
#[derive(Clone, Debug, Default)]
pub struct Head {
    pub name: Text,
    pub bases: Box<[Val]>,
    // the members a decorator or a store may change in the order they came
    pub members: Vec<(Box<str>, Val)>,
}

impl Head {
    /* the member `name` this head holds itself */
    #[inline(always)]
    pub fn member(&self, name: &str) -> Option<Val> { self.members.iter().find(|(n, _)| named(n, name)).map(|&(_, v)| v) }
}

/* whether member name `n` is `name` with mid lengths compared as two words */
#[inline(always)]
fn named(n: &str, name: &str) -> bool {
    let (a, b) = (n.as_bytes(), name.as_bytes());
    if a.len() != b.len() { return false; }
    if !(8..=16).contains(&a.len()) { return a == b; }
    a.first_chunk::<8>() == b.first_chunk::<8>() && a.last_chunk::<8>() == b.last_chunk::<8>()
}

impl Val {
    /* whether this meta makes its object a class */
    #[inline(always)]
    pub fn is_class_meta(&self) -> bool { self.0 & FORM == CLASS_TAG & FORM }

    /* whether this meta makes its object a type object */
    #[inline(always)]
    pub fn is_type_meta(&self) -> bool { self.0 & FORM == TYPE_TAG & FORM }

    /* the place of the head a class or type meta names */
    #[inline(always)]
    pub(crate) fn head(&self) -> Option<usize> { (self.is_class_meta() || self.is_type_meta()).then_some((self.0 >> 8 & 0xFFFF_FFFF) as usize) }
}

/* the meta of a class or a type object whose head lies at `at` */
#[inline(always)]
fn meta_at(class: bool, at: usize) -> Val { Val(if class { CLASS_TAG } else { TYPE_TAG } | (at as u64) << 8) }

impl HeapPool {
    #[inline(always)]
    pub(crate) fn tables(&self) -> Option<&Tables> { self.objects.as_deref() }

    #[inline]
    pub(crate) fn tables_mut(&mut self) -> &mut Tables { self.objects.get_or_insert_with(Default::default) }

    /* the meta and slots of object `v` */
    #[inline(always)]
    pub fn object(&self, v: Val) -> Option<(Val, &Rc<RefCell<DictMap>>)> {
        match self.try_get(v)? { HeapObj::Object { meta, slots } => Some((*meta, slots)), _ => None }
    }

    /* whether `v` is a class the program or a host made */
    #[inline(always)]
    pub fn is_class(&self, v: Val) -> bool { matches!(self.try_get(v), Some(HeapObj::Object { meta, .. }) if meta.is_class_meta()) }

    /* whether `v` is a type object of the runtime */
    #[inline(always)]
    pub fn is_type(&self, v: Val) -> bool { matches!(self.try_get(v), Some(HeapObj::Object { meta, .. }) if meta.is_type_meta()) }

    /* whether `v` is a class or a type object */
    #[inline]
    pub fn is_classlike(&self, v: Val) -> bool { matches!(self.try_get(v), Some(HeapObj::Object { meta, .. }) if meta.head().is_some()) }

    /* the class or type instance `v` was made of */
    #[inline(always)]
    pub fn class_of(&self, v: Val) -> Option<Val> {
        match self.try_get(v)? { HeapObj::Object { meta, .. } if meta.is_heap() => Some(*meta), _ => None }
    }

    /* the meta of object `v` which a guard compares with a class it holds */
    #[inline(always)]
    pub fn meta_of(&self, v: Val) -> Option<Val> {
        match self.try_get(v)? { HeapObj::Object { meta, .. } => Some(*meta), _ => None }
    }

    /* the fields of instance `v` */
    #[inline(always)]
    pub fn fields(&self, v: Val) -> Option<&Rc<RefCell<DictMap>>> {
        match self.try_get(v)? { HeapObj::Object { meta, slots } if meta.is_heap() => Some(slots), _ => None }
    }

    /* the class and fields of instance `v` */
    #[inline(always)]
    pub fn instance(&self, v: Val) -> Option<(Val, &Rc<RefCell<DictMap>>)> {
        match self.try_get(v)? { HeapObj::Object { meta, slots } if meta.is_heap() => Some((*meta, slots)), _ => None }
    }

    /* the head of class or type `v` */
    #[inline]
    pub fn head_of(&self, v: Val) -> Option<&Head> {
        let (meta, _) = self.object(v)?;
        self.heads.get(meta.head()?)
    }

    /* the name of class or type `v` */
    #[inline]
    pub fn class_name(&self, v: Val) -> Option<&str> { self.head_of(v).map(|h| &*h.name) }

    /* the name of the class or type whose meta is `meta` */
    #[inline(always)]
    pub fn meta_name(&self, meta: Val) -> Option<&str> { self.heads.get(meta.head()?).map(|h| &*h.name) }

    /* the name of type object `v` read with one look at its slot */
    #[inline(always)]
    pub fn type_named(&self, v: Val) -> Option<&str> { self.meta_of(v).filter(Val::is_type_meta).and_then(|m| self.meta_name(m)) }

    /* the name of type object `v` */
    #[inline]
    pub fn type_object_name(&self, v: Val) -> Option<&str> { self.type_named(v) }

    /* the bases of class `v` in their declared order and none for any other value */
    #[inline]
    pub fn bases(&self, v: Val) -> &[Val] { self.head_of(v).map_or(&[], |h| &h.bases) }

    /* the member `name` that class `v` holds itself */
    #[inline]
    pub fn own_member(&self, v: Val, name: &str) -> Option<Val> {
        let (meta, _) = self.object(v)?;
        self.class_head(meta)?.member(name)
    }

    /* the head of the class whose meta is `meta` */
    #[inline(always)]
    pub(crate) fn class_head(&self, meta: Val) -> Option<&Head> { if meta.is_class_meta() { self.heads.get(meta.head()?) } else { None } }

    /* the bases of the class meta `meta` names */
    #[inline(always)]
    pub fn bases_of(&self, meta: Val) -> &[Val] { meta.head().and_then(|at| self.heads.get(at)).map_or(&[], |h| &h.bases) }

    /* the names of the members class `v` holds itself in their order */
    pub fn member_names(&self, v: Val) -> Vec<String> {
        self.object(v).and_then(|(m, _)| self.class_head(m)).map_or_else(Vec::new, |h| h.members.iter().map(|(n, _)| String::from(&**n)).collect())
    }

    /* the head of class `v` for a store to change */
    fn class_head_mut(&mut self, v: Val) -> Option<&mut Head> {
        let (meta, _) = self.object(v)?;
        if !meta.is_class_meta() { return None; }
        self.heads.get_mut(meta.head()?)
    }

    /* stores member `name` of class `v` */
    pub fn set_member(&mut self, v: Val, name: &str, value: Val) -> Result<(), VmErr> {
        let Some(h) = self.class_head_mut(v) else { return Ok(()) };
        match h.members.iter_mut().find(|(n, _)| named(n, name)) {
            Some(slot) => slot.1 = value,
            None => h.members.push((name.into(), value)),
        }
        Ok(())
    }

    /* removes member `name` of class `v` and tells whether it held one */
    pub fn drop_member(&mut self, v: Val, name: &str) -> bool {
        let Some(h) = self.class_head_mut(v) else { return false };
        let before = h.members.len();
        h.members.retain(|(n, _)| !named(n, name));
        h.members.len() < before
    }

    /* a place for a head reusing one a sweep freed */
    fn put_head(&mut self, head: Head) -> usize {
        match self.objects.as_deref_mut().and_then(|t| t.free_heads.pop()) {
            Some(at) => { self.heads[at as usize] = head; at as usize }
            None => {
                // the first class or type takes room for a few at once
                if self.heads.capacity() == 0 { self.heads = Vec::with_capacity(4); }
                self.heads.push(head);
                self.heads.len() - 1
            }
        }
    }

    /* frees the head of an object of meta `meta` at slot `idx` a sweep freed */
    pub(crate) fn drop_head(&mut self, meta: Val, idx: u32) {
        let Some(at) = meta.head() else { return };
        let Some(head) = self.heads.get_mut(at) else { return };
        let name = core::mem::take(&mut head.name);
        (head.bases, head.members) = (Box::new([]), Vec::new());
        self.tables_mut().free_heads.push(at as u32);
        if !meta.is_type_meta() { return; }
        if self.err_cache.1 == idx + 1 { self.err_cache = (0, 0); }
        if self.unlisted == idx + 1 { self.unlisted = 0; }
        else { self.types.remove(hash_str(&name), idx); }
    }

    /* the empty slots type objects and instances with no fields yet share */
    fn shared_slots(&mut self) -> Rc<RefCell<DictMap>> { self.no_slots.get_or_insert_with(|| Rc::new(RefCell::new(DictMap::new()))).clone() }

    /* the fields of instance `v` its own to change with shared empty ones copied first */
    pub(crate) fn own_fields(&mut self, v: Val) -> Option<Rc<RefCell<DictMap>>> {
        let shared = self.no_slots.as_ref().map(Rc::as_ptr);
        match self.try_get_mut(v)? {
            HeapObj::Object { meta, slots } if meta.is_heap() => {
                if shared == Some(Rc::as_ptr(slots)) { *slots = Rc::new(RefCell::new(DictMap::new())); }
                Some(slots.clone())
            }
            _ => None,
        }
    }

    /* a class named `name` over `bases` holding `members` in their order */
    pub fn new_class(&mut self, name: impl Into<Box<str>>, bases: Vec<Val>, members: Vec<(Box<str>, Val)>) -> Result<Val, VmErr> {
        let at = self.put_head(Head { name: Text::Owned(name.into()), bases: bases.into(), members });
        let slots = self.shared_slots();
        let made = self.alloc(HeapObj::Object { meta: meta_at(true, at), slots });
        if made.is_err() { self.tables_mut().free_heads.push(at as u32); }
        made
    }

    /* the type object named `name` which every load of that name shares */
    pub fn type_object(&mut self, name: &str) -> Result<Val, VmErr> { self.type_object_in(name, None, true) }

    /* `type_object` of a name the runtime's tables hold which its head keeps with no copy */
    pub fn type_object_static(&mut self, name: &'static str) -> Result<Val, VmErr> { self.type_object_in(name, Some(name), true) }

    /* `type_object` where a memory error's own type skips the soft limit */
    fn type_object_in(&mut self, name: &str, kept: Option<&'static str>, soft: bool) -> Result<Val, VmErr> {
        if let Some(i) = self.listed_type(name) { return Ok(Val::heap(i)); }
        // the error type a raise made unlisted joins the table once a load names it
        if let Some(i) = self.unlisted_named(name) {
            self.unlisted = 0;
            self.list_type(i);
            return Ok(Val::heap(i));
        }
        let made = self.new_type(name, kept, soft)?;
        self.list_type(made.as_heap());
        Ok(made)
    }

    /* a type object named `name` which no table lists */
    fn new_type(&mut self, name: &str, kept: Option<&'static str>, soft: bool) -> Result<Val, VmErr> {
        let slots = self.shared_slots();
        let text = match kept { Some(s) => Text::Static(s), None => Text::Owned(name.into()) };
        let at = self.put_head(Head { name: text, ..Head::default() });
        let obj = HeapObj::Object { meta: meta_at(false, at), slots };
        let made = if soft { self.alloc(obj) } else { self.alloc_emergency(obj) };
        if made.is_err() { self.tables_mut().free_heads.push(at as u32); }
        made
    }

    /* the slot of the unlisted error type named `name` */
    #[inline]
    fn unlisted_named(&self, name: &str) -> Option<u32> {
        let i = self.unlisted.checked_sub(1)?;
        type_name_at(&self.slots, &self.heads, i).is_some_and(|n| same(n, name)).then_some(i)
    }

    /* the slot of the type object named `name` */
    #[inline]
    pub fn listed_type(&self, name: &str) -> Option<u32> {
        if self.types.is_empty() { return None; }
        let (slots, heads) = (&self.slots, &self.heads);
        self.types.find(hash_str(name), |i| type_name_at(slots, heads, i).is_some_and(|n| same(n, name)), |n| self.spend(n))
    }

    /* lists the type object at slot `idx` under its name */
    pub(crate) fn list_type(&mut self, idx: u32) {
        let Some(h) = type_name_at(&self.slots, &self.heads, idx).map(hash_str) else { return };
        self.enter(|p| &mut p.types, h, idx);
    }

    /* an instance of `meta` holding `fields` */
    #[inline]
    pub fn new_instance(&mut self, meta: Val, fields: DictMap) -> Result<Val, VmErr> {
        self.alloc(HeapObj::Object { meta, slots: Rc::new(RefCell::new(fields)) })
    }

    /* an error of the class named `name` made with `args` */
    pub fn new_error(&mut self, name: &str, args: Vec<Val>) -> Result<Val, VmErr> { self.error_in(name, args, true) }

    /* `new_error` past the soft limit for the error that reports the limit itself */
    pub fn new_error_emergency(&mut self, name: &str, args: Vec<Val>) -> Result<Val, VmErr> { self.error_in(name, args, false) }

    fn error_in(&mut self, name: &str, args: Vec<Val>, soft: bool) -> Result<Val, VmErr> {
        let meta = self.type_object_in(name, None, soft)?;
        self.error_with(meta, args, soft)
    }

    /* an engine error of kind `kind` made with `args` */
    pub fn engine_error(&mut self, kind: crate::lang::ErrKind, args: Vec<Val>) -> Result<Val, VmErr> {
        // the kind a raise made last finds its type with no lookup
        let meta = match self.err_cache {
            (k, at) if at != 0 && k == kind as u32 => Val::heap(at - 1),
            _ => {
                let t = self.raised_type(self.names.error(kind))?;
                self.err_cache = (kind as u32, t.as_heap() + 1);
                t
            }
        };
        self.error_with(meta, args, true)
    }

    /* `new_error` of a class the runtime's tables name made by a raise */
    pub fn new_error_static(&mut self, name: &'static str, args: Vec<Val>) -> Result<Val, VmErr> {
        let meta = self.raised_type(name)?;
        self.error_with(meta, args, true)
    }

    /* the type a raise of class `name` makes unlisted until a load names it */
    fn raised_type(&mut self, name: &'static str) -> Result<Val, VmErr> {
        if let Some(i) = self.listed_type(name).or_else(|| self.unlisted_named(name)) { return Ok(Val::heap(i)); }
        let t = self.new_type(name, Some(name), true)?;
        // a second kind raised lists the first so one stays unlisted at most
        if let Some(old) = self.unlisted.checked_sub(1) { self.list_type(old); }
        self.unlisted = t.as_heap() + 1;
        Ok(t)
    }

    /* an error of type object `meta` made with `args` */
    pub fn error_of(&mut self, meta: Val, args: Vec<Val>) -> Result<Val, VmErr> { self.error_with(meta, args, true) }

    fn error_with(&mut self, meta: Val, args: Vec<Val>, soft: bool) -> Result<Val, VmErr> {
        let obj = HeapObj::Object { meta, slots: self.shared_slots() };
        let v = if soft { self.alloc(obj)? } else { self.alloc_emergency(obj)? };
        let t = if soft { self.alloc(HeapObj::Tuple(args))? } else { self.alloc_emergency(HeapObj::Tuple(args))? };
        self.hold_args(v, t);
        Ok(v)
    }

    /* makes tuple `t` the arguments instance `v` holds as its args */
    #[inline]
    fn hold_args(&mut self, v: Val, t: Val) {
        if let Some(s) = self.slots.get_mut(v.as_heap() as usize) { s.chars = t.as_heap() + 1; }
    }

    /* makes `args` the arguments of instance `v` dropping an args field a store gave it */
    pub fn set_args(&mut self, v: Val, args: Vec<Val>) -> Result<(), VmErr> {
        let Some(f) = self.fields(v) else { return Ok(()) };
        let name = self.names.fields.args;
        let kept = if f.borrow().is_empty() { None } else { f.borrow().position_str(name, hash_str(name), self).map(|i| f.borrow().key_at(i)) };
        if let Some(key) = kept && let Some(f) = self.own_fields(v) { f.borrow_mut().remove(&key, self); }
        let t = self.alloc(HeapObj::Tuple(args))?;
        self.hold_args(v, t);
        Ok(())
    }

    /* the tuple of arguments instance `v` holds */
    #[inline]
    pub(crate) fn args_tuple(&self, v: Val) -> Option<Val> {
        if !v.is_heap() { return None; }
        let s = self.slots.get(v.as_heap() as usize)?;
        match s.obj.as_ref()? { HeapObj::Object { meta, .. } if s.chars != 0 && meta.is_heap() => Some(Val::heap(s.chars - 1)), _ => None }
    }

    /* the arguments instance `v` was made with */
    #[inline]
    pub fn made_args(&self, v: Val) -> Option<&[Val]> {
        if !v.is_heap() { return None; }
        self.held_at(v.as_heap() as usize)
    }

    /* the arguments of instance `v` that its args field or its making gave */
    pub fn error_args(&self, v: Val) -> Option<&[Val]> {
        match self.try_get(self.error_tuple(v)?)? { HeapObj::Tuple(t) => Some(t), _ => None }
    }

    /* the tuple `error_args` reads the arguments of instance `v` from */
    pub fn error_tuple(&self, v: Val) -> Option<Val> {
        if !v.is_heap() { return None; }
        let s = self.slots.get(v.as_heap() as usize)?;
        let HeapObj::Object { meta, slots } = s.obj.as_ref()? else { return None };
        if !meta.is_heap() { return None; }
        // a store of the args field wins over the arguments the instance was made with
        if let Some(f) = crate::r#unsafe::peek(slots) && !f.is_empty() {
            let name = self.names.fields.args;
            if let Some(i) = f.position_str(name, hash_str(name), self) && let t = f.value_at(i) && matches!(self.try_get(t), Some(HeapObj::Tuple(_))) { return Some(t); }
        }
        (s.chars != 0).then(|| Val::heap(s.chars - 1))
    }

    /* the name of the builtin error class instance `v` was made of with its args */
    pub fn error_parts(&self, v: Val) -> Option<(&str, &[Val])> {
        let meta = self.class_of(v)?;
        let name = self.type_object_name(meta)?;
        Some((name, self.error_args(v).unwrap_or(&[])))
    }

    /* the arguments of the instance at slot `idx` from the tuple it holds */
    #[inline(always)]
    pub(crate) fn held_at(&self, idx: usize) -> Option<&[Val]> {
        let s = self.slots.get(idx)?;
        let HeapObj::Object { meta, .. } = s.obj.as_ref()? else { return None };
        if s.chars == 0 || !meta.is_heap() { return None; }
        match self.slots.get(s.chars as usize - 1)?.obj.as_ref()? { HeapObj::Tuple(items) => Some(items), _ => None }
    }

    /* the fields function `f` was given and none for any other value */
    #[inline]
    pub fn fn_fields(&self, f: Val) -> Option<&[(Box<str>, Val)]> {
        if !f.is_heap() { return None; }
        self.fn_fields_at(f.as_heap() as usize)
    }

    /* the fields of the function at slot `idx` from the side table */
    #[inline(always)]
    pub(crate) fn fn_fields_at(&self, idx: usize) -> Option<&[(Box<str>, Val)]> {
        let s = self.slots.get(idx)?;
        if s.chars == 0 || !matches!(s.obj, Some(HeapObj::Func { .. })) { return None; }
        self.tables()?.fn_fields.get(s.chars as usize - 1).map(Vec::as_slice)
    }

    /* the field `name` function `f` was given */
    #[inline]
    pub fn fn_field(&self, f: Val, name: &str) -> Option<Val> { self.fn_fields(f)?.iter().find(|(n, _)| named(n, name)).map(|&(_, v)| v) }

    /* stores field `name` of function `f` making its place on the first */
    pub fn set_fn_field(&mut self, f: Val, name: &str, value: Val) {
        let i = f.as_heap() as usize;
        let Some(at) = self.slots.get(i).filter(|s| matches!(s.obj, Some(HeapObj::Func { .. }))).map(|s| s.chars) else { return };
        let at = if at != 0 { at - 1 } else {
            let p = { let t = self.tables_mut(); match t.free_fn_fields.pop() { Some(p) => p, None => { t.fn_fields.push(Vec::new()); t.fn_fields.len() as u32 - 1 } } };
            self.slots[i].chars = p + 1;
            p
        };
        let Some(fields) = self.objects.as_deref_mut().and_then(|t| t.fn_fields.get_mut(at as usize)) else { return };
        match fields.iter_mut().find(|(n, _)| named(n, name)) {
            Some(slot) => slot.1 = value,
            None => fields.push((name.into(), value)),
        }
    }

    /* removes field `name` of function `f` and tells whether it held one */
    pub fn drop_fn_field(&mut self, f: Val, name: &str) -> bool {
        let Some(at) = f.is_heap().then(|| self.slots.get(f.as_heap() as usize)).flatten().filter(|s| matches!(s.obj, Some(HeapObj::Func { .. }))).map(|s| s.chars).filter(|&a| a != 0) else { return false };
        let Some(fields) = self.objects.as_deref_mut().and_then(|t| t.fn_fields.get_mut(at as usize - 1)) else { return false };
        let before = fields.len();
        fields.retain(|(n, _)| !named(n, name));
        fields.len() < before
    }

    /* frees the field place of the function at slot `idx` a sweep frees */
    pub(crate) fn drop_fn_fields(&mut self, idx: u32) {
        let at = core::mem::take(&mut self.slots[idx as usize].chars);
        let Some(t) = self.objects.as_deref_mut().filter(|_| at != 0) else { return };
        t.fn_fields[at as usize - 1] = Vec::new();
        t.free_fn_fields.push(at - 1);
    }

    /* puts back the fields a restore read beside the functions that hold them */
    pub(crate) fn restore_fn_fields(&mut self, fields: FnFields) {
        for (i, list) in fields { for (n, v) in list { self.set_fn_field(Val::heap(i), &n, v); } }
    }

    /* puts back the argument tuples a restore read beside the instances that hold them */
    pub(crate) fn restore_args(&mut self, args: Vec<(u32, Val)>) -> bool {
        for (i, t) in args {
            if !matches!(self.try_get(t), Some(HeapObj::Tuple(_))) { return false; }
            self.hold_args(Val::heap(i), t);
        }
        true
    }

    /* the name of the class or type instance `v` was made of */
    #[inline]
    pub fn instance_type(&self, v: Val) -> Option<&str> { self.class_name(self.class_of(v)?) }

    /* the heads a snapshot writes beside the objects naming them */
    pub(crate) fn heads(&self) -> &[Head] { &self.heads }

    /* puts the heads a restore read in place with every other place free */
    pub(crate) fn restore_heads(&mut self, heads: Vec<Head>) {
        self.heads = heads;
        if let Some(t) = self.objects.as_deref_mut() { t.free_heads.clear(); }
    }

    /* whether a restored object names a head it read with bases that are classes */
    pub(crate) fn well_formed(&self, obj: &HeapObj) -> bool {
        let HeapObj::Object { meta, .. } = obj else { return true };
        match meta.head() {
            Some(at) => self.heads.get(at).is_some_and(|h| h.bases.iter().all(|&b| self.is_classlike(b)) && (meta.is_class_meta() || h.bases.is_empty())),
            // an instance was made of a class or a type
            None => self.is_classlike(*meta),
        }
    }
}

/* whether texts `a` and `b` are equal looking at their place first */
#[inline(always)]
fn same(a: &str, b: &str) -> bool { core::ptr::eq(a, b) || a == b }

/* the name of the type object at slot `i` off the slots and heads alone */
#[inline]
fn type_name_at<'h>(slots: &'h [super::HeapSlot], heads: &'h [Head], i: u32) -> Option<&'h str> {
    match slots.get(i as usize)?.obj.as_ref()? {
        HeapObj::Object { meta, .. } if meta.is_type_meta() => heads.get(meta.head()?).map(|h| &*h.name),
        _ => None,
    }
}

/* the meta a restore gives a class or type object whose head it read */
pub(crate) fn restored_meta(heads: &mut Vec<Head>, class: bool, head: Head) -> Val {
    heads.push(head);
    meta_at(class, heads.len() - 1)
}

