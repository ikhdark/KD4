from enum import Enum


class Unset(Enum):
    """An omitted setting, distinct from an explicit null reset."""

    value = "unset"


UNSET = Unset.value
