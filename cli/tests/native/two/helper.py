def f():
    return 41

class Box:
    def __init__(self, n):
        self.n = n
    def grow(self, k):
        self.n += k
        return self.n
