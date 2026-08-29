-- Knowledge Hub is navigation metadata, not a scheduled content source.
UPDATE knowledge_ui_views
SET config_json = '{"command_description":"Open the knowledge assistant home card","hub_label":"🏠 開啟 Knowledge Hub","hub_url":"https://app.notion.com/p/example-knowledge-hub"}'
WHERE view_id = 'home';

INSERT INTO schema_migrations(version) VALUES (12);
