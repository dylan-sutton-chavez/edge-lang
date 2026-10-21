struct Collatz {
    n: i64,
    done: bool,
}

impl Iterator for Collatz {
    type Item = i64;
    fn next(&mut self) -> Option<i64> {
        if self.done {
            return None;
        }
        let v = self.n;
        match v {
            1 => self.done = true,
            v if v % 2 == 0 => self.n = v / 2,
            v => self.n = 3 * v + 1,
        }
        Some(v)
    }
}

fn main() {
    let (mut best, mut total) = (0i64, 0i64);
    for start in 1..20000 {
        let steps = Collatz { n: start, done: false }.count() as i64;
        total += steps;
        if steps > best {
            best = steps;
        }
    }
    println!("{total} {best}");
}
