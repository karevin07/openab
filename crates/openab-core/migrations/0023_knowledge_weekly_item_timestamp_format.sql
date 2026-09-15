-- Incident (second one this cycle, after 0022's fix made it visible instead of
-- silent): a manual re-run's payload was rejected with "invalid knowledge
-- weekly item created_at" -- chrono::DateTime::parse_from_rfc3339 failed on at
-- least one item's created_at. The prompt already spelled out "皆為含
-- timezone 的 ISO-8601" for window_start/window_end/queried_at, but never
-- said the same for each item's created_at, which is validated by the exact
-- same strict RFC3339 parser.
--
-- Rather than patch one more field description and wait for the next drift,
-- give the model a literal example of the exact JSON shape
-- KnowledgeWeeklyAudit/KnowledgeWeeklySource/KnowledgeWeeklyItem in
-- discord_admin.rs require (field names, enum values, and a correctly
-- timezoned timestamp on every date field) so it has a concrete shape to
-- mirror instead of reconstructing the contract from prose each run.

UPDATE knowledge_global_actions
SET prompt_template = prompt_template
    || ' 格式範例（欄位名稱、型別與 created_at 的 timezone 都要完全比照，只是示範資料）：
{"window_start":"2026-09-08T00:00:00+08:00","window_end":"2026-09-15T00:00:00+08:00","queried_at":"2026-09-15T08:35:00+08:00","sources":[{"source_id":"github_ai_data_weekly","title":"GitHub AI & Data Weekly","url":"https://app.notion.com/p/...","status":"updated","error":"","items":[{"page_id":"...","title":"2026-09-14｜GitHub AI & Data Weekly","url":"https://app.notion.com/...","created_at":"2026-09-14T00:59:03Z"}]},{"source_id":"world_stories","title":"World Stories","url":"https://app.notion.com/p/...","status":"no_updates","error":"","items":[]}]}
created_at 直接使用 Notion 該頁 created_time 的原始值（已經含 timezone，通常是 Z 結尾），不可只寫日期、不可省略時區、不可自行重新格式化或改成本地時間。'
WHERE action_id = 'weekly_source_audit'
  AND prompt_template NOT LIKE '%格式範例%';

INSERT INTO schema_migrations(version) VALUES (23);
