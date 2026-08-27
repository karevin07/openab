-- Tighten Knowledge Home search: any list-shaped answer must use search cards.

UPDATE knowledge_global_actions
SET prompt_template = '使用 notion-knowledge Skill 的 Search／Synthesis 模式，依使用者條件唯讀查詢 Notion 並附上相關頁面連結。先讀 references/query.md 與 references/discord-cards.md。若回覆是 1 至 5 筆搜尋結果、最近項目、推薦清單或任何可點連結的清單，必須嚴格以 search card contract 回覆（不要 Markdown table、不要編號清單文字）。只有需要跨頁推論或長篇綜合分析時，才可用有來源連結的普通 Markdown。不要修改 Notion。'
WHERE action_id = 'search' AND surface_id = 'home';

INSERT INTO schema_migrations(version) VALUES (7);
