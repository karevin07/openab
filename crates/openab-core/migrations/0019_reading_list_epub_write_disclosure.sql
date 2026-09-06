-- The Reading List card gained a write action (📥 EPUB Intake, migration 0014)
-- and a read-only one (🔍 EPUB 健檢, migration 0016), but the help and
-- reading_list view copy still promised that every shortcut stays read-only.
-- Correct the disclosure so the confirmed-write path is spelled out.

UPDATE knowledge_ui_views
SET description = replace(
        description,
        '**閱讀清單**：依書名、狀態、主題、作者、地區、期望或評分查詢書籍，快捷操作維持唯讀。',
        '**閱讀清單**：查詢待讀、閱讀中、已讀與 EPUB 健檢維持唯讀；EPUB Intake 會先預覽，按「✅ 確認上傳」後才上傳 Drive 並寫入 Notion 的 EPUB 欄位。'
    )
WHERE view_id = 'help'
  AND description NOT LIKE '%按「✅ 確認上傳」後才上傳 Drive%';

UPDATE knowledge_ui_views
SET description = '查詢待讀、閱讀中、已讀與 EPUB 健檢維持唯讀，不會變更閱讀狀態或評分。EPUB Intake 會先顯示預覽，按「✅ 確認上傳」後才上傳 Drive 並寫入 EPUB 欄位。'
WHERE view_id = 'reading_list'
  AND description NOT LIKE '%確認上傳%';

INSERT INTO schema_migrations(version) VALUES (19);
