-- Route Reading List overview through the Discord synthesis card contract
-- so stats stay scannable and currently-reading books remain clickable.

UPDATE knowledge_actions
SET prompt_template = '唯讀完整統計 To Read、Reading、Read 數量與常見 Tags／Category；不要以 limited result 冒充完整統計。先讀 references/discord-cards.md，並嚴格以 synthesis card contract 回覆：overview 放統計摘要（純文字／bullet，不要 Markdown table）；items 最多 5 筆，優先放 Status = Reading 的書並附 Notion URL。若目前沒有 Reading，改放最多 5 本 To Read（Expect 高到低）並在 overview 註明「目前無閱讀中」。若資料庫完全沒有可連結的書，改回簡短 Markdown。不要修改 Notion。'
WHERE source_id = 'personal_reading_list' AND action_id = 'overview';

INSERT INTO schema_migrations(version) VALUES (5);
