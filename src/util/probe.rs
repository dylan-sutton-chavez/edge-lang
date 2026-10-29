use alloc::boxed::Box;
use alloc::vec;

/* the entry each place of an insertion ordered table holds found by probing */
#[derive(Clone, Debug, Default)]
pub struct Places {
    at: Box<[u32]>,
}

// a place no entry ever took and a place whose entry was removed
const NEVER: u32 = u32::MAX;
const GONE: u32 = u32::MAX - 1;

/* the probes a lookup takes free past which each one pays the meter */
pub const FREE_PROBES: usize = 32;

impl Places {
    /* places for `n` entries */
    pub fn with_capacity(n: usize) -> Self {
        if n == 0 { return Self::default(); }
        Self { at: vec![NEVER; places_for(n)].into_boxed_slice() }
    }

    /* the entries the places hold before the table must be built again larger */
    #[inline]
    pub fn room(&self) -> usize { self.at.len() - self.at.len() / 8 }

    /* empties every place and keeps their room */
    pub fn clear(&mut self) { self.at.fill(NEVER); }

    /* empties every place keeping the table when `n` entries fit it */
    pub fn reset(&mut self, n: usize) {
        if self.room() >= n { return self.clear(); }
        // the old places go first so the new ones are not built beside them
        *self = Self::default();
        *self = Self::with_capacity(n);
    }

    /* the entry `hit` takes under hash `h` with probes past the free ones paid */
    #[inline]
    pub fn find(&self, h: u64, mut hit: impl FnMut(usize) -> bool, pay: impl FnOnce(usize)) -> Option<usize> {
        let len = self.at.len();
        if !len.is_power_of_two() { return None; }
        let mask = len - 1;
        let (mut at, mut step) = (h as usize & mask, 0);
        let found = loop {
            let e = self.at[at];
            if e < GONE { if hit(e as usize) { break Some(e as usize); } }
            else if e == NEVER { break None; }
            step += 1;
            if step > mask { break None; }
            at = (at + step) & mask;
        };
        if step > FREE_PROBES { pay(step - FREE_PROBES); }
        found
    }

    /* the entry `find` takes or else the place a new entry under `h` goes */
    #[inline]
    pub fn place(&self, h: u64, mut hit: impl FnMut(usize) -> bool, pay: impl FnOnce(usize)) -> Result<usize, usize> {
        let mask = self.at.len().wrapping_sub(1);
        let (mut at, mut step) = (h as usize & mask, 0);
        let found = loop {
            match self.at.get(at) {
                Some(&e) if e < GONE => if hit(e as usize) { break Ok(e as usize); },
                Some(&GONE) => {}
                _ => break Err(at),
            }
            step += 1;
            if step > mask { break Err(usize::MAX); }
            at = (at + step) & mask;
        };
        if step > FREE_PROBES { pay(step - FREE_PROBES); }
        found
    }

    /* the entry `find` takes with its place left as one a removed entry held */
    #[inline]
    pub fn take(&mut self, h: u64, mut hit: impl FnMut(usize) -> bool, pay: impl FnOnce(usize)) -> Option<usize> {
        let mask = self.at.len().wrapping_sub(1);
        let (mut at, mut step) = (h as usize & mask, 0);
        let found = loop {
            match self.at.get(at) {
                Some(&e) if e < GONE => if hit(e as usize) { break Some(e as usize); },
                Some(&GONE) => {}
                _ => break None,
            }
            step += 1;
            if step > mask { break None; }
            at = (at + step) & mask;
        };
        if step > FREE_PROBES { pay(step - FREE_PROBES); }
        if found.is_some() { self.at[at] = GONE; }
        found
    }

    /* puts entry `i` in the place `place` ended at */
    #[inline]
    pub fn put(&mut self, at: usize, i: usize) { self.at[at] = i as u32; }

    /* puts entry `i` in the first place never taken along the probes of `h` */
    #[inline]
    pub fn push(&mut self, h: u64, i: usize) {
        let mask = self.at.len().wrapping_sub(1);
        let (mut at, mut step) = (h as usize & mask, 0);
        while let Some(&e) = self.at.get(at) {
            if e == NEVER { self.at[at] = i as u32; return; }
            step += 1;
            if step > mask { return; }
            at = (at + step) & mask;
        }
    }
}

/* the power of two places that hold `n` entries at seven eighths full or less */
fn places_for(n: usize) -> usize { n.saturating_mul(8).div_ceil(7).next_power_of_two().max(16) }

