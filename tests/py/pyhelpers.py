"""A pure-Python helper module; fpython compiles it to native code (no bytecode)."""
import math
from collections import Counter

GREETING = "hi"
_registry = {}


def register(name):
    def deco(fn):
        _registry[name] = fn
        return fn
    return deco


@register("square")
def square(x):
    return x * x


def stats(values, *, precision=2):
    if not values:
        raise ValueError("empty")
    mean = sum(values) / len(values)
    var = sum((v - mean) ** 2 for v in values) / len(values)
    return {"mean": round(mean, precision), "std": round(math.sqrt(var), precision), "n": len(values)}


def word_freq(text, top=3):
    words = [w.strip(".,!").lower() for w in text.split() if w]
    return Counter(words).most_common(top)


def safe_div(a, b):
    try:
        return a / b
    except ZeroDivisionError as e:
        return f"error: {e}"
    finally:
        _registry.setdefault("divs", 0)


def make_counter(start=0):
    count = start

    def inc(step=1):
        nonlocal count
        count += step
        return count
    return inc


class Shape:
    sides = 0

    def __init__(self, name):
        self.name = name

    def describe(self):
        return f"{self.name} with {self.sides} sides"

    def __repr__(self):
        return f"{type(self).__name__}({self.name!r})"


class Square(Shape):
    sides = 4

    def __init__(self, size):
        super().__init__("square")
        self.size = size

    @property
    def area(self):
        return self.size ** 2

    @staticmethod
    def unit():
        return Square(1)

    @classmethod
    def of(cls, size):
        return cls(size)


def classify(n):
    kinds = []
    for i in range(n):
        if i % 15 == 0:
            kinds.append("fizzbuzz")
        elif i % 3 == 0:
            kinds.append("fizz")
        elif i % 5 == 0:
            kinds.append("buzz")
        else:
            continue
    else:
        kinds.append("done")
    return kinds


def unpack_demo(seq):
    first, *middle, last = seq
    a, (b, c) = 1, (2, 3)
    return first, middle, last, a + b + c


def dict_comp(keys):
    return {k: len(k) for k in keys if k}, sorted({k[0] for k in keys if k})


def with_demo(path):
    with open(path, "w") as f:
        f.write("hello")
    with open(path) as f:
        return f.read()


def countdown(n):
    while n > 0:
        received = yield n
        if received:
            n -= received
        n -= 1
    return "liftoff"


def chained(*iters):
    for it in iters:
        yield from it


def lazy_squares(xs):
    return (x * x for x in xs if x % 2)


def first_match(xs, pred):
    return next((x for x in xs if pred(x)), None)


def guarded():
    try:
        yield 1
        yield 2
    finally:
        _registry["closed"] = True


def parse_ints(items):
    out = []
    for s in items:
        try:
            out.append(int(s))
        except ValueError:
            out.append(None)
        finally:
            _registry["parsed"] = True
    return out


class Recorder:
    def __init__(self):
        self.events = []

    async def __aenter__(self):
        self.events.append("enter")
        return self

    async def __aexit__(self, t, e, tb):
        self.events.append("exit " + (t.__name__ if t else "ok"))
        return t is KeyError


class Countdown:
    def __init__(self, n):
        self.n = n

    def __aiter__(self):
        return self

    async def __anext__(self):
        if self.n == 0:
            raise StopAsyncIteration
        self.n -= 1
        return self.n


async def async_add(a, b):
    import asyncio

    await asyncio.sleep(0)
    return a + b


async def async_demo():
    import asyncio

    rec = Recorder()
    total = 0
    async with rec:
        async for i in Countdown(3):
            total += await async_add(i, 10)
    async with rec:
        raise KeyError("suppressed")
    pair = await asyncio.gather(async_add(1, 2), async_add(3, 4))
    return total, pair, rec.events
