from __future__ import annotations

from dataclasses import dataclass, field
from typing import Protocol

from ._limits import _NotificationQueue
from ._message_router import MessageRouter
from .async_client import AsyncCodexClient
from .client import CodexClient
from .errors import TransportClosedError
from .generated.v2_all import (
    AccountLoginCompletedNotification,
    CancelLoginAccountResponse,
    ChatgptDeviceCodeLoginAccountParams,
    ChatgptDeviceCodeLoginAccountResponse,
    ChatgptLoginAccountParams,
    ChatgptLoginAccountResponse,
    LoginAccountParams,
)
from .models import Notification


def _retain_login_owner(
    client: CodexClient, login_id: str
) -> tuple[MessageRouter, _NotificationQueue, int]:
    with client._operation() as operation:
        router = operation.router
        return router, router.retain_login(login_id), operation.epoch


def _ensure_current_login(client: CodexClient, epoch: int) -> None:
    if client.process_epoch != epoch or client._proc is None:
        raise TransportClosedError("login handle belongs to a closed Codex process")


def _login_completion(
    notification: Notification, login_id: str
) -> AccountLoginCompletedNotification | None:
    payload = notification.payload
    if (
        notification.method == "account/login/completed"
        and isinstance(payload, AccountLoginCompletedNotification)
        and payload.login_id == login_id
    ):
        return payload
    return None


def _wait_for_login(
    notifications: _NotificationQueue, login_id: str
) -> AccountLoginCompletedNotification:
    while True:
        completed = _login_completion(notifications.get(), login_id)
        if completed is not None:
            return completed


async def _wait_for_login_async(
    notifications: _NotificationQueue, login_id: str
) -> AccountLoginCompletedNotification:
    while True:
        completed = _login_completion(await notifications.get_async(), login_id)
        if completed is not None:
            return completed


class _AsyncLoginOwner(Protocol):
    """Subset of AsyncCodex needed by async login handles."""

    _client: AsyncCodexClient


def start_chatgpt_login(client: CodexClient) -> ChatgptLoginHandle:
    """Start browser ChatGPT login and return the handle for that attempt."""
    with client._operation() as operation:
        operation.generation_bound = True
        response = client.account_login_start(
            LoginAccountParams(
                root=ChatgptLoginAccountParams(type="chatgpt"),
            )
        )
        response_root = response.root
        if not isinstance(response_root, ChatgptLoginAccountResponse):
            raise RuntimeError(f"unexpected ChatGPT login response: {response_root!r}")
        return ChatgptLoginHandle(
            client,
            response_root.login_id,
            response_root.auth_url,
        )


async def async_start_chatgpt_login(owner: _AsyncLoginOwner) -> AsyncChatgptLoginHandle:
    """Start async browser ChatGPT login and return that attempt's handle."""
    def start() -> AsyncChatgptLoginHandle:
        with owner._client._sync._operation() as operation:
            operation.generation_bound = True
            response = owner._client._sync.account_login_start(
                LoginAccountParams(
                    root=ChatgptLoginAccountParams(type="chatgpt"),
                )
            )
            response_root = response.root
            if not isinstance(response_root, ChatgptLoginAccountResponse):
                raise RuntimeError(f"unexpected ChatGPT login response: {response_root!r}")
            return AsyncChatgptLoginHandle(
                owner,
                response_root.login_id,
                response_root.auth_url,
            )

    return await owner._client._run_sync(start, on_cancel=lambda handle: handle.close())


def start_device_code_login(client: CodexClient) -> DeviceCodeLoginHandle:
    """Start device-code ChatGPT login and return the handle for that attempt."""
    with client._operation() as operation:
        operation.generation_bound = True
        response = client.account_login_start(
            LoginAccountParams(
                root=ChatgptDeviceCodeLoginAccountParams(type="chatgptDeviceCode"),
            )
        )
        response_root = response.root
        if not isinstance(response_root, ChatgptDeviceCodeLoginAccountResponse):
            raise RuntimeError(f"unexpected device-code login response: {response_root!r}")
        return DeviceCodeLoginHandle(
            client,
            response_root.login_id,
            response_root.verification_url,
            response_root.user_code,
        )


async def async_start_device_code_login(
    owner: _AsyncLoginOwner,
) -> AsyncDeviceCodeLoginHandle:
    """Start async device-code ChatGPT login and return that attempt's handle."""
    def start() -> AsyncDeviceCodeLoginHandle:
        with owner._client._sync._operation() as operation:
            operation.generation_bound = True
            response = owner._client._sync.account_login_start(
                LoginAccountParams(
                    root=ChatgptDeviceCodeLoginAccountParams(type="chatgptDeviceCode"),
                )
            )
            response_root = response.root
            if not isinstance(response_root, ChatgptDeviceCodeLoginAccountResponse):
                raise RuntimeError(f"unexpected device-code login response: {response_root!r}")
            return AsyncDeviceCodeLoginHandle(
                owner,
                response_root.login_id,
                response_root.verification_url,
                response_root.user_code,
            )

    return await owner._client._run_sync(start, on_cancel=lambda handle: handle.close())

