use core::cell::RefCell;
use alloc::rc::Rc;

use super::super::VM;
use super::super::types::*;

impl<'a> VM<'a> {


    // Build a set Val from items, rejecting unhashable elements first.
    #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
    pub(crate) fn set_from_items(&mut self, items: alloc::vec::Vec<Val>) -> Result<Val, VmErr> {
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; }
        self.alloc_set_result(items, false)
    }

    // Build a frozenset Val from items, rejecting unhashable elements first.
    #[cfg(all(target_arch = "wasm32", feature = "runtime"))]
    pub(crate) fn frozenset_from_items(&mut self, items: alloc::vec::Vec<Val>) -> Result<Val, VmErr> {
        for &v in items.iter().filter(|v| v.is_heap()) { self.require_element(v)?; }
        self.alloc_set_result(items, true)
    }

    pub fn build_set(&mut self, op: u16) -> Result<(), VmErr> {
        let items = self.pop_n(op as usize)?;
        let s = self.valset_of(&items)?;
        let val = self.heap.alloc(HeapObj::Set(Rc::new(RefCell::new(s))))?;
        self.push(val); Ok(())
    }

    #[inline]
    pub fn build_slice(&mut self, op: u16) -> Result<(), VmErr> {
        let step = if op == 3 { self.pop()? } else { Val::none() };
        let stop = self.pop()?;
        let start = self.pop()?;
        let val = self.heap.alloc(HeapObj::Slice(start, stop, step))?;
        self.push(val); Ok(())
    }









}