/* heap slots an intern table finds by the hash of what they hold */
#[derive(Clone, Debug, Default)]
pub struct Interns {
    // each place holds the low hash bits of its slot beside the slot
    at: Box<[(u32, u32)]>,
    live: usize,
    // places a slot took since the last build removed ones included
    used: usize,
}

impl Interns {
    pub fn len(&self) -> usize { self.live }
    pub fn is_empty(&self) -> bool { self.live == 0 }

    /* empties every place and keeps their room */
    pub fn clear(&mut self) {
        if self.used != 0 { self.at.fill((0, NEVER)); }
        (self.live, self.used) = (0, 0);
    }

    /* the slot under hash `h` that `hit` takes with probes past the free ones paid */
    #[inline]
    pub fn find(&self, h: u64, mut hit: impl FnMut(u32) -> bool, pay: impl FnOnce(usize)) -> Option<u32> {
        let mask = self.at.len().wrapping_sub(1);
        let (tag, mut at, mut step) = (h as u32, h as usize & mask, 0);
        let found = loop {
            match self.at.get(at) {
                Some(&(t, v)) if v < GONE => if t == tag && hit(v) { break Some(v); },
                Some(&(_, GONE)) => {}
                _ => break None,
            }
            step += 1;
            if step > mask { break None; }
            at = (at + step) & mask;
        };
        if step > FREE_PROBES { pay(step - FREE_PROBES); }
        found
    }

    /* puts slot `v` the table lacks under hash `h` paying long probes */
    pub fn insert(&mut self, h: u64, v: u32, pay: impl FnOnce(usize)) {
        let mut owed = 0;
        if self.used >= self.at.len() - self.at.len() / 8 { owed = self.rebuild(); }
        owed += self.put(h as u32, v).saturating_sub(FREE_PROBES);
        if owed != 0 { pay(owed); }
    }

    /* drops slot `v` stored under hash `h` and tells whether it was there */
    pub fn remove(&mut self, h: u64, v: u32) -> bool {
        let mask = self.at.len().wrapping_sub(1);
        let (mut at, mut step) = (h as usize & mask, 0);
        while let Some(e) = self.at.get_mut(at) {
            if e.1 == NEVER { return false; }
            if e.1 == v { (*e, self.live) = ((0, GONE), self.live - 1); return true; }
            step += 1;
            if step > mask { return false; }
            at = (at + step) & mask;
        }
        false
    }

    /* drops every slot `keep` refuses */
    pub fn retain(&mut self, mut keep: impl FnMut(u32) -> bool) {
        for e in self.at.iter_mut() {
            if e.1 < GONE && !keep(e.1) { (*e, self.live) = ((0, GONE), self.live - 1); }
        }
    }

    /* puts `v` at the first free place `tag` probes and counts the probes */
    fn put(&mut self, tag: u32, v: u32) -> usize {
        let mask = self.at.len() - 1;
        let (mut at, mut step) = (tag as usize & mask, 0);
        loop {
            let e = &mut self.at[at];
            if e.1 >= GONE {
                if e.1 == NEVER { self.used += 1; }
                (*e, self.live) = ((tag, v), self.live + 1);
                return step;
            }
            step += 1;
            at = (at + step) & mask;
        }
    }

