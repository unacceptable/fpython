"""Miscellaneous coverage for the native Python compiler: generators, scoping, unpacking."""
import contextlib
import functools


def gen_finally():
    try:
        yield 1
    finally:
        yield "cleanup"

def except_in_gen():
    for i in range(3):
        try:
            if i == 1:
                raise KeyError(i)
            yield i
        except KeyError as e:
            yield "err%s" % e

def ret_in_try_loop():
    for i in range(5):
        try:
            if i == 2:
                return i
        finally:
            pass
    return -1

def exc_var_cleared():
    e = 5
    try:
        raise ValueError
    except ValueError as e:
        pass
    try:
        return e
    except UnboundLocalError:
        return "cleared"

def sys_exc():
    import sys
    try:
        raise KeyError("q")
    except KeyError:
        inner = sys.exc_info()[0].__name__
    return inner, sys.exc_info()[0]

def nested_except():
    try:
        raise KeyError("outer")
    except KeyError as a:
        try:
            raise ValueError("inner")
        except ValueError as b:
            pass
        return repr(a)

def star_targets():
    out = []
    for a, *b in [(1, 2, 3), (4,)]:
        out.append((a, b))
    *c, = "xyz"
    [d, *e] = range(3)
    x = y = z = 0
    return out, c, d, e, x

def dict_order_kwargs(**kw):
    return list(kw)

def comp_scope():
    x = 10
    fs = [lambda: x for _ in range(2)]
    return [f() for f in fs], [x for x in range(3)], x

def big_ops():
    a = 2 ** 100
    a //= 3
    a <<= 2
    return a, -a, ~a, a % 7, divmod(a, 10)

def str_ops():
    s = "abc"
    s *= 2
    s += "!"
    return s[::-1], "%s-%d" % ("a", 3), "{}{}".format(1, 2), s.upper()

def run_part1():
    lines = [list(gen_finally()), list(except_in_gen()), ret_in_try_loop(), exc_var_cleared(), sys_exc(), nested_except(), star_targets(), dict_order_kwargs(b=1, a=2), comp_scope(), big_ops(), str_ops()]
    return "\n".join(repr(x) for x in lines)



def local_class():
    class Local:
        attr = 1
        def get(self):
            return self.attr
    return Local().get(), Local.__name__

class Holder:
    def gen(self):
        yield super().__repr__()[:1]
        yield __class__.__name__
    def nested(self):
        def inner():
            return __class__.__name__
        return inner()

def inner_gen():
    try:
        yield 1
        yield 2
    except KeyError:
        yield "inner caught"
    return "inner-ret"

def outer_gen():
    r = yield from inner_gen()
    yield r

def throw_through():
    g = outer_gen()
    a = next(g)
    b = g.throw(KeyError("k"))
    c = next(g)
    return a, b, c

@contextlib.contextmanager
def ctx(log):
    log.append("in")
    try:
        yield "val"
    finally:
        log.append("out")

def use_ctx():
    log = []
    with ctx(log) as v:
        log.append(v)
    with contextlib.suppress(KeyError):
        raise KeyError
    return log

def trace(fn):
    @functools.wraps(fn)
    def wrapper(*a, **k):
        return ("traced", fn(*a, **k))
    return wrapper

@trace
def traced_add(a, b=1):
    """doc"""
    return a + b

def mutable_default(x, acc=[]):
    acc.append(x)
    return len(acc)

def fact(n):
    return 1 if n <= 1 else n * fact(n - 1)

def nested_global():
    def setter():
        global NG
        NG = "set"
    setter()
    return NG

if fact(3) == 6:
    def cond_fn():
        return "first"
else:
    def cond_fn():
        return "second"

def kwonly(*, a, b=2):
    return a, b

def close_gen():
    log = []
    def g():
        try:
            yield 1
        except GeneratorExit:
            log.append("exit")
            raise
    it = g()
    next(it)
    it.close()
    return log

def generic[T](x: T) -> T:
    return x


class Box[T]:
    def __init__(self, v):
        self.v = v


def run_part3():
    return generic("g"), Box(5).v


def run_part2():
    lines = [local_class(), list(Holder().gen()), Holder().nested(), throw_through(), use_ctx(), traced_add(2), traced_add.__name__, traced_add.__doc__, mutable_default(1), mutable_default(2), fact(10), nested_global(), cond_fn(), kwonly(a=1), close_gen()]
    try:
        kwonly(1)
    except TypeError:
        lines.append("kwonly-typeerror")
    return "\n".join(repr(x) for x in lines)
