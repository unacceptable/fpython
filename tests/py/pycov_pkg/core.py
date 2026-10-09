VALUE = 42


def helper(x):
    from .sub.leaf import leaf_fn

    return (x * VALUE, leaf_fn())
