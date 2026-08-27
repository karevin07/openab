-- Route scheduled-source and Side Project synthesis actions through the
-- Discord synthesis card contract instead of Markdown tables.

UPDATE knowledge_actions
SET prompt_template = '唯讀比較最近 3 期實際週報，整理共同趨勢、獨有主題與值得追蹤的變化。先讀 references/query.md 與 references/discord-cards.md，並嚴格以 synthesis card contract 回覆：overview 放壓縮趨勢與追蹤重點（純文字／bullet，不要 Markdown table）；items 最多 5 筆，每筆附實際週報或重點故事的 Notion URL。Hub 不是結果，不要修改 Notion。'
WHERE source_id = 'github_ai_data_weekly' AND action_id = 'synthesis';

UPDATE knowledge_actions
SET prompt_template = '唯讀比較最近 3 期內容，整理重複趨勢、獨有主題與值得追蹤的變化。先讀 references/query.md 與 references/discord-cards.md，並嚴格以 synthesis card contract 回覆：overview 放壓縮趨勢與追蹤重點（純文字／bullet，不要 Markdown table）；items 最多 5 筆，每筆附代表性故事的 Notion URL。不要修改 Notion。'
WHERE source_id = 'world_stories' AND action_id = 'synthesis';

UPDATE knowledge_actions
SET prompt_template = '唯讀比較最近 3 期內容，整理重複趨勢、獨有主題與值得追蹤的變化。先讀 references/query.md 與 references/discord-cards.md，並嚴格以 synthesis card contract 回覆：overview 放壓縮趨勢與追蹤重點（純文字／bullet，不要 Markdown table）；items 最多 5 筆，每筆附代表性文章的 Notion URL。不要修改 Notion。'
WHERE source_id = 'weekly_reading_digest' AND action_id = 'synthesis';

UPDATE knowledge_actions
SET prompt_template = '唯讀整理 Draft Project Note 的主要方向、待釐清問題與可能下一步。先讀 references/project-notes.md、references/query.md 與 references/discord-cards.md，並嚴格以 synthesis card contract 回覆：overview 放壓縮方向與待釐清點（純文字／bullet，不要 Markdown table）；items 最多 5 筆，每項附 Notion 連結；不要把想法當成已採用規格。'
WHERE source_id = 'project_notes_alpha' AND action_id = 'synthesis';

UPDATE knowledge_actions
SET prompt_template = '唯讀整理 Draft Project Note 的主要方向、待釐清問題與可能下一步。先讀 references/project-notes.md、references/query.md 與 references/discord-cards.md，並嚴格以 synthesis card contract 回覆：overview 放壓縮方向與待釐清點（純文字／bullet，不要 Markdown table）；items 最多 5 筆，每項附 Notion 連結；不要把想法當成已採用規格。'
WHERE source_id = 'project_notes_beta' AND action_id = 'synthesis';

INSERT INTO schema_migrations(version) VALUES (4);
