#!/usr/bin/env python3
"""Expose Cursor's official CLI chats through the minimal ACP surface OpenAB uses."""

from __future__ import annotations

import base64
import binascii
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import threading
import time
from typing import Any, NamedTuple


CHAT_ID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)
CURSOR_AGENT = os.environ.get("CURSOR_AGENT_BIN", "cursor-agent")
WRITE_LOCK = threading.Lock()
ACTIVE_LOCK = threading.Lock()
SESSIONS: dict[str, str] = {}
ACTIVE: "ActivePrompt | None" = None
MAX_IMAGE_BYTES = 10 * 1024 * 1024
PARTIAL_DUPLICATE_WINDOW_MS = 2_000
MAX_RECENT_PARTIAL_CHUNKS = 32
MIN_AMBIGUOUS_DUPLICATE_CHARS = 12
MAX_SNAPSHOT_COMPARE_CHARS = 16_384
IMAGE_ROOT = Path(
    os.environ.get(
        "OPENAB_CURSOR_IMAGE_DIR",
        str(Path.home() / ".openab" / "cursor-cli-images"),
    )
).expanduser()
IMAGE_TYPES = {
    "image/gif": (".gif", lambda data: data.startswith((b"GIF87a", b"GIF89a"))),
    "image/jpeg": (".jpg", lambda data: data.startswith(b"\xff\xd8\xff")),
    "image/png": (".png", lambda data: data.startswith(b"\x89PNG\r\n\x1a\n")),
    "image/webp": (
        ".webp",
        lambda data: len(data) >= 12 and data[:4] == b"RIFF" and data[8:12] == b"WEBP",
    ),
}


class BridgeError(RuntimeError):
    pass


class PreparedPrompt(NamedTuple):
    text: str
    additional_dirs: tuple[str, ...]


class ActivePrompt:
    def __init__(self, request_id: Any, session_id: str) -> None:
        self.request_id = request_id
        self.session_id = session_id
        self.cancelled = threading.Event()
        self.process: subprocess.Popen[str] | None = None

    def cancel(self) -> None:
        self.cancelled.set()
        process = self.process
        if process is None or process.poll() is not None:
            return
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError):
            process.terminate()


def write_message(value: dict[str, Any]) -> None:
    with WRITE_LOCK:
        sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def reply(request_id: Any, result: dict[str, Any]) -> None:
    write_message({"jsonrpc": "2.0", "id": request_id, "result": result})


def error(request_id: Any, code: int, message: str) -> None:
    write_message(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": code, "message": message},
        }
    )


def update(session_id: str, value: dict[str, Any]) -> None:
    write_message(
        {
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {"sessionId": session_id, "update": value},
        }
    )


def validate_cwd(raw: Any) -> str:
    if not isinstance(raw, str) or not raw:
        raise BridgeError("cwd must be a non-empty absolute path")
    path = Path(raw)
    if not path.is_absolute() or not path.is_dir():
        raise BridgeError("cwd must be an existing absolute directory")
    return str(path.resolve())


def validate_chat_id(raw: Any) -> str:
    if not isinstance(raw, str) or not CHAT_ID_RE.fullmatch(raw):
        raise BridgeError("invalid Cursor chat ID")
    return raw.lower()


def chat_directories(session_id: str) -> list[Path]:
    root = Path.home() / ".cursor" / "chats"
    return [
        path.parent for path in root.glob(f"*/{session_id}/store.db") if path.is_file()
    ]


def verify_chat(session_id: str, cwd: str) -> None:
    matches = chat_directories(session_id)
    if len(matches) != 1:
        raise BridgeError("Cursor CLI chat checkpoint was not found")
    metadata_path = matches[0] / "meta.json"
    try:
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise BridgeError("Cursor CLI chat metadata is unavailable") from exc
    recorded_cwd = metadata.get("cwd")
    if not isinstance(recorded_cwd, str) or Path(recorded_cwd).resolve() != Path(cwd):
        raise BridgeError("Cursor CLI chat belongs to a different workspace")


