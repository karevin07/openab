import importlib.util
import io
import json
from pathlib import Path
import unittest
from unittest import mock


MODULE_PATH = Path(__file__).with_name("cursor_cli_acp.py")
SPEC = importlib.util.spec_from_file_location("cursor_cli_acp", MODULE_PATH)
assert SPEC and SPEC.loader
bridge = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bridge)


class CursorCliAcpTest(unittest.TestCase):
    def test_prompt_text_joins_text_and_resource_links(self):
        self.assertEqual(
            bridge.prompt_text(
                [
                    {"type": "text", "text": "hello"},
                    {"type": "resource_link", "uri": "https://example.test/context"},
                ]
            ),
            "hello\nhttps://example.test/context",
        )

    def test_prompt_text_rejects_images(self):
        with self.assertRaisesRegex(bridge.BridgeError, "text prompts only"):
            bridge.prompt_text([{"type": "image", "data": "ignored"}])

    def test_validate_chat_id_rejects_path_content(self):
        with self.assertRaisesRegex(bridge.BridgeError, "invalid Cursor chat ID"):
            bridge.validate_chat_id("../../store.db")

    def test_create_chat_accepts_reserved_id_before_store_exists(self):
        chat_id = "00000000-0000-0000-0000-000000000000"
        completed = mock.Mock(stdout=chat_id + "\n")
        with mock.patch.object(bridge.subprocess, "run", return_value=completed):
            self.assertEqual(bridge.create_chat("/tmp"), chat_id)

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
        self.assertEqual(message["params"]["update"]["sessionUpdate"], "agent_message_chunk")
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
                {"type": "assistant", "message": {"content": [{"type": "text", "text": "hello"}]}},
                state,
            )
        self.assertEqual(output.getvalue(), "")


if __name__ == "__main__":
    unittest.main()
