from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any

import httpx


@dataclass(slots=True)
class QueuedResponse:
    status_code: int = 200
    headers: dict[str, str] = field(default_factory=dict)
    content: bytes = b""

    def build(self, request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            self.status_code,
            headers=self.headers,
            content=self.content,
            request=request,
        )


def create_queued_transport(responses: list[QueuedResponse]) -> httpx.MockTransport:
    queue = list(responses)

    def handler(request: httpx.Request) -> httpx.Response:
        if not queue:
            raise AssertionError("unexpected httpx request with no queued response")
        return queue.pop(0).build(request)

    return httpx.MockTransport(handler)


class AsyncQueuedTransport(httpx.AsyncBaseTransport):
    def __init__(self, responses: list[QueuedResponse]) -> None:
        self._queue = list(responses)

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        if not self._queue:
            raise AssertionError("unexpected async httpx request with no queued response")
        return self._queue.pop(0).build(request)


def create_async_queued_transport(responses: list[QueuedResponse]) -> AsyncQueuedTransport:
    return AsyncQueuedTransport(responses)


def json_response(body: Any, *, status_code: int = 200) -> QueuedResponse:
    return QueuedResponse(
        status_code=status_code,
        headers={"content-type": "application/json"},
        content=json.dumps(body).encode("utf-8"),
    )


def sse_response(events: list[Any]) -> QueuedResponse:
    body = "".join(f"data: {json.dumps(event)}\n\n" for event in events)
    return QueuedResponse(
        status_code=200,
        headers={"content-type": "text/event-stream"},
        content=body.encode("utf-8"),
    )
