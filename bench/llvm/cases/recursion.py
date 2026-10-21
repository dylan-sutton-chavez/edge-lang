def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

def ack(m, n):
    if m == 0:
        return n + 1
    if n == 0:
        return ack(m - 1, 1)
    return ack(m - 1, ack(m, n - 1))

print(fib(25), sum(ack(2, k) for k in range(100)))
