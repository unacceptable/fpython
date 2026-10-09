"""A package compiled natively: relative imports and star imports."""
from .core import VALUE, helper
from . import core as core_mod
from .sub import leaf
from .sub.leaf import *

__all__ = ["VALUE", "helper", "summary"]


def summary():
    return (VALUE, helper(2), core_mod.__name__, leaf.LEAF, leaf_fn(), __name__, __package__)
