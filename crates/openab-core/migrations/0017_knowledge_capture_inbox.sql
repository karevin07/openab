INSERT INTO knowledge_global_actions
    (action_id, surface_id, label, button_style, title, prompt_template, behavior, visible, row_number, sort_order, config_json)
VALUES
    ('capture_inbox', 'home', '📥 批次 Inbox', 'primary', 'Knowledge Capture Inbox',
     '使用 notion-knowledge Skill 的 Capture Inbox 模式。輸入必須解析成 2 至 10 個唯一 public HTTPS URL；先呼叫 knowledge_capture_inbox create 建立 durable inbox。接著依 item_index 順序逐筆讀取來源、取得可驗證的來源發佈時間、fetch 最新 Knowledge Library schema 與既有 Content Type／Tags，並以完整 Source URL 搜尋檢查重複項目。每處理完一筆就呼叫 stage，記錄 title、summary、source_published_at、既有 Content Type、只含既有選項的 Tags、duplicate_action（create／update／skip）及重複頁 URL。來源無法讀取時呼叫 decide fail 並繼續，不可中止整批。全部 staging 完成後呼叫 next，依 capture_inbox_preview card contract 只顯示第一筆 ready 項目。不得在預覽階段寫入 Notion。',
     'modal', 1, 0, 15, '{}'),
    ('capture_inbox_accept', 'confirmation', '✅ 收錄', 'success', '收錄 Inbox 項目',
     '使用 notion-knowledge Skill 的 Capture Inbox Accept 模式。Discord 按鈕只授權 inbox_id 與 item_index 指定的單一項目。先以 knowledge_capture_inbox get 重新取得 durable proposal，確認 status=ready，再 fetch Knowledge Library 最新 schema 並用 exact Source URL 重新檢查重複；只能依 proposal 的 create 或 update 寫入。duplicate_action=skip 時不得寫入，應呼叫 decide skip 並將此筆回報為既有內容 no-op。寫入後必須 fetch 該 Notion row 驗證 title、Source URL、摘要、Published At、Content Type 與 Tags；驗證成功才呼叫 decide accept 並傳入精確 Notion page URL。之後呼叫 next；若還有 ready 項目，回傳下一張 capture_inbox_preview，否則回傳本批 summary。不得修改其他 inbox item 或其他 Notion page。',
     'prompt', 0, 0, 20, '{}'),
    ('capture_inbox_skip', 'confirmation', '略過', 'secondary', '略過 Inbox 項目',
     '使用 notion-knowledge Skill 的 Capture Inbox Skip 模式。Discord 按鈕只授權 inbox_id 與 item_index 指定的單一項目。以 knowledge_capture_inbox get 確認項目仍為 ready，然後呼叫 decide skip；不可讀寫 Notion。接著呼叫 next；若還有 ready 項目，回傳下一張 capture_inbox_preview，否則回傳本批 summary。',
     'prompt', 0, 0, 21, '{}'),
    ('capture_inbox_modify', 'confirmation', '✏️ 修改', 'secondary', '修改 Inbox 預覽',
     '使用 notion-knowledge Skill 的 Capture Inbox Modify 模式。先以 knowledge_capture_inbox modify 保存使用者針對 inbox_id 與 item_index 的修改指示，再 get 原 proposal。只依修改指示重新讀取必要來源或 Knowledge Library schema，重新檢查 exact Source URL 重複項目，並以 stage 更新同一項目的 title、summary、source_published_at、既有 Content Type、只含既有選項的 Tags、duplicate_action 及重複頁 URL。回傳同一筆更新後的 capture_inbox_preview；不得寫入 Notion、不得推進到下一筆。',
     'modal', 0, 0, 22, '{}')
ON CONFLICT(action_id) DO NOTHING;

INSERT INTO knowledge_global_action_inputs
    (action_id, input_id, label, placeholder, input_style, required, max_length, sort_order)
VALUES
    ('capture_inbox', 'urls', '文章網址（每行一個）', '貼上 2–10 個 HTTPS 網址，每行一個', 'paragraph', 1, 4000, 10),
    ('capture_inbox', 'note', '收藏原因（選填）', '這批內容為什麼值得收藏？', 'paragraph', 0, 1000, 20),
    ('capture_inbox', 'classification', '分類提示（選填）', '例如：AI、金融、Guide', 'short', 0, 200, 30),
    ('capture_inbox_modify', 'instruction', '修改指示', '例如：摘要聚焦風險管理，移除 AI Tag', 'paragraph', 1, 1000, 10)
ON CONFLICT(action_id, input_id) DO NOTHING;

UPDATE knowledge_ui_views
SET description = description || '
**批次 Inbox**：依序整理 2–10 個網址，逐筆選擇收錄、略過或修改。'
WHERE view_id = 'help' AND description NOT LIKE '%**批次 Inbox**%';

INSERT INTO schema_migrations(version) VALUES (17);
