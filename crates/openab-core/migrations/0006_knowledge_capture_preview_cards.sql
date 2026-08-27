-- Capture preview Discord cards with button confirmation, matching Project Note UX.

UPDATE knowledge_global_actions
SET prompt_template = '使用 notion-knowledge Skill 的 Capture 模式處理使用者條件。先讀取來源、檢查 Knowledge Library 重複項目並產生收錄預覽；先讀 references/capture.md 與 references/discord-cards.md，並嚴格以 capture_preview card contract 回覆（最多 5 筆）。不要直接寫入 Notion；只有使用者按下預覽卡上的確認收錄按鈕才可寫入。'
WHERE action_id = 'capture' AND surface_id = 'home';

INSERT INTO knowledge_global_actions
    (action_id, surface_id, label, button_style, title, prompt_template, behavior, visible, row_number, sort_order, config_json)
VALUES
    ('capture_confirm', 'confirmation', '✅ 確認收錄', 'success', '確認收錄', '使用 notion-knowledge Skill 的 Capture 模式執行確認寫入。這次按鈕是使用者對緊接在按鈕上方且仍未處理的收錄預覽所做的明確寫入授權。重新檢查 Knowledge Library 重複項目；只有預覽仍待確認且沒有歧義時，才依預覽的 create／update 建議寫入（skip 項目不要寫入）。若找不到唯一待確認預覽或已取消／處理，停止寫入。寫入後 fetch 驗證；已寫入的項目可用 search card contract 回覆，其餘簡短說明。不要修改未在預覽中的頁面。', 'prompt', 0, 0, 15, '{}'),
    ('capture_cancel', 'confirmation', '取消', 'secondary', '取消收錄', '', 'local', 0, 0, 16, '{"message_template":"已取消收錄預覽，未寫入 Notion。"}')
ON CONFLICT(action_id) DO NOTHING;

INSERT INTO schema_migrations(version) VALUES (6);
