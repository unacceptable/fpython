"""Idiomatic Python: binary trees as tuples."""


def make(depth):
    if depth == 0:
        return (None, None)
    depth -= 1
    return (make(depth), make(depth))


def check(node):
    left, right = node
    if left is None:
        return 1
    return 1 + check(left) + check(right)


def main():
    max_depth = 18
    long_lived = make(max_depth)
    total = 0
    for d in range(4, max_depth + 1, 2):
        iters = 1 << (max_depth - d + 4)
        total += sum(check(make(d)) for _ in range(iters))
    print(total, check(long_lived))


main()
