CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE knowledge_sources (
    source_id TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('scheduled', 'side_project', 'reading_list')),
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    notion_url TEXT NOT NULL,
    data_source_id TEXT,
    config_json TEXT NOT NULL DEFAULT '{}',
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    sort_order INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE knowledge_fields (
    source_id TEXT NOT NULL REFERENCES knowledge_sources(source_id) ON DELETE CASCADE,
    logical_name TEXT NOT NULL,
    notion_property TEXT NOT NULL,
    property_type TEXT NOT NULL,
    semantics TEXT NOT NULL DEFAULT '',
    options_json TEXT NOT NULL DEFAULT '[]',
    queryable INTEGER NOT NULL DEFAULT 1 CHECK (queryable IN (0, 1)),
    writable INTEGER NOT NULL DEFAULT 0 CHECK (writable IN (0, 1)),
    sort_order INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source_id, logical_name)
);

CREATE TABLE knowledge_actions (
    source_id TEXT NOT NULL REFERENCES knowledge_sources(source_id) ON DELETE CASCADE,
    action_id TEXT NOT NULL,
    label TEXT NOT NULL,
    button_style TEXT NOT NULL DEFAULT 'secondary' CHECK (button_style IN ('primary', 'secondary', 'success', 'danger')),
    title TEXT NOT NULL,
    prompt_template TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    sort_order INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source_id, action_id)
);

CREATE TABLE knowledge_action_inputs (
    source_id TEXT NOT NULL,
    action_id TEXT NOT NULL,
    input_id TEXT NOT NULL,
    label TEXT NOT NULL,
    placeholder TEXT NOT NULL DEFAULT '',
    input_style TEXT NOT NULL DEFAULT 'short' CHECK (input_style IN ('short', 'paragraph')),
    required INTEGER NOT NULL DEFAULT 0 CHECK (required IN (0, 1)),
    max_length INTEGER NOT NULL DEFAULT 300,
    sort_order INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (source_id, action_id, input_id),
    FOREIGN KEY (source_id, action_id) REFERENCES knowledge_actions(source_id, action_id) ON DELETE CASCADE
);

INSERT INTO knowledge_sources
    (source_id, source_kind, title, description, notion_url, data_source_id, config_json, sort_order)
VALUES
    ('github_ai_data_weekly', 'scheduled', '📚 Example AI & Data Weekly', 'Example weekly AI and data digest', 'https://www.notion.so/example-ai-data-weekly', NULL, '{"source_shape":"hub","retention":true}', 10),
    ('world_stories', 'scheduled', '🌍 Example World Stories', 'Example culture and history digest', 'https://www.notion.so/example-world-stories', 'collection://example-world-stories', '{"source_shape":"database","retention":true}', 20),
    ('weekly_reading_digest', 'scheduled', '📚 Example Weekly Reading Digest', 'Example weekly reading digest', 'https://www.notion.so/example-weekly-reading', 'collection://example-weekly-reading', '{"source_shape":"database","retention":true}', 30),
    ('project_notes_alpha', 'side_project', '🧰 Example Project Alpha', 'Example project notes and open questions', 'https://www.notion.so/example-project-notes', 'collection://example-project-notes', '{"notion_project":"Example Project Alpha"}', 40),
    ('project_notes_beta', 'side_project', '🧰 Example Project Beta', 'Example project notes and experiments', 'https://www.notion.so/example-project-notes', 'collection://example-project-notes', '{"notion_project":"Example Project Beta"}', 50),
    ('personal_reading_list', 'reading_list', '📕 Example Reading List', 'Example book backlog and reading status', 'https://www.notion.so/example-reading-list', 'collection://example-reading-list', '{"home_label":"📕 閱讀清單","retention":false}', 60);

INSERT INTO knowledge_fields
    (source_id, logical_name, notion_property, property_type, semantics, options_json, queryable, writable, sort_order)
