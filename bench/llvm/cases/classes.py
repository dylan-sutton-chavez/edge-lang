class Rect:
    def __init__(self, w, h):
        self.w, self.h = w, h
    def area(self):
        return self.w * self.h

class Square:
    def __init__(self, s):
        self.s = s
    def area(self):
        return self.s * self.s

class Tri:
    def __init__(self, b, h):
        self.b, self.h = b, h
    def area(self):
        return self.b * self.h // 2

def make(i):
    k = i % 3
    if k == 0:
        return Rect(i % 7 + 1, i % 5 + 1)
    if k == 1:
        return Square(i % 9 + 1)
    return Tri(i % 11 + 1, i % 13 + 1)

shapes = [make(i) for i in range(1000)]
total = 0
for _ in range(200):
    for s in shapes:
        total += s.area()
print(total)
