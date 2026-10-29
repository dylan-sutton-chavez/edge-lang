const MOVE: usize = 2;
const CASE: usize = 1;
const LONGEST: usize = 40;

fn substitution(a: u8, b: u8) -> usize {
    if a == b { 0 } else if a.eq_ignore_ascii_case(&b) { CASE } else { MOVE }
}

/* the distance from `a` to `b` capped at `max + 1` reusing `row` across calls */
fn distance(a: &[u8], b: &[u8], max: usize, row: &mut alloc::vec::Vec<usize>) -> usize {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[pre..], &b[pre..]);
    let post = a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count();
    let (a, b) = (&a[..a.len() - post], &b[..b.len() - post]);
    if a.is_empty() || b.is_empty() { return (a.len() + b.len()) * MOVE; }
    if a.len() > LONGEST || b.len() > LONGEST { return max + 1; }
    row.clear();
    row.extend((1..=b.len()).map(|i| i * MOVE));
    let mut result = 0;
    for (i, &ca) in a.iter().enumerate() {
        let (mut diag, mut left) = (i * MOVE, (i + 1) * MOVE);
        result = left;
        let mut least = usize::MAX;
        for (j, &cb) in b.iter().enumerate() {
            let sub = diag + substitution(ca, cb);
            diag = row[j];
            result = sub.min(left + MOVE).min(diag + MOVE);
            row[j] = result;
            left = result;
            least = least.min(result);
        }
        if least > max { return max + 1; }
    }
    result
}

/* the candidate nearest to `name` or None when every one is too far */
pub fn nearest<'c>(name: &str, candidates: impl IntoIterator<Item = &'c str>) -> Option<&'c str> {
    let mut best: Option<(&str, usize)> = None;
    let mut row = alloc::vec::Vec::new();
    for c in candidates {
        if c == name { continue; }
        let mut max = (name.len() + c.len() + 3) * MOVE / 6;
        if let Some((_, d)) = best { max = max.min(d.saturating_sub(1)); }
        // each extra character costs a move so a length gap past `max` is never near
        if name.len().abs_diff(c.len()) * MOVE > max { continue; }
        let d = distance(name.as_bytes(), c.as_bytes(), max, &mut row);
        if d <= max && best.is_none_or(|(_, b)| d < b) { best = Some((c, d)); }
    }
    best.map(|(c, _)| c)
}

/* the `Did you mean` suffix for the nearest candidate or empty when none is near */
pub fn did_you_mean<'c>(name: &str, candidates: impl IntoIterator<Item = &'c str>) -> alloc::string::String {
    match nearest(name, candidates) { Some(c) => crate::s!(". Did you mean '", str c, "'?"), None => alloc::string::String::new() }
}
