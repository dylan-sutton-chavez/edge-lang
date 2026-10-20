use super::types::{Val, HeapObj, HeapPool, VmErr};
use crate::parser::{SSAChunk, Value};

use alloc::{boxed::Box, rc::Rc, vec::Vec};

use super::{lower::Code, sites::Site};

/* What one chunk keeps between its calls. */
pub struct CachePool {
    /* The chunk's lowered code, which every frame running it shares. */
    pub code: Option<Rc<Code>>,
    /* The chunk's constants as values, made once and kept for good. */
    pub consts: Option<Box<[Val]>>,
    /* Set once a frame ran the chunk, its next call lowers it. */
    pub ran: bool,
    /* Back-edges and resumes its unlowered frames took, a resume past `HOT_LOOP` lowers it. */
    pub heat: u32,
    /* Each level's window, the first for a run, the next ones for runs inside it. */
    pub w0: super::window::Win,
    pub wins: Vec<super::window::Win>,
    /* The innermost run's window in view, where it starts, how long it is and the code it is laid out for. */
    pub window: usize,
    pub wlen: usize,
    /* How many runs of it are going, all but the innermost kept aside. */
    pub active: u32,
    pub wcode: usize,
    /* Whether its code may lay the functions it calls as sections, and its calls so far, past `lower::TIER` of which it is lowered again to lay them. */
    pub laid: bool,
    /* Whether its next lowering is whole, no frame but the one laying it again running its code. */
    pub whole: bool,
    pub calls: u32,
    /* How long its laid code was when last laid again for the other functions its calls met. */
    pub relaid: u32,
    /* What its code holds, `lower::shape`, 0 until read. */
    pub shape: u8,
    /* The function whose body it runs, `NO_FN` for a module's or a class's. */
    pub func: u32,
    /* Whether its code is the body as the compiler wrote it, kept from its first call until a lowering. */
    pub cold: bool,
}

pub(crate) const NO_FN: u32 = u32::MAX;

impl Default for CachePool {
    fn default() -> Self {
        Self { code: None, consts: None, ran: false, heat: 0, w0: super::window::Win { base: super::window::NO_WINDOW, len: 0, code: 0, owner: Val::undef() }, wins: Vec::new(), window: super::window::NO_WINDOW, wlen: 0, active: 0, wcode: 0, laid: false, whole: false, calls: 0, relaid: 0, shape: 0, func: NO_FN, cold: false }
    }
}

/* A chunk's constants as values, each string interned. */
pub(crate) fn const_vals(chunk: &SSAChunk, heap: &mut HeapPool) -> Result<Box<[Val]>, VmErr> {
    let mut out = Vec::with_capacity(chunk.constants.len());
    for c in &chunk.constants {
        out.push(match c {
            // A wide literal that fits inline demotes so hash and eq stay in sync with the short form.
            Value::Int(i) => heap.int(*i as i128)?,
            Value::BigInt(b) => crate::frontend::int::val(heap, b.clone())?,
            Value::Float(f) => Val::float(*f),
            Value::Imag(f) => crate::frontend::complex::val(heap, 0.0, *f)?,
            Value::Bool(b) => Val::bool(*b),
            Value::None => Val::none(),
            Value::Str(s) => heap.intern_str(s)?,
            Value::Bytes(b) => heap.alloc(HeapObj::Bytes(b.clone()))?,
        });
    }
    Ok(out.into_boxed_slice())
}

pub struct OpcodeCache {
    len: usize,
    /* Attribute and operator sites by ip, made on the first one learned and shared by every frame of the code. */
    sites: core::cell::OnceCell<Box<[core::cell::Cell<Site>]>>,
}

impl OpcodeCache {
    pub fn new(len: usize) -> Self { Self { len, sites: core::cell::OnceCell::new() } }

    /* The site at `ip`. */
    #[inline]
    pub(crate) fn site(&self, ip: usize) -> Site { self.sites.get().and_then(|s| s.get(ip)).map(core::cell::Cell::get).unwrap_or_default() }

    /* `f` of the site at `ip`, read in place since a site is large and a hot path needs a field or two. */
    #[inline(always)]
    pub(crate) fn with_site<R>(&self, ip: usize, f: impl FnOnce(&Site) -> R) -> R {
        match self.sites.get().and_then(|s| s.get(ip)) {
            // SAFETY the reference lives only inside `f`, and no site is set while a reader runs, as only `set_site` writes one.
            Some(cell) => f(unsafe { &*cell.as_ptr() }),
            None => f(&Site::Empty),
        }
    }

    pub(crate) fn set_site(&self, ip: usize, site: Site) {
        let sites = self.sites.get_or_init(|| (0..self.len).map(|_| core::cell::Cell::new(Site::Empty)).collect());
        if let Some(s) = sites.get(ip) { s.set(site); }
    }

    /* A cache of `len` sites, the first ones this one's, for code that grew past this one's end. */
    pub(crate) fn extended(&self, len: usize) -> Self {
        let out = Self::new(len);
        if let Some(sites) = self.sites.get() { for (k, s) in sites.iter().enumerate() { if !matches!(s.get(), Site::Empty) { out.set_site(k, s.get()); } } }
        out
    }

    pub(crate) fn site_roots(&self) -> impl Iterator<Item = Val> + '_ { self.sites.get().into_iter().flatten().flat_map(|s| s.get().roots()) }
}
