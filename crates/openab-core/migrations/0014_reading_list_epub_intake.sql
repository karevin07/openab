INSERT INTO knowledge_actions
    (source_id, action_id, label, button_style, title, prompt_template, sort_order)
VALUES
    ('personal_reading_list', 'intake', '📥 EPUB Intake', 'primary', 'Reading List｜EPUB Intake', '使用 notion-knowledge Skill 的 Reading List EPUB Intake 模式。建立 thread 後請使用者附上恰好一個 .epub 檔案；收到 [EPUB attachment] block 時讀 references/epub-intake.md、references/schema.md 與 references/discord-cards.md，先呼叫 reading_list_epub_preview 驗證，再配對唯一 Reading List row 並回傳 epub_intake_preview card。未按確認上傳前不得呼叫 commit、不得修改 Notion。', 5);

INSERT INTO knowledge_global_actions
    (action_id, surface_id, label, button_style, title, prompt_template, behavior, visible, row_number, sort_order, config_json)
VALUES
    ('epub_intake_confirm', 'confirmation', '✅ 確認上傳', 'success', '確認 EPUB 上傳', '使用 notion-knowledge Skill 的 Reading List EPUB Intake Confirmation。這次按鈕是使用者對緊接在按鈕上方且仍未處理的單一 EPUB 預覽所做的明確授權。先讀 references/epub-intake.md、references/schema.md 與 references/discord-cards.md。固定 intake_id、Discord preview message ID 與 Discord user ID 會列在執行上下文。只有 active session 中唯一未處理 preview 的 intake_id 與固定值相同、且 Notion row 仍為唯一配對時，才呼叫 reading_list_epub_commit，接著只更新該 row 的 EPUB 欄位並 read-after-write 驗證。', 'prompt', 0, 0, 25, '{}'),
    ('epub_intake_cancel', 'confirmation', '取消', 'secondary', '取消 EPUB 上傳', '', 'local', 0, 0, 26, '{"message_template":"已取消 EPUB Intake；未上傳 Drive，也未修改 Notion。暫存檔會自動過期。"}');

INSERT INTO schema_migrations(version) VALUES (14);
