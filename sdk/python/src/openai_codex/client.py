import json
import math
import os
import queue
import re
import subprocess
import threading
import time
import uuid
from _thread import LockType
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Iterator, TypeVar

from pydantic import BaseModel

from ._goal import _GoalOperationState
from ._message_router import MessageRouter
from ._version import __version__ as SDK_VERSION
from .errors import CodexError, InvalidRequestError, TransportClosedError
from .generated.notification_registry import NOTIFICATION_MODELS
from .generated.v2_all import (
    AccountLoginCompletedNotification,
    AgentMessageDeltaNotification,
    CancelLoginAccountResponse,
    ChatgptDeviceCodeLoginAccountResponse,
    ChatgptLoginAccountResponse,
    GetAccountParams as V2GetAccountParams,
    GetAccountResponse,
    IdleThreadStatus,
    LoginAccountParams as V2LoginAccountParams,
    LoginAccountResponse,
    LogoutAccountResponse,
    ModelListResponse,
    ThreadArchiveResponse,
    ThreadCompactStartResponse,
    ThreadForkParams as V2ThreadForkParams,
    ThreadForkResponse,
    ThreadGoalClearResponse,
    ThreadGoalSetResponse,
    ThreadGoalStatus,
    ThreadListParams as V2ThreadListParams,
    ThreadListResponse,
    ThreadReadResponse,
    ThreadResumeParams as V2ThreadResumeParams,
    ThreadResumeResponse,
    ThreadSetNameResponse,
    ThreadStartParams as V2ThreadStartParams,
    ThreadStartResponse,
    ThreadUnarchiveResponse,
    TurnCompletedNotification,
    TurnInterruptResponse,
    TurnStartParams as V2TurnStartParams,
    TurnStartResponse,
    TurnSteerResponse,
)
from .models import (
    InitializeResponse,
    JsonObject,
    JsonValue,
    Notification,
    UnknownNotification,
)
from .retry import retry_on_overload

ModelT = TypeVar("ModelT", bound=BaseModel)
ApprovalHandler = Callable[[str, JsonObject | None], JsonObject]
RUNTIME_PKG_NAME = "openai-codex-cli-bin"
_GOAL_START_TIMEOUT_S = 30.0
_STDERR_TAIL_MAX_BYTES = 64 * 1024
_STDERR_READ_CHARS = 8 * 1024
_STDERR_DRAIN_JOIN_TIMEOUT_S = 1.0
_STDERR_TRUNCATION_MARKER = f"[stderr truncated; showing last {_STDERR_TAIL_MAX_BYTES} bytes]\n"


@dataclass(slots=True)
class _Operation:
    deadline: float
    cancelled: threading.Event
    proc: subprocess.Popen[str] | None
    router: MessageRouter


@dataclass(slots=True)
class _Write:
    line: str
    done: threading.Event = field(default_factory=threading.Event)
    error: BaseException | None = None


@dataclass(slots=True)
class _ThreadStartLock:
    lock: LockType = field(default_factory=threading.Lock)
    users: int = 0


def _active_turn_id_from_error(exc: InvalidRequestError) -> str | None:
    match = re.search(r" but found `?([^`]+)`?$", exc.message)
    return match.group(1) if match is not None else None


def _params_dict(
    params: (
        V2ThreadStartParams
        | V2ThreadResumeParams
        | V2ThreadListParams
        | V2ThreadForkParams
        | V2TurnStartParams
        | V2GetAccountParams
        | V2LoginAccountParams
        | JsonObject
        | None
    ),
) -> JsonObject:
    if params is None:
        return {}
    if hasattr(params, "model_dump"):
        dumped = params.model_dump(
            by_alias=True,
            exclude_none=True,
            mode="json",
        )
        if not isinstance(dumped, dict):
            raise TypeError("Expected model_dump() to return dict")
        return dumped
    if isinstance(params, dict):
        return params
    raise TypeError(f"Expected generated params model or dict, got {type(params).__name__}")


def _installed_codex_path() -> Path:
    try:
        from codex_cli_bin import bundled_codex_path
    except ImportError as exc:
        raise FileNotFoundError(
            "Unable to locate the pinned Codex runtime. Install the published SDK build "
            f"with its {RUNTIME_PKG_NAME} dependency, or set CodexConfig.codex_bin "
            "explicitly."
        ) from exc

    return bundled_codex_path()


def _installed_codex_path_dirs() -> tuple[Path, ...]:
    from codex_cli_bin import bundled_path_dir

    path_dir = bundled_path_dir()
    return (path_dir,) if path_dir is not None else ()


def _prepend_path_dirs(env: dict[str, str], path_dirs: tuple[Path, ...]) -> None:
    if not path_dirs:
        return

    path_key = _path_env_key(env)
    for key in list(env):
        if key.upper() == "PATH" and key != path_key:
            env.pop(key)

    path_sep = os.pathsep
    existing_path = env.get(path_key, "")
    path_dir_values = [str(path_dir) for path_dir in path_dirs]
    existing_entries = [
        entry for entry in existing_path.split(path_sep) if entry and entry not in path_dir_values
    ]
    env[path_key] = path_sep.join([*path_dir_values, *existing_entries])


