n = 200000
sieve = [True] * (n + 1)
sieve[0] = sieve[1] = False
i = 2
while i * i <= n:
    if sieve[i]:
        for j in range(i * i, n + 1, i):
            sieve[j] = False
    i += 1
primes = [k for k in range(n + 1) if sieve[k]]
gaps = [primes[k + 1] - primes[k] for k in range(len(primes) - 1)]
print(len(primes), sum(primes) % 1000000007, max(gaps))
