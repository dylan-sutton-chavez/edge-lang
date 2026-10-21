fn main() {
    let n = 200000usize;
    let mut sieve = vec![true; n + 1];
    sieve[0] = false;
    sieve[1] = false;
    let mut i = 2;
    while i * i <= n {
        if sieve[i] {
            for j in (i * i..=n).step_by(i) {
                sieve[j] = false;
            }
        }
        i += 1;
    }
    let primes: Vec<i64> = (0..=n).filter(|&k| sieve[k]).map(|k| k as i64).collect();
    let gaps: Vec<i64> = (0..primes.len() - 1).map(|k| primes[k + 1] - primes[k]).collect();
    println!("{} {} {}", primes.len(), primes.iter().sum::<i64>() % 1000000007, gaps.iter().max().unwrap());
}
