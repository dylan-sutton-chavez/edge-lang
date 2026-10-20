/* The stable sort Python runs, natural runs found and extended by binary insertion, merged by run power with
   galloping, so a comparison that is no total order, or a `__lt__` that prints, sees the calls Python makes. */

use alloc::{boxed::Box, rc::Rc, vec::Vec};
use core::{cell::Cell, future::Future, pin::Pin, task::{Context, Poll, Waker}};

const MIN_GALLOP: usize = 7;
const MAX_MINRUN: usize = 64;

/* A run on the merge stack, where it starts, how long it is and its power. */
#[derive(Clone, Copy)]
struct Run { base: usize, len: usize, power: u32 }

struct State { min_gallop: usize, pending: Vec<Run>, n: usize, mr_e: u32, mr_mask: usize, mr_current: usize }

/* What answers whether the item `x` sorts before the item `y`, at once or once the loop that asked it has the answer. */
pub(crate) trait Less { type E; fn lt(&mut self, x: usize, y: usize) -> impl Future<Output = Result<bool, Self::E>>; }

/* A comparison answered at once. */
struct Now<F>(F);

impl<E, F: FnMut(usize, usize) -> Result<bool, E>> Less for Now<F> {
    type E = E;
    fn lt(&mut self, x: usize, y: usize) -> impl Future<Output = Result<bool, E>> { core::future::ready((self.0)(x, y)) }
}

/* Sorts `a` in place by `lt`, which answers whether the item `x` sorts before the item `y`. */
pub(crate) fn sort<E>(a: &mut [usize], lt: &mut impl FnMut(usize, usize) -> Result<bool, E>) -> Result<(), E> {
    let mut now = Now(lt);
    let mut sorting = core::pin::pin!(run(a, &mut now));
    // Every comparison is answered at once, so one poll runs it to its end.
    loop { if let Poll::Ready(r) = sorting.as_mut().poll(&mut Context::from_waker(Waker::noop())) { return r; } }
}

/* The comparison a held sort asks next and the answer it is given. */
#[derive(Default)]
struct Ask { pair: Cell<(usize, usize)>, answer: Cell<Option<bool>> }

/* A comparison the loop answers, asked once and given back when answered. */
struct Asking(Rc<Ask>);

impl Less for Asking {
    type E = core::convert::Infallible;
    fn lt(&mut self, x: usize, y: usize) -> impl Future<Output = Result<bool, Self::E>> {
        let ask = self.0.clone();
        ask.pair.set((x, y));
        ask.answer.set(None);
        core::future::poll_fn(move |_| match ask.answer.take() { Some(less) => Poll::Ready(Ok(less)), None => Poll::Pending })
    }
}

/* The same sort held between two comparisons, each asked of the loop, a `__lt__` the job runs as a frame. */
pub(crate) struct Held { sorting: Pin<Box<dyn Future<Output = Vec<usize>>>>, ask: Rc<Ask> }

impl Held {
    /* The sort of `order`, the indices of the items in the order they start in. */
    pub(crate) fn new(order: Vec<usize>) -> Self {
        let ask = Rc::new(Ask::default());
        let mut lt = Asking(ask.clone());
        let sorting = Box::pin(async move {
            let mut a = order;
            let Ok(()) = run(&mut a, &mut lt).await;
            a
        });
        Self { sorting, ask }
    }

    /* The two indices the next comparison asks, whether the first sorts before the second. */
    pub(crate) fn pair(&self) -> (usize, usize) { self.ask.pair.get() }

    /* Takes the answer of the comparison `pair` asked. */
    pub(crate) fn take(&mut self, less: bool) { self.ask.answer.set(Some(less)); }

    /* Runs on to the next comparison, or to the sorted indices once the sort is done. */
    pub(crate) fn advance(&mut self) -> Option<Vec<usize>> {
        match self.sorting.as_mut().poll(&mut Context::from_waker(Waker::noop())) { Poll::Ready(order) => Some(order), Poll::Pending => None }
    }
}

