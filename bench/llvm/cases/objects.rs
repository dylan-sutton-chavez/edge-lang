#[derive(Clone, Copy)]
struct Vec2 {
    x: i64,
    y: i64,
}

impl Vec2 {
    fn add(self, o: Vec2) -> Vec2 {
        Vec2 { x: self.x + o.x, y: self.y + o.y }
    }
    fn scale(self, k: i64) -> Vec2 {
        Vec2 { x: self.x * k % 1000003, y: self.y * k % 1000003 }
    }
    fn dot(self, o: Vec2) -> i64 {
        self.x * o.x + self.y * o.y
    }
}

fn main() {
    let mut acc = 0i64;
    let p = Vec2 { x: 1, y: 2 };
    for i in 0..200000 {
        let q = Vec2 { x: i, y: i + 1 }.scale(3).add(p);
        acc = (acc + q.dot(p)) % 1000000007;
    }
    println!("{acc}");
}