@dataclass(slots=True)
class ChatgptLoginHandle:
    """Live browser-login attempt returned by `Codex.login_chatgpt()`."""

    _client: CodexClient
    login_id: str
    auth_url: str
    _notifications: _NotificationQueue = field(init=False, repr=False)
    _router: MessageRouter = field(init=False, repr=False)
    _epoch: int = field(init=False, repr=False)

    def __post_init__(self) -> None:
        self._router, self._notifications, self._epoch = _retain_login_owner(
            self._client, self.login_id
        )

    def wait(self) -> AccountLoginCompletedNotification:
        """Wait for this browser login attempt's completion notification."""
        try:
            return _wait_for_login(self._notifications, self.login_id)
        finally:
            self.close()

    def cancel(self) -> CancelLoginAccountResponse:
        """Cancel this browser login attempt."""
        with self._client._operation():
            _ensure_current_login(self._client, self._epoch)
            return self._client.account_login_cancel(self.login_id)

    def close(self) -> None:
        """Discard this handle's notifications without changing server login state."""
        self._router.unregister_login(self.login_id, expected=self._notifications)
        self._notifications.fail(TransportClosedError("login handle closed"))


@dataclass(slots=True)
class DeviceCodeLoginHandle:
    """Live device-code login attempt returned by `Codex.login_chatgpt_device_code()`."""

    _client: CodexClient
    login_id: str
    verification_url: str
    user_code: str
    _notifications: _NotificationQueue = field(init=False, repr=False)
    _router: MessageRouter = field(init=False, repr=False)
    _epoch: int = field(init=False, repr=False)

    def __post_init__(self) -> None:
        self._router, self._notifications, self._epoch = _retain_login_owner(
            self._client, self.login_id
        )

    def wait(self) -> AccountLoginCompletedNotification:
        """Wait for this device-code login attempt's completion notification."""
        try:
            return _wait_for_login(self._notifications, self.login_id)
        finally:
            self.close()

    def cancel(self) -> CancelLoginAccountResponse:
        """Cancel this device-code login attempt."""
        with self._client._operation():
            _ensure_current_login(self._client, self._epoch)
            return self._client.account_login_cancel(self.login_id)

    def close(self) -> None:
        """Discard this handle's notifications without changing server login state."""
        self._router.unregister_login(self.login_id, expected=self._notifications)
        self._notifications.fail(TransportClosedError("login handle closed"))


@dataclass(slots=True)
class AsyncChatgptLoginHandle:
    """Live browser-login attempt returned by `AsyncCodex.login_chatgpt()`."""

    _codex: _AsyncLoginOwner
    login_id: str
    auth_url: str
    _notifications: _NotificationQueue = field(init=False, repr=False)
    _router: MessageRouter = field(init=False, repr=False)
    _epoch: int = field(init=False, repr=False)

    def __post_init__(self) -> None:
        self._router, self._notifications, self._epoch = _retain_login_owner(
            self._codex._client._sync, self.login_id
        )

    async def wait(self) -> AccountLoginCompletedNotification:
        """Wait for this browser login attempt's completion notification."""
        try:
            return await _wait_for_login_async(self._notifications, self.login_id)
        finally:
            self.close()

    async def cancel(self) -> CancelLoginAccountResponse:
        """Cancel this browser login attempt."""
        return await self._codex._client._run_sync(
            lambda: self._codex._client._sync.account_login_cancel(self.login_id),
            expected_epoch=self._epoch,
        )

    def close(self) -> None:
        """Discard this handle's notifications without changing server login state."""
        self._router.unregister_login(self.login_id, expected=self._notifications)
        self._notifications.fail(TransportClosedError("login handle closed"))


@dataclass(slots=True)
class AsyncDeviceCodeLoginHandle:
    """Live device-code attempt returned by `AsyncCodex.login_chatgpt_device_code()`."""

    _codex: _AsyncLoginOwner
    login_id: str
    verification_url: str
    user_code: str
    _notifications: _NotificationQueue = field(init=False, repr=False)
    _router: MessageRouter = field(init=False, repr=False)
    _epoch: int = field(init=False, repr=False)

    def __post_init__(self) -> None:
        self._router, self._notifications, self._epoch = _retain_login_owner(
            self._codex._client._sync, self.login_id
        )

    async def wait(self) -> AccountLoginCompletedNotification:
        """Wait for this device-code login attempt's completion notification."""
        try:
            return await _wait_for_login_async(self._notifications, self.login_id)
        finally:
            self.close()

    async def cancel(self) -> CancelLoginAccountResponse:
        """Cancel this device-code login attempt."""
        return await self._codex._client._run_sync(
            lambda: self._codex._client._sync.account_login_cancel(self.login_id),
            expected_epoch=self._epoch,
        )

    def close(self) -> None:
        """Discard this handle's notifications without changing server login state."""
        self._router.unregister_login(self.login_id, expected=self._notifications)
        self._notifications.fail(TransportClosedError("login handle closed"))
