UPDATE knowledge_global_actions
SET prompt_template = replace(
        prompt_template,
        '依 capture_inbox_preview card contract 只顯示第一筆 ready 項目',
        '依 capture_inbox_preview card contract 只顯示第一筆 ready 項目。該卡片與單篇 capture_preview 不同：JSON 頂層必須同時包含 next 回傳的 inbox_id 與 item_index，缺少任一個都會被 adapter 拒絕；heading 用「Capture Inbox｜<index> / <total>」，meta 用「Content Type · Tags · create|update|skip」，第二段是既有 Tags 而不是 Lifecycle'
    )
WHERE action_id = 'capture_inbox'
  AND prompt_template NOT LIKE '%JSON 頂層必須同時包含%';

UPDATE knowledge_global_actions
SET prompt_template = prompt_template
    || ' 回傳的 capture_inbox_preview 必須帶上本次操作的 inbox_id 與 item_index，並使用「Content Type · Tags · create|update|skip」的 meta 格式。'
WHERE action_id IN ('capture_inbox_accept', 'capture_inbox_skip', 'capture_inbox_modify')
  AND prompt_template NOT LIKE '%必須帶上本次操作的 inbox_id%';

INSERT INTO schema_migrations(version) VALUES (18);
