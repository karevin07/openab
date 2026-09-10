-- Align Capture Inbox with the sidecar's constrained URL processor.  The old
-- prompt asked the agent to fetch every page itself before staging it.
UPDATE knowledge_global_actions
SET prompt_template = '使用 notion-knowledge Skill 的 Capture Inbox 模式。輸入必須解析成 2 至 10 個唯一 public HTTPS URL；先呼叫 knowledge_capture_inbox create 建立 durable inbox，再對每個 item_index 呼叫 process，由 sidecar 以受限 HTTP client 擷取 HTML metadata。依序 fetch 最新 Knowledge Library schema 與既有 Content Type／Tags，以完整 Source URL 搜尋檢查重複項目，完成摘要與語意分類後呼叫 stage。來源無法處理時呼叫 decide fail 並繼續，不可中止整批。全部 staging 完成後呼叫 next，依 capture_inbox_preview card contract 只顯示第一筆 ready 項目。該卡片 JSON 頂層必須同時包含 next 回傳的 inbox_id 與 item_index；heading 用「Capture Inbox｜<index> / <total>」，meta 用「Content Type · Tags · create|update|skip」。不得在預覽階段寫入 Notion。'
WHERE action_id = 'capture_inbox';

-- Recommendations are the normal Reading List entry point.  Intake is moved
-- behind the operations submenu by the Discord renderer, but remains enabled
-- so its existing prompt/modal workflow can be reused.
UPDATE knowledge_actions
SET sort_order = 5, button_style = 'primary'
WHERE source_id = 'personal_reading_list' AND action_id = 'recommend';

UPDATE knowledge_actions
SET sort_order = 80, button_style = 'secondary'
WHERE source_id = 'personal_reading_list' AND action_id = 'intake';

INSERT INTO knowledge_ui_views
    (view_id, title, description, colour, footer, field_label, field_value, select_placeholder, config_json)
VALUES
    ('reading_list_operations', '🧰 Reading List 整理工具',
     '集中處理缺少 Expect、Tags 一致性、EPUB 批次配對與作業狀態。所有外部寫入都必須綁定明確項目並重新驗證。',
     5793266, 'Knowledge · Reading List Operations', '', '', '', '{}')
ON CONFLICT(view_id) DO UPDATE SET
    title=excluded.title,
    description=excluded.description,
    colour=excluded.colour,
    footer=excluded.footer;

INSERT INTO knowledge_actions
    (source_id, action_id, label, button_style, title, prompt_template, enabled, sort_order)
VALUES
    ('personal_reading_list', 'operations', '🧰 整理工具', 'secondary',
     'Reading List｜整理工具', '由 Discord 本機開啟整理工具選單，不提交 agent prompt。', 1, 70),
    ('personal_reading_list', 'expect_review', '⭐ 補 Expect', 'primary',
     'Reading List｜補 Expect',
     '依 references/reading-list-operations.md 的 Expect review queue 流程執行。fetch 所有 Status = To Read 且 Expect 為空的 row；若沒有可續用的 batch，呼叫 reading_list_operations review_create。接著呼叫 review_next，依 reading_expect_review card contract 顯示唯一 pending item。卡片頂層 batch_id 與 item_index 必須直接複製工具結果。此步驟不可寫入 Notion。', 1, 71),
    ('personal_reading_list', 'taxonomy', '🏷️ Tags 健檢', 'secondary',
     'Reading List｜Tags 健檢',
     'fetch 完整 Reading List 後呼叫 reading_list_operations taxonomy_lint。以 epub_audit card contract 顯示最多 25 個實際問題，overview 必須包含 scanned、affected 與各 issue count。這是唯讀檢查，不可修改 Notion。', 1, 72),
    ('personal_reading_list', 'epub_match', '🔗 EPUB 批次配對', 'secondary',
     'Reading List｜EPUB 批次配對',
     '依 references/reading-list-operations.md 的 Batch EPUB matcher 流程執行。folder 必須解析為使用者提供的單一 Google Drive folder ID；呼叫 drive_inventory 只讀取第一層，fetch 完整 Reading List inventory，再呼叫 match 且 batch_size=20。將 source_folder_id 加入每筆 match，呼叫 reconcile_plan（此操作會持久化 plan），再呼叫 reconcile_next。依 reading_epub_match_review card contract 顯示唯一 pending item；plan_id 與 item_index 必須直接複製工具結果。此預覽不可寫入 Notion、移動或刪除 Drive 檔案。', 1, 73),
    ('personal_reading_list', 'ops_status', '📊 作業狀態', 'secondary',
     'Reading List｜作業狀態',
     '呼叫 reading_list_operations status，簡潔列出 open/completed review batches、pending review items、reconciliation states 與最後更新時間。這是唯讀狀態，不可修改 Notion 或 Drive。', 1, 74)
