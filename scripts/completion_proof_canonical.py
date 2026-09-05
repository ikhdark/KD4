"""Shared canonical JSON and domain-separated proof-hash primitives.

This module contains only the closed no-float RFC 8785/JCS subset shared by
completion-proof contracts. It deliberately has no runner, inventory,
admission, or filesystem behavior.
"""

from __future__ import annotations

import hashlib
import json
import unicodedata
from typing import Any


class CanonicalJcsError(ValueError):
    """Raised when a value is outside the shared canonical JSON contract."""


def _require_nfc(value: str) -> None:
    if any(0xD800 <= ord(character) <= 0xDFFF for character in value):
        raise CanonicalJcsError("string contains a Unicode surrogate")
    if unicodedata.normalize("NFC", value) != value:
        raise CanonicalJcsError(f"string is not NFC: {value!r}")


def _validate_jcs_value(value: Any) -> None:
    if value is None or isinstance(value, bool):
        return
    if isinstance(value, int):
        if not (-(2**53) + 1 <= value <= 2**53 - 1):
            raise CanonicalJcsError("integer is outside the exact I-JSON range")
        return
    if isinstance(value, float):
        raise CanonicalJcsError("canonical JSON does not permit floats")
    if isinstance(value, str):
        _require_nfc(value)
        return
    if isinstance(value, list):
        for item in value:
            _validate_jcs_value(item)
        return
    if isinstance(value, dict):
        for key, item in value.items():
            if not isinstance(key, str):
                raise CanonicalJcsError("JSON object key is not a string")
            _require_nfc(key)
            _validate_jcs_value(item)
        return
    raise CanonicalJcsError(f"unsupported canonical JSON value: {type(value)!r}")


def canonical_jcs(value: Any) -> bytes:
    """Encode the shared no-float RFC 8785/JCS subset."""

    _validate_jcs_value(value)
    return _encode_jcs(value).encode("utf-8")


def _encode_jcs(value: Any) -> str:
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    if isinstance(value, list):
        return "[" + ",".join(_encode_jcs(item) for item in value) + "]"
    if isinstance(value, dict):
        keys = sorted(value, key=lambda key: key.encode("utf-16-be"))
        return "{" + ",".join(
            f"{_encode_jcs(key)}:{_encode_jcs(value[key])}" for key in keys
        ) + "}"
    raise AssertionError("value was validated before encoding")


def proof_hash(domain: str, value: Any) -> str:
    if not domain or not domain.isascii() or "\x00" in domain:
        raise CanonicalJcsError("hash domain must be nonempty ASCII without NUL")
    return hashlib.sha256(
        domain.encode("ascii") + b"\x00" + canonical_jcs(value)
    ).hexdigest()


__all__ = ["CanonicalJcsError", "canonical_jcs", "proof_hash"]
