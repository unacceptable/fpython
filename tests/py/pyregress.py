"""Regression cases for the Python->C compiler (bug backlog B24-B27)."""


def set_splat():
    return sorted({*(3, 4), *[5]})


def implicit_context():
    try:
        try:
            raise KeyError("a")
        except KeyError:
            raise ValueError("b")
    except ValueError as e:
        return repr(e.__context__)


def class_in_function(v):
    class C:
        attr = v
        twice = attr * 2

        def get(self):
            return v

    return C.attr, C.twice, C().get()


class A:
    class B:
        pass

    def m(self):
        pass


def outer():
    def inner():
        pass

    return inner.__qualname__, (lambda: 0).__qualname__


def qualnames():
    return A.B.__qualname__, A.m.__qualname__, outer()
