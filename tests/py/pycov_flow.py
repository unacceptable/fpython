"""Statement and control-flow coverage for the native Python compiler."""
import os.path as osp
import xml.dom as xdom

try:
    import does_not_exist_pycov
except ImportError:
    does_not_exist_pycov = None

if osp.sep:
    from collections import OrderedDict as OD
else:
    OD = dict

COUNTER = 0
LOG = []


def bump(n):
    global COUNTER
    COUNTER += n
    return COUNTER


def set_and_delete_global():
    global TEMP
    TEMP = 1
    had = "TEMP" in globals()
    del TEMP
    return had, "TEMP" in globals()


def loops():
    out = []
    i = 0
    while i < 10:
        i += 1
        if i == 3:
            continue
        if i == 6:
            break
        out.append(i)
    else:
        out.append("no-break")
    j = 0
    while j < 2:
        j += 1
    else:
        out.append("while-else")
    for k in range(5):
        if k == 2:
            break
    else:
        out.append("unreached")
    for k in range(2):
        pass
    else:
        out.append("for-else")
    return out


def handlers(kind):
    try:
        if kind == 0:
            raise KeyError("k")
        if kind == 1:
            raise ValueError("v")
        if kind == 2:
            raise TypeError("t")
        if kind == 3:
            [][1]
        if kind == 4:
            raise RuntimeError("r")
    except KeyError as e:
        return "key", str(e)
    except (ValueError, TypeError) as e:
        return "val/type", type(e).__name__
    except IndexError:
        return "index"
    except:
        return "bare"
    else:
        return "else"
    finally:
        LOG.append(kind)


def finally_flow():
    out = []
    for i in range(4):
        try:
            if i == 1:
                continue
            if i == 3:
                break
            out.append(i)
        finally:
            out.append("f%d" % i)
    while True:
        try:
            try:
                break
            finally:
                out.append("inner")
        finally:
            out.append("outer")
    return out


def finally_return():
    try:
        return "try"
    finally:
        LOG.append("finally_return")


def finally_override():
    try:
        raise ValueError("lost")
    finally:
        return "override"


def reraise():
    try:
        try:
            raise ValueError("first")
        except ValueError:
            raise
    except ValueError as e:
        return "reraised " + str(e)


def chain():
    try:
        try:
            1 / 0
        except ZeroDivisionError as z:
            raise LookupError("wrapped") from z
    except LookupError as e:
        return type(e.__cause__).__name__, e.__suppress_context__


def chain_none():
    try:
        try:
            1 / 0
        except ZeroDivisionError:
            raise LookupError("plain") from None
    except LookupError as e:
        return e.__cause__, e.__suppress_context__


def asserts(x):
    try:
        assert x > 0, f"bad {x}"
        assert x < 100
        return "ok"
    except AssertionError as e:
        return "assert: " + str(e)


class Bag:
    pass


def deletes():
    out = []
    x = 1
    y = 2
    del x
    try:
        print(x)
    except UnboundLocalError:
        out.append("unbound")
    b = Bag()
    b.p = 1
    b.q = 2
    del b.p
    out.append(hasattr(b, "p"))
    lst = [1, 2, 3, 4]
    d = {"a": 1, "b": 2}
    del lst[0], d["a"]
    out.append((lst, d))
    m, n = 3, 4
    del (m, n)
    [u, v] = [5, 6]
    del [u, v]
    try:
        del y
        del y
    except UnboundLocalError:
        out.append("double-del")
    cell = 10

    def reader():
        return cell

    out.append(reader())
    del cell
    try:
        reader()
    except NameError:
        out.append("cell-deleted")
    return out


def closures():
    total = 0
    names = []

    def outer(step):
        def inner():
            nonlocal total
            total += step
            names.append(step)
            return total
        return inner

    a = outer(1)
    b = outer(10)
    a()
    b()
    a()

    def late():
        return later

    try:
        late()
    except NameError as e:
        names.append("free-unbound")
    later = "bound"
    names.append(late())
    return total, names


class CM:
    def __init__(self, name, log, swallow=False):
        self.name = name
        self.log = log
        self.swallow = swallow

    def __enter__(self):
        self.log.append("enter " + self.name)
        return self.name

    def __exit__(self, t, e, tb):
        self.log.append("exit %s %s" % (self.name, t.__name__ if t else None))
        return self.swallow


def withs():
    log = []
    with CM("a", log) as a, CM("b", log) as b, CM("c", log):
        log.append(a + b)
    with CM("outer", log, swallow=True):
        with CM("inner", log):
            raise KeyError("x")
    for i in range(3):
        with CM(str(i), log):
            if i == 0:
                continue
            if i == 1:
                break
    return log


def with_return():
    log = []
    with CM("r", log):
        return log


def gen_protocol():
    def g():
        received = []
        try:
            while True:
                try:
                    v = yield len(received)
                    received.append(v)
                except ValueError as e:
                    received.append("thrown " + str(e))
                    yield "handled"
        finally:
            LOG.append(("closed", tuple(received)))

    it = g()
    out = [next(it), it.send("a"), it.send("b"), it.throw(ValueError("x")), next(it)]
    it.close()

    def bare():
        yield
        yield
        return "done"

    out.append(list(bare()))
    b = bare()
    next(b)
    next(b)
    try:
        next(b)
    except StopIteration as s:
        out.append(s.value)

    def delegating():
        r = yield from bare()
        yield r

    out.append(list(delegating()))
    return out


def walrus_global():
    if (found := bump(5)) > 0:
        return found


def run_all():
    lines = [
        osp.basename("/a/b/c.txt"),
        xdom.__name__,
        does_not_exist_pycov,
        OD.__name__,
        bump(2),
        set_and_delete_global(),
        loops(),
        [handlers(k) for k in range(6)],
        finally_flow(),
        finally_return(),
        finally_override(),
        reraise(),
        chain(),
        chain_none(),
        asserts(5),
        asserts(-1),
        asserts(500),
        deletes(),
        closures(),
        withs(),
        with_return(),
        gen_protocol(),
        walrus_global(),
        COUNTER,
        LOG,
    ]
    return "\n".join(repr(x) for x in lines)