def _path_env_key(env: dict[str, str]) -> str:
    matching_keys = [key for key in env if key.upper() == "PATH"]
    if "Path" in matching_keys:
        return "Path"
    return matching_keys[-1] if matching_keys else "PATH"


@dataclass(frozen=True)
class CodexBinResolverOps:
    installed_codex_path: Callable[[], Path]
    path_exists: Callable[[Path], bool]


def _default_codex_bin_resolver_ops() -> CodexBinResolverOps:
    return CodexBinResolverOps(
        installed_codex_path=_installed_codex_path,
        path_exists=lambda path: path.exists(),
    )


def resolve_codex_bin(config: "CodexConfig", ops: CodexBinResolverOps) -> Path:
    if config.codex_bin is not None:
        codex_bin = Path(config.codex_bin)
        if not ops.path_exists(codex_bin):
            raise FileNotFoundError(
                f"Codex binary not found at {codex_bin}. Set CodexConfig.codex_bin "
                "to a valid binary path."
            )
        return codex_bin

    return ops.installed_codex_path()


def _resolve_codex_bin(config: "CodexConfig") -> Path:
    return resolve_codex_bin(config, _default_codex_bin_resolver_ops())


@dataclass(slots=True)
class CodexConfig:
    """Configuration for launching and identifying the local Codex runtime.

    Most callers can use ``Codex()`` without configuration. Set ``codex_bin``
    only when intentionally using a specific local Codex executable.
    """

    codex_bin: str | None = None
    launch_args_override: tuple[str, ...] | None = None
    config_overrides: tuple[str, ...] = ()
    cwd: str | None = None
    env: dict[str, str] | None = None
    client_name: str = "codex_python_sdk"
    client_title: str = "Codex Python SDK"
    client_version: str = SDK_VERSION
    experimental_api: bool = True
    operation_timeout_s: float = 300.0
    shutdown_timeout_s: float = 2.0
    max_message_bytes: int = 8 * 1024 * 1024
    max_buffered_notifications: int = 4096
    max_buffer_bytes: int = 64 * 1024 * 1024
    max_notification_routes: int = 256
    max_in_flight_requests: int = 32

    def __post_init__(self) -> None:
        for name in ("operation_timeout_s", "shutdown_timeout_s"):
            value = getattr(self, name)
            if (
                not isinstance(value, (int, float))
                or isinstance(value, bool)
                or not math.isfinite(value)
                or value <= 0
            ):
                raise ValueError(f"{name} must be finite and > 0")
        for name in (
            "max_message_bytes",
            "max_buffered_notifications",
            "max_buffer_bytes",
            "max_notification_routes",
            "max_in_flight_requests",
        ):
            value = getattr(self, name)
            if not isinstance(value, int) or isinstance(value, bool) or value < 1:
                raise ValueError(f"{name} must be a positive integer")


