//! Persistent Discord task metadata for mobile-friendly lifecycle UI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Ready,
    Cursor,
    Failed,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub guild_id: u64,
    pub project_channel_id: u64,
    #[serde(default)]
    pub workspace_alias: String,
    pub thread_id: u64,
    pub title: String,
    pub created_by: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message_id: Option<u64>,
    pub state: TaskState,
    #[serde(default)]
    pub queued_messages: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Most recent text request submitted for this task. Used by the Discord
    /// recovery controls to retry or edit a failed turn. Older registries load
    /// without migration because the field defaults to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct TaskRegistry {
    tasks: Arc<RwLock<HashMap<u64, TaskRecord>>>,
    path: PathBuf,
}

impl TaskRegistry {
    pub fn load(path: PathBuf) -> Self {
        let entries: Vec<TaskRecord> = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|error| {
                warn!(%error, path = %path.display(), "failed to parse task registry, starting empty");
                Vec::new()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                warn!(%error, path = %path.display(), "failed to read task registry, starting empty");
                Vec::new()
            }
        };
        let tasks = entries
            .into_iter()
            .map(|task| (task.thread_id, task))
            .collect::<HashMap<_, _>>();
        info!(count = tasks.len(), path = %path.display(), "loaded task registry");
        Self {
            tasks: Arc::new(RwLock::new(tasks)),
            path,
        }
    }

    pub fn task_for_thread(&self, thread_id: u64) -> Option<TaskRecord> {
        self.tasks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&thread_id)
            .cloned()
    }

    pub fn ensure(&self, task: TaskRecord) -> anyhow::Result<(TaskRecord, bool)> {
        let mut tasks = self
            .tasks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(current) = tasks.get(&task.thread_id) {
            return Ok((current.clone(), false));
        }
        let thread_id = task.thread_id;
        tasks.insert(thread_id, task.clone());
        if let Err(error) = self.persist_locked(&tasks) {
            tasks.remove(&thread_id);
            return Err(error);
        }
        Ok((task, true))
    }

    pub fn set_status_message(
        &self,
        thread_id: u64,
        message_id: u64,
    ) -> anyhow::Result<TaskRecord> {
        self.update(thread_id, |task| task.status_message_id = Some(message_id))
    }

    pub fn enqueue(&self, thread_id: u64) -> anyhow::Result<TaskRecord> {
        self.update(thread_id, |task| {
            task.queued_messages = task.queued_messages.saturating_add(1);
            if task.state != TaskState::Running {
                task.state = TaskState::Queued;
            }
            task.last_error = None;
        })
    }

    pub fn start_turn(&self, thread_id: u64, batch_size: usize) -> anyhow::Result<TaskRecord> {
        self.update(thread_id, |task| {
            task.queued_messages = task.queued_messages.saturating_sub(batch_size);
            task.state = TaskState::Running;
            task.last_error = None;
        })
    }

    pub fn finish_turn(&self, thread_id: u64, error: Option<String>) -> anyhow::Result<TaskRecord> {
        self.update(thread_id, |task| {
            if let Some(error) = error {
                task.state = TaskState::Failed;
                task.queued_messages = 0;
                task.last_error = Some(error);
            } else if task.queued_messages > 0 {
                task.state = TaskState::Queued;
                task.last_error = None;
            } else {
                task.state = TaskState::Ready;
                task.last_error = None;
            }
        })
    }

    pub fn set_state(&self, thread_id: u64, state: TaskState) -> anyhow::Result<TaskRecord> {
        self.update(thread_id, |task| {
            task.state = state;
            if matches!(state, TaskState::Closed | TaskState::Cursor) {
                task.queued_messages = 0;
            }
            if state != TaskState::Failed {
                task.last_error = None;
            }
        })
    }

    pub fn record_prompt(&self, thread_id: u64, prompt: &str) -> anyhow::Result<TaskRecord> {
        self.update(thread_id, |task| {
            let prompt = prompt.trim();
            task.last_prompt = if prompt.is_empty() {
                None
            } else {
                Some(prompt.chars().take(4000).collect())
            };
        })
    }

    pub fn recent_for_project(&self, project_channel_id: u64, limit: usize) -> Vec<TaskRecord> {
        let mut tasks = self
            .tasks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|task| task.project_channel_id == project_channel_id)
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by_key(|task| std::cmp::Reverse(task.updated_at));
        tasks.truncate(limit);
        tasks
    }

    /// Remove one task from the Discord UI registry.
    ///
    /// Session state must be closed separately before calling this method. Keeping
    /// the two operations explicit prevents a UI cleanup from silently destroying
    /// a resumable Cursor session.
    pub fn remove_task(&self, thread_id: u64) -> anyhow::Result<Option<TaskRecord>> {
        let mut tasks = self
            .tasks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(task) = tasks.remove(&thread_id) else {
            return Ok(None);
        };
        if let Err(error) = self.persist_locked(&tasks) {
            tasks.insert(thread_id, task);
            return Err(error);
        }
        Ok(Some(task))
    }

    pub fn remove_project(&self, project_channel_id: u64) -> anyhow::Result<usize> {
        let mut tasks = self
            .tasks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = tasks.len();
        tasks.retain(|_, task| task.project_channel_id != project_channel_id);
        let removed = before - tasks.len();
        if removed > 0 {
            self.persist_locked(&tasks)?;
        }
        Ok(removed)
    }

    fn update(
        &self,
        thread_id: u64,
        mutate: impl FnOnce(&mut TaskRecord),
    ) -> anyhow::Result<TaskRecord> {
        let mut tasks = self
            .tasks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let task = tasks
            .get_mut(&thread_id)
            .ok_or_else(|| anyhow::anyhow!("Discord task is not registered"))?;
        let original = task.clone();
        mutate(task);
        task.updated_at = Utc::now();
        let updated = task.clone();
        if let Err(error) = self.persist_locked(&tasks) {
            tasks.insert(thread_id, original);
            return Err(error);
        }
        Ok(updated)
    }

    fn persist_locked(&self, tasks: &HashMap<u64, TaskRecord>) -> anyhow::Result<()> {
        let mut entries = tasks.values().cloned().collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            a.project_channel_id
                .cmp(&b.project_channel_id)
                .then_with(|| a.created_at.cmp(&b.created_at))
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

    fn task(thread_id: u64) -> TaskRecord {
        let now = Utc::now();
        TaskRecord {
            guild_id: 1,
            project_channel_id: 10,
            workspace_alias: "openab".into(),
            thread_id,
            title: format!("Task {thread_id}"),
            created_by: 99,
            status_message_id: None,
            state: TaskState::Ready,
            queued_messages: 0,
            last_error: None,
            last_prompt: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn lifecycle_and_queue_are_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let registry = TaskRegistry::load(path.clone());
        registry.ensure(task(20)).unwrap();
        registry.enqueue(20).unwrap();
        registry.enqueue(20).unwrap();
        let running = registry.start_turn(20, 1).unwrap();
        assert_eq!(running.state, TaskState::Running);
        assert_eq!(running.queued_messages, 1);
        let queued = registry.finish_turn(20, None).unwrap();
        assert_eq!(queued.state, TaskState::Queued);

        let failed = registry
            .finish_turn(20, Some("agent exited".into()))
            .unwrap();
        assert_eq!(failed.state, TaskState::Failed);
        assert_eq!(failed.queued_messages, 0);

        let restored = TaskRegistry::load(path);
        assert_eq!(restored.task_for_thread(20).unwrap(), failed);
    }

    #[test]
    fn recent_tasks_are_scoped_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::load(dir.path().join("tasks.json"));
        registry.ensure(task(20)).unwrap();
        registry.ensure(task(21)).unwrap();
        registry.enqueue(20).unwrap();
        let mut other = task(30);
        other.project_channel_id = 11;
        registry.ensure(other).unwrap();

        let recent = registry.recent_for_project(10, 10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].thread_id, 20);
    }

    #[test]
    fn retry_prompt_is_trimmed_truncated_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let registry = TaskRegistry::load(path.clone());
        registry.ensure(task(20)).unwrap();

        let updated = registry
            .record_prompt(20, &format!("  {}  ", "x".repeat(4100)))
            .unwrap();
        assert_eq!(updated.last_prompt.as_ref().unwrap().chars().count(), 4000);

        let restored = TaskRegistry::load(path);
        assert_eq!(restored.task_for_thread(20).unwrap(), updated);
    }

    #[test]
    fn remove_task_only_removes_the_selected_record_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let registry = TaskRegistry::load(path.clone());
        registry.ensure(task(20)).unwrap();
        registry.ensure(task(21)).unwrap();

        let removed = registry.remove_task(20).unwrap().unwrap();
        assert_eq!(removed.thread_id, 20);
        assert!(registry.task_for_thread(20).is_none());
        assert!(registry.task_for_thread(21).is_some());

        let restored = TaskRegistry::load(path);
        assert!(restored.task_for_thread(20).is_none());
        assert!(restored.task_for_thread(21).is_some());
        assert!(restored.remove_task(999).unwrap().is_none());
    }
}
