class Vec2:
    def __init__(self, x, y):
        self.x, self.y = x, y
    def add(self, o):
        return Vec2(self.x + o.x, self.y + o.y)
    def scale(self, k):
        return Vec2(self.x * k % 1000003, self.y * k % 1000003)
    def dot(self, o):
        return self.x * o.x + self.y * o.y

acc = 0
p = Vec2(1, 2)
for i in range(200000):
    q = Vec2(i, i + 1).scale(3).add(p)
    acc = (acc + q.dot(p)) % 1000000007
print(acc)
