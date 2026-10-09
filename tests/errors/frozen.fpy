# expect: frozen dataclass
from dataclasses import dataclass


@dataclass(frozen=True)
class P:
    x: int


p = P(1)
p.x = 2
