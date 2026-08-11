//! Persistent Discord project-channel bindings.
//!
//! The registry stores workspace aliases rather than host paths. Aliases are
//! resolved through the workspace router, keeping absolute paths out of Discord
//! metadata and preventing the command surface from accepting arbitrary paths.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBinding {
    pub guild_id: u64,
    pub channel_id: u64,
    pub workspace_alias: String,
    pub created_by: u64,
    /// Legacy Phase 4A field. Migrated into `access_role_ids` during load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_role_id: Option<u64>,
    #[serde(default)]
    pub access_user_ids: Vec<u64>,
    #[serde(default)]
    pub access_role_ids: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_message_id: Option<u64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectAccessTarget {
    User(u64),
    Role(u64),
}

#[derive(Clone)]
pub struct ProjectRegistry {
    bindings: Arc<RwLock<HashMap<u64, ProjectBinding>>>,
    path: PathBuf,
}

impl ProjectRegistry {
    pub fn load(path: PathBuf) -> Self {
        let entries: Vec<ProjectBinding> = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|error| {
                warn!(%error, path = %path.display(), "failed to parse project registry, starting empty");
                Vec::new()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                warn!(%error, path = %path.display(), "failed to read project registry, starting empty");
                Vec::new()
            }
        };

        let mut bindings = HashMap::with_capacity(entries.len());
        for mut binding in entries {
            if binding.workspace_alias.is_empty() {
                warn!(
                    channel_id = binding.channel_id,
                    "ignoring project binding with empty workspace alias"
                );
                continue;
            }
            if let Some(role_id) = binding.access_role_id.take() {
                if !binding.access_role_ids.contains(&role_id) {
                    binding.access_role_ids.push(role_id);
                }
            }
            binding.access_user_ids.sort_unstable();
            binding.access_user_ids.dedup();
            binding.access_role_ids.sort_unstable();
            binding.access_role_ids.dedup();
            bindings.insert(binding.channel_id, binding);
        }
        info!(count = bindings.len(), path = %path.display(), "loaded project registry");
        Self {
            bindings: Arc::new(RwLock::new(bindings)),
            path,
        }
    }

    pub fn contains_channel(&self, channel_id: u64) -> bool {
        self.bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&channel_id)
    }

    pub fn channel_ids(&self) -> Vec<u64> {
        self.bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect()
    }

    pub fn binding_for_channel(&self, channel_id: u64) -> Option<ProjectBinding> {
        self.bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&channel_id)
            .cloned()
    }

    pub fn binding_for_alias(&self, guild_id: u64, alias: &str) -> Option<ProjectBinding> {
        self.bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .find(|binding| binding.guild_id == guild_id && binding.workspace_alias == alias)
            .cloned()
    }

    pub fn list_guild(&self, guild_id: u64) -> Vec<ProjectBinding> {
        let mut entries: Vec<_> = self
            .bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|binding| binding.guild_id == guild_id)
            .cloned()
            .collect();
        entries.sort_by(|a, b| a.workspace_alias.cmp(&b.workspace_alias));
        entries
    }

    pub fn all(&self) -> Vec<ProjectBinding> {
        self.bindings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub fn add(&self, binding: ProjectBinding) -> anyhow::Result<()> {
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if bindings.contains_key(&binding.channel_id) {
            anyhow::bail!("channel is already registered as a project");
        }
        if bindings.values().any(|current| {
            current.guild_id == binding.guild_id
                && current.workspace_alias == binding.workspace_alias
        }) {
            anyhow::bail!("workspace already has a project channel in this server");
        }

        let channel_id = binding.channel_id;
        bindings.insert(channel_id, binding);
        if let Err(error) = self.persist_locked(&bindings) {
            bindings.remove(&channel_id);
            return Err(error);
        }
        Ok(())
    }

    pub fn remove(&self, guild_id: u64, channel_id: u64) -> anyhow::Result<Option<ProjectBinding>> {
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(binding) = bindings.get(&channel_id).cloned() else {
            return Ok(None);
        };
        if binding.guild_id != guild_id {
            return Ok(None);
        }

        bindings.remove(&channel_id);
        if let Err(error) = self.persist_locked(&bindings) {
            bindings.insert(channel_id, binding);
            return Err(error);
        }
        Ok(Some(binding))
    }

    pub fn add_access(
        &self,
        guild_id: u64,
        channel_id: u64,
        target: ProjectAccessTarget,
    ) -> anyhow::Result<ProjectBinding> {
        self.update_access(guild_id, channel_id, target, true)
    }

    pub fn remove_access(
        &self,
        guild_id: u64,
        channel_id: u64,
        target: ProjectAccessTarget,
    ) -> anyhow::Result<ProjectBinding> {
        self.update_access(guild_id, channel_id, target, false)
    }

    pub fn set_home_message_id(
        &self,
        guild_id: u64,
        channel_id: u64,
        message_id: u64,
    ) -> anyhow::Result<ProjectBinding> {
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let binding = bindings
            .get_mut(&channel_id)
            .filter(|binding| binding.guild_id == guild_id)
            .ok_or_else(|| anyhow::anyhow!("project channel is not registered in this server"))?;
        let original = binding.clone();
        binding.home_message_id = Some(message_id);
        let updated = binding.clone();
        if let Err(error) = self.persist_locked(&bindings) {
            bindings.insert(channel_id, original);
            return Err(error);
        }
        Ok(updated)
    }

    fn update_access(
        &self,
        guild_id: u64,
        channel_id: u64,
        target: ProjectAccessTarget,
        add: bool,
    ) -> anyhow::Result<ProjectBinding> {
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let binding = bindings
            .get_mut(&channel_id)
            .filter(|binding| binding.guild_id == guild_id)
            .ok_or_else(|| anyhow::anyhow!("project channel is not registered in this server"))?;
        let original = binding.clone();
        let ids = match target {
            ProjectAccessTarget::User(_) => &mut binding.access_user_ids,
            ProjectAccessTarget::Role(_) => &mut binding.access_role_ids,
        };
        let id = match target {
            ProjectAccessTarget::User(id) | ProjectAccessTarget::Role(id) => id,
        };
        if add {
            if ids.contains(&id) {
                anyhow::bail!("access target is already registered");
            }
            ids.push(id);
            ids.sort_unstable();
        } else {
            let before = ids.len();
            ids.retain(|current| *current != id);
            if ids.len() == before {
                anyhow::bail!("access target is not registered");
            }
        }
        let updated = binding.clone();
        if let Err(error) = self.persist_locked(&bindings) {
            bindings.insert(channel_id, original);
            return Err(error);
        }
        Ok(updated)
    }

    fn persist_locked(&self, bindings: &HashMap<u64, ProjectBinding>) -> anyhow::Result<()> {
        let mut entries: Vec<_> = bindings.values().cloned().collect();
        entries.sort_by(|a, b| {
            a.guild_id
                .cmp(&b.guild_id)
                .then_with(|| a.workspace_alias.cmp(&b.workspace_alias))
        });
        let data = serde_json::to_string_pretty(&entries)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(guild_id: u64, channel_id: u64, alias: &str) -> ProjectBinding {
        ProjectBinding {
            guild_id,
            channel_id,
            workspace_alias: alias.to_string(),
            created_by: 99,
            access_role_id: None,
            access_user_ids: Vec::new(),
            access_role_ids: Vec::new(),
            home_message_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn registry_persists_and_reloads_bindings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("discord-projects.json");
        let registry = ProjectRegistry::load(path.clone());
        registry.add(binding(1, 10, "openab")).unwrap();

        let restored = ProjectRegistry::load(path);
        assert!(restored.contains_channel(10));
        assert_eq!(
            restored.binding_for_channel(10).unwrap().workspace_alias,
            "openab"
        );
    }

    #[test]
    fn registry_rejects_duplicate_workspace_in_same_guild() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ProjectRegistry::load(dir.path().join("projects.json"));
        registry.add(binding(1, 10, "openab")).unwrap();

        assert!(registry.add(binding(1, 11, "openab")).is_err());
        assert!(registry.add(binding(2, 11, "openab")).is_ok());
    }

    #[test]
    fn remove_is_scoped_to_guild() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ProjectRegistry::load(dir.path().join("projects.json"));
        registry.add(binding(1, 10, "openab")).unwrap();

        assert!(registry.remove(2, 10).unwrap().is_none());
        assert!(registry.remove(1, 10).unwrap().is_some());
        assert!(!registry.contains_channel(10));
    }

    #[test]
    fn home_message_id_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects.json");
        let registry = ProjectRegistry::load(path.clone());
        registry.add(binding(1, 10, "openab")).unwrap();

        registry.set_home_message_id(1, 10, 123).unwrap();

        let restored = ProjectRegistry::load(path);
        assert_eq!(
            restored.binding_for_channel(10).unwrap().home_message_id,
            Some(123)
        );
    }

    #[test]
    fn legacy_role_is_migrated_and_access_can_be_updated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("projects.json");
        let legacy = serde_json::json!([{
            "guild_id": 1,
            "channel_id": 10,
            "workspace_alias": "openab",
            "created_by": 99,
            "access_role_id": 42,
            "created_at": Utc::now(),
        }]);
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();

        let registry = ProjectRegistry::load(path);
        let migrated = registry.binding_for_channel(10).unwrap();
        assert_eq!(migrated.access_role_ids, vec![42]);
        assert!(migrated.home_message_id.is_none());
        assert!(migrated.access_role_id.is_none());

        registry
            .add_access(1, 10, ProjectAccessTarget::User(7))
            .unwrap();
        let updated = registry
            .remove_access(1, 10, ProjectAccessTarget::Role(42))
            .unwrap();
        assert_eq!(updated.access_user_ids, vec![7]);
        assert!(updated.access_role_ids.is_empty());
    }
}
