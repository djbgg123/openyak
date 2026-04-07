from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from ._models import JsonValue, ThreadResyncRequiredEvent, ThreadSnapshot


class OpenyakError(Exception):
    """Base class for SDK errors."""


@dataclass(slots=True)
class OpenyakApiError(OpenyakError):
    status: int
    code: str
    message: str
    details: JsonValue | None = None

    def __str__(self) -> str:
        return f"{self.code}: {self.message}"


@dataclass(slots=True)
class OpenyakCompatibilityError(OpenyakError):
    message: str
    expected: str
    received: str | None = None

    def __str__(self) -> str:
        return self.message


class OpenyakProtocolError(OpenyakError):
    """Raised when the server response cannot be decoded as the locked contract."""


@dataclass(slots=True)
class OpenyakResyncRequiredError(OpenyakError):
    event: ThreadResyncRequiredEvent

    def __str__(self) -> str:
        return (
            f"thread.resync_required for {self.event.thread_id} after skipping "
            f"{self.event.payload.skipped} events"
        )


@dataclass(slots=True)
class OpenyakReconnectRequiredError(OpenyakError):
    thread_id: str
    run_id: str
    latest_snapshot: ThreadSnapshot | None = None

    def __str__(self) -> str:
        return (
            f"stream disconnected before {self.run_id} reached a terminal event; replay is "
            "unavailable on the current local server contract"
        )
