-- Incident: the model appended the OPENAB_KNOWLEDGE_WEEKLY_V1 marker directly
-- to the end of a trailing aside ("...let me verify completeness for both
-- database sources.OPENAB_KNOWLEDGE_WEEKLY_V1") with no line break, and the
-- JSON itself never arrived in that same message. discord_admin.rs's marker
-- detection is hardened separately to at least surface a visible parse
-- failure when this happens again; this migration tightens the prompt itself
-- to make the slip less likely by naming the exact failure mode and telling
-- the model to do any final verification before starting its last message,
-- not inside it.

UPDATE knowledge_global_actions
SET prompt_template = prompt_template
    || ' 完成核對後才開始輸出最後一則訊息：不要在同一則訊息裡先寫任何確認、核對或「讓我檢查」之類的過場句——那則訊息必須以 OPENAB_KNOWLEDGE_WEEKLY_V1 這一行開頭，前面不能有任何文字，這一行也不能接著同一行的其他內容；JSON 從下一行開始。曾經發生過模型把這個 marker 直接接在前一句話尾端、JSON 卻沒有一起送出的情況，那筆統計就會遺失且不會有任何錯誤訊息。'
WHERE action_id = 'weekly_source_audit'
  AND prompt_template NOT LIKE '%完成核對後才開始輸出最後一則訊息%';

INSERT INTO schema_migrations(version) VALUES (22);
