def collatz(n):
    while n != 1:
        yield n
        n = n // 2 if n % 2 == 0 else 3 * n + 1
    yield 1

def lengths(limit):
    for start in range(1, limit):
        steps = 0
        for _ in collatz(start):
            steps += 1
        yield steps

best = 0
total = 0
for steps in lengths(20000):
    total += steps
    if steps > best:
        best = steps
print(total, best)
