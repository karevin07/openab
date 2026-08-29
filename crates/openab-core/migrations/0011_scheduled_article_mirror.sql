UPDATE knowledge_global_actions
SET prompt_template = prompt_template || '

Scheduled article mirror contract: in the same OPENAB_KNOWLEDGE_CARDS_V1 object, add an articles array holding the entries examined during this run so later questions can be answered without re-querying Notion. Each element needs source_id (one of github_ai_data_weekly, world_stories, weekly_reading_digest), page_id, title, and an https url; published_at is an optional timezone-aware ISO-8601 timestamp and summary is optional. Emit at most 500 entries, never repeat the same source_id and page_id pair, and only include entries actually read from the source during this run. The articles array is telemetry, not card content: it is never rendered in Discord, so it must not replace or duplicate the retention items array, which still carries only verified Decision=Pending rows. Omit the articles array entirely when no source could be read. Never invent titles, IDs, URLs, or timestamps.'
WHERE action_id = 'retention_scan';

INSERT INTO schema_migrations(version) VALUES (11);