/* The sort itself, every comparison through `lt`. */
async fn run<L: Less>(a: &mut [usize], lt: &mut L) -> Result<(), L::E> {
    let n = a.len();
    if n < 2 { return Ok(()); }
    let mut mr_e = 0;
    while n >> mr_e >= MAX_MINRUN { mr_e += 1; }
    let mut ms = State { min_gallop: MIN_GALLOP, pending: Vec::new(), n, mr_e, mr_mask: (1 << mr_e) - 1, mr_current: 0 };
    let (mut lo, mut rem) = (0, n);
    while rem > 0 {
        let mut run = count_run(&mut a[lo..lo + rem], lt).await?;
        ms.mr_current += ms.n;
        let minrun = ms.mr_current >> ms.mr_e;
        ms.mr_current &= ms.mr_mask;
        // A short run grows to the run length by insertion.
        if run < minrun {
            let force = rem.min(minrun);
            binarysort(&mut a[lo..lo + force], run, lt).await?;
            run = force;
        }
        if let Some(&top) = ms.pending.last() {
            let power = powerloop(top.base, top.len, run, ms.n);
            while ms.pending.len() > 1 && ms.pending[ms.pending.len() - 2].power > power { ms.merge_at(a, ms.pending.len() - 2, lt).await?; }
            let last = ms.pending.len() - 1;
            ms.pending[last].power = power;
        }
        ms.pending.push(Run { base: lo, len: run, power: 0 });
        lo += run;
        rem -= run;
    }
    while ms.pending.len() > 1 {
        let mut i = ms.pending.len() - 2;
        if i > 0 && ms.pending[i - 1].len < ms.pending[i + 1].len { i -= 1; }
        ms.merge_at(a, i, lt).await?;
    }
    Ok(())
}

/* Stable binary insertion of `a[ok..]` into the sorted `a[..ok]`. */
async fn binarysort<L: Less>(a: &mut [usize], ok: usize, lt: &mut L) -> Result<(), L::E> {
    for ok in ok.max(1)..a.len() {
        let pivot = a[ok];
        let (mut l, mut r) = (0, ok);
        while l < r {
            let m = (l + r) >> 1;
            if lt.lt(pivot, a[m]).await? { r = m; } else { l = m + 1; }
        }
        a.copy_within(l..ok, l + 1);
        a[l] = pivot;
    }
    Ok(())
}

/* The length of the run `a` starts with, a descending one reversed in place, its equal stretches kept in order. */
async fn count_run<L: Less>(a: &mut [usize], lt: &mut L) -> Result<usize, L::E> {
    let total = a.len();
    let mut n = 1;
    while n < total && !lt.lt(a[n], a[n - 1]).await? { n += 1; }
    if n == total { return Ok(n); }
    // An ascending prefix of more than one item is a run unless every item in it is equal.
    if n > 1 {
        if lt.lt(a[0], a[n - 1]).await? { return Ok(n); }
        a[..n].reverse();
    }
    n += 1;
    let mut neq = 0;
    let reverse_equal = |a: &mut [usize], n: usize, neq: &mut usize| if *neq > 0 { let k = *neq + 1; a[n - k..n].reverse(); *neq = 0; };
    while n < total {
        if lt.lt(a[n], a[n - 1]).await? { reverse_equal(a, n, &mut neq); }
        else if lt.lt(a[n - 1], a[n]).await? { break; }
        else { neq += 1; }
        n += 1;
    }
    reverse_equal(a, n, &mut neq);
    a[..n].reverse();
    // The reversed run may go on ascending.
    while n < total && !lt.lt(a[n], a[n - 1]).await? { n += 1; }
    Ok(n)
}

/* Where `key` goes in the sorted `a`, before any equal item, searching out from `hint`. */
async fn gallop_left<L: Less>(key: usize, a: &[usize], hint: usize, lt: &mut L) -> Result<usize, L::E> {
    let n = a.len() as isize;
    let hint = hint as isize;
    let (mut lastofs, mut ofs): (isize, isize) = (0, 1);
    if lt.lt(a[hint as usize], key).await? {
        let maxofs = n - hint;
        while ofs < maxofs {
            if lt.lt(a[(hint + ofs) as usize], key).await? { lastofs = ofs; ofs = (ofs << 1) + 1; } else { break; }
        }
        if ofs > maxofs { ofs = maxofs; }
        lastofs += hint;
        ofs += hint;
    } else {
        let maxofs = hint + 1;
        while ofs < maxofs {
            if lt.lt(a[(hint - ofs) as usize], key).await? { break; }
            lastofs = ofs;
            ofs = (ofs << 1) + 1;
        }
        if ofs > maxofs { ofs = maxofs; }
        let k = lastofs;
        lastofs = hint - ofs;
        ofs = hint - k;
    }
    lastofs += 1;
    while lastofs < ofs {
        let m = lastofs + ((ofs - lastofs) >> 1);
        if lt.lt(a[m as usize], key).await? { lastofs = m + 1; } else { ofs = m; }
    }
    Ok(ofs as usize)
}

