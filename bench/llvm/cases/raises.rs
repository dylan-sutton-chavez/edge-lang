fn check(n: i64) -> Result<i64, String> {
    if n % 1000 == 999 && n > 40000 {
        return Err(format!("no room for {n}"));
    }
    Ok(n * 2)
}

fn main() {
    let mut total = 0i64;
    for i in 0..50000 {
        match check(i) {
            Ok(v) => total += v,
            Err(e) => {
                eprintln!("ValueError: {e}");
                std::process::exit(1);
            }
        }
        if i % 10000 == 0 {
            println!("{total}");
        }
    }
}
