from __future__ import annotations

from collections.abc import AsyncIterable, AsyncIterator, Iterable, Iterator
from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class SseFrame:
    event: str | None
    data: str


def iter_sse_frames(lines: Iterable[str]) -> Iterator[SseFrame]:
    data_lines: list[str] = []
    event_name: str | None = None
    for raw_line in lines:
        line = raw_line.rstrip("\r")
        if line == "":
            if data_lines:
                yield SseFrame(event=event_name, data="\n".join(data_lines))
            data_lines = []
            event_name = None
            continue
        if line.startswith(":"):
            continue
        if line.startswith("event:"):
            event_name = line[6:].strip()
            continue
        if line.startswith("data:"):
            data_lines.append(line[5:].lstrip())
    if data_lines:
        yield SseFrame(event=event_name, data="\n".join(data_lines))


async def aiter_sse_frames(lines: AsyncIterable[str]) -> AsyncIterator[SseFrame]:
    data_lines: list[str] = []
    event_name: str | None = None
    async for raw_line in lines:
        line = raw_line.rstrip("\r")
        if line == "":
            if data_lines:
                yield SseFrame(event=event_name, data="\n".join(data_lines))
            data_lines = []
            event_name = None
            continue
        if line.startswith(":"):
            continue
        if line.startswith("event:"):
            event_name = line[6:].strip()
            continue
        if line.startswith("data:"):
            data_lines.append(line[5:].lstrip())
    if data_lines:
        yield SseFrame(event=event_name, data="\n".join(data_lines))