ON CONFLICT(source_id, action_id) DO UPDATE SET
    label=excluded.label,
    button_style=excluded.button_style,
    title=excluded.title,
    prompt_template=excluded.prompt_template,
    enabled=excluded.enabled,
    sort_order=excluded.sort_order;

INSERT INTO knowledge_action_inputs
    (source_id, action_id, input_id, label, placeholder, input_style, required, max_length, sort_order)
VALUES
    ('personal_reading_list', 'epub_match', 'folder', 'Google Drive 來源資料夾',
     'https://drive.google.com/drive/folders/...', 'short', 1, 1000, 10)
ON CONFLICT(source_id, action_id, input_id) DO UPDATE SET
    label=excluded.label,
    placeholder=excluded.placeholder,
    input_style=excluded.input_style,
    required=excluded.required,
    max_length=excluded.max_length,
    sort_order=excluded.sort_order;

INSERT INTO knowledge_global_actions
    (action_id, surface_id, label, button_style, title, prompt_template, behavior, visible, row_number, sort_order, config_json)
VALUES
    ('reading_expect_score', 'confirmation', '設定 Expect', 'success',
     'Reading List｜設定 Expect',
     '使用 notion-knowledge Skill 的 Expect review queue 流程。按鈕只授權 batch_id 與 item_index 指定的單一 pending item。先呼叫 reading_list_operations review_next/get equivalent 確認目前 durable item 身分；將使用者選擇的 Expect 寫入該 exact Notion row，fetch 驗證 Expect 後才呼叫 review_decide decision=score。接著呼叫 review_next，若尚未完成就回傳下一張 reading_expect_review card，否則回傳 batch summary。不得修改其他欄位或其他 row。',
     'prompt', 0, 0, 80, '{}'),
    ('reading_expect_skip', 'confirmation', '稍後再評', 'secondary',
     'Reading List｜略過 Expect',
     '使用 notion-knowledge Skill 的 Expect review queue 流程。按鈕只授權 batch_id 與 item_index 指定的單一 pending item。不得寫入 Notion；呼叫 reading_list_operations review_decide decision=skip，再呼叫 review_next。若尚未完成就回傳下一張 reading_expect_review card，否則回傳 batch summary。',
     'prompt', 0, 0, 81, '{}'),
    ('reading_epub_match_apply', 'confirmation', '套用配對', 'success',
     'Reading List｜套用 EPUB 配對',
     '使用 notion-knowledge Skill 的 Reconciliation 流程。按鈕只授權 plan_id 與 item_index 指定的單一 pending action。先呼叫 reading_list_operations reconcile_next，若回傳 item_index 不同則停止。只允許 state=high 或 medium 且 action=link_then_move_then_forum 或 manual_review 的明確配對；依序寫入並 fetch 驗證 exact Notion EPUB 欄位、呼叫 drive_move confirm=true 移動 exact file、呼叫 reading_list_epub_forum_sync 並驗證 thread ID。三步結果完整後才呼叫 reconcile_decide decision=apply 並傳入 result metadata；任一步失敗則呼叫 decision=fail 保存錯誤，不得假裝成功。接著以 reconcile_next 回傳下一張 reading_epub_match_review 或 summary。不得刪除 Drive 檔案。',
     'prompt', 0, 0, 82, '{}'),
    ('reading_epub_match_skip', 'confirmation', '略過配對', 'secondary',
     'Reading List｜略過 EPUB 配對',
     '使用 notion-knowledge Skill 的 Reconciliation 流程。按鈕只授權 plan_id 與 item_index 指定的單一 pending action。先呼叫 reading_list_operations reconcile_next 確認同一 item_index，再呼叫 reconcile_decide decision=skip；不得寫入 Notion、移動或刪除 Drive 檔案。接著回傳下一張 reading_epub_match_review 或 summary。',
     'prompt', 0, 0, 83, '{}')
ON CONFLICT(action_id) DO UPDATE SET
    title=excluded.title,
    prompt_template=excluded.prompt_template,
    behavior=excluded.behavior;

UPDATE knowledge_ui_views
SET description = '待讀推薦是主要入口；搜尋、概覽與 EPUB 健檢維持唯讀。需要補 Expect、檢查 Tags、批次配對 EPUB 或查看作業狀態時，請開啟「🧰 整理工具」。'
WHERE view_id = 'reading_list';

INSERT INTO schema_migrations(version) VALUES (20);