VALUES
    ('github_ai_data_weekly', 'title', 'title', 'title', '週報標題', '[]', 1, 0, 10),
    ('github_ai_data_weekly', 'edition_date', 'page title/content', 'derived', '從週報子頁標題與內容辨識期別；Hub 不是結果', '[]', 1, 0, 20),
    ('github_ai_data_weekly', 'content', 'page content', 'content', '全文關鍵字', '[]', 1, 0, 30),
    ('world_stories', 'title', 'Title', 'title', '故事標題', '[]', 1, 0, 10),
    ('world_stories', 'category', 'Category', 'select', '內容分類', '[]', 1, 0, 20),
    ('world_stories', 'country', 'Country', 'select', '國家', '[]', 1, 0, 30),
    ('world_stories', 'date', 'Date', 'date', '故事日期', '[]', 1, 0, 40),
    ('world_stories', 'featured', 'Featured', 'checkbox', '精選保護條件', '[true,false]', 1, 0, 50),
    ('world_stories', 'read_later', 'Read Later', 'checkbox', '稍後閱讀保護條件', '[true,false]', 1, 0, 60),
    ('world_stories', 'story_type', 'Story Type', 'select', 'News、History、Culture 等故事類型', '[]', 1, 0, 70),
    ('world_stories', 'status', 'Status', 'select', '來源原生狀態', '[]', 1, 0, 80),
    ('weekly_reading_digest', 'title', '文章標題', 'title', '文章標題', '[]', 1, 0, 10),
    ('weekly_reading_digest', 'topic', '主題', 'multi_select', '文章主題', '[]', 1, 0, 20),
    ('weekly_reading_digest', 'source_author', '來源／作者', 'text', '文章來源或作者', '[]', 1, 0, 30),
    ('weekly_reading_digest', 'recommendation', '推薦度', 'select', '有序推薦度，必須轉成明確允許集合，不能比較星號字串', '["★★★★★","★★★★☆","★★★☆☆"]', 1, 0, 40),
    ('weekly_reading_digest', 'edition', '推薦週次', 'text', '推薦期別', '[]', 1, 0, 50),
    ('weekly_reading_digest', 'published_at', '發表日期', 'date', '文章發表日期', '[]', 1, 0, 60),
    ('weekly_reading_digest', 'status', '閱讀狀態', 'select', '待讀、已讀、收藏、略過', '["待讀","已讀","收藏","略過"]', 1, 0, 70),
    ('project_notes_alpha', 'content_type', 'Content Type', 'select', 'Project Note、Project Spec、Decision、Experiment', '[]', 1, 1, 10),
    ('project_notes_alpha', 'project', 'Project', 'select', '固定為 config.notion_project', '["Example Project Alpha"]', 1, 1, 20),
    ('project_notes_alpha', 'lifecycle', 'Lifecycle', 'select', 'Draft、Current、Superseded、Archived', '["Draft","Current","Superseded","Archived"]', 1, 1, 30),
    ('project_notes_alpha', 'status', 'Status', 'select', 'Inbox、Read、Archived', '["Inbox","Read","Archived"]', 1, 1, 40),
    ('project_notes_beta', 'content_type', 'Content Type', 'select', 'Project Note、Project Spec、Decision、Experiment', '[]', 1, 1, 10),
    ('project_notes_beta', 'project', 'Project', 'select', '固定為 config.notion_project', '["Example Project Beta"]', 1, 1, 20),
    ('project_notes_beta', 'lifecycle', 'Lifecycle', 'select', 'Draft、Current、Superseded、Archived', '["Draft","Current","Superseded","Archived"]', 1, 1, 30),
    ('project_notes_beta', 'status', 'Status', 'select', 'Inbox、Read、Archived', '["Inbox","Read","Archived"]', 1, 1, 40),
    ('personal_reading_list', 'title', 'Name', 'title', '書名與主要文字搜尋欄位', '[]', 1, 1, 10),
    ('personal_reading_list', 'status', 'Status', 'select', '待讀、閱讀中、已讀分別正規化為 To Read、Reading、Read', '["To Read","Reading","Read"]', 1, 1, 20),
    ('personal_reading_list', 'expectation', 'Expect', 'number', '閱讀前期望或優先度；待讀推薦由高到低', '[]', 1, 1, 30),
    ('personal_reading_list', 'score', 'Score', 'number', '閱讀後評分；不可取代 Expect', '[]', 1, 1, 40),
    ('personal_reading_list', 'author', 'Auther', 'text', '作者；保留資料庫既有拼字', '[]', 1, 1, 50),
    ('personal_reading_list', 'category', 'Category', 'select', '書籍分類', '["科普","學術","小說","傳記"]', 1, 1, 60),
    ('personal_reading_list', 'topics', 'Tags', 'multi_select', '主題標籤，使用原生 option', '["Marketing","Product","Engineering","Economic","Business","O''Reilly","Sociology"," Politics","Finance","Philosophy","Inspirational","Law","Psychology","Biology","History","Science","Knowledge","Novel","Learning","Communication","Technology","Skill","Management"]', 1, 1, 70),
    ('personal_reading_list', 'origin', 'From', 'multi_select', '來源地區', '["歐美","台灣","簡轉繁","日韓","中港"]', 1, 1, 80),
    ('personal_reading_list', 'series', 'Series', 'select', '書籍系列', '[]', 1, 1, 90),
    ('personal_reading_list', 'description', 'Description', 'text', '書籍描述或個人備註', '[]', 1, 1, 100),
    ('personal_reading_list', 'external_link', 'Link', 'url', '書店或外部參考連結', '[]', 1, 1, 110),
    ('personal_reading_list', 'reading_date', 'Date', 'date', '使用者管理的閱讀日期', '[]', 1, 1, 120),
    ('personal_reading_list', 'created_at', 'Create Date', 'created_time', '唯讀建立時間', '[]', 1, 0, 130),
    ('personal_reading_list', 'pending', 'Pending', 'formula', '目前 connector 不可 Query SQL；需要 SortByPending View 或逐頁 fetch', '[]', 0, 0, 140);