class CodexClient:
    """Synchronous typed JSON-RPC client for `codex app-server` over stdio."""

    def __init__(
        self,
        config: CodexConfig | None = None,
        approval_handler: ApprovalHandler | None = None,
    ) -> None:
        self.config = config or CodexConfig()
        self._approval_handler = approval_handler or self._default_approval_handler
        self._proc: subprocess.Popen[str] | None = None
        self._process_epoch = 0
        self._lifecycle_lock = threading.Lock()
        self._thread_start_locks_guard = threading.Lock()
        self._thread_start_locks: dict[str, _ThreadStartLock] = {}
        self._router = self._new_router()
        self._operation_local = threading.local()
        self._operation_slots = threading.BoundedSemaphore(self.config.max_in_flight_requests)
        self._closing = False
        self._stopped = threading.Event()
        self._writes: queue.Queue[_Write | None] = queue.Queue(self.config.max_in_flight_requests)
        self._writer_thread: threading.Thread | None = None
        self._stderr_lock = threading.Lock()
        self._stderr_tail_bytes = bytearray()
        self._stderr_truncated = False
        self._stderr_thread: threading.Thread | None = None
        self._reader_thread: threading.Thread | None = None

    def _new_router(self) -> MessageRouter:
        return MessageRouter(
            max_notifications=self.config.max_buffered_notifications,
            max_buffer_bytes=self.config.max_buffer_bytes,
            max_routes=self.config.max_notification_routes,
            max_requests=self.config.max_in_flight_requests,
            operation_timeout_s=self.config.operation_timeout_s,
        )

    @contextmanager
    def _operation(
        self, cancelled: threading.Event | None = None, *, control: bool = False
    ) -> Iterator[_Operation]:
        current = getattr(self._operation_local, "current", None)
        if current is not None:
            yield current
            return
        if not control and not self._operation_slots.acquire(blocking=False):
            raise CodexError("in-flight operation limit exceeded")
        operation = _Operation(
            time.monotonic() + self.config.operation_timeout_s,
            cancelled if cancelled is not None else threading.Event(),
            self._proc,
            self._router,
        )
        self._operation_local.current = operation
        try:
            yield operation
        finally:
            del self._operation_local.current
            if not control:
                self._operation_slots.release()

    def _check_operation(self, operation: _Operation) -> float:
        remaining = operation.deadline - time.monotonic()
        error: BaseException | None = None
        if operation.cancelled.is_set():
            error = TransportClosedError("operation cancelled; transport closed")
        elif remaining <= 0:
            error = TimeoutError("operation deadline exceeded; transport closed")
        if error is not None:
            if operation.proc is not None:
                self._abort_transport(operation.proc, operation.router, error)
            raise error
        return min(remaining, 0.05)

    def _abort_transport(
        self, proc: subprocess.Popen[str], router: MessageRouter, error: BaseException
    ) -> None:
        # Never wait for a pipe owner or acquire its I/O lock here.
        router.fail_all(error)
        with self._lifecycle_lock:
            if proc is self._proc:
                self._stopped.set()
        try:
            proc.terminate()
        except OSError:
            pass

    def __enter__(self) -> "CodexClient":
        self.start()
        return self

    def __exit__(self, _exc_type, _exc, _tb) -> None:
        self.close()

    def start(self) -> None:
        with self._lifecycle_lock:
            if self._closing:
                raise TransportClosedError("Codex process is closing")
            if self._proc is not None:
                if self._stopped.is_set():
                    raise TransportClosedError("failed transport must be closed before restart")
                return
            if any(
                thread is not None and thread.is_alive()
                for thread in (
                    self._reader_thread,
                    self._stderr_thread,
                    self._writer_thread,
                )
            ):
                raise TransportClosedError("previous transport workers have not stopped")

            path_dirs: tuple[Path, ...] = ()
            if self.config.launch_args_override is not None:
                args = list(self.config.launch_args_override)
            else:
                codex_bin = _resolve_codex_bin(self.config)
                if self.config.codex_bin is None:
                    path_dirs = _installed_codex_path_dirs()
                args = [str(codex_bin)]
                for kv in self.config.config_overrides:
                    args.extend(["--config", kv])
                args.extend(["app-server", "--listen", "stdio://"])

            env = os.environ.copy()
            if self.config.env:
                env.update(self.config.env)
            _prepend_path_dirs(env, path_dirs)

            proc = subprocess.Popen(
                args,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="replace",
                cwd=self.config.cwd,
                env=env,
                bufsize=1,
            )
            router = self._new_router()
            self._router = router
            with self._stderr_lock:
                self._stderr_tail_bytes.clear()
                self._stderr_truncated = False
            self._proc = proc
            self._process_epoch += 1
            self._stopped = threading.Event()
            self._writes = queue.Queue(self.config.max_in_flight_requests)
            self._writer_thread = threading.Thread(
                target=self._writer_loop,
                args=(proc, router, self._writes, self._stopped),
                name="codex-stdin",
                daemon=True,
            )
            self._writer_thread.start()
            operation = getattr(self._operation_local, "current", None)
            if operation is not None and operation.proc is None:
                operation.proc, operation.router = proc, router
            self._start_stderr_drain_thread(proc)
            self._start_reader_thread(proc, router)

    def close(self) -> None:
        with self._lifecycle_lock:
            if self._proc is None or self._closing:
                return
            self._closing = True
            proc, router = self._proc, self._router
            stopped, writes = self._stopped, self._writes
            threads = (self._writer_thread, self._stderr_thread, self._reader_thread)
            stopped.set()
        deadline = time.monotonic() + self.config.shutdown_timeout_s
        try:
            router.fail_all(TransportClosedError("Codex process was closed"))
            # The writer owns stdin, including close/flush. Terminate first so a
            # full pipe cannot keep shutdown behind a blocked write or flush.
            while True:
                try:
                    pending = writes.get_nowait()
                except queue.Empty:
                    break
                if pending is not None:
                    pending.error = TransportClosedError("Codex process was closed")
                    pending.done.set()
            writes.put_nowait(None)
            try:
                proc.terminate()
                proc.wait(timeout=max(0.0, (deadline - time.monotonic()) / 2))
            except (OSError, subprocess.TimeoutExpired):
                try:
                    proc.kill()
                except OSError:
                    pass
                try:
                    proc.wait(timeout=max(0.0, deadline - time.monotonic()))
                except subprocess.TimeoutExpired:
                    pass
            for thread in threads:
                if thread is not None and thread is not threading.current_thread():
                    thread.join(timeout=max(0.0, deadline - time.monotonic()))
        finally:
            with self._lifecycle_lock:
                # Keep an unreaped process reachable, and prohibit restart while
                # old pipe owners remain alive (including inherited pipe handles).
                if proc.poll() is not None:
                    self._proc = None
                self._closing = False

    def initialize(self) -> InitializeResponse:
        with self._operation():
            result = self.request(
                "initialize",
                {
                    "clientInfo": {
                        "name": self.config.client_name,
                        "title": self.config.client_title,
                        "version": self.config.client_version,
                    },
                    "capabilities": {
                        "experimentalApi": self.config.experimental_api,
                    },
                },
                response_model=InitializeResponse,
            )
            self.notify("initialized", None)
            return result

    @property
    def process_epoch(self) -> int:
        """Return the transport generation used to reject stale handles."""
        return self._process_epoch

    def request(
        self,
        method: str,
        params: JsonObject | None,
        *,
        response_model: type[ModelT],
    ) -> ModelT:
        result = self._request_raw(method, params)
        if not isinstance(result, dict):
            raise CodexError(f"{method} response must be a JSON object")
        return response_model.model_validate(result)

    def _request_raw(self, method: str, params: JsonObject | None = None) -> JsonValue:
        """Send one bounded JSON-RPC operation through this transport generation."""
        with self._operation() as operation:
            with self._lifecycle_lock:
                proc, router = self._proc, self._router
                if proc is None or self._closing:
                    raise TransportClosedError("Codex process is not running")
                if operation.proc is not None and operation.proc is not proc:
                    raise TransportClosedError("operation belongs to a previous transport")
                operation.proc, operation.router = proc, router
                request_id = str(uuid.uuid4())
                waiter = router.create_response_waiter(request_id)
            try:
                message: JsonObject = {"id": request_id, "method": method}
                if params is not None:
                    message["params"] = params
                self._write_message(message, proc=proc)
                while True:
                    timeout = self._check_operation(operation)
                    router.check_failure()
                    try:
                        item = waiter.get(timeout=timeout)
                        break
                    except queue.Empty:
                        continue
                if isinstance(item, BaseException):
                    raise item
                return item
            finally:
                router.discard_response_waiter(request_id)

    def notify(self, method: str, params: JsonObject | None = None) -> None:
        """Send a JSON-RPC notification without waiting for a response."""
        message: JsonObject = {"method": method}
        if params is not None:
            message["params"] = params
        self._write_message(message)

    def next_notification(self, timeout_s: float | None = None) -> Notification:
        """Return the next notification that is not scoped to an active turn."""
        return self._router.next_global_notification(timeout_s)

    def register_login_notifications(self, login_id: str) -> None:
        """Start routing notifications for one interactive login attempt."""
        operation = getattr(self._operation_local, "current", None)
        self._router.register_login(login_id, deadline=operation.deadline if operation else None)

    def unregister_login_notifications(self, login_id: str) -> None:
        """Stop routing notifications for one interactive login attempt."""
        self._router.unregister_login(login_id)

    def next_login_notification(
        self, login_id: str, timeout_s: float | None = None
    ) -> Notification:
        """Return the next routed notification for the requested login id."""
        return self._router.next_login_notification(login_id, timeout_s)

    def register_turn_notifications(self, turn_id: str) -> None:
        """Start routing notifications for one turn into its dedicated queue."""
        operation = getattr(self._operation_local, "current", None)
        self._router.register_turn(turn_id, deadline=operation.deadline if operation else None)

    def unregister_turn_notifications(self, turn_id: str) -> None:
        """Stop routing notifications for one turn into its dedicated queue."""
        self._router.unregister_turn(turn_id)

    def next_turn_notification(self, turn_id: str, timeout_s: float | None = None) -> Notification:
        """Return the next routed notification for the requested turn id."""
        return self._router.next_turn_notification(turn_id, timeout_s)

    def register_goal_operation(self, thread_id: str) -> _GoalOperationState:
        """Register a private thread-scoped route for a logical goal turn."""
        operation = getattr(self._operation_local, "current", None)
        return self._router.register_goal(
            thread_id, deadline=operation.deadline if operation else None
        )

    def reserve_goal_operation(self, thread_id: str) -> _GoalOperationState:
        """Reserve a private thread route before replacing its stored goal."""
        operation = getattr(self._operation_local, "current", None)
        return self._router.reserve_goal(
            thread_id, deadline=operation.deadline if operation else None
        )

    def unregister_goal_operation(self, state: _GoalOperationState) -> None:
        """Release routing state for one logical goal turn."""
        self._router.unregister_goal(state)

    def next_goal_notification(
        self, state: _GoalOperationState, timeout_s: float | None = None
    ) -> Notification:
        """Wait for the next notification in a logical goal turn."""
        return state.next_notification(timeout_s)

    def account_login_start(
        self,
        params: V2LoginAccountParams | JsonObject,
    ) -> LoginAccountResponse:
        with self._operation():
            response = self.request(
                "account/login/start",
                _params_dict(params),
                response_model=LoginAccountResponse,
            )
            response_root = response.root
            if isinstance(
                response_root,
                ChatgptLoginAccountResponse | ChatgptDeviceCodeLoginAccountResponse,
            ):
                self.register_login_notifications(response_root.login_id)
            return response

    def account_login_cancel(self, login_id: str) -> CancelLoginAccountResponse:
        return self.request(
            "account/login/cancel",
            {"loginId": login_id},
            response_model=CancelLoginAccountResponse,
        )

    def account_read(
        self,
        params: V2GetAccountParams | JsonObject | None = None,
    ) -> GetAccountResponse:
        return self.request(
            "account/read",
            _params_dict(params),
            response_model=GetAccountResponse,
        )

    def account_logout(self) -> LogoutAccountResponse:
        return self.request("account/logout", None, response_model=LogoutAccountResponse)

    def thread_start(
        self, params: V2ThreadStartParams | JsonObject | None = None
    ) -> ThreadStartResponse:
        return self.request(
            "thread/start", _params_dict(params), response_model=ThreadStartResponse
        )

    def thread_resume(
        self,
        thread_id: str,
        params: V2ThreadResumeParams | JsonObject | None = None,
    ) -> ThreadResumeResponse:
        payload = {"threadId": thread_id, **_params_dict(params)}
        return self.request("thread/resume", payload, response_model=ThreadResumeResponse)

    def thread_list(
        self, params: V2ThreadListParams | JsonObject | None = None
    ) -> ThreadListResponse:
        return self.request("thread/list", _params_dict(params), response_model=ThreadListResponse)

    def thread_read(self, thread_id: str, include_turns: bool = False) -> ThreadReadResponse:
        return self.request(
            "thread/read",
            {"threadId": thread_id, "includeTurns": include_turns},
            response_model=ThreadReadResponse,
        )

    def thread_fork(
        self,
        thread_id: str,
        params: V2ThreadForkParams | JsonObject | None = None,
    ) -> ThreadForkResponse:
        payload = {"threadId": thread_id, **_params_dict(params)}
        return self.request("thread/fork", payload, response_model=ThreadForkResponse)

    def thread_archive(self, thread_id: str) -> ThreadArchiveResponse:
        return self.request(
            "thread/archive",
            {"threadId": thread_id},
            response_model=ThreadArchiveResponse,
        )

    def thread_unarchive(self, thread_id: str) -> ThreadUnarchiveResponse:
        return self.request(
            "thread/unarchive",
            {"threadId": thread_id},
            response_model=ThreadUnarchiveResponse,
        )

    def thread_set_name(self, thread_id: str, name: str) -> ThreadSetNameResponse:
        return self.request(
            "thread/name/set",
            {"threadId": thread_id, "name": name},
            response_model=ThreadSetNameResponse,
        )

    def thread_compact(self, thread_id: str) -> ThreadCompactStartResponse:
        return self.request(
            "thread/compact/start",
            {"threadId": thread_id},
            response_model=ThreadCompactStartResponse,
        )

    def thread_goal_clear(self, thread_id: str) -> ThreadGoalClearResponse:
        """Clear the persisted goal for a thread before replacing it."""
        return self.request(
            "thread/goal/clear",
            {"threadId": thread_id},
            response_model=ThreadGoalClearResponse,
        )

    def thread_goal_set(
        self,
        thread_id: str,
        *,
        objective: str | None = None,
        status: ThreadGoalStatus | None = None,
    ) -> ThreadGoalSetResponse:
        """Create or update the persisted goal for a thread."""
        payload: JsonObject = {"threadId": thread_id}
        if objective is not None:
            payload["objective"] = objective
        if status is not None:
            payload["status"] = status.value
        return self.request(
            "thread/goal/set",
            payload,
            response_model=ThreadGoalSetResponse,
        )

    def pause_goal(self, thread_id: str) -> ThreadGoalSetResponse:
        """Pause the active goal used by a logical goal turn."""
        return self.thread_goal_set(thread_id, status=ThreadGoalStatus.paused)

    def cancel_goal_operation(self, state: _GoalOperationState) -> None:
        """Best-effort cleanup after a logical goal operation is cancelled."""
        try:
            self.pause_goal(state.thread_id)
        except Exception:
            pass
        self._interrupt_goal_operation(state)

    def _interrupt_goal_operation(self, state: _GoalOperationState) -> None:
        turn_id = state.turn_for_interrupt()
        if turn_id is None:
            return
        try:
            self.turn_interrupt(state.thread_id, turn_id)
        except InvalidRequestError as exc:
            if not exc.message.startswith("expected active turn id"):
                return
            next_turn_id = _active_turn_id_from_error(exc) or state.current_turn()
            if next_turn_id is None or next_turn_id == turn_id:
                return
            try:
                self.turn_interrupt(state.thread_id, next_turn_id)
            except Exception:
                pass
        except Exception:
            pass

    def start_goal_operation(
        self,
        thread_id: str,
        objective: str,
    ) -> tuple[_GoalOperationState, str]:
        """Start a logical goal and wait for its runtime-generated first turn."""
        with self._thread_start_lock(thread_id):
            return self._start_goal_operation(thread_id, objective)

    def _start_goal_operation(
        self,
        thread_id: str,
        objective: str,
    ) -> tuple[_GoalOperationState, str]:
        thread = self.thread_read(thread_id).thread
        if not isinstance(thread.status.root, IdleThreadStatus):
            raise InvalidRequestError(
                -32600,
                f"thread must be idle before starting a goal: {thread_id}",
            )
        if thread.ephemeral or thread.path is None:
            raise InvalidRequestError(
                -32600,
                f"thread must be persisted before starting a goal: {thread_id}",
            )

        state = self.reserve_goal_operation(thread_id)
        activated = False
        try:
            self.thread_goal_clear(thread_id)
            state.activate_turn_routing()
            self.thread_goal_set(
                thread_id,
                objective=objective,
                status=ThreadGoalStatus.active,
            )
            activated = True
            deadline = time.monotonic() + _GOAL_START_TIMEOUT_S
            operation = self._operation_local.current
            turn_id = None
            while turn_id is None and time.monotonic() < deadline:
                timeout = min(self._check_operation(operation), deadline - time.monotonic())
                turn_id = state.wait_for_start(max(0.0, timeout))
            if turn_id is None:
                raise CodexError(
                    "timed out waiting for goal turn to start after "
                    f"{int(_GOAL_START_TIMEOUT_S)} seconds"
                )
            return state, turn_id
        except BaseException as exc:
            if activated or not isinstance(exc, InvalidRequestError):
                self.cancel_goal_operation(state)
            state.finish()
            self.unregister_goal_operation(state)
            raise

    def turn_start(
        self,
        thread_id: str,
        input_items: list[JsonObject] | JsonObject | str,
        params: V2TurnStartParams | JsonObject | None = None,
    ) -> TurnStartResponse:
        """Start a turn and register its notification queue as early as possible."""
        with self._thread_start_lock(thread_id):
            if self._router.has_goal(thread_id):
                raise InvalidRequestError(
                    -32600,
                    f"thread has an active goal operation: {thread_id}",
                )
            payload = {
                **_params_dict(params),
                "threadId": thread_id,
                "input": self._normalize_input_items(input_items),
            }
            started = self.request("turn/start", payload, response_model=TurnStartResponse)
            self.register_turn_notifications(started.turn.id)
            return started

    @contextmanager
    def _thread_start_lock(self, thread_id: str) -> Iterator[None]:
        with self._operation() as operation:
            with self._thread_start_lock_inner(thread_id, operation):
                yield

    @contextmanager
    def _thread_start_lock_inner(self, thread_id: str, operation: _Operation) -> Iterator[None]:
        with self._thread_start_locks_guard:
            entry = self._thread_start_locks.get(thread_id)
            if entry is None:
                entry = _ThreadStartLock()
                self._thread_start_locks[thread_id] = entry
            entry.users += 1
        try:
            while not entry.lock.acquire(timeout=self._check_operation(operation)):
                pass
            try:
                yield
            finally:
                entry.lock.release()
        finally:
            with self._thread_start_locks_guard:
                entry.users -= 1
                if entry.users == 0:
                    self._thread_start_locks.pop(thread_id, None)

    def turn_interrupt(self, thread_id: str, turn_id: str) -> TurnInterruptResponse:
        return self.request(
            "turn/interrupt",
            {"threadId": thread_id, "turnId": turn_id},
            response_model=TurnInterruptResponse,
        )

    def turn_steer(
        self,
        thread_id: str,
        expected_turn_id: str,
        input_items: list[JsonObject] | JsonObject | str,
    ) -> TurnSteerResponse:
        return self.request(
            "turn/steer",
            {
                "threadId": thread_id,
                "expectedTurnId": expected_turn_id,
                "input": self._normalize_input_items(input_items),
            },
            response_model=TurnSteerResponse,
        )

    def model_list(self, include_hidden: bool = False) -> ModelListResponse:
        return self.request(
            "model/list",
            {"includeHidden": include_hidden},
            response_model=ModelListResponse,
        )

    def request_with_retry_on_overload(
        self,
        method: str,
        params: JsonObject | None,
        *,
        response_model: type[ModelT],
        max_attempts: int = 3,
        initial_delay_s: float = 0.25,
        max_delay_s: float = 2.0,
    ) -> ModelT:
        with self._operation() as operation:
            return retry_on_overload(
                lambda: self.request(method, params, response_model=response_model),
                max_attempts=max_attempts,
                initial_delay_s=initial_delay_s,
                max_delay_s=max_delay_s,
                timeout_s=max(0.0, operation.deadline - time.monotonic()),
                _cancelled=operation.cancelled,
            )

    def wait_for_turn_completed(self, turn_id: str) -> TurnCompletedNotification:
        """Block on the routed turn stream until the matching completion arrives."""
        self.register_turn_notifications(turn_id)
        try:
            while True:
                notification = self.next_turn_notification(turn_id)
                if (
                    notification.method == "turn/completed"
                    and isinstance(notification.payload, TurnCompletedNotification)
                    and notification.payload.turn.id == turn_id
                ):
                    return notification.payload
        finally:
            self.unregister_turn_notifications(turn_id)

    def wait_for_login_completed(
        self,
        login_id: str,
    ) -> AccountLoginCompletedNotification:
        """Block until the matching interactive login attempt completes."""
        self.register_login_notifications(login_id)
        try:
            while True:
                notification = self.next_login_notification(login_id)
                if (
                    notification.method == "account/login/completed"
                    and isinstance(notification.payload, AccountLoginCompletedNotification)
                    and notification.payload.login_id == login_id
                ):
                    return notification.payload
        finally:
            self.unregister_login_notifications(login_id)

    def stream_text(
        self,
        thread_id: str,
        text: str,
        params: V2TurnStartParams | JsonObject | None = None,
    ) -> Iterator[AgentMessageDeltaNotification]:
        """Start a text turn and yield only its agent-message delta payloads."""
        started = self.turn_start(thread_id, text, params=params)
        turn_id = started.turn.id
        self.register_turn_notifications(turn_id)
        try:
            while True:
                notification = self.next_turn_notification(turn_id)
                if (
                    notification.method == "item/agentMessage/delta"
                    and isinstance(notification.payload, AgentMessageDeltaNotification)
                    and notification.payload.turn_id == turn_id
                ):
                    yield notification.payload
                    continue
                if (
                    notification.method == "turn/completed"
                    and isinstance(notification.payload, TurnCompletedNotification)
                    and notification.payload.turn.id == turn_id
                ):
                    break
        finally:
            self.unregister_turn_notifications(turn_id)

    def _coerce_notification(self, method: str, params: object) -> Notification:
        params_dict = params if isinstance(params, dict) else {}

        model = NOTIFICATION_MODELS.get(method)
        if model is None:
            return Notification(method=method, payload=UnknownNotification(params=params_dict))

        try:
            payload = model.model_validate(params_dict)
        except Exception as exc:  # noqa: BLE001
            raise CodexError(f"Invalid payload for known notification {method!r}") from exc
        return Notification(method=method, payload=payload)

    def _normalize_input_items(
        self,
        input_items: list[JsonObject] | JsonObject | str,
    ) -> list[JsonObject]:
        if isinstance(input_items, str):
            return [{"type": "text", "text": input_items}]
        if isinstance(input_items, dict):
            return [input_items]
        return input_items

    def _default_approval_handler(self, method: str, params: JsonObject | None) -> JsonObject:
        """Fail closed unless the caller explicitly installs an approval handler."""
        if method == "item/commandExecution/requestApproval":
            return {"decision": "decline"}
        if method == "item/fileChange/requestApproval":
            return {"decision": "decline"}
        if method == "item/permissions/requestApproval":
            return {"permissions": {}, "scope": "turn"}
        raise CodexError(f"Unsupported server request: {method}")

    def _start_stderr_drain_thread(self, proc: subprocess.Popen[str]) -> None:
        if proc.stderr is None:
            return
        stderr = proc.stderr

        def _drain() -> None:
            try:
                while chunk := stderr.read(_STDERR_READ_CHARS):
                    self._append_stderr(chunk)
            finally:
                stderr.close()

        self._stderr_thread = threading.Thread(target=_drain, daemon=True)
        self._stderr_thread.start()

    def _start_reader_thread(self, proc: subprocess.Popen[str], router: MessageRouter) -> None:
        """Start the sole stdout reader that fans messages into router queues."""
        if proc.stdout is None:
            return

        self._reader_thread = threading.Thread(
            target=self._reader_loop, args=(proc, router), daemon=True
        )
        self._reader_thread.start()

    def _reader_loop(
        self,
        proc: subprocess.Popen[str] | None = None,
        router: MessageRouter | None = None,
    ) -> None:
        """Continuously classify transport messages into requests, responses, and events."""
        active_router = router or self._router
        try:
            while True:
                msg = self._read_message(proc)
                if "method" in msg and "id" in msg:
                    response = self._handle_server_request(msg)
                    # The single reader has reserved control admission: an
                    # approval reply must not be rejected by saturated RPC slots.
                    with self._operation(control=True):
                        self._write_message({"id": msg["id"], "result": response}, proc=proc)
                    continue
                if "method" in msg and "id" not in msg:
                    method = msg["method"]
                    if isinstance(method, str):
                        active_router.route_notification(
                            self._coerce_notification(method, msg.get("params"))
                        )
                    continue
                active_router.route_response(msg)
        except BaseException as exc:
            active_router.fail_all(exc)
            if proc is not None:
                self._abort_transport(proc, active_router, exc)
        finally:
            if proc is not None and proc.stdout is not None:
                proc.stdout.close()

    def _append_stderr(self, chunk: str) -> None:
        encoded = chunk.encode("utf-8")
        if not encoded:
            return

        with self._stderr_lock:
            if len(encoded) >= _STDERR_TAIL_MAX_BYTES:
                self._stderr_truncated = (
                    self._stderr_truncated
                    or bool(self._stderr_tail_bytes)
                    or len(encoded) > _STDERR_TAIL_MAX_BYTES
                )
                self._stderr_tail_bytes = bytearray(encoded[-_STDERR_TAIL_MAX_BYTES:])
                return

            overflow = len(self._stderr_tail_bytes) + len(encoded) - _STDERR_TAIL_MAX_BYTES
            if overflow > 0:
                self._stderr_truncated = True
                del self._stderr_tail_bytes[:overflow]
            self._stderr_tail_bytes.extend(encoded)

    def _stderr_tail(self) -> str:
        with self._stderr_lock:
            tail = bytes(self._stderr_tail_bytes)
            truncated = self._stderr_truncated

        start = 0
        if truncated:
            while start < min(3, len(tail)) and tail[start] & 0xC0 == 0x80:
                start += 1
        rendered = tail[start:].decode("utf-8", errors="replace")
        return f"{_STDERR_TRUNCATION_MARKER}{rendered}" if truncated else rendered

    def _handle_server_request(self, msg: dict[str, JsonValue]) -> JsonObject:
        method = msg["method"]
        params = msg.get("params")
        if not isinstance(method, str):
            return {}
        return self._approval_handler(
            method,
            params if isinstance(params, dict) else None,
        )

    def _writer_loop(
        self,
        proc: subprocess.Popen[str],
        router: MessageRouter,
        writes: queue.Queue[_Write | None],
        stopped: threading.Event,
    ) -> None:
        try:
            while not stopped.is_set():
                try:
                    pending = writes.get(timeout=0.05)
                except queue.Empty:
                    continue
                if pending is None:
                    break
                try:
                    if stopped.is_set() or proc.stdin is None:
                        raise TransportClosedError("Codex process was closed")
                    proc.stdin.write(pending.line)
                    proc.stdin.flush()
                except BaseException as exc:
                    pending.error = exc
                    self._abort_transport(proc, router, exc)
                    break
                finally:
                    pending.done.set()
        finally:
            if proc.stdin is not None:
                try:
                    proc.stdin.close()
                except OSError:
                    pass

    def _write_message(
        self, payload: JsonObject, *, proc: subprocess.Popen[str] | None = None
    ) -> None:
        with self._operation() as operation:
            line = json.dumps(payload) + "\n"
            if len(line.encode("utf-8")) > self.config.max_message_bytes:
                raise CodexError("outbound message size limit exceeded")
            pending = _Write(line)
            with self._lifecycle_lock:
                target = proc or self._proc
                if (
                    target is None
                    or target is not self._proc
                    or self._closing
                    or self._stopped.is_set()
                ):
                    raise TransportClosedError("Codex process is not running")
                if operation.proc is not None and operation.proc is not target:
                    raise TransportClosedError("operation belongs to a previous transport")
                operation.proc, operation.router = target, self._router
                self._check_operation_before_write(operation)
                try:
                    self._writes.put_nowait(pending)
                except queue.Full as exc:
                    raise CodexError("outbound message queue limit exceeded") from exc
            while not pending.done.wait(self._check_operation(operation)):
                operation.router.check_failure()
                if self._stopped.is_set():
                    raise TransportClosedError("Codex process was closed during write")
            if pending.error is not None:
                raise pending.error

    def _check_operation_before_write(self, operation: _Operation) -> None:
        # Called under the lifecycle lock: do not invoke transport shutdown here.
        if operation.cancelled.is_set():
            raise TransportClosedError("operation cancelled before write")
        if time.monotonic() >= operation.deadline:
            raise TimeoutError("operation deadline exceeded before write")

    def _read_message(self, proc: subprocess.Popen[str] | None = None) -> dict[str, JsonValue]:
        target = proc or self._proc
        if target is None or target.stdout is None:
            raise TransportClosedError("Codex process is not running")

        line = target.stdout.readline(self.config.max_message_bytes + 1)
        if len(line.encode("utf-8")) > self.config.max_message_bytes:
            raise CodexError("inbound message size limit exceeded")
        if not line:
            stderr_thread = self._stderr_thread
            if stderr_thread is not None and stderr_thread is not threading.current_thread():
                stderr_thread.join(timeout=_STDERR_DRAIN_JOIN_TIMEOUT_S)
            raise TransportClosedError(
                f"Codex process closed stdout. stderr_tail={self._stderr_tail()}"
            )

        try:
            message = json.loads(line)
        except json.JSONDecodeError as exc:
            raise CodexError(f"Invalid JSON-RPC line: {line!r}") from exc

        if not isinstance(message, dict):
            raise CodexError(f"Invalid JSON-RPC payload: {message!r}")
        return message


def default_codex_home() -> str:
    return str(Path.home() / ".codex")
