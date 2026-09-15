from __future__ import annotations

from enum import Enum
from typing import NoReturn

from .generated.v2_all import (
    AskForApproval,
    AskForApprovalValue,
)


class ApprovalMode(str, Enum):
    """High-level approval behavior for escalated permission requests."""

    deny_all = "deny_all"


def _approval_mode_settings(
    approval_mode: ApprovalMode,
) -> AskForApproval:
    """Map the public approval mode to generated app-server start params."""
    if not isinstance(approval_mode, ApprovalMode):
        supported = ", ".join(mode.value for mode in ApprovalMode)
        raise ValueError(f"approval_mode must be one of: {supported}")

    match approval_mode:
        case ApprovalMode.deny_all:
            return AskForApproval(root=AskForApprovalValue.never)
        case _:
            return _assert_never_approval_mode(approval_mode)


def _assert_never_approval_mode(approval_mode: NoReturn) -> NoReturn:
    """Make approval mode mapping exhaustive for static type checkers."""
    raise AssertionError(f"Unhandled approval mode: {approval_mode!r}")


def _approval_mode_override_settings(
    approval_mode: ApprovalMode | None,
) -> AskForApproval | None:
    """Map an optional public approval mode to app-server override params."""
    if approval_mode is None:
        return None
    return _approval_mode_settings(approval_mode)
