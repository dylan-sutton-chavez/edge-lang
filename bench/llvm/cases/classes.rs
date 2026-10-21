enum Shape {
    Rect { w: i64, h: i64 },
    Square { s: i64 },
    Tri { b: i64, h: i64 },
}

impl Shape {
    fn area(&self) -> i64 {
        match self {
            Shape::Rect { w, h } => w * h,
            Shape::Square { s } => s * s,
            Shape::Tri { b, h } => b * h / 2,
        }
    }
}

fn make(i: i64) -> Shape {
    match i % 3 {
        0 => Shape::Rect { w: i % 7 + 1, h: i % 5 + 1 },
        1 => Shape::Square { s: i % 9 + 1 },
        _ => Shape::Tri { b: i % 11 + 1, h: i % 13 + 1 },
    }
}

fn main() {
    let shapes: Vec<Shape> = (0..1000).map(make).collect();
    let mut total = 0i64;
    for _ in 0..200 {
        for s in &shapes {
            total += s.area();
        }
    }
    println!("{total}");
}
