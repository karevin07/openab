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
        let transaction = connection
            .transaction()
            .context("start knowledge catalog migration")?;
        let applied = transaction
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
                [],
                |_| Ok(()),
            )
            .is_ok()
            && transaction
                .query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = 1",
                    [],
                    |_| Ok(()),
                )
                .is_ok();
        if !applied {
            transaction
                .execute_batch(MIGRATION_0001)
                .context("apply knowledge catalog migration 0001")?;
        }
        transaction
            .commit()
            .context("commit knowledge catalog migration")?;
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
        Ok(Self { sources })
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
        assert_eq!(reading.data_source_id, "collection://example-reading-list");
        assert!(reading.fields.iter().any(|field| {
            field.logical_name == "author" && field.notion_property == "Auther"
        }));
        let search = reading.action("search").unwrap();
        assert_eq!(search.inputs.len(), 5);
    }

    #[test]
    fn migration_is_idempotent_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("knowledge.db");
        let first = KnowledgeCatalog::open_or_seed(Some(&path)).unwrap();
        let second = KnowledgeCatalog::open_or_seed(Some(&path)).unwrap();
        assert_eq!(first.sources.len(), second.sources.len());
    }
}
