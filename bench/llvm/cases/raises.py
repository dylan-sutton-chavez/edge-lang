def check(n):
    if n % 1000 == 999 and n > 40000:
        raise ValueError(f"no room for {n}")
    return n * 2

total = 0
for i in range(50000):
    total += check(i)
    if i % 10000 == 0:
        print(total)
