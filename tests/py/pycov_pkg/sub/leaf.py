from .. import core
from . import PARENT_VALUE

__all__ = ["leaf_fn"]
LEAF = "leaf"
_hidden = 1


def leaf_fn():
    return (LEAF, core.VALUE, PARENT_VALUE, __name__)
