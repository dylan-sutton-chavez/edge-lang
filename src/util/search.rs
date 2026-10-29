use core::cmp::max;

// a word of ones in each byte and of the top bit of each byte
const LOW: u64 = 0x0101_0101_0101_0101;
const HIGH: u64 = 0x8080_8080_8080_8080;

/* whether any byte of the word is zero */
#[inline]
const fn zero_byte(w: u64) -> bool { w.wrapping_sub(LOW) & !w & HIGH != 0 }

/* the first place of byte `b` read a word at a time */
#[inline]
pub fn memchr(b: u8, hay: &[u8]) -> Option<usize> {
    // a short haystack walks cheaper byte by byte than a word at a time
    if hay.len() < 16 { return hay.iter().position(|&x| x == b); }
    let wide = LOW.wrapping_mul(u64::from(b));
    let mut at = 0;
    for c in hay.as_chunks::<8>().0 {
        if zero_byte(u64::from_le_bytes(*c) ^ wide) { break; }
        at += 8;
    }
    hay.get(at..).and_then(|rest| rest.iter().position(|&x| x == b)).map(|i| at + i)
}

/* the last place of byte `b` read a word at a time */
#[inline]
pub fn memrchr(b: u8, hay: &[u8]) -> Option<usize> {
    if hay.len() < 16 { return hay.iter().rposition(|&x| x == b); }
    let wide = LOW.wrapping_mul(u64::from(b));
    let mut end = hay.len();
    for c in hay.as_rchunks::<8>().1.iter().rev() {
        if zero_byte(u64::from_le_bytes(*c) ^ wide) { break; }
        end -= 8;
    }
    hay.get(..end).and_then(|head| head.iter().rposition(|&x| x == b))
}

/* whether byte `b` occurs in `hay` */
#[inline]
pub fn contains(hay: &[u8], b: u8) -> bool { memchr(b, hay).is_some() }

/* how many times byte `b` occurs in `hay` counted a word at a time */
#[inline]
pub fn count_byte(hay: &[u8], b: u8) -> usize {
    let wide = LOW.wrapping_mul(u64::from(b));
    let (words, rest) = hay.as_chunks::<8>();
    let mut n = 0;
    for c in words {
        let x = u64::from_le_bytes(*c) ^ wide;
        // only a zero byte keeps its top bit clear once its low bits carry
        let y = !((x & !HIGH).wrapping_add(!HIGH) | x | !HIGH);
        n += y.count_ones() as usize;
    }
    n + rest.iter().filter(|&&x| x == b).count()
}

// a haystack this short is scanned directly where the two way setup would cost more
const SHORT: usize = 32;

/* the first start of a needle of two or more bytes in a short haystack */
#[inline]
fn scan(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    let last = hay.len().checked_sub(needle.len())?;
    (0..=last).find(|&i| hay[i] == first && hay[i + 1..].iter().zip(rest).all(|(a, b)| a == b))
}

/* the last start of a needle of two or more bytes in a short haystack */
#[inline]
fn scan_back(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let (&first, rest) = needle.split_first()?;
    let last = hay.len().checked_sub(needle.len())?;
    (0..=last).rev().find(|&i| hay[i] == first && hay[i + 1..].iter().zip(rest).all(|(a, b)| a == b))
}

/* where `needle` first starts in `hay` in time linear in both */
#[inline]
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    match *needle {
        [] => Some(0),
        [b] => memchr(b, hay),
        _ if hay.len() <= SHORT => scan(hay, needle),
        _ => TwoWay::new(needle, false).front(hay, needle),
    }
}

/* where `needle` last starts in `hay` in time linear in both */
#[inline]
pub fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    match *needle {
        [] => Some(hay.len()),
        [b] => memrchr(b, hay),
        _ if hay.len() <= SHORT => scan_back(hay, needle),
        _ => TwoWay::new(needle, true).back(hay, needle),
    }
}

/* the starts of a nonempty needle without overlap from the front or the back */
pub struct Matches<'a> {
    hay: &'a [u8],
    needle: &'a [u8],
    // the haystack left to search lies between these
    front: usize,
    back: usize,
    from_back: bool,
    // the factorization made the first time a long haystack is searched
    way: Option<TwoWay>,
}

impl<'a> Matches<'a> {
    /* the matches from the front */
    #[inline]
    pub fn new(hay: &'a [u8], needle: &'a [u8]) -> Self { Matches { hay, needle, front: 0, back: hay.len(), from_back: false, way: None } }

    /* the matches from the back */
    #[inline]
    pub fn back(hay: &'a [u8], needle: &'a [u8]) -> Self { Matches { from_back: true, ..Self::new(hay, needle) } }
}

impl Iterator for Matches<'_> {
    type Item = usize;

    #[inline(always)]
    fn next(&mut self) -> Option<usize> {
        let (needle, n) = (self.needle, self.needle.len());
        if n == 0 || self.back - self.front < n { return None; }
        let hay = self.hay.get(self.front..self.back)?;
        let at = match (needle, self.from_back) {
            (&[b], false) => memchr(b, hay),
            (&[b], true) => memrchr(b, hay),
            (_, false) if hay.len() <= SHORT => scan(hay, needle),
            (_, true) if hay.len() <= SHORT => scan_back(hay, needle),
            (_, back) => {
                let way = self.way.get_or_insert_with(|| TwoWay::new(needle, back));
                if back { way.back(hay, needle) } else { way.front(hay, needle) }
            }
        }? + self.front;
        if self.from_back { self.back = at; } else { self.front = at + n; }
        Some(at)
    }
}

