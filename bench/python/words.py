"""Idiomatic Python: word counting with collections.Counter."""
from collections import Counter

VOCAB = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu"]


def make_text(n):
    parts = []
    append = parts.append
    x = 12345
    for _ in range(n):
        x = (x * 1103515245 + 12345) % 2147483648
        append(VOCAB[x % len(VOCAB)] + str(x % 97))
    return " ".join(parts)


def main():
    counts = Counter(make_text(2000000).split())
    top = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:3]
    print(len(counts), top)


main()
