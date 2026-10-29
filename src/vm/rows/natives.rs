use crate::lang::{Args, Cx, NativeId, Out};
use crate::syntax::chunk::{SSAChunk, SITE_KW1, SITE_PINNED, SITE_TABLE, SITE_VOID, native_site, site_in_place};
use super::super::steps::{no_native, plain_line};
use super::super::VM;
use super::super::lower;
use super::super::lower::Code;
use crate::code::Ins;
use crate::value::*;

// the most positionals a native takes off its rows with no copy to the stack
const FEW: usize = 4;

impl VM<'_> {
    /* native call `i` over rows its positionals read in place giving the words it took */
    #[inline(never)]
    pub(crate) fn native_rows(&mut self, i: usize, code: &Code, chunk: &SSAChunk) -> Result<usize, VmErr> {
        let s0 = code.rows as usize;
        let w = code.ins[i];
        self.native_at(i, s0, w.a, w, code, chunk)
    }

    /* `native_rows` of site `a` over the rows from `s0` with its spread kept in `spread` */
    #[inline(always)]
    pub(crate) fn native_at(&mut self, i: usize, s0: usize, a: u16, spread: Ins, code: &Code, chunk: &SSAChunk) -> Result<usize, VmErr> {
        let sp = self.top as usize - s0;
        self.pending.call_ip = Some(code.orig(i));
        // a native in place no binding redirects takes its few bare positionals where they lie
        let (id, pos) = site_in_place(a);
        if a & (SITE_TABLE | SITE_KW1) == 0 && pos <= FEW && (a & SITE_PINNED != 0 || !self.native_rebound(id))
            && let Some(n) = self.natives.get(NativeId(id)) && n.sig.fits(pos) {
            let at = self.base + s0 + sp - pos;
            // the arm already asked the heap op and a plain output writes rows in place
            let units = match n.fast_out {
                Some(f) if self.plain(&self.regs[at..at + pos]) => plain_line(f, &self.heap, &self.regs[at..at + pos], &mut self.out_line),
                _ => None,
            };
            let out = match units {
                Some(units) => { self.wrote(units)?; Out::Val(Val::none()) }
                None => {
                    let mut few = [Val::undef(); FEW];
                    match self.regs[at..at + pos] {
                        [] => {}
                        [x] => few[0] = x,
                        [x, y] => (few[0], few[1]) = (x, y),
                        [x, y, z] => (few[0], few[1], few[2]) = (x, y, z),
                        [x, y, z, w, ..] => few = [x, y, z, w],
                    }
                    (n.run)(&mut Cx::new(self), Args { recv: Val::undef(), pos: &few[..pos], kw: &[], named: &[] })?
                }
            };
            let void = a & SITE_VOID != 0;
            let Out::Val(v) = out else { return self.native_stepped(out, n.steps(), s0 + sp - pos).map(|()| 1) };
            self.pay_work()?;
            // a statement's value leaves no row and one a PopTop drops skips it too
            if void { self.top = (s0 + sp - pos) as u32; return Ok(1); }
            if lower::drops(code.ins.get(i + 1)) { self.top = (s0 + sp - pos) as u32; return Ok(2); }
            self.regs[at] = v;
            self.top = (s0 + sp - pos + 1) as u32;
            return Ok(1);
        }
        let site = native_site(a, &chunk.native_sites).ok_or_else(no_native)?;
        // anything else runs as the stack form over the rows
        if site.spread { self.spread_for(code, spread, i, chunk); }
        let module = code.module_at(i);
        let ran = if site.void { self.native_op_void(site, i, module, chunk) } else { self.native_op(site, module, chunk) };
        (self.pending_exec_safe, self.job_ok) = (false, false);
        ran.map(|()| 1)
    }

    /* runs the step a native over rows gave with its value due at row `below` */
    #[cold]
    #[inline(never)]
    fn native_stepped(&mut self, out: Out, ok: bool, below: usize) -> Result<(), VmErr> {
        self.top = below as u32;
        self.pending_exec_safe = false;
        self.out_pushed(out, ok)
    }
}