/* Where `key` goes in the sorted `a`, after any equal item, searching out from `hint`. */
async fn gallop_right<L: Less>(key: usize, a: &[usize], hint: usize, lt: &mut L) -> Result<usize, L::E> {
    let n = a.len() as isize;
    let hint = hint as isize;
    let (mut lastofs, mut ofs): (isize, isize) = (0, 1);
    if lt.lt(key, a[hint as usize]).await? {
        let maxofs = hint + 1;
        while ofs < maxofs {
            if lt.lt(key, a[(hint - ofs) as usize]).await? { lastofs = ofs; ofs = (ofs << 1) + 1; } else { break; }
        }
        if ofs > maxofs { ofs = maxofs; }
        let k = lastofs;
        lastofs = hint - ofs;
        ofs = hint - k;
    } else {
        let maxofs = n - hint;
        while ofs < maxofs {
            if lt.lt(key, a[(hint + ofs) as usize]).await? { break; }
            lastofs = ofs;
            ofs = (ofs << 1) + 1;
        }
        if ofs > maxofs { ofs = maxofs; }
        lastofs += hint;
        ofs += hint;
    }
    lastofs += 1;
    while lastofs < ofs {
        let m = lastofs + ((ofs - lastofs) >> 1);
        if lt.lt(key, a[m as usize]).await? { ofs = m; } else { lastofs = m + 1; }
    }
    Ok(ofs as usize)
}

/* The power of the run at `s1` of length `n1` before one of length `n2`, its depth in the merge tree over `n`. */
fn powerloop(s1: usize, n1: usize, n2: usize, n: usize) -> u32 {
    let mut result = 0;
    let mut a = 2 * s1 + n1;
    let mut b = a + n1 + n2;
    loop {
        result += 1;
        if a >= n { a -= n; b -= n; } else if b >= n { break; }
        a <<= 1;
        b <<= 1;
    }
    result
}

impl State {
    /* Merges the runs at stack places `i` and `i + 1`. */
    async fn merge_at<L: Less>(&mut self, a: &mut [usize], i: usize, lt: &mut L) -> Result<(), L::E> {
        let (Run { base: mut sa, len: mut na, .. }, Run { base: sb, len: nb, .. }) = (self.pending[i], self.pending[i + 1]);
        self.pending[i].len = na + nb;
        self.pending.remove(i + 1);
        // Items of the first run already before the second, and of the second already after the first, stay put.
        let k = gallop_right(a[sb], &a[sa..sa + na], 0, lt).await?;
        sa += k;
        na -= k;
        if na == 0 { return Ok(()); }
        let nb = gallop_left(a[sa + na - 1], &a[sb..sb + nb], nb - 1, lt).await?;
        if nb == 0 { return Ok(()); }
        if na <= nb { self.merge_lo(a, sa, na, sb, nb, lt).await } else { self.merge_hi(a, sa, na, sb, nb, lt).await }
    }

