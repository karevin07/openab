import base64
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


MODULE_PATH = Path(__file__).with_name("cursor_cli_acp.py")
SPEC = importlib.util.spec_from_file_location("cursor_cli_acp", MODULE_PATH)
assert SPEC and SPEC.loader
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)


class CursorCliAcpTest(unittest.TestCase):
    chat_id = "00000000-0000-0000-0000-000000000000"

    def test_prepare_prompt_joins_text_and_resource_links(self):
        self.assertEqual(
            bridge.prepare_prompt(
                [
                    {"type": "text", "text": "hello"},
                    {"type": "resource_link", "uri": "https://example.test/context"},
                ],
                self.chat_id,
            ),
            bridge.PreparedPrompt(
                text="hello\nhttps://example.test/context", additional_dirs=()
            ),
        )

    def test_prepare_prompt_persists_image_and_adds_directory(self):
        raw = b"\x89PNG\r\n\x1a\nimage-data"
        with tempfile.TemporaryDirectory() as directory:
            prompt = bridge.prepare_prompt(
                [
                    {"type": "text", "text": "what is this?"},
                    {
                        "type": "image",
                        "mimeType": "image/png",
                        "data": base64.b64encode(raw).decode("ascii"),
                    },
                ],
                self.chat_id,
                Path(directory),
            )

            image_files = list(Path(prompt.additional_dirs[0]).glob("*.png"))
            self.assertEqual(len(image_files), 1)
            image_path = image_files[0]
            self.assertEqual(image_path.read_bytes(), raw)
            self.assertIn(f"path: {image_path}", prompt.text)
            self.assertIn("Inspect this image", prompt.text)

    def test_prepare_prompt_rejects_invalid_image_data(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(bridge.BridgeError, "invalid base64"):
                bridge.prepare_prompt(
                    [
                        {
                            "type": "image",
                            "mimeType": "image/png",
                            "data": "not-base64",
                        }
                    ],
                    self.chat_id,
                    Path(directory),
                )

    def test_cursor_command_adds_only_the_image_directory(self):
        active = bridge.ActivePrompt(1, self.chat_id)
        prompt = bridge.PreparedPrompt(
            text="inspect it", additional_dirs=("/tmp/session-images",)
        )
        command = bridge.cursor_prompt_command(active, "/tmp/workspace", prompt)

        self.assertEqual(
            command[:7],
            [
                bridge.CURSOR_AGENT,
                "--workspace",
                "/tmp/workspace",
                "--add-dir",
                "/tmp/session-images",
                "--resume",
                self.chat_id,
            ],
        )
        self.assertEqual(command[-1], "inspect it")

    def test_validate_chat_id_rejects_path_content(self):
        with self.assertRaisesRegex(bridge.BridgeError, "invalid Cursor chat ID"):
            bridge.validate_chat_id("../../store.db")

    def test_create_chat_accepts_reserved_id_before_store_exists(self):
        completed = mock.Mock(stdout=self.chat_id + "\n")
        with mock.patch.object(bridge.subprocess, "run", return_value=completed):
            self.assertEqual(bridge.create_chat("/tmp"), self.chat_id)

    def test_assistant_event_emits_acp_chunk(self):
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(
                "00000000-0000-0000-0000-000000000000",
                {
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": "hello"}]},
                },
                state,
            )
        message = json.loads(output.getvalue())
        self.assertEqual(
            message["params"]["update"]["sessionUpdate"], "agent_message_chunk"
        )
        self.assertEqual(message["params"]["update"]["content"]["text"], "hello")
        self.assertTrue(state["sent_text"])

    def test_final_assistant_event_does_not_repeat_streamed_deltas(self):
        state = {
            "sent_text": True,
            "streamed_text": "hello",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(
                "00000000-0000-0000-0000-000000000000",
                {
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": "hello"}]},
                },
                state,
            )
        self.assertEqual(output.getvalue(), "")

    def test_duplicate_partial_event_forms_emit_text_once(self):
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        event = {
            "type": "assistant",
            "timestamp_ms": 1_000,
            "message": {"content": [{"type": "text", "text": "checking files"}]},
        }
        duplicate = {
            **event,
            "timestamp_ms": 1_100,
            "model_call_id": "model-call-1",
        }
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(self.chat_id, event, state)
            bridge.emit_stream_event(self.chat_id, duplicate, state)

        messages = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(len(messages), 1)
        self.assertEqual(
            messages[0]["params"]["update"]["content"]["text"], "checking files"
        )
        self.assertEqual(state["streamed_text"], "checking files")

    def test_intentional_repeated_delta_from_same_model_call_is_preserved(self):
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        event = {
            "type": "assistant",
            "timestamp_ms": 1_000,
            "model_call_id": "model-call-1",
            "message": {"content": [{"type": "text", "text": "ha"}]},
        }
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(self.chat_id, event, state)
            bridge.emit_stream_event(
                self.chat_id, {**event, "timestamp_ms": 1_100}, state
            )

        messages = output.getvalue().splitlines()
        self.assertEqual(len(messages), 2)
        self.assertEqual(state["streamed_text"], "haha")

    def test_repeated_short_delta_does_not_drop_later_path_separator(self):
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()

        def partial(text, timestamp, model_call_id=None):
            event = {
                "type": "assistant",
                "timestamp_ms": timestamp,
                "message": {"content": [{"type": "text", "text": text}]},
            }
            if model_call_id is not None:
                event["model_call_id"] = model_call_id
            bridge.emit_stream_event(self.chat_id, event, state)

        with mock.patch.object(bridge.sys, "stdout", output):
            partial("/", 1_000)
            partial("/", 1_010, "model-call-1")
            partial("art", 1_020)
            partial("/", 1_030)
            partial("/", 1_040, "model-call-1")
            partial("portraits", 1_050)

        self.assertEqual(state["streamed_text"], "/art/portraits")
        chunks = [
            json.loads(line)["params"]["update"]["content"]["text"]
            for line in output.getvalue().splitlines()
        ]
        self.assertEqual(chunks, ["/", "art", "/", "portraits"])

    def test_long_duplicate_delta_from_same_model_call_is_removed(self):
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        text = "從正文歸納世界觀，先開 session 並掃讀現有設定與關鍵規則。"
        event = {
            "type": "assistant",
            "timestamp_ms": 1_000,
            "model_call_id": "model-call-1",
            "message": {"content": [{"type": "text", "text": text}]},
        }
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(self.chat_id, event, state)
            bridge.emit_stream_event(
                self.chat_id, {**event, "timestamp_ms": 1_100}, state
            )

        self.assertEqual(len(output.getvalue().splitlines()), 1)
        self.assertEqual(state["streamed_text"], text)

    def test_delayed_long_adjacent_duplicate_is_removed(self):
        text = "從正文歸納世界觀，先開 session 並掃讀現有設定與關鍵規則。"
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        event = {
            "type": "assistant",
            "timestamp_ms": 1_000,
            "model_call_id": "model-call-1",
            "message": {"content": [{"type": "text", "text": text}]},
        }
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(self.chat_id, event, state)
            bridge.emit_stream_event(
                self.chat_id,
                {
                    **event,
                    "timestamp_ms": (
                        1_000 + bridge.PARTIAL_DUPLICATE_WINDOW_MS + 10_000
                    ),
                },
                state,
            )

        self.assertEqual(len(output.getvalue().splitlines()), 1)
        self.assertEqual(state["streamed_text"], text)

    def test_delayed_long_duplicate_after_tool_event_is_removed(self):
        text = "Inspect the workspace before changing the implementation."
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(
                self.chat_id,
                {
                    "type": "assistant",
                    "timestamp_ms": 1_000,
                    "model_call_id": "model-call-1",
                    "message": {"content": [{"type": "text", "text": text}]},
                },
                state,
            )
            bridge.emit_stream_event(
                self.chat_id,
                {
                    "type": "tool_call",
                    "subtype": "started",
                    "call_id": "tool-1",
                    "tool_call": {"readToolCall": {}},
                },
                state,
            )
            bridge.emit_stream_event(
                self.chat_id,
                {
                    "type": "assistant",
                    "timestamp_ms": (
                        1_000 + bridge.PARTIAL_DUPLICATE_WINDOW_MS + 10_000
                    ),
                    "model_call_id": "model-call-2",
                    "message": {"content": [{"type": "text", "text": text}]},
                },
                state,
            )

        text_messages = [
            json.loads(line)
            for line in output.getvalue().splitlines()
            if json.loads(line)["params"]["update"]["sessionUpdate"]
            == "agent_message_chunk"
        ]
        self.assertEqual(len(text_messages), 1)
        self.assertEqual(state["streamed_text"], text)

    def test_per_tool_phase_snapshot_does_not_repeat_current_segment(self):
        first = "first progress sentence"
        second = "second progress sentence"
        state = {
            "sent_text": True,
            "streamed_text": first,
            "partial_segment_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(
                self.chat_id,
                {
                    "type": "assistant",
                    "timestamp_ms": 2_000,
                    "message": {"content": [{"type": "text", "text": second}]},
                },
                state,
            )
            bridge.emit_stream_event(
                self.chat_id,
                {
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": second}]},
                },
                state,
            )

        messages = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(len(messages), 1)
        self.assertEqual(messages[0]["params"]["update"]["content"]["text"], second)
        self.assertEqual(state["streamed_text"], first + second)

    def test_snapshot_after_tool_event_does_not_repeat_previous_segment(self):
        text = "progress before the tool call"
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(
                self.chat_id,
                {
                    "type": "assistant",
                    "timestamp_ms": 2_000,
                    "message": {"content": [{"type": "text", "text": text}]},
                },
                state,
            )
            bridge.emit_stream_event(
                self.chat_id,
                {
                    "type": "tool_call",
                    "subtype": "started",
                    "call_id": "tool-1",
                    "tool_call": {"readToolCall": {}},
                },
                state,
            )
            bridge.emit_stream_event(
                self.chat_id,
                {
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": text}]},
                },
                state,
            )

        messages = [json.loads(line) for line in output.getvalue().splitlines()]
        text_messages = [
            message
            for message in messages
            if message["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        ]
        self.assertEqual(len(text_messages), 1)
        self.assertEqual(state["streamed_text"], text)

    def test_matching_partial_forms_outside_window_are_preserved(self):
        state = {
            "sent_text": False,
            "streamed_text": "",
            "result_seen": False,
            "is_error": False,
        }
        output = io.StringIO()
        event = {
            "type": "assistant",
            "timestamp_ms": 1_000,
            "message": {"content": [{"type": "text", "text": "repeat"}]},
        }
        with mock.patch.object(bridge.sys, "stdout", output):
            bridge.emit_stream_event(self.chat_id, event, state)
            bridge.emit_stream_event(
                self.chat_id,
                {
                    **event,
                    "timestamp_ms": 1_000 + bridge.PARTIAL_DUPLICATE_WINDOW_MS + 1,
                    "model_call_id": "model-call-2",
                },
                state,
            )

        self.assertEqual(len(output.getvalue().splitlines()), 2)
        self.assertEqual(state["streamed_text"], "repeatrepeat")


if __name__ == "__main__":
    unittest.main()
