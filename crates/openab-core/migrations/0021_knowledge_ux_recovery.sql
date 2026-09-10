INSERT INTO knowledge_global_actions
    (action_id, surface_id, label, button_style, title, prompt_template, behavior, visible, row_number, sort_order, config_json)
VALUES
    ('capture_inbox_resume', 'home', '⏯️ 繼續 Inbox', 'secondary',
     'Knowledge Capture Inbox｜繼續',
     '使用 notion-knowledge Skill 的 Capture Inbox Resume 模式。以使用者提供的 inbox_id 呼叫 knowledge_capture_inbox next；若仍有 queued 項目，逐筆呼叫 process、完成 Knowledge Library 重複檢查與既有 taxonomy 語意分類後 stage，來源失敗則 decide fail，且不可中止後續項目。最後再次呼叫 next。若有 ready item，依 capture_inbox_preview contract 顯示唯一下一筆，並把 summary 的 total、queued、ready、accepted、skipped、failed 原樣放在卡片頂層；若已完成則回傳 summary。不得重播 accepted／skipped 項目，也不得在預覽階段寫入 Notion。',
     'modal', 1, 1, 15, '{}'),
    ('capture_inbox_retry', 'confirmation', '重試失敗項目', 'secondary',
     'Knowledge Capture Inbox｜重試失敗項目',
     '使用 notion-knowledge Skill 的 Capture Inbox Retry 模式。Discord 按鈕只授權指定 inbox_id。呼叫 knowledge_capture_inbox retry 重新排入該 inbox 所有 failed 項目；只對 newly queued 項目依序 process、完成 Knowledge Library exact Source URL 重複檢查與既有 taxonomy 語意分類後 stage，若仍失敗則 decide fail，且不可影響 accepted／skipped 項目。最後呼叫 next；若有 ready item，依 capture_inbox_preview contract 顯示唯一下一筆，並帶齊 summary counters，否則回傳 summary。預覽階段不得寫入 Notion。',
     'prompt', 0, 0, 23, '{}'),
    ('knowledge_card_retry', 'recovery', '重試卡片', 'primary',
     'Knowledge 卡片｜重新產生',
     '上一個 Knowledge 回覆的結構化卡片驗證失敗。請根據同一對話中最近一個 Knowledge 使用者請求與你的既有處理結果，重新輸出一次；嚴格遵守 notion-knowledge references/discord-cards.md 對應 contract。只輸出 OPENAB_KNOWLEDGE_CARDS_V1 marker 與單一合法 JSON envelope，不要解釋、不要 Markdown code fence。不得因重試而重複任何外部寫入。',
     'prompt', 0, 0, 90, '{}'),
    ('knowledge_card_plain', 'recovery', '改用文字', 'secondary',
     'Knowledge 結果｜文字回覆',
     '上一個 Knowledge 回覆的結構化卡片驗證失敗。請根據同一對話中最近一個 Knowledge 使用者請求與你的既有處理結果，改以精簡繁體中文 Markdown 回覆，附上已驗證的來源連結。不得輸出 OPENAB_KNOWLEDGE_CARDS_V1，不得因改寫而重複任何外部寫入。',
     'prompt', 0, 0, 91, '{}')
ON CONFLICT(action_id) DO UPDATE SET
    label=excluded.label,
    button_style=excluded.button_style,
    title=excluded.title,
    prompt_template=excluded.prompt_template,
    behavior=excluded.behavior,
    visible=excluded.visible,
    row_number=excluded.row_number,
    sort_order=excluded.sort_order;

INSERT INTO knowledge_global_action_inputs
    (action_id, input_id, label, placeholder, input_style, required, max_length, sort_order)
VALUES
    ('capture_inbox_resume', 'inbox_id', 'Inbox ID',
     'xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx', 'short', 1, 36, 10)
ON CONFLICT(action_id, input_id) DO UPDATE SET
    label=excluded.label,
    placeholder=excluded.placeholder,
    input_style=excluded.input_style,
    required=excluded.required,
    max_length=excluded.max_length,
    sort_order=excluded.sort_order;

UPDATE knowledge_global_actions
SET prompt_template = prompt_template
    || ' 每張 capture_inbox_preview 都必須把 next.summary 的 total、queued、ready、accepted、skipped、failed 原樣放在 JSON 頂層。'
WHERE action_id IN ('capture_inbox', 'capture_inbox_accept', 'capture_inbox_skip', 'capture_inbox_modify')
  AND prompt_template NOT LIKE '%next.summary 的 total%';

UPDATE knowledge_ui_views
SET description = description || '
**繼續 Inbox**：輸入先前卡片顯示的 Inbox ID，可從 SQLite 保存的進度接續。'
WHERE view_id = 'help' AND description NOT LIKE '%**繼續 Inbox**%';

INSERT INTO schema_migrations(version) VALUES (21);
