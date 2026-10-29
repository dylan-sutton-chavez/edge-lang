use crate::value::{Val, HeapObj, HeapPool, VmErr};
use crate::syntax::chunk::{SSAChunk, Value};

use alloc::{boxed::Box, rc::Rc, vec::Vec};

use super::{lower::Code, sites::Site};
use crate::r#unsafe::lent;

/* what one chunk keeps between its calls */
pub struct CachePool {
    /* the chunk's lowered code which every frame running it shares */
    pub code: Option<Rc<Code>>,
    /* the chunk's constants as values made once and kept for good */
    pub consts: Option<Box<[Val]>>,
    /* set once a frame ran the chunk so its next call lowers it */
    pub ran: bool,
    /* back-edges and resumes its unlowered frames took and a resume past `HOT_LOOP` lowers it */
    pub heat: u32,
    /* each level's window as `w0` for a run and `wins` for runs inside it */
    pub w0: super::window::Win,
    pub wins: Vec<super::window::Win>,
    /* the innermost run's window start and length with `wcode` the code it is laid for */
    pub window: usize,
    pub wlen: usize,
    /* how many runs of it are going with all but the innermost kept aside */
    pub active: u32,
    pub wcode: usize,
    /* whether its code may lay callees as sections and `calls` past `TIER` lowers it again */
    pub laid: bool,
    /* whether its next lowering is whole with no other frame running its code */
    pub whole: bool,
    pub calls: u32,
    /* how long its laid code was when last relaid for other functions its calls met */
    pub relaid: u32,
    /* what its code holds as the bits `shape` gives and 0 until read */
    pub shape: u8,
    /* the function whose body it runs or `NO_FN` for a module's or a class's */
    pub func: u32,
    /* whether its code is the compiled body kept from its first call until a lowering */
    pub cold: bool,
    // its chunk's deepest stack once found with none inside when only the run knows
    #[cfg(debug_assertions)]
    pub depth: Option<Option<u16>>,
}

pub(crate) const NO_FN: u32 = u32::MAX;

impl Default for CachePool {
    fn default() -> Self {
        Self { code: None, consts: None, ran: false, heat: 0, w0: super::window::Win { base: super::window::NO_WINDOW, len: 0, code: 0, owner: Val::undef() }, wins: Vec::new(), window: super::window::NO_WINDOW, wlen: 0, active: 0, wcode: 0, laid: false, whole: false, calls: 0, relaid: 0, shape: 0, func: NO_FN, cold: false, #[cfg(debug_assertions)] depth: None }
    }
}

/* a chunk's constants as values with each string interned */
pub(crate) fn const_vals(chunk: &SSAChunk, heap: &mut HeapPool) -> Result<Box<[Val]>, VmErr> {
    let mut out = Vec::with_capacity(chunk.constants.len());
    for c in &chunk.constants {
        out.push(match c {
            // a wide literal that fits inline demotes so hash and eq match the short form
            Value::Int(i) => heap.int(*i as i128)?,
            Value::Wide { neg, base, digits } => heap.wide_digits(*neg, digits, u32::from(*base))?,
            Value::Float(f) => Val::float(*f),
            Value::Bool(b) => Val::bool(*b),
            Value::None => Val::none(),
            // texts and bytes sharing one hash stop once the probes they paid pass the budget
            Value::Str(s) => { let v = heap.intern_str(s)?; heap.within_budget()?; v }
            Value::Bytes(b) => { let v = heap.alloc(HeapObj::Bytes(b.clone()))?; heap.within_budget()?; v }
        });
    }
    Ok(out.into_boxed_slice())
}

pub struct OpcodeCache {
    len: usize,
    /* attribute and operator sites made once one is learned and shared by every frame */
    sites: core::cell::OnceCell<Box<[core::cell::Cell<Site>]>>,
}

impl OpcodeCache {
    pub fn new(len: usize) -> Self { Self { len, sites: core::cell::OnceCell::new() } }

    /* the site at `ip` */
    #[inline]
    pub(crate) fn site(&self, ip: usize) -> Site { self.sites.get().and_then(|s| s.get(ip)).map(core::cell::Cell::get).unwrap_or_default() }

    /* `f` of the site at `ip` in place to spare hot paths a large copy */
    #[inline(always)]
    pub(crate) fn with_site<R>(&self, ip: usize, f: impl FnOnce(&Site) -> R) -> R {
        match self.sites.get().and_then(|s| s.get(ip)) {
            // the reference lives only inside `f` and no site is set while a reader runs
            Some(cell) => lent(cell, f),
            None => f(&Site::Empty),
        }
    }

    pub(crate) fn set_site(&self, ip: usize, site: Site) {
        let sites = self.sites.get_or_init(|| (0..self.len).map(|_| core::cell::Cell::new(Site::Empty)).collect());
        if let Some(s) = sites.get(ip) { s.set(site); }
    }

    /* a cache of `len` sites keeping this one's for code grown past its end */
    pub(crate) fn extended(&self, len: usize) -> Self {
        let out = Self::new(len);
        if let Some(sites) = self.sites.get() { for (k, s) in sites.iter().enumerate() { if !matches!(s.get(), Site::Empty) { out.set_site(k, s.get()); } } }
        out
    }

    pub(crate) fn site_roots(&self) -> impl Iterator<Item = Val> + '_ { self.sites.get().into_iter().flatten().flat_map(|s| s.get().roots()) }
}
