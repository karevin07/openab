//! SQLite-backed configuration catalog for the Discord knowledge assistant.
//!
//! Notion remains the content store. This catalog only owns source identity,
//! schema mappings, UI actions, and form definitions.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tracing::warn;

const MIGRATION_0001: &str = include_str!("../migrations/0001_knowledge_catalog.sql");
const MIGRATION_0002: &str = include_str!("../migrations/0002_knowledge_workflows.sql");
const MIGRATION_0003: &str = include_str!("../migrations/0003_knowledge_search_cards.sql");
const MIGRATION_0004: &str = include_str!("../migrations/0004_knowledge_synthesis_cards.sql");
const MIGRATION_0005: &str = include_str!("../migrations/0005_knowledge_reading_overview_cards.sql");
const MIGRATION_0006: &str = include_str!("../migrations/0006_knowledge_capture_preview_cards.sql");
const MIGRATION_0007: &str = include_str!("../migrations/0007_knowledge_search_prompt_tighten.sql");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeField {
    pub logical_name: String,
    pub notion_property: String,
    pub property_type: String,
    pub semantics: String,
    pub options_json: String,
    pub queryable: bool,
    pub writable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeActionInput {
    pub input_id: String,
    pub label: String,
    pub placeholder: String,
    pub input_style: String,
    pub required: bool,
    pub max_length: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeAction {
    pub action_id: String,
    pub label: String,
    pub button_style: String,
    pub title: String,
    pub prompt_template: String,
    pub inputs: Vec<KnowledgeActionInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeView {
    pub view_id: String,
    pub title: String,
    pub description: String,
    pub colour: u32,
    pub footer: String,
    pub field_label: String,
    pub field_value: String,
    pub select_placeholder: String,
    pub config_json: String,
}

impl KnowledgeView {
    pub fn config_string(&self, key: &str) -> Option<String> {
        serde_json::from_str::<Value>(&self.config_json)
            .ok()?
            .get(key)?
            .as_str()
            .map(ToOwned::to_owned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeGlobalAction {
    pub action_id: String,
    pub surface_id: String,
    pub label: String,
    pub button_style: String,
    pub title: String,
    pub prompt_template: String,
    pub behavior: String,
    pub visible: bool,
    pub row_number: u8,
    pub config_json: String,
    pub inputs: Vec<KnowledgeActionInput>,
}

impl KnowledgeGlobalAction {
    pub fn config_string(&self, key: &str) -> Option<String> {
        serde_json::from_str::<Value>(&self.config_json)
            .ok()?
            .get(key)?
            .as_str()
            .map(ToOwned::to_owned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgePolicy {
    pub policy_id: String,
    pub retention_days: u16,
    pub grace_days: u16,
    pub max_items: u8,
    pub queue_name: String,
    pub config_json: String,
}

impl KnowledgePolicy {
    pub fn adapter_context(&self) -> String {
        format!(
            "Structured Knowledge Policy\npolicy_id: {}\nretention_days: {}\ngrace_days: {}\nmax_items: {}\nqueue_name: {}\nconfig: {}",
            self.policy_id,
            self.retention_days,
            self.grace_days,
            self.max_items,
            self.queue_name,
            self.config_json
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeSource {
    pub source_id: String,
    pub source_kind: String,
    pub title: String,
    pub description: String,
    pub notion_url: String,
    pub data_source_id: String,
    pub config_json: String,
    pub fields: Vec<KnowledgeField>,
    pub actions: Vec<KnowledgeAction>,
}

impl KnowledgeSource {
    pub fn config_string(&self, key: &str) -> Option<String> {
        serde_json::from_str::<Value>(&self.config_json)
            .ok()?
            .get(key)?
            .as_str()
            .map(ToOwned::to_owned)
    }

    pub fn action(&self, action_id: &str) -> Option<&KnowledgeAction> {
        self.actions
            .iter()
            .find(|action| action.action_id == action_id)
    }

    pub fn adapter_context(&self) -> String {
        let fields = self
            .fields
            .iter()
            .map(|field| {
                format!(
                    "- {} => {} ({}, queryable={}, writable={}): {}{}",
                    field.logical_name,
                    field.notion_property,
                    field.property_type,
                    field.queryable,
                    field.writable,
                    field.semantics,
                    if field.options_json == "[]" {
                        String::new()
                    } else {
                        format!("; options={}", field.options_json)
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Structured Knowledge Adapter\nsource_id: {}\nsource_kind: {}\ntitle: {}\nNotion URL: {}\ndata_source_id: {}\nconfig: {}\nfields:\n{}",
            self.source_id,
            self.source_kind,
            self.title,
            self.notion_url,
            if self.data_source_id.is_empty() {
                "not_applicable"
            } else {
                &self.data_source_id
            },
            self.config_json,
            fields
        )
    }
}

#[derive(Debug, Clone)]
pub struct KnowledgeCatalog {
    pub sources: Vec<KnowledgeSource>,
    pub views: Vec<KnowledgeView>,
    pub global_actions: Vec<KnowledgeGlobalAction>,
    pub policies: Vec<KnowledgePolicy>,
}

impl KnowledgeCatalog {
    pub fn open_or_seed(path: Option<&Path>) -> Result<Self> {
        let mut connection = match path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("create knowledge catalog directory {}", parent.display())
                    })?;
                }
                Connection::open(path)
                    .with_context(|| format!("open knowledge catalog {}", path.display()))?
            }
            None => Connection::open_in_memory().context("open in-memory knowledge catalog")?,
        };
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")
            .context("configure knowledge catalog")?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (\
                    version INTEGER PRIMARY KEY, \
                    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\
                );",
            )
            .context("initialize knowledge catalog migrations")?;
        for (version, migration) in [
            (1, MIGRATION_0001),
            (2, MIGRATION_0002),
            (3, MIGRATION_0003),
            (4, MIGRATION_0004),
            (5, MIGRATION_0005),
            (6, MIGRATION_0006),
            (7, MIGRATION_0007),
        ] {
            let applied = connection
                .query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = ?1",
                    params![version],
                    |_| Ok(()),
                )
                .is_ok();
            if applied {
                continue;
            }
            let transaction = connection
                .transaction()
                .context("start knowledge catalog migration")?;
            transaction
                .execute_batch(migration)
                .with_context(|| format!("apply knowledge catalog migration {version:04}"))?;
            transaction
                .commit()
                .context("commit knowledge catalog migration")?;
        }
        Self::load(&connection)
    }

    fn load(connection: &Connection) -> Result<Self> {
        let mut source_statement = connection.prepare(
            "SELECT source_id, source_kind, title, description, notion_url, \
                    COALESCE(data_source_id, ''), config_json \
             FROM knowledge_sources WHERE enabled = 1 ORDER BY sort_order, source_id",
        )?;
        let source_rows = source_statement.query_map([], |row| {
            Ok(KnowledgeSource {
                source_id: row.get(0)?,
                source_kind: row.get(1)?,
                title: row.get(2)?,
                description: row.get(3)?,
                notion_url: row.get(4)?,
                data_source_id: row.get(5)?,
                config_json: row.get(6)?,
                fields: Vec::new(),
                actions: Vec::new(),
            })
        })?;
        let mut sources = source_rows.collect::<rusqlite::Result<Vec<_>>>()?;

        for source in &mut sources {
            let mut field_statement = connection.prepare(
                "SELECT logical_name, notion_property, property_type, semantics, options_json, \
                        queryable, writable \
                 FROM knowledge_fields WHERE source_id = ?1 ORDER BY sort_order, logical_name",
            )?;
            source.fields = field_statement
                .query_map(params![source.source_id], |row| {
                    Ok(KnowledgeField {
                        logical_name: row.get(0)?,
                        notion_property: row.get(1)?,
                        property_type: row.get(2)?,
                        semantics: row.get(3)?,
                        options_json: row.get(4)?,
                        queryable: row.get(5)?,
                        writable: row.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut action_statement = connection.prepare(
                "SELECT action_id, label, button_style, title, prompt_template \
                 FROM knowledge_actions WHERE source_id = ?1 AND enabled = 1 \
                 ORDER BY sort_order, action_id",
            )?;
            let mut actions = action_statement
                .query_map(params![source.source_id], |row| {
                    Ok(KnowledgeAction {
                        action_id: row.get(0)?,
                        label: row.get(1)?,
                        button_style: row.get(2)?,
                        title: row.get(3)?,
                        prompt_template: row.get(4)?,
                        inputs: Vec::new(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for action in &mut actions {
                let mut input_statement = connection.prepare(
                    "SELECT input_id, label, placeholder, input_style, required, max_length \
                     FROM knowledge_action_inputs WHERE source_id = ?1 AND action_id = ?2 \
                     ORDER BY sort_order, input_id",
                )?;
                action.inputs = input_statement
                    .query_map(params![source.source_id, action.action_id], |row| {
                        Ok(KnowledgeActionInput {
                            input_id: row.get(0)?,
                            label: row.get(1)?,
                            placeholder: row.get(2)?,
                            input_style: row.get(3)?,
                            required: row.get(4)?,
                            max_length: row.get(5)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
            }
            source.actions = actions;
        }
        let mut view_statement = connection.prepare(
            "SELECT view_id, title, description, colour, footer, field_label, field_value, \
                    select_placeholder, config_json FROM knowledge_ui_views ORDER BY view_id",
        )?;
        let views = view_statement
            .query_map([], |row| {
                Ok(KnowledgeView {
                    view_id: row.get(0)?,
                    title: row.get(1)?,
                    description: row.get(2)?,
                    colour: row.get(3)?,
                    footer: row.get(4)?,
                    field_label: row.get(5)?,
                    field_value: row.get(6)?,
                    select_placeholder: row.get(7)?,
                    config_json: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut global_statement = connection.prepare(
            "SELECT action_id, surface_id, label, button_style, title, prompt_template, \
                    behavior, visible, row_number, config_json \
             FROM knowledge_global_actions ORDER BY surface_id, row_number, sort_order, action_id",
        )?;
        let mut global_actions = global_statement
            .query_map([], |row| {
                Ok(KnowledgeGlobalAction {
                    action_id: row.get(0)?,
                    surface_id: row.get(1)?,
                    label: row.get(2)?,
                    button_style: row.get(3)?,
                    title: row.get(4)?,
                    prompt_template: row.get(5)?,
                    behavior: row.get(6)?,
                    visible: row.get(7)?,
                    row_number: row.get(8)?,
                    config_json: row.get(9)?,
                    inputs: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for action in &mut global_actions {
            let mut input_statement = connection.prepare(
                "SELECT input_id, label, placeholder, input_style, required, max_length \
                 FROM knowledge_global_action_inputs WHERE action_id = ?1 \
                 ORDER BY sort_order, input_id",
            )?;
            action.inputs = input_statement
                .query_map(params![&action.action_id], |row| {
                    Ok(KnowledgeActionInput {
                        input_id: row.get(0)?,
                        label: row.get(1)?,
                        placeholder: row.get(2)?,
                        input_style: row.get(3)?,
                        required: row.get(4)?,
                        max_length: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }

        let mut policy_statement = connection.prepare(
            "SELECT policy_id, retention_days, grace_days, max_items, queue_name, config_json \
             FROM knowledge_policies ORDER BY policy_id",
        )?;
        let policies = policy_statement
            .query_map([], |row| {
                Ok(KnowledgePolicy {
                    policy_id: row.get(0)?,
                    retention_days: row.get(1)?,
                    grace_days: row.get(2)?,
                    max_items: row.get(3)?,
                    queue_name: row.get(4)?,
                    config_json: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(Self {
            sources,
            views,
            global_actions,
            policies,
        })
    }

    pub fn source(&self, source_id: &str) -> Option<&KnowledgeSource> {
        self.sources
            .iter()
            .find(|source| source.source_id == source_id)
    }

    pub fn sources_by_kind(&self, source_kind: &str) -> Vec<&KnowledgeSource> {
        self.sources
            .iter()
            .filter(|source| source.source_kind == source_kind)
            .collect()
    }

    pub fn view(&self, view_id: &str) -> Option<&KnowledgeView> {
        self.views.iter().find(|view| view.view_id == view_id)
    }

    pub fn global_action(&self, action_id: &str) -> Option<&KnowledgeGlobalAction> {
        self.global_actions
            .iter()
            .find(|action| action.action_id == action_id)
    }

    pub fn global_actions_for(&self, surface_id: &str) -> Vec<&KnowledgeGlobalAction> {
        self.global_actions
            .iter()
            .filter(|action| action.surface_id == surface_id && action.visible)
            .collect()
    }

    pub fn policy(&self, policy_id: &str) -> Option<&KnowledgePolicy> {
        self.policies
            .iter()
            .find(|policy| policy.policy_id == policy_id)
    }

    pub fn global_prompt(&self, action_id: &str) -> Option<String> {
        let action = self.global_action(action_id)?;
        let mut prompt = action.prompt_template.clone();
        if let Some(policy_id) = action.config_string("policy_id") {
            prompt.push_str("\n\n");
            prompt.push_str(&self.policy(&policy_id)?.adapter_context());
        }
        if action.config_string("include_source_kind").as_deref() == Some("scheduled") {
            for source in self.sources_by_kind("scheduled") {
                prompt.push_str("\n\n");
                prompt.push_str(&source.adapter_context());
            }
        }
        Some(prompt)
    }
}

fn catalog_path() -> Option<PathBuf> {
    std::env::var_os("OPENAB_KNOWLEDGE_DB")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

static CATALOG: LazyLock<KnowledgeCatalog> = LazyLock::new(|| {
    KnowledgeCatalog::open_or_seed(catalog_path().as_deref()).unwrap_or_else(|error| {
        warn!(%error, "persistent knowledge catalog unavailable; using seeded in-memory catalog");
        KnowledgeCatalog::open_or_seed(None).expect("embedded knowledge catalog migration is valid")
    })
});

pub fn knowledge_catalog() -> &'static KnowledgeCatalog {
    &CATALOG
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_seeds_sources_fields_actions_and_inputs() {
        let catalog = KnowledgeCatalog::open_or_seed(None).unwrap();
        assert_eq!(catalog.sources_by_kind("scheduled").len(), 3);
        assert_eq!(catalog.sources_by_kind("side_project").len(), 2);
        let reading = catalog.source("personal_reading_list").unwrap();
        assert_eq!(
            reading.data_source_id,
            "collection://example-reading-list"
        );
        assert!(reading
            .fields
            .iter()
            .any(|field| { field.logical_name == "author" && field.notion_property == "Auther" }));
        let search = reading.action("search").unwrap();
        assert_eq!(search.inputs.len(), 5);
        assert_eq!(catalog.views.len(), 6);
        assert_eq!(catalog.global_actions_for("home").len(), 6);
        assert_eq!(catalog.global_action("capture").unwrap().inputs.len(), 3);
        assert_eq!(
            catalog
                .policy("scheduled_retention")
                .unwrap()
                .retention_days,
            45
        );
        assert!(catalog
            .global_prompt("retention_scan")
            .unwrap()
            .contains("grace_days: 7"));
        let recent = catalog.global_prompt("recent").unwrap();
        assert!(recent.contains("discord-cards.md"));
        assert!(recent.contains("search card contract"));
        assert!(recent.contains("不要輸出 Markdown table"));
        let search = catalog.global_prompt("search").unwrap();
        assert!(search.contains("1 至 5 筆搜尋結果"));
        assert!(search.contains("必須嚴格以 search card contract"));
        assert!(search.contains("不要編號清單文字"));
        assert!(search.contains("跨頁推論或長篇綜合分析"));
        let world = catalog
            .sources
            .iter()
            .find(|source| source.source_id == "world_stories")
            .unwrap();
        let synthesis = world.action("synthesis").unwrap();
        assert!(synthesis.prompt_template.contains("synthesis card contract"));
        assert!(synthesis.prompt_template.contains("不要 Markdown table"));
        let reading = catalog.source("personal_reading_list").unwrap();
        let overview = reading.action("overview").unwrap();
        assert!(overview.prompt_template.contains("synthesis card contract"));
        assert!(overview.prompt_template.contains("To Read"));
        let capture = catalog.global_prompt("capture").unwrap();
        assert!(capture.contains("capture_preview card contract"));
        assert!(capture.contains("不要直接寫入 Notion"));
        let confirm = catalog.global_action("capture_confirm").unwrap();
        assert_eq!(confirm.behavior, "prompt");
        assert!(confirm.prompt_template.contains("明確寫入授權"));
        assert_eq!(
            catalog.global_action("capture_cancel").unwrap().behavior,
            "local"
        );
    }

    #[test]
    fn migration_is_idempotent_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.db");
        let first = KnowledgeCatalog::open_or_seed(Some(&path)).unwrap();
        let second = KnowledgeCatalog::open_or_seed(Some(&path)).unwrap();
        assert_eq!(first.sources.len(), second.sources.len());
    }

    #[test]
    fn migration_upgrades_existing_v1_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.db");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(MIGRATION_0001).unwrap();
        drop(connection);

        let catalog = KnowledgeCatalog::open_or_seed(Some(&path)).unwrap();
        assert_eq!(catalog.views.len(), 6);
        assert_eq!(
            catalog.global_action("retention_scan").unwrap().behavior,
            "prompt"
        );
        let connection = Connection::open(path).unwrap();
        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(version, 7);
        let world = catalog
            .sources
            .iter()
            .find(|source| source.source_id == "world_stories")
            .unwrap();
        assert!(world
            .action("synthesis")
            .unwrap()
            .prompt_template
            .contains("synthesis card contract"));
        let reading = catalog.source("personal_reading_list").unwrap();
        assert!(reading
            .action("overview")
            .unwrap()
            .prompt_template
            .contains("synthesis card contract"));
        assert!(reading
            .action("overview")
            .unwrap()
            .prompt_template
            .contains("不要 Markdown table"));
        assert!(catalog
            .global_prompt("capture")
            .unwrap()
            .contains("capture_preview card contract"));
        assert!(catalog.global_action("capture_confirm").is_some());
        assert!(catalog.global_action("capture_cancel").is_some());
    }
}
