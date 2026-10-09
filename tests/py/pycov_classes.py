"""Class-machinery coverage for the native Python compiler."""
from dataclasses import dataclass, field
from enum import Enum
import functools

REGISTRY = []


def register(cls):
    REGISTRY.append(cls.__name__)
    return cls


def tag(label):
    def deco(cls):
        cls.label = label
        return cls
    return deco


class Meta(type):
    def __new__(mcls, name, bases, ns, **kwargs):
        ns["created_by"] = "Meta"
        ns["meta_kwargs"] = sorted(kwargs.items())
        return super().__new__(mcls, name, bases, ns)

    def __init__(cls, name, bases, ns, **kwargs):
        super().__init__(name, bases, ns)

    @classmethod
    def __prepare__(mcls, name, bases, **kwargs):
        d = {}
        d["prepared"] = True
        return d


class WithMeta(metaclass=Meta, flavor="vanilla", size=3):
    pass


class PluginBase:
    plugins = []

    def __init_subclass__(cls, key=None, **kw):
        super().__init_subclass__(**kw)
        PluginBase.plugins.append((cls.__name__, key))


class PluginA(PluginBase, key="a"):
    pass


class PluginB(PluginBase):
    pass


@register
@tag("decorated")
class Decorated:
    x: int = 1
    y: str
    counter = 0
    counter += 5
    tmp = "scratch"
    del tmp
    squares = [i * i for i in range(4)]
    names = globals()["REGISTRY"] is REGISTRY

    class Inner:
        value = "inner"

    def method(self):
        return __class__.__name__, self.Inner.value


class Base:
    def __init__(self, v):
        self.v = v

    def show(self):
        return "Base(%s)" % self.v

    @classmethod
    def make(cls, v):
        return cls(v)


class Child(Base):
    def __init__(self, v, w):
        super().__init__(v)
        self.w = w

    def show(self):
        return "Child[" + super().show() + "," + str(self.w) + "]"

    def explicit(self):
        return super(Child, self).show()

    @classmethod
    def make(cls, v):
        inst = super().make(v) if False else cls(v, v * 2)
        return inst

    def cls_name(self):
        return __class__.__name__

    def via_lambda(self):
        f = lambda: super(Child, self).show()
        return f()


class GrandChild(Child):
    def show(self):
        return "Grand<" + super().show() + ">"


class Temp:
    def __init__(self):
        self._c = 0.0

    @property
    def celsius(self):
        return self._c

    @celsius.setter
    def celsius(self, v):
        self._c = v

    @celsius.deleter
    def celsius(self):
        self._c = None

    @staticmethod
    def convert(c):
        return c * 9 / 5 + 32

    @functools.cached_property
    def heavy(self):
        return "computed"


class Vec:
    __slots__ = ("x", "y")

    def __init__(self, x, y):
        self.x = x
        self.y = y

    def __add__(self, o):
        return Vec(self.x + o.x, self.y + o.y)

    def __sub__(self, o):
        return Vec(self.x - o.x, self.y - o.y)

    def __mul__(self, k):
        return Vec(self.x * k, self.y * k)

    __rmul__ = __mul__

    def __neg__(self):
        return Vec(-self.x, -self.y)

    def __eq__(self, o):
        return isinstance(o, Vec) and (self.x, self.y) == (o.x, o.y)

    def __hash__(self):
        return hash((self.x, self.y))

    def __lt__(self, o):
        return (self.x, self.y) < (o.x, o.y)

    def __len__(self):
        return 2

    def __iter__(self):
        yield self.x
        yield self.y

    def __getitem__(self, i):
        return (self.x, self.y)[i]

    def __contains__(self, v):
        return v in (self.x, self.y)

    def __bool__(self):
        return bool(self.x or self.y)

    def __call__(self, scale):
        return self * scale

    def __repr__(self):
        return f"Vec({self.x}, {self.y})"


class Named:
    def __set_name__(self, owner, name):
        self.name = owner.__name__ + "." + name

    def __get__(self, obj, objtype=None):
        return self.name


class UsesNamed:
    field_a = Named()


@dataclass(order=True)
class Point:
    x: int
    y: int = 0
    tags: list = field(default_factory=list)

    def norm1(self):
        return abs(self.x) + abs(self.y)


class Color(Enum):
    RED = 1
    GREEN = 2

    def describe(self):
        return f"{self.name}={self.value}"


class Account:
    count = 0

    def __init__(self):
        Account.count += 1
        self.balance = 0

    def deposit(self, amount):
        self.balance += amount
        return self


def run_all():
    t = Temp()
    t.celsius = 25.0
    first = t.celsius
    del t.celsius
    v = Vec(1, 2)
    w = Vec(3, 4)
    p = Point(3, -4)
    a = Account().deposit(10).deposit(5)
    Account()
    lines = [
        (WithMeta.created_by, WithMeta.meta_kwargs, WithMeta.prepared),
        PluginBase.plugins,
        (REGISTRY, Decorated.label, list(Decorated.__annotations__), Decorated.counter),
        (hasattr(Decorated, "tmp"), Decorated.squares, Decorated.names, Decorated().method()),
        (Child(1, 2).show(), GrandChild(5, 6).show(), Child(7, 8).explicit()),
        (Child.make(3).show(), Child(0, 0).cls_name(), Child(9, 9).via_lambda()),
        (first, t.celsius, Temp.convert(100), t.heavy),
        (v + w, w - v, v * 3, 2 * v, -v, v == Vec(1, 2), v != w, sorted([w, v]), len(v), list(v)),
        (v[1], 2 in v, 5 in v, bool(Vec(0, 0)), v(10), len({v, Vec(1, 2)})),
        UsesNamed().field_a,
        (p, p.norm1(), p < Point(4), Point(1, tags=["a"]).tags),
        (Color.RED.describe(), Color(2), [c.name for c in Color]),
        (a.balance, Account.count),
    ]
    return "\n".join(repr(x) for x in lines)
