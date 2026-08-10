#!/usr/bin/env python3
"""Expose Cursor's official CLI chats through the minimal ACP surface OpenAB uses."""

from __future__ import annotations

import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import threading
import time
from typing import Any


CHAT_ID_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-"
    r"[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)
CURSOR_AGENT = os.environ.get("CURSOR_AGENT_BIN", "cursor-agent")
WRITE_LOCK = threading.Lock()
ACTIVE_LOCK = threading.Lock()
SESSIONS: dict[str, str] = {}
ACTIVE: "ActivePrompt | None" = None


class BridgeError(RuntimeError):
    pass


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
    return [path.parent for path in root.glob(f"*/{session_id}/store.db") if path.is_file()]


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
                (candidate.lower() for candidate in candidates if CHAT_ID_RE.fullmatch(candidate)),
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


def prompt_text(blocks: Any) -> str:
    if not isinstance(blocks, list):
        raise BridgeError("prompt must be an array")
    parts: list[str] = []
    for block in blocks:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text" and isinstance(block.get("text"), str):
            parts.append(block["text"])
        elif block.get("type") == "resource_link" and isinstance(block.get("uri"), str):
            parts.append(block["uri"])
        else:
            raise BridgeError("this Cursor CLI bridge currently supports text prompts only")
    text = "\n".join(part for part in parts if part)
    if not text:
        raise BridgeError("prompt contains no text")
    return text


def tool_title(value: Any) -> str:
    if not isinstance(value, dict):
        return "Cursor tool"
    tool_call = value.get("tool_call") or value.get("toolCall")
    if isinstance(tool_call, dict) and tool_call:
        return str(next(iter(tool_call))).removesuffix("ToolCall")
    return str(value.get("name") or value.get("tool_name") or "Cursor tool")


def emit_stream_event(session_id: str, event: dict[str, Any], state: dict[str, Any]) -> None:
    event_type = event.get("type")
    if event_type == "assistant":
        message = event.get("message")
        content = message.get("content") if isinstance(message, dict) else event.get("content")
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
                # With --stream-partial-output Cursor emits timestamped deltas,
                # followed by one untimestamped assistant event containing the
                # complete response. Only forward the part not already streamed.
                if "timestamp_ms" not in event and streamed:
                    if text == streamed:
                        continue
                    if text.startswith(streamed):
                        text = text[len(streamed) :]
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
        state["is_error"] = bool(event.get("is_error")) or event.get("subtype") == "error"
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


def run_prompt(active: ActivePrompt, cwd: str, text: str) -> None:
    global ACTIVE
    state: dict[str, Any] = {
        "sent_text": False,
        "streamed_text": "",
        "result_seen": False,
        "is_error": False,
    }
    command = [
        CURSOR_AGENT,
        "--workspace",
        cwd,
        "--resume",
        active.session_id,
        "--print",
        "--force",
        "--output-format",
        "stream-json",
        "--stream-partial-output",
        text,
    ]
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
                    "agentInfo": {"name": "cursor-cli-bridge", "version": "0.1.0"},
                    "agentCapabilities": {
                        "loadSession": True,
                        "promptCapabilities": {
                            "image": False,
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
            text = prompt_text(params.get("prompt"))
            with ACTIVE_LOCK:
                if ACTIVE is not None:
                    raise BridgeError("another Cursor prompt is already running")
                ACTIVE = ActivePrompt(request_id, session_id)
                active = ACTIVE
            threading.Thread(target=run_prompt, args=(active, cwd, text), daemon=False).start()
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
                error(message.get("id") if isinstance(message, dict) else None, -32600, "invalid request")
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
