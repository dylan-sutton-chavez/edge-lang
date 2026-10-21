runs = []

def f(xs):
    out = []
    for x in xs:
        try:
            try:
                out.append(10 // x)
            finally:
                runs.append(x)
        except ZeroDivisionError:
            out.append('z')
    return out

def g(x):
    try:
        return 10 // x
    finally:
        runs.append('g')
        return 'fin'

def h():
    for i in range(3):
        try:
            if i == 1:
                continue
            if i == 2:
                break
        finally:
            runs.append(i)
    return 'h'

print(f([1, 0, 2]), g(0), g(5), h(), runs)
