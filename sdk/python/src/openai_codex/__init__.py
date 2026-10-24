"""Python SDK for running Codex workflows.

Start with :class:`Codex` for synchronous applications or
:class:`AsyncCodex` for async applications. Most programs create a thread and
run a turn::

    from openai_codex import Codex, Sandbox

    with Codex() as codex:
        thread = codex.thread_start(sandbox=Sandbox.workspace_write)
        result = thread.run("Describe this project.")
        print(result.final_response)
"""

from importlib import import_module as _import_module
from typing import TYPE_CHECKING as _TYPE_CHECKING
from typing import Any as _Any

from ._version import __version__

if _TYPE_CHECKING:
    from ._run import TurnRunError
    from .api import (
        ApprovalMode,
        AsyncChatgptLoginHandle,
        AsyncCodex,
        AsyncDeviceCodeLoginHandle,
        AsyncThread,
        AsyncTurnHandle,
        ChatgptLoginHandle,
        Codex,
        DeviceCodeLoginHandle,
        ImageInput,
        Input,
        InputItem,
        LocalImageInput,
        MentionInput,
        RunInput,
        Sandbox,
        SkillInput,
        TextInput,
        Thread,
        TurnHandle,
        TurnResult,
    )
    from .client import CodexConfig
    from .errors import (
        CodexError,
        CodexRpcError,
        InternalRpcError,
        InvalidParamsError,
        InvalidRequestError,
        JsonRpcError,
        MethodNotFoundError,
        ParseError,
        RetryLimitExceededError,
        ServerBusyError,
        TransportClosedError,
        is_retryable_error,
    )
    from .retry import retry_on_overload

# Resolve the same owning module once on demand. Do not synthesize wrappers:
# callers, type checkers and pickle keep the original exported object identity.
_LAZY_EXPORTS = {
    "CodexConfig": ".client",
    "Codex": ".api",
    "AsyncCodex": ".api",
    "ApprovalMode": ".api",
    "Sandbox": ".api",
    "ChatgptLoginHandle": ".api",
    "DeviceCodeLoginHandle": ".api",
    "AsyncChatgptLoginHandle": ".api",
    "AsyncDeviceCodeLoginHandle": ".api",
    "Thread": ".api",
    "AsyncThread": ".api",
    "TurnHandle": ".api",
    "AsyncTurnHandle": ".api",
    "TurnResult": ".api",
    "TurnRunError": "._run",
    "Input": ".api",
    "InputItem": ".api",
    "RunInput": ".api",
    "TextInput": ".api",
    "ImageInput": ".api",
    "LocalImageInput": ".api",
    "SkillInput": ".api",
    "MentionInput": ".api",
    "retry_on_overload": ".retry",
    "CodexError": ".errors",
    "TransportClosedError": ".errors",
    "JsonRpcError": ".errors",
    "CodexRpcError": ".errors",
    "ParseError": ".errors",
    "InvalidRequestError": ".errors",
    "MethodNotFoundError": ".errors",
    "InvalidParamsError": ".errors",
    "InternalRpcError": ".errors",
    "ServerBusyError": ".errors",
    "RetryLimitExceededError": ".errors",
    "is_retryable_error": ".errors",
}
_LAZY_SUBMODULES = {"_run", "api", "client", "errors", "models", "retry"}


def __getattr__(name: str) -> _Any:
    module_name = _LAZY_EXPORTS.get(name)
    if module_name is not None:
        value = getattr(_import_module(module_name, __name__), name)
    elif name in _LAZY_SUBMODULES:
        value = _import_module(f".{name}", __name__)
    else:
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}") from None
    globals()[name] = value
    return value


def __dir__() -> list[str]:
    return sorted(set(globals()) | set(__all__) | _LAZY_SUBMODULES)


__all__ = [
    "__version__",
    "CodexConfig",
    "Codex",
    "AsyncCodex",
    "ApprovalMode",
    "Sandbox",
    "ChatgptLoginHandle",
    "DeviceCodeLoginHandle",
    "AsyncChatgptLoginHandle",
    "AsyncDeviceCodeLoginHandle",
    "Thread",
    "AsyncThread",
    "TurnHandle",
    "AsyncTurnHandle",
    "TurnResult",
    "TurnRunError",
    "Input",
    "InputItem",
    "RunInput",
    "TextInput",
    "ImageInput",
    "LocalImageInput",
    "SkillInput",
    "MentionInput",
    "retry_on_overload",
    "CodexError",
    "TransportClosedError",
    "JsonRpcError",
    "CodexRpcError",
    "ParseError",
    "InvalidRequestError",
    "MethodNotFoundError",
    "InvalidParamsError",
    "InternalRpcError",
    "ServerBusyError",
    "RetryLimitExceededError",
    "is_retryable_error",
]
