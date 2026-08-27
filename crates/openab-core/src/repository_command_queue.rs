//! Durable, typed FIFO queue for allowlisted repository commands.
//!
//! Jobs persist only a configured command ID (plus an optional validated book
//! slug). The Discord worker resolves the current allowlist and workspace again
//! immediately before execution, so this file can never become a shell script.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::control_db::ControlDb;

const MAX_FINISHED_JOBS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryCommandState {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl RepositoryCommandState {
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryCommandResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryCommandJob {
    pub id: u64,
    pub workspace_alias: String,
    pub project_channel_id: u64,
    pub requested_by: u64,
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book_slug: Option<String>,
    pub state: RepositoryCommandState,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub recovered_from_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RepositoryCommandResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueDisk {
    #[serde(default = "default_next_id")]
    next_id: u64,
    #[serde(default)]
    jobs: Vec<RepositoryCommandJob>,
}

fn default_next_id() -> u64 {
    1
}

#[derive(Debug)]
struct QueueInner {
    next_id: u64,
    jobs: Vec<RepositoryCommandJob>,
    active_workers: HashSet<String>,
}

#[derive(Clone)]
pub struct RepositoryCommandQueue {
    inner: Arc<Mutex<QueueInner>>,
    cancellations: Arc<tokio::sync::Mutex<HashMap<u64, oneshot::Sender<()>>>>,
    backend: RepositoryCommandQueueBackend,
}

#[derive(Clone)]
enum RepositoryCommandQueueBackend {
    Json(PathBuf),
    Sqlite(ControlDb),
}

impl RepositoryCommandQueue {
    pub fn load(path: PathBuf) -> Self {
        let disk = match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str::<QueueDisk>(&data).unwrap_or_else(|error| {
                warn!(%error, path = %path.display(), "failed to parse repository command queue, starting empty");
                QueueDisk {
                    next_id: 1,
                    jobs: Vec::new(),
                }
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => QueueDisk {
                next_id: 1,
                jobs: Vec::new(),
            },
            Err(error) => {
                warn!(%error, path = %path.display(), "failed to read repository command queue, starting empty");
                QueueDisk {
                    next_id: 1,
                    jobs: Vec::new(),
                }
            }
        };
        let mut inner = QueueInner {
            next_id: disk.next_id.max(1),
            jobs: disk.jobs,
            active_workers: HashSet::new(),
        };
        let mut recovered = 0;
        for job in &mut inner.jobs {
            if job.state == RepositoryCommandState::Running {
                job.state = RepositoryCommandState::Pending;
                job.started_at = None;
                job.finished_at = None;
                job.recovered_from_active = true;
                job.result = None;
                recovered += 1;
            }
        }
        // Keep IDs monotonic even if an older hand-edited file has a stale counter.
        inner.next_id = inner
            .jobs
            .iter()
            .map(|job| job.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(inner.next_id);
        let queue = Self {
            inner: Arc::new(Mutex::new(inner)),
            cancellations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            backend: RepositoryCommandQueueBackend::Json(path.clone()),
        };
        if recovered > 0 {
            let inner = queue.lock();
            if let Err(error) = queue.persist_locked(&inner) {
                warn!(%error, "failed to persist recovered repository command queue");
            }
        }
        info!(recovered, path = %path.display(), "loaded repository command queue");
        queue
    }

    pub fn load_from_control_db(db: ControlDb, legacy_path: PathBuf) -> anyhow::Result<Self> {
        let (next_id, jobs) = db.load_or_import_repository_jobs(&legacy_path)?;
        let mut inner = QueueInner {
            next_id: next_id.max(1),
            jobs,
            active_workers: HashSet::new(),
        };
        let recovered = recover_active_jobs(&mut inner);
        normalize_next_id(&mut inner);
        let queue = Self {
            inner: Arc::new(Mutex::new(inner)),
            cancellations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            backend: RepositoryCommandQueueBackend::Sqlite(db.clone()),
        };
        if recovered > 0 {
            let inner = queue.lock();
            queue.persist_locked(&inner)?;
        }
        info!(recovered, path = %db.path().display(), "loaded repository command queue from control database");
        Ok(queue)
    }

    pub fn enqueue(
        &self,
        workspace_alias: &str,
        project_channel_id: u64,
        requested_by: u64,
        command_id: &str,
        book_slug: Option<String>,
    ) -> anyhow::Result<(RepositoryCommandJob, usize)> {
        let mut inner = self.lock();
        let previous_next_id = inner.next_id;
        let job = RepositoryCommandJob {
            id: inner.next_id,
            workspace_alias: workspace_alias.to_string(),
            project_channel_id,
            requested_by,
            command_id: command_id.to_string(),
            book_slug,
            state: RepositoryCommandState::Pending,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            recovered_from_active: false,
            result: None,
        };
        inner.next_id = inner.next_id.saturating_add(1);
        inner.jobs.push(job.clone());
        if let Err(error) = self.persist_locked(&inner) {
            inner.jobs.pop();
            inner.next_id = previous_next_id;
            return Err(error);
        }
        let position = inner
            .jobs
            .iter()
            .filter(|candidate| {
                candidate.workspace_alias == workspace_alias
                    && matches!(
                        candidate.state,
                        RepositoryCommandState::Pending | RepositoryCommandState::Running
                    )
                    && candidate.id <= job.id
            })
            .count();
        Ok((job, position))
    }

    pub fn jobs_for(&self, workspace_alias: &str) -> Vec<RepositoryCommandJob> {
        self.lock()
            .jobs
            .iter()
            .filter(|job| job.workspace_alias == workspace_alias)
            .cloned()
            .collect()
    }

    pub fn job(&self, id: u64) -> Option<RepositoryCommandJob> {
        self.lock().jobs.iter().find(|job| job.id == id).cloned()
    }

    pub fn pending_aliases(&self) -> Vec<String> {
        let mut aliases = self
            .lock()
            .jobs
            .iter()
            .filter(|job| job.state == RepositoryCommandState::Pending)
            .map(|job| job.workspace_alias.clone())
            .collect::<Vec<_>>();
        aliases.sort();
        aliases.dedup();
        aliases
    }

    pub fn active_count_for_project(
        &self,
        workspace_alias: &str,
        project_channel_id: u64,
    ) -> usize {
        self.lock()
            .jobs
            .iter()
            .filter(|job| {
                job.workspace_alias == workspace_alias
                    && job.project_channel_id == project_channel_id
                    && matches!(
                        job.state,
                        RepositoryCommandState::Pending | RepositoryCommandState::Running
                    )
            })
            .count()
    }

    /// Reserve the one worker allowed to execute this repository's FIFO.
    pub fn start_worker(&self, workspace_alias: &str) -> bool {
        self.lock()
            .active_workers
            .insert(workspace_alias.to_string())
    }

    /// Atomically claim the next job or release this repository's worker slot.
    pub fn claim_next_or_stop(
        &self,
        workspace_alias: &str,
    ) -> anyhow::Result<Option<RepositoryCommandJob>> {
        let mut inner = self.lock();
        let Some(index) = inner.jobs.iter().position(|job| {
            job.workspace_alias == workspace_alias && job.state == RepositoryCommandState::Pending
        }) else {
            inner.active_workers.remove(workspace_alias);
            return Ok(None);
        };
        inner.jobs[index].state = RepositoryCommandState::Running;
        inner.jobs[index].started_at = Some(Utc::now());
        inner.jobs[index].recovered_from_active = false;
        if let Err(error) = self.persist_locked(&inner) {
            inner.jobs[index].state = RepositoryCommandState::Pending;
            inner.jobs[index].started_at = None;
            inner.active_workers.remove(workspace_alias);
            return Err(error);
        }
        Ok(Some(inner.jobs[index].clone()))
    }

    pub async fn register_cancellation(&self, id: u64, sender: oneshot::Sender<()>) {
        self.cancellations.lock().await.insert(id, sender);
    }

    pub async fn clear_cancellation(&self, id: u64) {
        self.cancellations.lock().await.remove(&id);
    }

    /// Cancel a pending job immediately, or signal the active worker.
    pub async fn cancel(&self, id: u64) -> anyhow::Result<RepositoryCommandJob> {
        {
            let mut inner = self.lock();
            let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
                anyhow::bail!("repository command job not found");
            };
            match inner.jobs[index].state {
                RepositoryCommandState::Pending => {
                    let previous = inner.jobs[index].clone();
                    inner.jobs[index].state = RepositoryCommandState::Cancelled;
                    inner.jobs[index].finished_at = Some(Utc::now());
                    inner.jobs[index].result = Some(cancelled_result());
                    let cancelled = inner.jobs[index].clone();
                    self.prune_finished_locked(&mut inner);
                    if let Err(error) = self.persist_locked(&inner) {
                        if let Some(current) = inner.jobs.iter_mut().find(|job| job.id == id) {
                            *current = previous;
                        } else {
                            let restore_at = index.min(inner.jobs.len());
                            inner.jobs.insert(restore_at, previous);
                        }
                        return Err(error);
                    }
                    return Ok(cancelled);
                }
                RepositoryCommandState::Running => {}
                _ => anyhow::bail!("repository command job has already finished"),
            }
        }
        let Some(sender) = self.cancellations.lock().await.remove(&id) else {
            anyhow::bail!("repository command is starting; try again in a moment");
        };
        let _ = sender.send(());
        self.job(id)
            .ok_or_else(|| anyhow::anyhow!("repository command job not found"))
    }

    pub fn move_next(&self, id: u64) -> anyhow::Result<RepositoryCommandJob> {
        let mut inner = self.lock();
        let previous_jobs = inner.jobs.clone();
        let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
            anyhow::bail!("repository command job not found");
        };
        if inner.jobs[index].state != RepositoryCommandState::Pending {
            anyhow::bail!("only waiting commands can move next");
        }
        let alias = inner.jobs[index].workspace_alias.clone();
        let target = inner.jobs.iter().position(|job| {
            job.workspace_alias == alias && job.state == RepositoryCommandState::Pending
        });
        if let Some(target) = target {
            if target != index {
                let job = inner.jobs.remove(index);
                let target = if index < target { target - 1 } else { target };
                inner.jobs.insert(target, job);
            }
        }
        if let Err(error) = self.persist_locked(&inner) {
            inner.jobs = previous_jobs;
            return Err(error);
        }
        Ok(inner.jobs.iter().find(|job| job.id == id).cloned().unwrap())
    }

    pub fn remove_pending(&self, id: u64) -> anyhow::Result<RepositoryCommandJob> {
        let mut inner = self.lock();
        let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
            anyhow::bail!("repository command job not found");
        };
        if inner.jobs[index].state != RepositoryCommandState::Pending {
            anyhow::bail!("only waiting commands can be removed");
        }
        let removed = inner.jobs.remove(index);
        if let Err(error) = self.persist_locked(&inner) {
            inner.jobs.insert(index, removed);
            return Err(error);
        }
        Ok(removed)
    }

    pub fn finish(
        &self,
        id: u64,
        state: RepositoryCommandState,
        result: RepositoryCommandResult,
    ) -> anyhow::Result<RepositoryCommandJob> {
        if !state.is_finished() {
            anyhow::bail!("repository command result must be terminal");
        }
        let mut inner = self.lock();
        let Some(index) = inner.jobs.iter().position(|job| job.id == id) else {
            anyhow::bail!("repository command job not found");
        };
        inner.jobs[index].state = state;
        inner.jobs[index].finished_at = Some(Utc::now());
        inner.jobs[index].result = Some(result);
        let finished = inner.jobs[index].clone();
        self.prune_finished_locked(&mut inner);
        self.persist_locked(&inner)?;
        Ok(finished)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn prune_finished_locked(&self, inner: &mut QueueInner) {
        let finished = inner
            .jobs
            .iter()
            .filter(|job| job.state.is_finished())
            .count();
        if finished <= MAX_FINISHED_JOBS {
            return;
        }
        let remove_count = finished - MAX_FINISHED_JOBS;
        let ids = inner
            .jobs
            .iter()
            .filter(|job| job.state.is_finished())
            .take(remove_count)
            .map(|job| job.id)
            .collect::<HashSet<_>>();
        inner.jobs.retain(|job| !ids.contains(&job.id));
    }

    fn persist_locked(&self, inner: &QueueInner) -> anyhow::Result<()> {
        match &self.backend {
            RepositoryCommandQueueBackend::Json(path) => {
                let data = serde_json::to_string_pretty(&QueueDisk {
                    next_id: inner.next_id,
                    jobs: inner.jobs.clone(),
                })?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let temporary = path.with_extension("json.tmp");
                std::fs::write(&temporary, data)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
                }
                std::fs::rename(temporary, path)?;
            }
            RepositoryCommandQueueBackend::Sqlite(db) => {
                db.replace_repository_jobs(inner.next_id, &inner.jobs)?;
            }
        }
        Ok(())
    }
}

fn recover_active_jobs(inner: &mut QueueInner) -> usize {
    let mut recovered = 0;
    for job in &mut inner.jobs {
        if job.state == RepositoryCommandState::Running {
            job.state = RepositoryCommandState::Pending;
            job.started_at = None;
            job.finished_at = None;
            job.recovered_from_active = true;
            job.result = None;
            recovered += 1;
        }
    }
    recovered
}

fn normalize_next_id(inner: &mut QueueInner) {
    inner.next_id = inner
        .jobs
        .iter()
        .map(|job| job.id)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(inner.next_id);
}

pub fn cancelled_result() -> RepositoryCommandResult {
    RepositoryCommandResult {
        exit_code: None,
        timed_out: false,
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
        elapsed_ms: 0,
        error: Some("Cancelled by a user".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> (tempfile::TempDir, RepositoryCommandQueue) {
        let dir = tempfile::tempdir().unwrap();
        let queue = RepositoryCommandQueue::load(dir.path().join("commands.json"));
        (dir, queue)
    }

    #[test]
    fn fifo_is_scoped_per_repository() {
        let (_dir, queue) = queue();
        let (one, _) = queue.enqueue("one", 1, 10, "status", None).unwrap();
        let (other, _) = queue.enqueue("other", 2, 10, "pull", None).unwrap();
        let (two, position) = queue.enqueue("one", 1, 10, "pull", None).unwrap();
        assert_eq!(position, 2);
        assert!(queue.start_worker("one"));
        assert!(!queue.start_worker("one"));
        assert_eq!(queue.claim_next_or_stop("one").unwrap().unwrap().id, one.id);
        queue
            .finish(
                one.id,
                RepositoryCommandState::Completed,
                cancelled_result(),
            )
            .unwrap();
        assert_eq!(queue.claim_next_or_stop("one").unwrap().unwrap().id, two.id);
        assert!(queue.start_worker("other"));
        assert_eq!(
            queue.claim_next_or_stop("other").unwrap().unwrap().id,
            other.id
        );
    }

    #[test]
    fn move_next_and_remove_only_touch_pending_jobs() {
        let (_dir, queue) = queue();
        let (one, _) = queue.enqueue("repo", 1, 10, "one", None).unwrap();
        let (two, _) = queue.enqueue("repo", 1, 10, "two", None).unwrap();
        queue.move_next(two.id).unwrap();
        assert_eq!(queue.jobs_for("repo")[0].id, two.id);
        queue.remove_pending(one.id).unwrap();
        assert_eq!(queue.jobs_for("repo").len(), 1);
    }

    #[test]
    fn running_jobs_return_to_pending_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("commands.json");
        let queue = RepositoryCommandQueue::load(path.clone());
        let (job, _) = queue.enqueue("repo", 1, 10, "pull", None).unwrap();
        assert!(queue.start_worker("repo"));
        queue.claim_next_or_stop("repo").unwrap();
        drop(queue);

        let restored = RepositoryCommandQueue::load(path);
        let job = restored.job(job.id).unwrap();
        assert_eq!(job.state, RepositoryCommandState::Pending);
        assert!(job.recovered_from_active);
        assert_eq!(restored.pending_aliases(), vec!["repo"]);
    }

    #[tokio::test]
    async fn pending_and_running_jobs_can_be_cancelled() {
        let (_dir, queue) = queue();
        let (pending, _) = queue.enqueue("repo", 1, 10, "one", None).unwrap();
        let cancelled = queue.cancel(pending.id).await.unwrap();
        assert_eq!(cancelled.state, RepositoryCommandState::Cancelled);

        let (running, _) = queue.enqueue("repo", 1, 10, "two", None).unwrap();
        assert!(queue.start_worker("repo"));
        queue.claim_next_or_stop("repo").unwrap();
        let (sender, receiver) = oneshot::channel();
        queue.register_cancellation(running.id, sender).await;
        queue.cancel(running.id).await.unwrap();
        receiver.await.unwrap();
    }
}
