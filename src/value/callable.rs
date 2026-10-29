use super::{ExternFn, HeapObj, HeapPool, Val, VmErr};
use crate::lang::NativeId;

impl HeapPool {
    /* the value of the native at `id` which every read of it shares */
    #[inline]
    pub fn native_value(&mut self, id: NativeId) -> Result<Val, VmErr> { self.alloc(HeapObj::Native(id)) }

    /* `func` with `recv` fixed as its first argument which every such read shares */
    #[inline]
    pub fn bind(&mut self, recv: Val, func: Val) -> Result<Val, VmErr> { self.alloc(HeapObj::Bound { recv, func }) }

    /* the native at `id` bound to `recv` with no slot for the native itself */
    #[inline]
    pub fn bind_native(&mut self, recv: Val, id: NativeId) -> Result<Val, VmErr> { self.bind(recv, Val::native_ref(id)) }

    /* the place of native value `v` and none for any other value */
    #[inline(always)]
    pub fn native_of(&self, v: Val) -> Option<NativeId> { match self.try_get(v)? { &HeapObj::Native(id) => Some(id), _ => None } }

    /* the receiver and the function of bound value `v` */
    #[inline(always)]
    pub fn bound_of(&self, v: Val) -> Option<(Val, Val)> { match self.try_get(v)? { &HeapObj::Bound { recv, func } => Some((recv, func)), _ => None } }

    /* the receiver and the native of a bound native */
    #[inline(always)]
    pub fn bound_native(&self, v: Val) -> Option<(Val, NativeId)> {
        let (recv, func) = self.bound_of(v)?;
        Some((recv, func.as_native_ref()?))
    }

    /* whether `v` is a function or one bound to a receiver */
    #[inline]
    pub fn is_user_call(&self, v: Val) -> bool {
        match self.try_get(v) { Some(HeapObj::Func { .. }) => true, Some(&HeapObj::Bound { func, .. }) => func.is_heap(), _ => false }
    }

    /* the place a host binding takes which the same binding given again shares */
    pub fn host_native(&mut self, f: &ExternFn) -> NativeId { host_place(&mut self.tables_mut().externs, f) }

    /* the value of host binding `f` */
    pub fn host_value(&mut self, f: &ExternFn) -> Result<Val, VmErr> {
        let id = self.host_native(f);
        self.native_value(id)
    }

    /* the host binding the place `id` names */
    #[inline]
    pub fn host_at(&self, id: NativeId) -> Option<&ExternFn> { self.tables()?.externs.get(id.host_at()?) }

    /* the host binding value `v` holds */
    #[inline]
    pub fn host_of(&self, v: Val) -> Option<&ExternFn> { self.host_at(self.native_of(v)?) }
}

/* the place of `f` in `externs` which takes it once */
pub(crate) fn host_place(externs: &mut alloc::vec::Vec<ExternFn>, f: &ExternFn) -> NativeId {
    let at = match externs.iter().position(|e| alloc::sync::Arc::ptr_eq(&e.func, &f.func) && e.name == f.name) {
        Some(at) => at,
        None => { externs.push(f.clone()); externs.len() - 1 }
    };
    NativeId::host(at)
}
