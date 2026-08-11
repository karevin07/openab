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


if __name__ == "__main__":
    unittest.main()
