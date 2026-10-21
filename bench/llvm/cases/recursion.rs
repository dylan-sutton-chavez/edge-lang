fn fib(n: i64) -> i64 {
    if n < 2 {
        return n;
    }
    fib(n - 1) + fib(n - 2)
}

fn ack(m: i64, n: i64) -> i64 {
    match (m, n) {
        (0, n) => n + 1,
        (m, 0) => ack(m - 1, 1),
        (m, n) => ack(m - 1, ack(m, n - 1)),
    }
}

fn main() {
    println!("{} {}", fib(25), (0..100).map(|k| ack(2, k)).sum::<i64>());
}
