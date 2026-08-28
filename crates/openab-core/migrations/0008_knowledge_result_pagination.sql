-- Allow read-only result cards to return enough rows for Components V2 pagination.
-- Confirmation/retention cards deliberately keep their five-item safety bound.

UPDATE knowledge_actions
SET prompt_template = replace(
    replace(
        replace(prompt_template, '最新 5 筆', '最新 25 筆'),
        '最近更新的 5 筆', '最近更新的 25 筆'
    ),
    '最多 5 筆', '最多 25 筆'
)
WHERE action_id IN ('latest', 'search', 'recent', 'recommend', 'current', 'synthesis', 'overview');

UPDATE knowledge_global_actions
SET prompt_template = '使用 notion-knowledge Skill 的 Search／Synthesis 模式，依使用者條件唯讀查詢 Notion 並附上相關頁面連結。先讀 references/query.md 與 references/discord-cards.md。若回覆是 1 至 25 筆搜尋結果、最近項目、推薦清單或任何可點連結的清單，必須嚴格以 search card contract 回覆（不要 Markdown table、不要編號清單文字）；超過 5 筆會由 Discord 卡片自動分頁。只有需要跨頁推論或長篇綜合分析時，才可用有來源連結的普通 Markdown。不要修改 Notion。'
WHERE action_id = 'search';

UPDATE knowledge_global_actions
SET prompt_template = replace(prompt_template, '最近收錄的 5 筆', '最近收錄的 25 筆')
WHERE action_id = 'recent';

INSERT INTO schema_migrations(version) VALUES (8);
