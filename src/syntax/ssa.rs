use crate::value::hash_key;

use alloc::{boxed::Box, vec::Vec};

/* an open loop with `start` its continue target and `cleanup_base` the finally depth at entry */
pub(crate) struct LoopCtx {
    pub(super) start: u16,
    pub(super) breaks: Vec<usize>,
    pub(super) is_for: bool,
    pub(super) cleanup_base: usize,
}

// trail positions where the block of a join opened and where its last branch began
pub(crate) struct JoinNode {
    pub(super) start: usize,
    pub(super) mid: Option<usize>,
}

/* the current version of each name a chunk binds with its text allocated once */
#[derive(Default, Clone)]
pub(crate) struct Versions {
    index: hashbrown::HashTable<u32>,
    entries: Vec<(Box<str>, u32)>,
}

impl Versions {
    fn find(&self, name: &str, hash: u64) -> Option<usize> {
        self.index.find(hash, |&i| &*self.entries[i as usize].0 == name).map(|&i| i as usize)
    }

    pub(super) fn get(&self, name: &str) -> Option<u32> {
        self.find(name, hash_key(name)).map(|i| self.entries[i].1)
    }

    pub(super) fn name(&self, slot: u32) -> &str { &self.entries[slot as usize].0 }

    pub(super) fn at(&self, slot: u32) -> u32 { self.entries[slot as usize].1 }

    pub(super) fn set_at(&mut self, slot: u32, ver: u32) { self.entries[slot as usize].1 = ver; }

    pub(super) fn contains(&self, name: &str) -> bool { self.get(name).is_some() }

    /* returns the slot and the replaced version which is zero for a new name */
    pub(super) fn set(&mut self, name: &str, ver: u32) -> (u32, u32) {
        let hash = hash_key(name);
        if let Some(i) = self.find(name, hash) { return (i as u32, core::mem::replace(&mut self.entries[i].1, ver)); }
        let i = self.entries.len() as u32;
        self.entries.push((name.into(), ver));
        let entries = &self.entries;
        self.index.insert_unique(hash, i, |&j| hash_key(&entries[j as usize].0));
        (i, 0)
    }

    /* moves `name` one version up returning its slot and the version it had */
    pub(super) fn bump(&mut self, name: &str) -> (u32, u32) {
        match self.find(name, hash_key(name)) {
            Some(i) => { self.entries[i].1 += 1; (i as u32, self.entries[i].1 - 1) }
            None => self.set(name, 1),
        }
    }

    /* how many names it holds which `truncate` cuts back to */
    pub(super) fn len(&self) -> usize { self.entries.len() }

    /* drops every name past the first `len` from the entries and the index */
    pub(super) fn truncate(&mut self, len: usize) {
        for i in len..self.entries.len() {
            if let Ok(e) = self.index.find_entry(hash_key(&self.entries[i].0), |&j| j as usize == i) { e.remove(); }
        }
        self.entries.truncate(len);
    }
}

/* synthetic temps of desugarings whose leading `#` keeps them out of module attributes */
pub const SSA_TMP_CMP: &str = "#cmp";
pub const SSA_TMP_MATCH: &str = "#match";
pub const SSA_TMP_MATCH_ITEM: &str = "#match_item";

// the parameter a comprehension receives its iterator through
pub const COMP_ARG: &str = "#arg";
// a base count meaning `class B(*bases)` gathered the bases in one list
pub const SPREAD_BASES: u16 = u16::MAX;

// a parameter name without the `*` `**` or `~` before it or the `=` after
pub fn param_base_name(p: &str) -> &str {
    // the markers are ASCII so cutting at bytes keeps char boundaries
    let b = p.as_bytes();
    let (mut s, mut e) = (0, b.len());
    while s < e && matches!(b[s], b'*' | b'~') { s += 1; }
    while e > s && b[e - 1] == b'=' { e -= 1; }
    &p[s..e]
}

/* a `<bare>_<digits>` SSA name split by one routine instead of inline at each call site */
pub struct SsaName<'a> {
    pub bare: &'a str,
    pub version: u32,
}

impl<'a> SsaName<'a> {
    // `None` for synthetic temps and any name not ending in `_<digits>`
    pub fn parse(name: &'a str) -> Option<Self> {
        // reads the trailing digits back to the `_` before them in one pass
        let b = name.as_bytes();
        let mut pos = b.len();
        while pos > 0 && b[pos - 1].is_ascii_digit() { pos -= 1; }
        if pos == b.len() || pos == 0 || b[pos - 1] != b'_' { return None; }
        let mut version: u32 = 0;
        for &d in &b[pos..] { version = version.checked_mul(10)?.checked_add((d - b'0') as u32)?; }
        Some(Self { bare: &name[..pos - 1], version })
    }

    // the bare name and version of any name with zero when it has no suffix
    pub fn parse_or_bare(name: &'a str) -> (&'a str, u32) {
        Self::parse(name)
            .map(|s| (s.bare, s.version))
            .unwrap_or((name, 0))
    }
}

/* strips the `_<digits>` suffix for diagnostics and keeps a name without one */
pub fn ssa_strip(name: &str) -> &str {
    SsaName::parse(name).map(|s| s.bare).unwrap_or(name)
}
