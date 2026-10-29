use super::VM;
use crate::value::*;

impl<'a> VM<'a> {

    /* one unit plus the heap's counted work for native loops that bypass the back-edge counter */
    #[inline]
    pub(crate) fn charge_step(&mut self) -> Result<(), VmErr> {
        self.charge_steps(1 + self.heap.take_work())
    }

    /* pays the native work counted since the last charge before the next instruction runs */
    #[inline]
    pub(crate) fn pay_work(&mut self) -> Result<(), VmErr> {
        let w = self.heap.take_work();
        if w == 0 { Ok(()) } else { self.charge_steps(w) }
    }

    /* charges `n` units at once for native builtins like sort whose cost scales with input */
    #[inline]
    pub(crate) fn charge_steps(&mut self, n: usize) -> Result<(), VmErr> {
        let n = n as u64;
        if self.budget < n { self.budget = 0; return Err(cold_budget()); }
        self.budget -= n;
        self.heap.set_budget(self.budget);
        Ok(())
    }
}
