from __future__ import annotations

import json
import os
import tempfile
from dataclasses import asdict, dataclass
from pathlib import Path


@dataclass(frozen=True)
class StoredSession:
    session_id: str
    messages: tuple[str, ...]
    input_tokens: int
    output_tokens: int



def _default_session_dir() -> Path:
    override = os.environ.get('OPENYAK_PORT_SESSION_DIR')
    if override:
        return Path(override).expanduser()
    return Path(tempfile.gettempdir()) / 'openyak-port-sessions'


DEFAULT_SESSION_DIR = _default_session_dir()


def _session_path(session_id: str, directory: Path | None = None) -> Path:
    if not session_id.strip():
        raise ValueError('session_id must not be empty')
    target_dir = (directory or DEFAULT_SESSION_DIR).expanduser()
    resolved_dir = target_dir.resolve(strict=False)
    path = (target_dir / f'{session_id}.json').resolve(strict=False)
    if path.parent != resolved_dir:
        raise ValueError('session_id must resolve to a file directly inside the session directory')
    return path


def save_session(session: StoredSession, directory: Path | None = None) -> Path:
    path = _session_path(session.session_id, directory)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(asdict(session), indent=2), encoding='utf-8')
    return path


def load_session(session_id: str, directory: Path | None = None) -> StoredSession:
    data = json.loads(_session_path(session_id, directory).read_text(encoding='utf-8'))
    return StoredSession(
        session_id=data['session_id'],
        messages=tuple(data['messages']),
        input_tokens=data['input_tokens'],
        output_tokens=data['output_tokens'],
    )
