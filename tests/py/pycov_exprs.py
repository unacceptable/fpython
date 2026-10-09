"""Expression coverage for the native Python compiler: literals, operators, calls, f-strings."""

BIG = 123456789012345678901234567890
HEXBIG = 0xFFFFFFFFFFFFFFFFFFFF
PI = 3.25
CPLX = 2j
RAW = b"ab\x00\xff"
DOTS = ...
ANNOTATED: int = 5
a = b = c = [1]


def literals():
    out = []
    out.append(BIG * 2 + HEXBIG)
    out.append(PI * 2.0 - 0.5)
    out.append((1 + CPLX) * 3j)
    out.append(RAW + b"!")
    out.append(len(RAW))
    out.append(DOTS is Ellipsis)
    out.append(a is b is c)
    out.append(ANNOTATED)
    out.append(1e300 * 10)
    out.append(-0.0)
    return out


def unary(x):
    return -x, +x, ~x, not x, -(-x)


def binops(x, y):
    return [x + y, x - y, x * y, x / y, x // y, x % y, x ** y, x << y, x >> 1, x & y, x | y, x ^ y]


class Mat:
    def __init__(self, v):
        self.v = v

    def __matmul__(self, other):
        return Mat(self.v * other.v)

    def __imatmul__(self, other):
        self.v = self.v + other.v
        return self

    def __repr__(self):
        return f"Mat({self.v})"


def inplace(x, y):
    res = []
    for op in range(12):
        v = x
        if op == 0:
            v += y
        elif op == 1:
            v -= y
        elif op == 2:
            v *= y
        elif op == 3:
            v /= y
        elif op == 4:
            v //= y
        elif op == 5:
            v %= y
        elif op == 6:
            v **= y
        elif op == 7:
            v <<= y
        elif op == 8:
            v >>= 1
        elif op == 9:
            v &= y
        elif op == 10:
            v |= y
        else:
            v ^= y
        res.append(v)
    m = Mat(2)
    m @= Mat(5)
    res.append(m)
    res.append(Mat(3) @ Mat(4))
    return res


def compares(x):
    return [1 < x <= 10, x == 5, x != 5, x > 3, x >= 3, x in (1, 5), x not in [5], x is None, x is not None, 0 < x < 3 < 4]


class Grid:
    def __init__(self):
        self.data = {}

    def __getitem__(self, key):
        return self.data.get(key, ("missing", key))

    def __setitem__(self, key, value):
        self.data[key] = value

    def __delitem__(self, key):
        del self.data[key]


def subscripts():
    g = Grid()
    g[1, 2] = "a"
    g[1:2, ::3] = "s"
    r = [g[1, 2], g[1:2, ::3], g[..., 0]]
    lst = list(range(10))
    r.append(lst[::-2])
    r.append(lst[2:8:3])
    r.append(lst[:3])
    d = {"k": 1}
    d["k"] += 41
    lst[0] -= 5
    lst[1:3] = ["x", "y"]
    del lst[5:]
    r.append(d)
    r.append(lst)
    counts = {"a": [1]}
    counts["a"][0] *= 7
    r.append(counts)
    del g[1, 2]
    r.append(sorted(g.data.items(), key=str))
    return r


def packing():
    xs = [1, 2]
    ys = (3, 4)
    t = (*xs, *ys, 5)
    s = {*xs, *list(ys), 0}
    d1 = {"a": 1}
    d = {**d1, "b": 2, **{"c": 3}}
    l2 = [0, *ys]
    return t, sorted(s), d, l2


def kw(a, b=2, /, c=3, *args, d, e=5, **rest):
    return (a, b, c, args, d, e, sorted(rest.items()))


def calls():
    args = [10, 20, 30, 40]
    opts = {"d": 4, "z": 26}
    out = [
        kw(1, d=0),
        kw(*args, **opts),
        kw(1, *[2], d=3, **{"y": 1}),
        kw(1, 2, 3, *(4, 5), d=6, e=7, f=8),
        max(*args),
        sorted([3, 1, 2], reverse=True),
        sum(x * x for x in range(5)),
        min(x for x in [4, 2, 8]),
        tuple(c for c in "ab"),
        set(x % 3 for x in range(9)),
        frozenset(x for x in [1, 1]),
        dict((k, k * 2) for k in range(3)),
        ", ".join(str(n) for n in range(4)),
        any(x > 3 for x in range(5)),
    ]
    return out


def lambdas():
    add = lambda x, y=10: x + y
    star = lambda *a, **k: (a, sorted(k))
    base = 100
    capture = lambda v: v + base
    fns = [lambda i=i: i * i for i in range(3)]
    return add(1), add(1, 2), star(1, 2, q=3), capture(5), [f() for f in fns], (lambda: "now")()


def walrus(items):
    out = []
    if (n := len(items)) > 2:
        out.append(n)
    while (item := items.pop()) != 0:
        out.append(item)
    out.append([y for x in range(5) if (y := x * 2) > 4])
    out.append(y)
    return out


def fstrings(name, value):
    width = 8
    prec = 3
    return [
        f"{name!r} {name!s} {name!a}",
        f"{value:>{width}.{prec}f}|",
        f"{value=}",
        f"{value=:.1f}",
        f"{name=!s}",
        f"{'nested'!r:^12}",
        f"{{literal}} {value:,.2f} {42:#x} {42:08b}",
        f"{3.5:{'<'}{width}}|",
        f"{'é'!a}",
    ]


def conditional(x):
    return "big" if x > 10 else "small" if x > 0 else "neg", x and "t" or "f", x or None


def nested_comps():
    grid = [[r * 3 + c for c in range(3)] for r in range(3)]
    flat = [v for row in grid for v in row if v % 2 if v > 1]
    pairs = {(i, j) for i in range(3) for j in range(i)}
    inv = {v: k for k, v in {"a": 1, "b": 2}.items()}
    gen = list(x + y for x in range(3) for y in (10, 20))
    return grid, flat, sorted(pairs), inv, gen


def module_namespace():
    return sorted(k for k in globals() if k in ("BIG", "PI", "a"))


TOP_GLOBALS = sorted(k for k in globals() if k in ("BIG", "RAW"))


def run_all():
    lines = [
        literals(),
        unary(5),
        unary(0),
        binops(7, 2),
        inplace(7, 2),
        compares(5),
        compares(2),
        subscripts(),
        packing(),
        calls(),
        lambdas(),
        walrus([0, 1, 2, 3]),
        fstrings("bob", 3.14159),
        conditional(20),
        conditional(5),
        conditional(-1),
        conditional(0),
        nested_comps(),
        module_namespace(),
        TOP_GLOBALS,
    ]
    return "\n".join(repr(x) for x in lines)