/* the critical factorization the two way search of crochemore and perrin walks */
struct TwoWay {
    // where the right half of the factorization starts searching forward and back
    cut: usize,
    cut_back: usize,
    period: usize,
    // the low six bits of each needle byte as a set
    bytes: u64,
    // a needle whose period is long keeps no memory of a partial match
    long: bool,
}

impl TwoWay {
    /* the factorization of `needle` where only a search from the back needs the backward cut */
    #[inline]
    fn new(needle: &[u8], back: bool) -> TwoWay {
        let (less, less_period) = suffix(needle, false);
        let (more, more_period) = suffix(needle, true);
        let (cut, period) = if less > more { (less, less_period) } else { (more, more_period) };
        let set = |part: &[u8]| part.iter().fold(0u64, |a, &b| a | 1 << (b & 0x3f));
        // the left half repeating at the period means the period is exact
        if needle.get(..cut) == needle.get(period..period + cut) {
            let cut_back = if back { needle.len() - max(back_suffix(needle, period, false), back_suffix(needle, period, true)) } else { 0 };
            TwoWay { cut, cut_back, period, bytes: set(&needle[..period]), long: false }
        } else {
            TwoWay { cut, cut_back: cut, period: max(cut, needle.len() - cut) + 1, bytes: set(needle), long: true }
        }
    }

    /* whether byte `b` may occur in the needle */
    #[inline]
    fn holds(&self, b: u8) -> bool { self.bytes >> (b & 0x3f) & 1 != 0 }

    /* the first start of the needle in `hay` */
    #[inline(never)]
    fn front(&self, hay: &[u8], needle: &[u8]) -> Option<usize> {
        let (n, mut pos, mut memory) = (needle.len(), 0usize, 0usize);
        'search: loop {
            let &tail = hay.get(pos + n - 1)?;
            // a last byte the needle never holds moves the window past itself
            if !self.holds(tail) {
                pos += n;
                memory = 0;
                continue 'search;
            }
            let start = if self.long { self.cut } else { max(self.cut, memory) };
            for i in start..n {
                if needle[i] != hay[pos + i] {
                    pos += i - self.cut + 1;
                    memory = 0;
                    continue 'search;
                }
            }
            let low = if self.long { 0 } else { memory };
            for i in (low..self.cut).rev() {
                if needle[i] != hay[pos + i] {
                    pos += self.period;
                    if !self.long { memory = n - self.period; }
                    continue 'search;
                }
            }
            return Some(pos);
        }
    }

    /* the last start of the needle in `hay` */
    #[inline(never)]
    fn back(&self, hay: &[u8], needle: &[u8]) -> Option<usize> {
        let (n, mut end, mut memory) = (needle.len(), hay.len(), needle.len());
        'search: loop {
            let &head = hay.get(end.checked_sub(n)?)?;
            if !self.holds(head) {
                end -= n;
                memory = n;
                continue 'search;
            }
            let crit = if self.long { self.cut_back } else { self.cut_back.min(memory) };
            for i in (0..crit).rev() {
                if needle[i] != hay[end - n + i] {
                    end -= self.cut_back - i;
                    memory = n;
                    continue 'search;
                }
            }
            let high = if self.long { n } else { memory };
            for i in self.cut_back..high {
                if needle[i] != hay[end - n + i] {
                    end -= self.period;
                    if !self.long { memory = self.period; }
                    continue 'search;
                }
            }
            return Some(end - n);
        }
    }
}

/* the start and period of the greatest suffix of `s` in one order */
#[inline]
fn suffix(s: &[u8], greater: bool) -> (usize, usize) {
    let (mut left, mut right, mut offset, mut period) = (0, 1, 0, 1);
    while let Some(&a) = s.get(right + offset) {
        let b = s[left + offset];
        if (a < b && !greater) || (a > b && greater) {
            right += offset + 1;
            offset = 0;
            period = right - left;
        } else if a == b {
            if offset + 1 == period { right += offset + 1; offset = 0; } else { offset += 1; }
        } else {
            left = right;
            right += 1;
            offset = 0;
            period = 1;
        }
    }
    (left, period)
}

/* where the greatest suffix of `s` read backward starts counted from its end */
#[inline]
fn back_suffix(s: &[u8], known: usize, greater: bool) -> usize {
    let (mut left, mut right, mut offset, mut period, n) = (0, 1, 0, 1, s.len());
    while right + offset < n {
        let (a, b) = (s[n - (1 + right + offset)], s[n - (1 + left + offset)]);
        if (a < b && !greater) || (a > b && greater) {
            right += offset + 1;
            offset = 0;
            period = right - left;
        } else if a == b {
            if offset + 1 == period { right += offset + 1; offset = 0; } else { offset += 1; }
        } else {
            left = right;
            right += 1;
            offset = 0;
            period = 1;
        }
        if period == known { break; }
    }
    left
}
