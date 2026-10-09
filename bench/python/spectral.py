"""Idiomatic Python: spectral norm with generator expressions and sum()."""
import math


def a(i, j):
    return 1.0 / ((i + j) * (i + j + 1) // 2 + i + 1)


def mul_av(v):
    n = len(v)
    return [sum(a(i, j) * v[j] for j in range(n)) for i in range(n)]


def mul_atv(v):
    n = len(v)
    return [sum(a(j, i) * v[j] for j in range(n)) for i in range(n)]


def mul_atav(v):
    return mul_atv(mul_av(v))


def main():
    n = 1000
    u = [1.0] * n
    for _ in range(10):
        v = mul_atav(u)
        u = mul_atav(v)
    vbv = sum(x * y for x, y in zip(u, v))
    vv = sum(x * x for x in v)
    print(f"{math.sqrt(vbv / vv):.9f}")


main()