INSERT INTO knowledge_actions
    (source_id, action_id, label, button_style, title, prompt_template, sort_order)
VALUES
    ('github_ai_data_weekly', 'latest', '🆕 最新文章', 'primary', 'GitHub AI & Data Weekly｜最新文章', '使用 notion-knowledge Skill 唯讀列出最新 5 筆實際週報，依期別由新到舊，並以 search card contract 回覆。Hub 不是結果，不要修改 Notion。', 10),
    ('github_ai_data_weekly', 'search', '🔎 搜尋內容', 'secondary', 'GitHub AI & Data Weekly｜搜尋內容', '依結構化 adapter 與使用者條件唯讀搜尋實際週報，最多 5 筆，以 search card contract 回覆。不要修改 Notion。', 20),
    ('github_ai_data_weekly', 'synthesis', '📊 跨期整理', 'secondary', 'GitHub AI & Data Weekly｜跨期整理', '唯讀比較最近 3 期實際週報，整理共同趨勢、獨有主題與值得追蹤的變化，附 Notion 連結。不要修改 Notion。', 30),
    ('github_ai_data_weekly', 'retention', '🗑️ 待刪除', 'secondary', 'GitHub AI & Data Weekly｜待刪除', '唯讀查詢 Knowledge Retention Queue 中本來源 Decision = Pending 的項目，依 Delete After 列出最多 5 筆，以 retention card contract 回覆。不要新增 Queue 項目或修改來源。', 40),
    ('world_stories', 'latest', '🆕 最新文章', 'primary', 'World Stories｜最新文章', '依原生 Date 由新到舊唯讀列出最新 5 筆實際故事，以 search card contract 回覆。不要修改 Notion。', 10),
    ('world_stories', 'search', '🔎 搜尋內容', 'secondary', 'World Stories｜搜尋內容', '依結構化 adapter 與使用者條件使用參數化 SQL 唯讀搜尋，最多 5 筆，以 search card contract 回覆。不要修改 Notion。', 20),
    ('world_stories', 'synthesis', '📊 跨期整理', 'secondary', 'World Stories｜跨期整理', '唯讀比較最近 3 期內容，整理重複趨勢、獨有主題與值得追蹤的變化，附 Notion 連結。不要修改 Notion。', 30),
    ('world_stories', 'retention', '🗑️ 待刪除', 'secondary', 'World Stories｜待刪除', '唯讀查詢 Knowledge Retention Queue 中本來源 Decision = Pending 的項目，依 Delete After 列出最多 5 筆，以 retention card contract 回覆。不要新增 Queue 項目或修改來源。', 40),
    ('weekly_reading_digest', 'latest', '🆕 最新文章', 'primary', 'Example Weekly Reading Digest｜最新文章', '依推薦週次或發表日期由新到舊唯讀列出最新 5 筆實際文章，以 search card contract 回覆。不要修改 Notion。', 10),
    ('weekly_reading_digest', 'search', '🔎 搜尋內容', 'secondary', 'Example Weekly Reading Digest｜搜尋內容', '依結構化 adapter 與使用者條件使用參數化 SQL 唯讀搜尋，最低推薦度須轉成明確允許集合，最多 5 筆，以 search card contract 回覆。不要修改 Notion。', 20),
    ('weekly_reading_digest', 'synthesis', '📊 跨期整理', 'secondary', 'Example Weekly Reading Digest｜跨期整理', '唯讀比較最近 3 期內容，整理重複趨勢、獨有主題與值得追蹤的變化，附 Notion 連結。不要修改 Notion。', 30),
    ('weekly_reading_digest', 'retention', '🗑️ 待刪除', 'secondary', 'Example Weekly Reading Digest｜待刪除', '唯讀查詢 Knowledge Retention Queue 中本來源 Decision = Pending 的項目，依 Delete After 列出最多 5 筆，以 retention card contract 回覆。不要新增 Queue 項目或修改來源。', 40),
    ('project_notes_alpha', 'new', '💡 記下想法', 'primary', 'Example Project Alpha｜備忘預覽', '準備一筆 Content Type = Project Note、Lifecycle = Draft、Status = Inbox 的新增預覽。先檢查同 Project 近似筆記，不要寫入；依 project_note_preview card contract 回覆且只能一筆。類型只允許 Idea、Question、Todo、Reference，未提供時用 Idea。', 10),
    ('project_notes_alpha', 'recent', '🗒️ 最近備忘', 'secondary', 'Example Project Alpha｜最近備忘', '唯讀查詢最近更新的 5 筆 Project Note，依 project_notes card contract 回覆。不要修改 Notion。', 20),
    ('project_notes_alpha', 'search', '🔎 搜尋筆記', 'secondary', 'Example Project Alpha｜搜尋備忘', '唯讀查詢 Content Type = Project Note 且 Project 精確相符的內容，最多 5 筆，依 project_notes card contract 回覆。不要修改 Notion。', 30),
    ('project_notes_alpha', 'synthesis', '🧭 整理想法', 'secondary', 'Example Project Alpha｜整理目前想法', '唯讀整理 Draft Project Note 的主要方向、待釐清問題與可能下一步，每項附 Notion 連結；不要把想法當成已採用規格。', 40),
    ('project_notes_beta', 'new', '💡 記下想法', 'primary', 'Example Project Beta｜備忘預覽', '準備一筆 Content Type = Project Note、Lifecycle = Draft、Status = Inbox 的新增預覽。先檢查同 Project 近似筆記，不要寫入；依 project_note_preview card contract 回覆且只能一筆。類型只允許 Idea、Question、Todo、Reference，未提供時用 Idea。', 10),
    ('project_notes_beta', 'recent', '🗒️ 最近備忘', 'secondary', 'Example Project Beta｜最近備忘', '唯讀查詢最近更新的 5 筆 Project Note，依 project_notes card contract 回覆。不要修改 Notion。', 20),
    ('project_notes_beta', 'search', '🔎 搜尋筆記', 'secondary', 'Example Project Beta｜搜尋備忘', '唯讀查詢 Content Type = Project Note 且 Project 精確相符的內容，最多 5 筆，依 project_notes card contract 回覆。不要修改 Notion。', 30),
    ('project_notes_beta', 'synthesis', '🧭 整理想法', 'secondary', 'Example Project Beta｜整理目前想法', '唯讀整理 Draft Project Note 的主要方向、待釐清問題與可能下一步，每項附 Notion 連結；不要把想法當成已採用規格。', 40),
    ('personal_reading_list', 'recommend', '📚 待讀推薦', 'primary', 'Reading List｜待讀推薦', '唯讀查詢 Status = To Read，依 Expect DESC、Create Date DESC 列出最多 5 筆，以 search card contract 回覆。Score 不得替代 Expect。不要修改 Notion。', 10),
    ('personal_reading_list', 'current', '📖 正在閱讀', 'secondary', 'Reading List｜正在閱讀', '唯讀查詢 Status = Reading，列出最多 5 筆，以 search card contract 回覆。保留作者、Tags、Series 與 Date。不要修改 Notion。', 20),
    ('personal_reading_list', 'search', '🔎 搜尋書籍', 'secondary', 'Reading List｜搜尋書籍', '依結構化 adapter 與使用者條件使用參數化 SQL 唯讀查詢，最多 5 筆，以 search card contract 回覆。Notion row 是卡片 URL，外部 Link 可放摘要。不要修改 Notion。', 30),
    ('personal_reading_list', 'overview', '📊 閱讀概覽', 'secondary', 'Reading List｜閱讀概覽', '唯讀完整統計 To Read、Reading、Read 數量、常見 Tags 與 Category，並列出目前閱讀中的書；不要以 limited result 冒充完整統計。不要修改 Notion。', 40);

