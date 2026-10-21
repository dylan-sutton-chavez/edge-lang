class Shape:
    def __init__(self, name):
        self.name = name
    def area(self):
        raise NotImplementedError(self.name)

class Rect(Shape):
    def __init__(self, w, h):
        super().__init__("rect")
        self.w, self.h = w, h
    def area(self):
        return self.w * self.h

class Circle(Shape):
    def __init__(self, r):
        super().__init__("circle")
        self.r = r
    def area(self):
        return 3.0 * self.r * self.r

def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)

def squares(n):
    for i in range(n):
        yield i * i

def mix(n):
    point, acc = (3, 4), 0
    for i in range(n):
        acc = (acc + i * point[0] - point[1]) % 1000003
    return acc

def triple(n):
    return [n] * 3

def kept():
    keep = [7, 8]
    triple(1)
    return keep[0]

def scaled(k):
    return lambda v: f"{v * k:.1f}" if isinstance(v, float) else str(v * k)

label = scaled(1)
shapes = [Rect(i, i + 1) if i % 2 else Circle(i) for i in range(6)] + [Shape("blob")]
total, seen, words = 0, {}, []
for s in shapes:
    try:
        total += s.area()
        seen[s.name] = seen.get(s.name, 0) + 1
    except NotImplementedError as e:
        words.append(f"no area for {e}")
    finally:
        words.append(label(total))
print(label(total), sorted(seen.items()), fib(20), sum(squares(100)), mix(20000))
print(" ".join(words), [x if x % 3 else str(x) for x in range(7)], scaled(2)(21), kept())