    /* Merges from the left, the shorter first run held aside. */
    async fn merge_lo<L: Less>(&mut self, a: &mut [usize], sa: usize, mut na: usize, sb: usize, mut nb: usize, lt: &mut L) -> Result<(), L::E> {
        let tmp: Vec<usize> = a[sa..sa + na].to_vec();
        let (mut dest, mut pa, mut pb) = (sa, 0, sb);
        a[dest] = a[pb]; dest += 1; pb += 1; nb -= 1;
        // `false` ends with the rest of the held run copied back, `true` with the last held item after the second run.
        let copy_b = 'merge: {
            if nb == 0 { break 'merge false; }
            if na == 1 { break 'merge true; }
            let mut min_gallop = self.min_gallop;
            loop {
                let (mut acount, mut bcount) = (0, 0);
                loop {
                    if lt.lt(a[pb], tmp[pa]).await? {
                        a[dest] = a[pb]; dest += 1; pb += 1;
                        bcount += 1; acount = 0; nb -= 1;
                        if nb == 0 { break 'merge false; }
                        if bcount >= min_gallop { break; }
                    } else {
                        a[dest] = tmp[pa]; dest += 1; pa += 1;
                        acount += 1; bcount = 0; na -= 1;
                        if na == 1 { break 'merge true; }
                        if acount >= min_gallop { break; }
                    }
                }
                min_gallop += 1;
                loop {
                    min_gallop -= (min_gallop > 1) as usize;
                    self.min_gallop = min_gallop;
                    let k = gallop_right(a[pb], &tmp[pa..pa + na], 0, lt).await?;
                    acount = k;
                    if k > 0 {
                        a[dest..dest + k].copy_from_slice(&tmp[pa..pa + k]);
                        dest += k; pa += k; na -= k;
                        if na == 1 { break 'merge true; }
                        if na == 0 { break 'merge false; }
                    }
                    a[dest] = a[pb]; dest += 1; pb += 1; nb -= 1;
                    if nb == 0 { break 'merge false; }
                    let k = gallop_left(tmp[pa], &a[pb..pb + nb], 0, lt).await?;
                    bcount = k;
                    if k > 0 {
                        a.copy_within(pb..pb + k, dest);
                        dest += k; pb += k; nb -= k;
                        if nb == 0 { break 'merge false; }
                    }
                    a[dest] = tmp[pa]; dest += 1; pa += 1; na -= 1;
                    if na == 1 { break 'merge true; }
                    if acount < MIN_GALLOP && bcount < MIN_GALLOP { break; }
                }
                min_gallop += 1;
                self.min_gallop = min_gallop;
            }
        };
        if copy_b {
            a.copy_within(pb..pb + nb, dest);
            a[dest + nb] = tmp[pa];
        } else if na > 0 {
            a[dest..dest + na].copy_from_slice(&tmp[pa..pa + na]);
        }
        Ok(())
    }

    /* Merges from the right, the shorter second run held aside. */
    async fn merge_hi<L: Less>(&mut self, a: &mut [usize], sa: usize, mut na: usize, sb: usize, mut nb: usize, lt: &mut L) -> Result<(), L::E> {
        let tmp: Vec<usize> = a[sb..sb + nb].to_vec();
        // Signed places, since the first run may be used up past its start.
        let (mut dest, mut pa, mut pb) = ((sb + nb - 1) as isize, (sa + na - 1) as isize, nb as isize - 1);
        let at = |i: isize| i as usize;
        a[at(dest)] = a[at(pa)]; dest -= 1; pa -= 1; na -= 1;
        // `false` ends with the rest of the held run copied back, `true` with the first held item before the first run.
        let copy_a = 'merge: {
            if na == 0 { break 'merge false; }
            if nb == 1 { break 'merge true; }
            let mut min_gallop = self.min_gallop;
            loop {
                let (mut acount, mut bcount) = (0, 0);
                loop {
                    if lt.lt(tmp[at(pb)], a[at(pa)]).await? {
                        a[at(dest)] = a[at(pa)]; dest -= 1; pa -= 1;
                        acount += 1; bcount = 0; na -= 1;
                        if na == 0 { break 'merge false; }
                        if acount >= min_gallop { break; }
                    } else {
                        a[at(dest)] = tmp[at(pb)]; dest -= 1; pb -= 1;
                        bcount += 1; acount = 0; nb -= 1;
                        if nb == 1 { break 'merge true; }
                        if bcount >= min_gallop { break; }
                    }
                }
                min_gallop += 1;
                loop {
                    min_gallop -= (min_gallop > 1) as usize;
                    self.min_gallop = min_gallop;
                    let k = na - gallop_right(tmp[at(pb)], &a[sa..sa + na], na - 1, lt).await?;
                    acount = k;
                    if k > 0 {
                        dest -= k as isize; pa -= k as isize;
                        a.copy_within(at(pa + 1)..at(pa + 1) + k, at(dest + 1));
                        na -= k;
                        if na == 0 { break 'merge false; }
                    }
                    a[at(dest)] = tmp[at(pb)]; dest -= 1; pb -= 1; nb -= 1;
                    if nb == 1 { break 'merge true; }
                    let k = nb - gallop_left(a[at(pa)], &tmp[..nb], nb - 1, lt).await?;
                    bcount = k;
                    if k > 0 {
                        dest -= k as isize; pb -= k as isize;
                        a[at(dest + 1)..at(dest + 1) + k].copy_from_slice(&tmp[at(pb + 1)..at(pb + 1) + k]);
                        nb -= k;
                        if nb == 1 { break 'merge true; }
                        if nb == 0 { break 'merge false; }
                    }
                    a[at(dest)] = a[at(pa)]; dest -= 1; pa -= 1; na -= 1;
                    if na == 0 { break 'merge false; }
                    if acount < MIN_GALLOP && bcount < MIN_GALLOP { break; }
                }
                min_gallop += 1;
                self.min_gallop = min_gallop;
            }
        };
        if copy_a {
            let n = na as isize;
            a.copy_within(at(pa + 1 - n)..at(pa + 1), at(dest + 1 - n));
            dest -= n;
            a[at(dest)] = tmp[at(pb)];
        } else if nb > 0 {
            let from = at(dest - (nb as isize - 1));
            a[from..from + nb].copy_from_slice(&tmp[..nb]);
        }
        Ok(())
    }
}
