def parse(i):
    if i % 7 == 0:
        raise ValueError(i)
    if i % 11 == 0:
        raise KeyError(i)
    return i * 3

ok, bad, missing = 0, 0, 0
for i in range(200000):
    try:
        ok += parse(i)
    except ValueError:
        bad += 1
    except KeyError:
        missing += 1
print(ok, bad, missing)
