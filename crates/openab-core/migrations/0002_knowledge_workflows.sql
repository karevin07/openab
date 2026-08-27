CREATE TABLE knowledge_ui_views (
    view_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    colour INTEGER NOT NULL DEFAULT 3096813,
    footer TEXT NOT NULL DEFAULT '',
    field_label TEXT NOT NULL DEFAULT '',
    field_value TEXT NOT NULL DEFAULT '',
    select_placeholder TEXT NOT NULL DEFAULT '',
    config_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE knowledge_global_actions (
    action_id TEXT PRIMARY KEY,
    surface_id TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    button_style TEXT NOT NULL DEFAULT 'secondary'
        CHECK (button_style IN ('primary', 'secondary', 'success', 'danger')),
    title TEXT NOT NULL,
    prompt_template TEXT NOT NULL DEFAULT '',
    behavior TEXT NOT NULL
        CHECK (behavior IN ('modal', 'prompt', 'view', 'local')),
    visible INTEGER NOT NULL DEFAULT 1 CHECK (visible IN (0, 1)),
    row_number INTEGER NOT NULL DEFAULT 0,
    sort_order INTEGER NOT NULL DEFAULT 0,
    config_json TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE knowledge_global_action_inputs (
    action_id TEXT NOT NULL REFERENCES knowledge_global_actions(action_id) ON DELETE CASCADE,
    input_id TEXT NOT NULL,
    label TEXT NOT NULL,
    placeholder TEXT NOT NULL DEFAULT '',
    input_style TEXT NOT NULL DEFAULT 'short'
        CHECK (input_style IN ('short', 'paragraph')),
    required INTEGER NOT NULL DEFAULT 0 CHECK (required IN (0, 1)),
    max_length INTEGER NOT NULL DEFAULT 300,
    sort_order INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (action_id, input_id)
);

CREATE TABLE knowledge_policies (
    policy_id TEXT PRIMARY KEY,
    retention_days INTEGER NOT NULL,
    grace_days INTEGER NOT NULL,
    max_items INTEGER NOT NULL,
    queue_name TEXT NOT NULL,
    config_json TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO knowledge_ui_views
    (view_id, title, description, colour, footer, field_label, field_value, select_placeholder, config_json)
VALUES
    ('home', '📚 知識整理小幫手', '快速收錄文章、查詢 Notion、整理 Side Project 筆記、瀏覽書籍閱讀清單，或操作固定排程資料庫。每次操作會建立獨立 thread，方便持續追問。', 3096813, '也可以直接 @知識整理小幫手 描述需求', '安全規則', '查詢預設唯讀；收錄與專案更新會先提供預覽，取得確認後才寫入 Notion。', '', '{"command_description":"Open the knowledge assistant home card"}'),
    ('help', '❓ 知識整理小幫手使用方式', '**收錄文章**：先整理欄位與重複項目，確認後才寫入。
**查詢知識**：唯讀搜尋 Notion 並附來源。
**Side Project**：記下 Draft 想法、查看最近備忘、搜尋或唯讀整理目前方向；按下儲存備忘後才寫入。
**閱讀清單**：依書名、狀態、主題、作者、地區、期望或評分查詢書籍，快捷操作維持唯讀。
**最近收錄**：唯讀列出 Knowledge Library 最近 5 筆。
**排程來源**：從下拉選單選擇固定來源，可查看最新文章、依原生欄位搜尋、跨期整理或查看待刪除清單。', 3096813, '', '', '', '', '{}'),
    ('side_projects', '🧰 Side Projects', '記錄還沒落地的想法、備忘、問題與待研究方向。先選擇專案；未確認的內容不會寫入 Notion。', 5763719, '', '', '', '選擇 Side Project', '{}'),
    ('scheduled_source', '', '選擇要執行的操作。搜尋會使用這個來源的原生欄位並以文章卡片回傳；待刪除清單可直接選擇要永久保留的項目。', 3113197, '', '', '', '選擇排程資料庫', '{}'),
    ('reading_list', '', '使用原生書籍欄位查詢待讀、閱讀中與已讀內容。快捷操作維持唯讀，不會變更閱讀狀態或評分。', 15105570, '', '', '', '', '{}'),
    ('side_project', '', '把還沒確定的內容保存為 Draft 備忘，不會自動當成正式規格或決策。查詢與整理維持唯讀。', 5763719, '', 'Notion Project', '', '', '{}');

INSERT INTO knowledge_global_actions
    (action_id, surface_id, label, button_style, title, prompt_template, behavior, visible, row_number, sort_order, config_json)
VALUES
    ('capture', 'home', '➕ 收錄文章', 'primary', '收錄文章', '使用 notion-knowledge Skill 的 Capture 模式處理使用者條件。先讀取來源、檢查 Knowledge Library 重複項目並產生收錄預覽；不要直接寫入 Notion，等待使用者在 thread 明確確認。', 'modal', 1, 0, 10, '{}'),
    ('search', 'home', '🔎 查詢知識', 'secondary', '查詢知識', '使用 notion-knowledge Skill 的 Search／Synthesis 模式，依使用者條件唯讀查詢 Notion 並附上相關頁面連結。不要修改 Notion。', 'modal', 1, 0, 20, '{}'),
    ('project', 'home', '🧰 Side Project', 'secondary', 'Side Projects', '', 'view', 1, 0, 30, '{"view_id":"side_projects"}'),
    ('reading_list', 'home', '📕 閱讀清單', 'secondary', 'Reading List', '', 'view', 1, 0, 40, '{"source_kind":"reading_list"}'),
    ('recent', 'home', '🕘 最近收錄', 'secondary', '最近收錄', '使用 notion-knowledge Skill 的 Search 模式，唯讀查詢 Knowledge Library 最近收錄的 5 筆內容。依建立或更新時間由新到舊列出標題、Content Type、Project（若有）、Status、Lifecycle 與 Notion 連結。不要修改 Notion。', 'prompt', 1, 1, 10, '{}'),
    ('help', 'home', '❓ 使用說明', 'secondary', '使用說明', '', 'view', 1, 1, 20, '{"view_id":"help"}'),
    ('retention_keep', 'confirmation', '永久保留', 'success', '永久保留', '使用 notion-knowledge Skill 的 Retention Review 模式執行 Retention Keep。這次 Discord 選單操作是使用者對單一 Queue 項目的明確保留授權。先 fetch 並核對 Queue row、target 與 source；只有 Decision 為 Pending 或 Trash Due 且所有 ID 相符時，才把 Queue Decision 設為 Keep、Confirmed By 設為 Discord user ID、Processed At 設為今天。不要修改來源頁面。寫入後重新 fetch 驗證。', 'prompt', 0, 0, 10, '{"policy_id":"scheduled_retention","select_placeholder":"選擇要永久保留的文章","option_prefix":"保留｜","option_description":"取消這筆待刪除項目"}'),
    ('project_note_save', 'confirmation', '✅ 儲存備忘', 'success', '儲存備忘', '使用 notion-knowledge Skill 的 Project Note Capture 模式執行確認寫入。這次按鈕是使用者對緊接在按鈕上方且仍未處理的單一 Project Note 預覽所做的明確寫入授權。重新取得 Knowledge Library schema 並檢查同一 Project 的重複筆記；只有預覽與固定 Project 相符且沒有歧義時才新增。若找不到唯一待確認預覽或已取消／處理，停止寫入。寫入後 fetch 驗證，並依 project_notes card contract 回覆。不要修改或取代既有正式規格。', 'prompt', 0, 0, 20, '{}'),
    ('project_note_cancel', 'confirmation', '取消', 'secondary', '取消備忘', '', 'local', 0, 0, 30, '{"message_template":"取消 {source_title} 備忘，未寫入 Notion。"}'),
    ('retention_scan', 'cron', '', 'secondary', '排程來源 Retention Review', '使用 notion-knowledge Skill 的 Retention Review 模式。只掃描 Structured Knowledge Adapter 中 retention=true 的 scheduled sources，以 Notion createdTime 套用結構化 retention policy，排除來源原生保護狀態，並用 Target Page ID 對 Queue 去重。處理到期項目時，只有 Notion 工具明確支援 trash 才能移到垃圾桶；否則標記 Trash Due 且不可修改來源。最後依 retention card contract 回傳待確認卡片並逐筆核對 Queue 寫入。', 'prompt', 0, 0, 40, '{"policy_id":"scheduled_retention","include_source_kind":"scheduled"}');

INSERT INTO knowledge_global_action_inputs
    (action_id, input_id, label, placeholder, input_style, required, max_length, sort_order)
VALUES
    ('capture', 'source', '網址或內容', '貼上文章網址、Notion 頁面連結或要整理的文字', 'paragraph', 1, 4000, 10),
    ('capture', 'note', '收藏原因（選填）', '為什麼想收藏？希望保留哪些重點？', 'paragraph', 0, 1000, 20),
    ('capture', 'classification', '分類提示（選填）', '例如：AI、新聞、Guide、Project 名稱', 'short', 0, 200, 30),
    ('search', 'question', '想查詢的問題', '例如：某個 Side Project 目前採用哪個方案？', 'paragraph', 1, 4000, 10),
    ('search', 'scope', '範圍（選填）', 'Project、主題、時間範圍或文章類型', 'short', 0, 300, 20);

INSERT INTO knowledge_policies
    (policy_id, retention_days, grace_days, max_items, queue_name, config_json)
VALUES
    ('scheduled_retention', 45, 7, 5, 'Knowledge Retention Queue', '{"eligible_decisions":["Pending","Trash Due"],"trash_fallback":"Trash Due"}');

INSERT INTO schema_migrations(version) VALUES (2);
