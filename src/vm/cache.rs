use super::types::{Val, HeapObj, HeapPool, VmErr};
use crate::parser::{SSAChunk, Value};

use alloc::{boxed::Box, rc::Rc, vec::Vec};

use super::{lower::Code, sites::Site};

/* What one chunk keeps between its calls. */
#[derive(Default)]
pub struct CachePool {
    /* The chunk's lowered code, which every frame running it shares. */
    pub code: Option<Rc<Code>>,
    /* The chunk's constants as values, made once and kept for good. */
    pub consts: Option<Box<[Val]>>,
    /* Set once a frame ran the chunk, its next call lowers it. */
    pub ran: bool,
    /* Back-edges and resumes its unlowered frames took, a resume past `HOT_LOOP` lowers it. */
    pub heat: u32,
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

    pub(crate) fn set_site(&self, ip: usize, site: Site) {
        let sites = self.sites.get_or_init(|| (0..self.len).map(|_| core::cell::Cell::new(Site::Empty)).collect());
        if let Some(s) = sites.get(ip) { s.set(site); }
    }

    pub(crate) fn site_roots(&self) -> impl Iterator<Item = Val> + '_ { self.sites.get().into_iter().flatten().flat_map(|s| s.get().roots()) }
}