INSERT INTO knowledge_action_inputs
    (source_id, action_id, input_id, label, placeholder, input_style, required, max_length, sort_order)
VALUES
    ('github_ai_data_weekly', 'search', 'title', '標題包含', '例如：GitHub AI & Data Weekly', 'short', 0, 300, 10),
    ('github_ai_data_weekly', 'search', 'native_filter', '內容關鍵字', '例如：Context Database', 'short', 0, 300, 20),
    ('github_ai_data_weekly', 'search', 'range', '週次或時間範圍', '例如：最近 4 期、2026 年 8 月', 'short', 0, 300, 30),
    ('world_stories', 'search', 'title', '標題包含', '例如：日本、王室、文化資產', 'short', 0, 300, 10),
    ('world_stories', 'search', 'native_filter', '分類或國家', '例如：王室、Japan', 'short', 0, 300, 20),
    ('world_stories', 'search', 'state', '精選／待深入研究', 'Featured、Read Later 或不限', 'short', 0, 300, 30),
    ('world_stories', 'search', 'secondary_filter', '故事類型', 'News、History、Culture…', 'short', 0, 300, 40),
    ('world_stories', 'search', 'range', '時間範圍', '例如：最近 4 週、2026 年 8 月', 'short', 0, 300, 50),
    ('weekly_reading_digest', 'search', 'title', '標題包含', '例如：Agent reliability', 'short', 0, 300, 10),
    ('weekly_reading_digest', 'search', 'native_filter', '最低推薦度', '例如：★★★★☆', 'short', 0, 300, 20),
    ('weekly_reading_digest', 'search', 'secondary_filter', '主題', '例如：AI Agent、Data Platform', 'short', 0, 300, 30),
    ('weekly_reading_digest', 'search', 'state', '閱讀狀態', '待讀、已讀、收藏、略過', 'short', 0, 300, 40),
    ('weekly_reading_digest', 'search', 'range', '時間範圍', '例如：最近 4 週、2026 年 8 月', 'short', 0, 300, 50),
    ('project_notes_alpha', 'new', 'title', '標題', '例如：加入新的控制方式', 'short', 1, 120, 10),
    ('project_notes_alpha', 'new', 'note', '想法／備忘', '描述目前想到的內容，不需要先整理完整', 'paragraph', 1, 4000, 20),
    ('project_notes_alpha', 'new', 'kind', '類型（選填）', 'Idea、Question、Todo 或 Reference', 'short', 0, 100, 30),
    ('project_notes_alpha', 'new', 'next_step', '下一步／待確認（選填）', '例如：先測試 Web Bluetooth 延遲', 'paragraph', 0, 1000, 40),
    ('project_notes_alpha', 'new', 'source_url', '相關連結（選填）', '文章、產品、GitHub 或 Notion URL', 'short', 0, 1000, 50),
    ('project_notes_alpha', 'search', 'query', '標題或內容關鍵字', '例如：功能、風險、實驗', 'short', 0, 300, 10),
    ('project_notes_alpha', 'search', 'kind', '類型（選填）', 'Idea、Question、Todo 或 Reference', 'short', 0, 100, 20),
    ('project_notes_alpha', 'search', 'state', '狀態（選填）', 'Draft、Current 或 Archived', 'short', 0, 100, 30),
    ('project_notes_alpha', 'search', 'range', '時間範圍（選填）', '例如：最近 30 天、2026 年 8 月', 'short', 0, 200, 40),
    ('project_notes_beta', 'new', 'title', '標題', '例如：整理下一個實驗大綱', 'short', 1, 120, 10),
    ('project_notes_beta', 'new', 'note', '想法／備忘', '描述目前想到的內容，不需要先整理完整', 'paragraph', 1, 4000, 20),
    ('project_notes_beta', 'new', 'kind', '類型（選填）', 'Idea、Question、Todo 或 Reference', 'short', 0, 100, 30),
    ('project_notes_beta', 'new', 'next_step', '下一步／待確認（選填）', '例如：先驗證下一階段目標', 'paragraph', 0, 1000, 40),
    ('project_notes_beta', 'new', 'source_url', '相關連結（選填）', '文章、產品、GitHub 或 Notion URL', 'short', 0, 1000, 50),
    ('project_notes_beta', 'search', 'query', '標題或內容關鍵字', '例如：內容、流程、成效', 'short', 0, 300, 10),
    ('project_notes_beta', 'search', 'kind', '類型（選填）', 'Idea、Question、Todo 或 Reference', 'short', 0, 100, 20),
    ('project_notes_beta', 'search', 'state', '狀態（選填）', 'Draft、Current 或 Archived', 'short', 0, 100, 30),
    ('project_notes_beta', 'search', 'range', '時間範圍（選填）', '例如：最近 30 天、2026 年 8 月', 'short', 0, 200, 40),
    ('personal_reading_list', 'search', 'title', '書名包含', '例如：經濟學、世界史', 'short', 0, 300, 10),
    ('personal_reading_list', 'search', 'state', '閱讀狀態', 'To Read、Reading、Read 或不限', 'short', 0, 300, 20),
    ('personal_reading_list', 'search', 'topic', '主題或分類', '例如：History、Engineering、科普', 'short', 0, 300, 30),
    ('personal_reading_list', 'search', 'author_origin', '作者或來源地區', '例如：余英時、台灣、日韓', 'short', 0, 300, 40),
    ('personal_reading_list', 'search', 'rating', '最低期望／評分', '例如：Expect >= 5、Score >= 4', 'short', 0, 300, 50);

INSERT INTO schema_migrations(version) VALUES (1);
