"""Idiomatic Python: Sieve of Eratosthenes with a bytearray and slice assignment."""


def sieve(n):
    flags = bytearray([1]) * (n + 1)
    flags[0] = flags[1] = 0
    i = 2
    while i * i <= n:
        if flags[i]:
            flags[i * i :: i] = bytes(len(range(i * i, n + 1, i)))
        i += 1
    return flags.count(1)


print(sieve(20000000))
