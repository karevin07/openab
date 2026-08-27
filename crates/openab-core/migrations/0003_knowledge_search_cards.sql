-- Make list-shaped Knowledge Home searches opt in to the Discord card
-- envelope. The renderer already validates this contract; keeping the routing
-- instruction in SQLite avoids hardcoded workflow prompts in Rust.

UPDATE knowledge_global_actions
SET prompt_template = '使用 notion-knowledge Skill 的 Search／Synthesis 模式，依使用者條件唯讀查詢 Notion 並附上相關頁面連結。先讀 references/query.md；當回覆重點是 1 至 5 筆搜尋結果、最近項目或推薦清單時，也讀 references/discord-cards.md，並嚴格以 search card contract 回覆，不要輸出 Markdown table。若問題需要跨頁推論或長篇綜合分析，維持有來源連結的普通 Markdown。不要修改 Notion。'
WHERE action_id = 'search' AND surface_id = 'home';

UPDATE knowledge_global_actions
SET prompt_template = '使用 notion-knowledge Skill 的 Search 模式，唯讀查詢 Knowledge Library 最近收錄的 5 筆內容。依建立時間（無法取得時才用更新時間）由新到舊；先讀 references/query.md 與 references/discord-cards.md，並嚴格以 search card contract 回覆，不要輸出 Markdown table。每張卡片 title 放標題、url 放該 Notion row、meta 依序合併 Content Type、Project（若有）、Status、Lifecycle，summary 放簡短內容摘要。不要修改 Notion。'
WHERE action_id = 'recent' AND surface_id = 'home';

INSERT INTO schema_migrations(version) VALUES (3);
