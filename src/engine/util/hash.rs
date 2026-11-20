use core::hash::{BuildHasher, Hasher};

/* the FxHash multiplier from rustc-hash */
const K: u64 = 0x517cc1b727220a95;

/* a fixed seed so iteration order stays the same across runs */
const FIXED_SEED: u64 = 0x9e3779b97f4a7c15;

#[derive(Clone, Default)]
pub(crate) struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(buf)).wrapping_mul(K);
        }
    }
    #[inline(always)] fn write_u8(&mut self, i: u8) { self.write_u64(i as u64); }
    #[inline(always)] fn write_u16(&mut self, i: u16) { self.write_u64(i as u64); }
    #[inline(always)] fn write_u32(&mut self, i: u32) { self.write_u64(i as u64); }
    #[inline(always)] fn write_u64(&mut self, i: u64) { self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(K); }
    #[inline(always)] fn write_usize(&mut self, i: usize) { self.write_u64(i as u64); }
    /* rotates the well mixed high bits down since hashbrown buckets by the low ones */
    #[inline(always)] fn finish(&self) -> u64 { self.0.rotate_left(26) }
}

#[derive(Clone)]
pub(crate) struct FxBuildHasher(u64);

impl FxBuildHasher {
    #[inline]
    pub(crate) fn new() -> Self { Self(FIXED_SEED) }
}

impl Default for FxBuildHasher {
    fn default() -> Self { Self::new() }
}

impl BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    #[inline(always)]
    fn build_hasher(&self) -> FxHasher { FxHasher(self.0) }
}

pub(crate) type FxHashMap<K, V> = hashbrown::HashMap<K, V, FxBuildHasher>;
pub(crate) type FxHashSet<T> = hashbrown::HashSet<T, FxBuildHasher>;
