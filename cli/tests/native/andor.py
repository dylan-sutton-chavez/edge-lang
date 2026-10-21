def side(n, v):
    print(n)
    return v

print(0 or 'fallback', (0 and 5) + 1, 3 and 4, '' or 0 or 7)
print(side('a', False) and side('b', True))
print(side('c', True) or side('d', True))
if 1 > 2 or 3 > 1:
    print('in')
