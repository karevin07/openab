-- Incident (2026-10-06): the weekly audit payload was rejected with
-- "knowledge weekly error is too long". The model could not read a verbatim
-- created_time for the hub-shaped github_ai_data_weekly source (its editions
-- are standalone child pages; the connector only exposes createdTime for
-- data-source rows), so 0023's "use the raw created_time" rule pushed it to
-- report `partial` with a 718-char explanation. discord_admin.rs now clips
-- overlong errors instead of dropping the whole report; this migration states
-- the error length budget and gives hub sources a concrete created_at rule
-- matching what every earlier run used (the created_date_range search hit's
-- timestamp).

UPDATE knowledge_global_actions
SET prompt_template = prompt_template
    || ' error 最多 300 字元，用一句話說明原因即可，不要寫查證過程。hub 型來源（source_shape=hub、data_source_id=not_applicable）的子頁沒有可直接讀取的 created_time：以 notion-search 搭配 page_url=hub 與 created_date_range 篩選找出區間內的子頁，並用該筆搜尋結果的 timestamp 作為 created_at，照常回報 updated，不要因為拿不到 created_time 就改成 partial。只有當該 timestamp 落在區間外（代表頁面之後又被編輯過）時，才改用 partial，並在 error 簡短說明。'
WHERE action_id = 'weekly_source_audit'
  AND prompt_template NOT LIKE '%hub 型來源%';

INSERT INTO schema_migrations(version) VALUES (24);
