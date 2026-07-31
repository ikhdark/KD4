from __future__ import annotations

from dataclasses import dataclass
from typing import TypeAlias

from pydantic import BaseModel

from .generated.notification_registry import GeneratedNotificationPayload

JsonScalar: TypeAlias = str | int | float | bool | None
JsonValue: TypeAlias = JsonScalar | dict[str, "JsonValue"] | list["JsonValue"]
JsonObject: TypeAlias = dict[str, JsonValue]


@dataclass(slots=True)
class UnknownNotification:
    params: JsonObject


NotificationPayload: TypeAlias = GeneratedNotificationPayload | UnknownNotification


@dataclass(slots=True)
class Notification:
    method: str
    payload: NotificationPayload


class ServerInfo(BaseModel):
    name: str | None = None
    version: str | None = None


class InitializeResponse(BaseModel):
    serverInfo: ServerInfo | None = None
    userAgent: str | None = None
    platformFamily: str | None = None
    platformOs: str | None = None
