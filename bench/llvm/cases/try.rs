enum Fail {
    Value,
    Key,
}

fn parse(i: i64) -> Result<i64, Fail> {
    if i % 7 == 0 {
        return Err(Fail::Value);
    }
    if i % 11 == 0 {
        return Err(Fail::Key);
    }
    Ok(i * 3)
}

fn main() {
    let (mut ok, mut bad, mut missing) = (0i64, 0i64, 0i64);
    for i in 0..200000 {
        match parse(i) {
            Ok(v) => ok += v,
            Err(Fail::Value) => bad += 1,
            Err(Fail::Key) => missing += 1,
        }
    }
    println!("{ok} {bad} {missing}");
}
