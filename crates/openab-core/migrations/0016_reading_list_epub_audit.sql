INSERT INTO knowledge_actions
    (source_id, action_id, label, button_style, title, prompt_template, sort_order)
VALUES
    ('personal_reading_list', 'audit', '🔍 EPUB 健檢', 'secondary',
     'Reading List｜EPUB 一致性檢查',
     '使用 notion-knowledge Skill 的 Reading List EPUB Audit 模式。先 fetch personal Reading List schema，再以完整 pagination 取得全部 row；每筆只保留 Notion page URL、title、EPUB Status、EPUB Link、EPUB File ID、EPUB SHA256 與 Discord Thread ID。不得用 limited search result 冒充完整 inventory。將完整 inventory 一次傳給 reading_list_epub_audit；此工具唯讀掃描設定的 Drive 目錄第一層並驗證 Forum thread。依工具回傳的 status、sources、summary 與 issues 產生 epub_audit card contract：overview 必須列出 Notion row、Drive EPUB、Forum thread ID、error、warning 數量與 Complete 或 Partial；items 只能使用工具回傳 issue，不得自行猜測。若沒有 issue，仍建立一筆連到 Reading List 的「三方資料一致」項目。最多輸出 25 筆 issue 並註明工具實際 issue 總數。不要修改 Notion、Drive 或 Discord。',
     45);

INSERT INTO schema_migrations(version) VALUES (16);