    /* builds the places again doubling once half hold slots and gives the probes owed */
    #[cold]
    #[inline(never)]
    fn rebuild(&mut self) -> usize {
        let len = self.at.len();
        let grow = (self.live + 1) * 2 > len - len / 8;
        let old = core::mem::replace(&mut self.at, vec![(0, NEVER); if grow { (len * 2).max(4) } else { len }].into_boxed_slice());
        (self.live, self.used) = (0, 0);
        old.iter().filter(|e| e.1 < GONE).map(|&(t, v)| self.put(t, v).saturating_sub(FREE_PROBES)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    // hashes that spread mixed with hashes sharing their low bits or all their bits
    fn hash(i: usize) -> u64 {
        match i % 3 {
            0 => (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15),
            1 => (i as u64) << 40,
            _ => 0x5bd1_e995 ^ ((i as u64 % 7) << 57),
        }
    }

    // inserts as a table does and builds a larger one past its room
    fn insert(p: &mut Places, hashes: &[u64], alive: impl Fn(usize) -> bool, i: usize) {
        if i + 1 > p.room() {
            p.reset(i + 1);
            for (j, &h) in hashes.iter().enumerate().take(i + 1) { if alive(j) { p.push(h, j); } }
        } else {
            p.push(hashes[i], i);
        }
    }

    #[test]
    fn entries_are_found_freed_and_found_again_as_a_map_finds_them() {
        let hashes: Vec<u64> = (0..4200).map(hash).collect();
        let mut p = Places::default();
        let mut live = BTreeMap::new();
        let mut next = 0;
        for round in 0..6usize {
            for _ in 0..700 {
                live.insert(next, ());
                insert(&mut p, &hashes, |j| live.contains_key(&j), next);
                next += 1;
            }
            for i in (round * 300..next).step_by(3) {
                if live.remove(&i).is_some() { assert_eq!(p.take(hashes[i], |e| e == i, |_| {}), Some(i)); }
            }
            for (i, &h) in hashes.iter().enumerate().take(next) {
                let got = p.find(h, |e| e == i, |_| {});
                assert_eq!(got.is_some(), live.contains_key(&i), "entry {i} in round {round}");
            }
        }
    }

    #[test]
    fn a_place_found_by_a_miss_takes_the_entry_the_walk_would() {
        let hashes: Vec<u64> = (0..300).map(hash).collect();
        let mut p = Places::default();
        for i in 0..300 {
            if i + 1 > p.room() { insert(&mut p, &hashes, |_| true, i); continue; }
            let at = p.place(hashes[i], |e| e == i, |_| {}).unwrap_err();
            p.put(at, i);
            assert_eq!(p.find(hashes[i], |e| e == i, |_| {}), Some(i));
        }
        for (i, &h) in hashes.iter().enumerate() { assert_eq!(p.place(h, |e| e == i, |_| {}), Ok(i)); }
    }

    #[test]
    fn keys_sharing_one_hash_pay_for_each_probe_past_the_free_ones() {
        let all = [42u64; 300];
        let mut p = Places::default();
        for e in 0..300 { insert(&mut p, &all, |_| true, e); }
        let mut paid = 0;
        for e in 0..300 { assert_eq!(p.find(42, |x| x == e, |n| paid += n), Some(e)); }
        // three hundred lookups along one chain pay for about thirty five thousand places
        assert!(paid > 30_000, "{paid}");
        let spread: Vec<u64> = (0..1000).map(|e| (e as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)).collect();
        let mut q = Places::default();
        for e in 0..1000 { insert(&mut q, &spread, |_| true, e); }
        let mut free = 0;
        for (e, &h) in spread.iter().enumerate() { assert_eq!(q.find(h, |x| x == e, |n| free += n), Some(e)); }
        assert_eq!(free, 0);
    }

    #[test]
    fn interned_slots_are_found_dropped_and_kept_through_every_build() {
        let mut t = Interns::default();
        let mut live = BTreeMap::new();
        for round in 0..6u32 {
            for v in round * 700..round * 700 + 700 {
                assert_eq!(t.find(hash(v as usize), |x| x == v, |_| {}), None);
                t.insert(hash(v as usize), v, |_| {});
                live.insert(v, ());
            }
            for v in (round * 300..round * 700 + 700).step_by(3) {
                if live.remove(&v).is_some() { assert!(t.remove(hash(v as usize), v)); }
            }
            if round == 4 { t.retain(|v| v % 5 != 0); live.retain(|v, _| v % 5 != 0); }
            assert_eq!(t.len(), live.len());
            for v in 0..round * 700 + 700 {
                assert_eq!(t.find(hash(v as usize), |x| x == v, |_| {}).is_some(), live.contains_key(&v), "slot {v} in round {round}");
            }
        }
        t.clear();
        assert!(t.is_empty() && t.find(hash(7), |_| true, |_| {}).is_none());
    }

    #[test]
    fn interned_slots_sharing_one_hash_pay_for_each_probe_past_the_free_ones() {
        let (mut t, mut paid) = (Interns::default(), 0);
        for v in 0..300 {
            assert_eq!(t.find(42, |x| x == v, |n| paid += n), None);
            t.insert(42, v, |n| paid += n);
        }
        // three hundred misses and puts along one chain pay for about seventy thousand places
        assert!(paid > 60_000, "{paid}");
        let mut free = 0;
        let mut q = Interns::default();
        for v in 0..1000u32 { q.insert((v as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15), v, |n| free += n); }
        for v in 0..1000u32 { assert_eq!(q.find((v as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15), |x| x == v, |n| free += n), Some(v)); }
        assert_eq!(free, 0);
    }
}
