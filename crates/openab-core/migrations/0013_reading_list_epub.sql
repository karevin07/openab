INSERT INTO knowledge_fields
    (source_id, logical_name, notion_property, property_type, semantics, options_json, queryable, writable, sort_order)
VALUES
    ('personal_reading_list', 'epub_link', 'EPUB Link', 'url', 'Google Drive EPUB 永久檔案頁面；不要保存會過期的 signed URL', '[]', 1, 1, 150),
    ('personal_reading_list', 'epub_file_id', 'EPUB File ID', 'text', 'Google Drive file ID，供同步器定位與更新同一本書', '[]', 1, 1, 160),
    ('personal_reading_list', 'epub_added_at', 'EPUB Added At', 'date', 'EPUB 最近成功上傳時間', '[]', 1, 1, 170),
    ('personal_reading_list', 'epub_sha256', 'EPUB SHA256', 'text', '內容雜湊，供重複檔案與版本判斷', '[]', 1, 1, 180),
    ('personal_reading_list', 'recommended_at', 'Recommended At', 'date', '實際推薦時間；最近推薦必須以此欄排序，不得以 Create Date 代替', '[]', 1, 1, 190),
    ('personal_reading_list', 'recommended_by', 'Recommended By', 'select', '推薦來源', '["Knowledge Assistant","User","Other"]', 1, 1, 200),
    ('personal_reading_list', 'discord_thread_id', 'Discord Thread ID', 'text', 'reading-list-epub Forum 的書籍 thread ID', '[]', 1, 1, 210),
    ('personal_reading_list', 'epub_status', 'EPUB Status', 'select', 'EPUB 可用狀態', '["Missing","Available","Failed"]', 1, 1, 220);

INSERT INTO knowledge_actions
    (source_id, action_id, label, button_style, title, prompt_template, sort_order)
VALUES
    ('personal_reading_list', 'recent_finance', '💰 最近金融推薦', 'secondary', 'Reading List｜最近金融推薦', '唯讀查詢 Tags 包含 Finance 且 Recommended By = Knowledge Assistant 的書籍，必須依 Recommended At DESC 排序並列出最多 5 筆，以 search card contract 回覆。保留 EPUB Status 與 EPUB Link；沒有 Recommended At 的舊資料不得冒充最近推薦。不要修改 Notion。', 15);

INSERT INTO schema_migrations(version) VALUES (13);