def create_chat(cwd: str) -> str:
    command = [CURSOR_AGENT, "--workspace", cwd, "create-chat"]
    last_error: Exception | None = None
    for attempt in range(3):
        try:
            completed = subprocess.run(
                command,
                cwd=cwd,
                check=True,
                capture_output=True,
                text=True,
                timeout=60,
            )
            candidates = [line.strip() for line in completed.stdout.splitlines()]
            session_id = next(
                (
                    candidate.lower()
                    for candidate in candidates
                    if CHAT_ID_RE.fullmatch(candidate)
                ),
                None,
            )
            if session_id is None:
                raise BridgeError("Cursor CLI did not return a chat ID")
            # `create-chat` reserves the ID; Cursor writes the store and metadata
            # only when the first prompt completes.
            return session_id
        except (OSError, subprocess.SubprocessError, BridgeError) as exc:
            last_error = exc
            if attempt < 2:
                time.sleep(0.5 * (attempt + 1))
    raise BridgeError("Cursor CLI could not create a chat") from last_error


def persist_image(
    block: dict[str, Any], session_id: str, image_root: Path = IMAGE_ROOT
) -> Path:
    media_type = block.get("mimeType")
    data = block.get("data")
    if not isinstance(media_type, str) or media_type not in IMAGE_TYPES:
        raise BridgeError("image prompt has an unsupported MIME type")
    if not isinstance(data, str) or not data:
        raise BridgeError("image prompt has no base64 data")
    if len(data) > (MAX_IMAGE_BYTES * 4 // 3) + 8:
        raise BridgeError("image prompt exceeds the 10 MB limit")
    try:
        raw = base64.b64decode(data, validate=True)
    except (binascii.Error, ValueError) as exc:
        raise BridgeError("image prompt contains invalid base64 data") from exc
    if len(raw) > MAX_IMAGE_BYTES:
        raise BridgeError("image prompt exceeds the 10 MB limit")

    extension, has_valid_magic = IMAGE_TYPES[media_type]
    if not has_valid_magic(raw):
        raise BridgeError("image prompt content does not match its MIME type")

    session_dir = image_root.resolve() / validate_chat_id(session_id)
    session_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    session_dir.chmod(0o700)
    digest = hashlib.sha256(raw).hexdigest()
    image_path = session_dir / f"{digest}{extension}"
    if not image_path.exists():
        temporary_path = session_dir / (
            f".{digest}.{os.getpid()}.{threading.get_ident()}.tmp"
        )
        try:
            temporary_path.write_bytes(raw)
            temporary_path.chmod(0o600)
            os.replace(temporary_path, image_path)
        finally:
            temporary_path.unlink(missing_ok=True)
    return image_path


def prepare_prompt(
    blocks: Any, session_id: str, image_root: Path = IMAGE_ROOT
) -> PreparedPrompt:
    if not isinstance(blocks, list):
        raise BridgeError("prompt must be an array")
    parts: list[str] = []
    additional_dirs: list[str] = []
    for block in blocks:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text" and isinstance(block.get("text"), str):
            parts.append(block["text"])
        elif block.get("type") == "resource_link" and isinstance(block.get("uri"), str):
            parts.append(block["uri"])
        elif block.get("type") == "image":
            image_path = persist_image(block, session_id, image_root)
            parts.append(
                "[Attached image available as a local file]\n"
                f"path: {image_path}\n"
                f"media_type: {block.get('mimeType')}\n"
                "Inspect this image as part of the user's request."
            )
            image_dir = str(image_path.parent)
            if image_dir not in additional_dirs:
                additional_dirs.append(image_dir)
        else:
            raise BridgeError(
                "this Cursor CLI bridge does not support this prompt content type"
            )
    text = "\n".join(part for part in parts if part)
    if not text:
        raise BridgeError("prompt contains no text")
    return PreparedPrompt(text=text, additional_dirs=tuple(additional_dirs))


def tool_title(value: Any) -> str:
    if not isinstance(value, dict):
        return "Cursor tool"
    tool_call = value.get("tool_call") or value.get("toolCall")
    if isinstance(tool_call, dict) and tool_call:
        return str(next(iter(tool_call))).removesuffix("ToolCall")
    return str(value.get("name") or value.get("tool_name") or "Cursor tool")


def is_duplicate_partial_chunk(
    event: dict[str, Any], text: str, state: dict[str, Any]
) -> bool:
    """Detect Cursor's duplicate partial event forms without dropping real deltas."""
    timestamp = event.get("timestamp_ms")
    if not isinstance(timestamp, int):
        return False
    model_call_id = event.get("model_call_id")
    if not isinstance(model_call_id, str):
        model_call_id = None

    # Cursor can replay a completed partial chunk after a slow tool call or a
    # delayed stream event. In that case the duplicate may arrive well outside
    # the short event-form window below. Remember the immediately preceding
    # timestamped chunk for the whole turn (tool events intentionally do not
    # clear it) and suppress exact, non-trivial replays. Short repeated deltas
    # such as "ha" remain untouched.
    previous = state.get("last_partial_chunk")
    state["last_partial_chunk"] = (timestamp, model_call_id, text)
    if isinstance(previous, tuple) and len(previous) == 3 and previous[2] == text:
        previous_timestamp, previous_model_call_id, _ = previous
        is_long_chunk = len(text.strip()) >= MIN_AMBIGUOUS_DUPLICATE_CHARS
        is_same_event = (
            previous_timestamp == timestamp
            and previous_model_call_id == model_call_id
        )
        is_adjacent_form_pair = (
            abs(timestamp - previous_timestamp) <= PARTIAL_DUPLICATE_WINDOW_MS
            and ((previous_model_call_id is None) != (model_call_id is None))
        )
        # Short deltas such as `/` legitimately recur within a path. Treat the
        # two Cursor event forms as duplicates only when they are adjacent;
        # matching an older opposite-form entry can otherwise erase a later
        # path separator. Long chunks remain safe to suppress when replayed
        # after tools or other delayed events.
        if is_long_chunk or is_same_event or is_adjacent_form_pair:
            return True

    recent = state.setdefault("recent_partial_chunks", [])
    cutoff = timestamp - PARTIAL_DUPLICATE_WINDOW_MS
    recent[:] = [item for item in recent if item[0] >= cutoff]
    duplicate = len(text.strip()) >= MIN_AMBIGUOUS_DUPLICATE_CHARS and any(
        previous_text == text
        and abs(timestamp - previous_timestamp) <= PARTIAL_DUPLICATE_WINDOW_MS
        and (
            (
                previous_timestamp == timestamp
                and previous_model_call_id == model_call_id
            )
            or ((previous_model_call_id is None) != (model_call_id is None))
            or len(text.strip()) >= MIN_AMBIGUOUS_DUPLICATE_CHARS
        )
        for previous_timestamp, previous_model_call_id, previous_text in recent
    )
    recent.append((timestamp, model_call_id, text))
    del recent[:-MAX_RECENT_PARTIAL_CHUNKS]
    return duplicate


def significant_suffix_prefix_overlap(previous: str, current: str) -> int:
    """Return a long overlap where a cumulative Cursor chunk repeats prior text."""
    max_overlap = min(len(previous), len(current), MAX_SNAPSHOT_COMPARE_CHARS)
    for size in range(max_overlap, MIN_AMBIGUOUS_DUPLICATE_CHARS - 1, -1):
        if previous.endswith(current[:size]):
            return size
    return 0


def is_near_duplicate_snapshot(previous: str, current: str) -> bool:
    """Detect a revised Cursor snapshot that cannot replace the ACP delta stream."""
    if min(len(previous.strip()), len(current.strip())) < MIN_AMBIGUOUS_DUPLICATE_CHARS:
        return False
    previous = previous[-MAX_SNAPSHOT_COMPARE_CHARS:]
    current = current[-MAX_SNAPSHOT_COMPARE_CHARS:]
    length_ratio = min(len(previous), len(current)) / max(len(previous), len(current))
    if length_ratio < 0.75:
        return False
    previous_pairs = set(zip(previous, previous[1:]))
    current_pairs = set(zip(current, current[1:]))
    if not previous_pairs or not current_pairs:
        return False
    similarity = 2 * len(previous_pairs & current_pairs) / (
        len(previous_pairs) + len(current_pairs)
    )
    return similarity >= 0.85


def emit_stream_event(
    session_id: str, event: dict[str, Any], state: dict[str, Any]
) -> None:
    event_type = event.get("type")
    if event_type == "assistant":
        message = event.get("message")
        content = (
            message.get("content")
            if isinstance(message, dict)
            else event.get("content")
        )
        if isinstance(content, str):
            content = [{"type": "text", "text": content}]
        if not isinstance(content, list):
            return
        for block in content:
            if not isinstance(block, dict):
                continue
            if block.get("type") == "text" and isinstance(block.get("text"), str):
                text = block["text"]
                streamed = state.get("streamed_text", "")
                # `--stream-partial-output` can emit the same timestamped delta
                # twice: first without model_call_id and then with it. Forward
                # exactly one copy while preserving intentional repeated deltas.
                if is_duplicate_partial_chunk(event, text, state):
                    continue
                segment = state.get("partial_segment_text", "")
                if "timestamp_ms" in event:
                    if state.pop("tool_boundary_since_partial", False):
                        segment = ""
                    overlap = significant_suffix_prefix_overlap(segment, text)
                    if overlap:
                        text = text[overlap:]
                    state["partial_segment_text"] = segment + text
                # With --stream-partial-output Cursor emits timestamped deltas,
                # followed by untimestamped assistant snapshots. Cursor can emit
                # one snapshot per tool phase, not just one for the whole turn.
                # Compare against the current segment before the turn aggregate.
                if "timestamp_ms" not in event and streamed:
                    state["partial_segment_text"] = ""
                    if segment and text == segment:
                        continue
                    if segment and text.startswith(segment):
                        text = text[len(segment) :]
                    elif text == streamed:
                        continue
                    elif text.startswith(streamed):
                        text = text[len(streamed) :]
                    elif is_near_duplicate_snapshot(segment, text) or is_near_duplicate_snapshot(
                        streamed, text
                    ):
                        continue
                    else:
                        overlap = significant_suffix_prefix_overlap(streamed, text)
                        if overlap:
                            text = text[overlap:]
                if text:
                    state["sent_text"] = True
                    state["streamed_text"] = streamed + text
                    update(
                        session_id,
                        {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": text},
                        },
                    )
    elif event_type == "tool_call":
        state["tool_boundary_since_partial"] = True
        call_id = event.get("call_id") or event.get("tool_call_id") or event.get("id")
        if not isinstance(call_id, str) or not call_id:
            return
        subtype = event.get("subtype")
        if subtype in {"completed", "failed"}:
            update(
                session_id,
                {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": "failed" if subtype == "failed" else "completed",
                },
            )
        else:
            update(
                session_id,
                {
                    "sessionUpdate": "tool_call",
                    "toolCallId": call_id,
                    "title": tool_title(event),
                    "kind": "other",
                    "status": "in_progress",
                },
            )
    elif event_type == "result":
        state["result_seen"] = True
        state["is_error"] = (
            bool(event.get("is_error")) or event.get("subtype") == "error"
        )
        result = event.get("result")
        if not state["sent_text"] and isinstance(result, str) and result:
            state["sent_text"] = True
            update(
                session_id,
                {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": result},
                },
            )


def cursor_prompt_command(
    active: ActivePrompt, cwd: str, prompt: PreparedPrompt
) -> list[str]:
    command = [CURSOR_AGENT, "--workspace", cwd]
    for directory in prompt.additional_dirs:
        command.extend(["--add-dir", directory])
    command.extend(
        [
            "--resume",
            active.session_id,
            "--print",
            "--force",
            "--output-format",
            "stream-json",
            "--stream-partial-output",
            prompt.text,
        ]
    )
    return command


def run_prompt(active: ActivePrompt, cwd: str, prompt: PreparedPrompt) -> None:
    global ACTIVE
    state: dict[str, Any] = {
        "sent_text": False,
        "streamed_text": "",
        "result_seen": False,
        "is_error": False,
    }
    command = cursor_prompt_command(active, cwd, prompt)
    try:
        process = subprocess.Popen(
            command,
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
            start_new_session=True,
        )
        active.process = process
        if active.cancelled.is_set():
            active.cancel()
        assert process.stdout is not None
        for line in process.stdout:
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(event, dict):
                emit_stream_event(active.session_id, event, state)
        return_code = process.wait()
        if active.cancelled.is_set():
            reply(active.request_id, {"stopReason": "cancelled"})
        elif return_code != 0 or state["is_error"]:
            error(active.request_id, -32000, "Cursor CLI prompt failed")
        else:
            reply(active.request_id, {"stopReason": "end_turn"})
    except OSError:
        error(active.request_id, -32000, "Cursor CLI could not be started")
    finally:
        with ACTIVE_LOCK:
            if ACTIVE is active:
                ACTIVE = None


def handle_request(message: dict[str, Any]) -> None:
    global ACTIVE
    request_id = message.get("id")
    method = message.get("method")
    params = message.get("params") or {}
    try:
        if method == "initialize":
            reply(
                request_id,
                {
                    "protocolVersion": 1,
                    "agentInfo": {"name": "cursor-cli-bridge", "version": "0.2.0"},
                    "agentCapabilities": {
                        "loadSession": True,
                        "promptCapabilities": {
                            "image": True,
                            "audio": False,
                            "embeddedContext": False,
                        },
                    },
                    "authMethods": [],
                },
            )
        elif method == "session/new":
            cwd = validate_cwd(params.get("cwd"))
            session_id = create_chat(cwd)
            SESSIONS[session_id] = cwd
            reply(request_id, {"sessionId": session_id})
        elif method == "session/load":
            cwd = validate_cwd(params.get("cwd"))
            session_id = validate_chat_id(params.get("sessionId"))
            verify_chat(session_id, cwd)
            SESSIONS[session_id] = cwd
            reply(request_id, {})
        elif method == "session/prompt":
            session_id = validate_chat_id(params.get("sessionId"))
            cwd = SESSIONS.get(session_id)
            if cwd is None:
                raise BridgeError("session is not loaded")
            prompt = prepare_prompt(params.get("prompt"), session_id)
            with ACTIVE_LOCK:
                if ACTIVE is not None:
                    raise BridgeError("another Cursor prompt is already running")
                ACTIVE = ActivePrompt(request_id, session_id)
                active = ACTIVE
            threading.Thread(
                target=run_prompt, args=(active, cwd, prompt), daemon=False
            ).start()
        else:
            error(request_id, -32601, f"unsupported ACP method: {method}")
    except BridgeError as exc:
        error(request_id, -32602, str(exc))


def handle_notification(message: dict[str, Any]) -> None:
    if message.get("method") != "session/cancel":
        return
    params = message.get("params") or {}
    session_id = params.get("sessionId")
    with ACTIVE_LOCK:
        active = ACTIVE
        if active is not None and active.session_id == session_id:
            active.cancel()


def main() -> int:
    try:
        for line in sys.stdin:
            try:
                message = json.loads(line)
            except json.JSONDecodeError:
                error(None, -32700, "invalid JSON")
                continue
            if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
                error(
                    message.get("id") if isinstance(message, dict) else None,
                    -32600,
                    "invalid request",
                )
            elif "id" in message:
                handle_request(message)
            else:
                handle_notification(message)
    finally:
        with ACTIVE_LOCK:
            active = ACTIVE
            if active is not None:
                active.cancel()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
