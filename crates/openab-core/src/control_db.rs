//! SQLite-backed persistence for Discord control-plane state.
//!
//! The database is intentionally separate from agent and knowledge data. Rust
//! continues to own permission checks and command execution; this store owns
//! mutable project, task, queue, and UI catalog records.

use crate::project_registry::ProjectBinding;
use crate::remind::Reminder;
use crate::repository_command_queue::RepositoryCommandJob;
use crate::task_registry::TaskRecord;
use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tracing::info;

const SCHEMA_VERSION: i64 = 2;

#[derive(Clone)]
pub struct ControlDb {
    path: PathBuf,
    connection: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for ControlDb {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl ControlDb {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open control database at {}", path.display()))?;
        set_private_permissions(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(SCHEMA_V1)?;
        connection.execute(
            "INSERT OR IGNORE INTO control_schema_migrations(version) VALUES (?1)",
            [SCHEMA_VERSION],
        )?;
        seed_ui_catalog(&connection)?;
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(suffix);
            let sidecar = PathBuf::from(sidecar);
            if sidecar.exists() {
                set_private_permissions(&sidecar)?;
            }
        }
        info!(path = %path.display(), schema_version = SCHEMA_VERSION, "opened control database");
        Ok(Self {
            path,
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_or_import_projects(
        &self,
        legacy_path: &Path,
    ) -> anyhow::Result<Vec<ProjectBinding>> {
        self.import_legacy_if_needed("discord-projects", legacy_path, |tx, entries| {
            replace_projects(tx, entries)
        })?;
        let connection = self.lock();
        read_payloads(
            &connection,
            "SELECT payload FROM project_bindings ORDER BY channel_id",
        )
    }

    pub fn replace_projects(&self, entries: &[ProjectBinding]) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_projects(&tx, entries)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_or_import_tasks(&self, legacy_path: &Path) -> anyhow::Result<Vec<TaskRecord>> {
        self.import_legacy_if_needed("discord-tasks", legacy_path, |tx, entries| {
            replace_tasks(tx, entries)
        })?;
        let connection = self.lock();
        read_payloads(&connection, "SELECT payload FROM tasks ORDER BY thread_id")
    }

    pub fn replace_tasks(&self, entries: &[TaskRecord]) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_tasks(&tx, entries)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_or_import_repository_jobs(
        &self,
        legacy_path: &Path,
    ) -> anyhow::Result<(u64, Vec<RepositoryCommandJob>)> {
        #[derive(serde::Deserialize)]
        struct LegacyQueue {
            #[serde(default = "default_next_id")]
            next_id: u64,
            #[serde(default)]
            jobs: Vec<RepositoryCommandJob>,
        }
        fn default_next_id() -> u64 {
            1
        }

        harden_legacy_backup(legacy_path)?;
        if !self.legacy_imported("repository-command-queue")? {
            let legacy = read_legacy::<LegacyQueue>(legacy_path)?;
            let mut connection = self.lock();
            let tx = connection.transaction()?;
            if let Some(legacy) = legacy {
                backup_legacy_file(legacy_path)?;
                replace_repository_jobs(&tx, legacy.next_id, &legacy.jobs)?;
            }
            record_legacy_import(&tx, "repository-command-queue", legacy_path)?;
            tx.commit()?;
        }

        let connection = self.lock();
        let next_id = connection
            .query_row(
                "SELECT next_id FROM repository_command_state WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )?
            .max(1) as u64;
        let jobs = read_payloads(
            &connection,
            "SELECT payload FROM repository_command_jobs ORDER BY queue_order, id",
        )?;
        Ok((next_id, jobs))
    }

    pub fn replace_repository_jobs(
        &self,
        next_id: u64,
        jobs: &[RepositoryCommandJob],
    ) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_repository_jobs(&tx, next_id, jobs)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_or_import_session_mappings(
        &self,
        legacy_path: &Path,
    ) -> anyhow::Result<HashMap<String, String>> {
        self.import_string_map_if_needed("thread-map", legacy_path, |tx, entries| {
            replace_session_column(tx, "session_id", entries)
        })?;
        self.read_string_map(
            "SELECT thread_key, session_id FROM session_records WHERE session_id IS NOT NULL",
        )
    }

    pub fn replace_session_mappings(
        &self,
        entries: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_session_column(&tx, "session_id", entries)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_or_import_session_workdirs(
        &self,
        legacy_path: &Path,
    ) -> anyhow::Result<HashMap<String, String>> {
        self.import_string_map_if_needed("session-meta", legacy_path, |tx, entries| {
            replace_session_column(tx, "workdir", entries)
        })?;
        self.read_string_map(
            "SELECT thread_key, workdir FROM session_records WHERE workdir IS NOT NULL",
        )
    }

    pub fn replace_session_workdirs(
        &self,
        entries: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_session_column(&tx, "workdir", entries)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_or_import_dispatch_queue(&self, legacy_path: &Path) -> anyhow::Result<Vec<u8>> {
        harden_legacy_backup(legacy_path)?;
        if !self.legacy_imported("discord-queue")? {
            let legacy = match std::fs::read(legacy_path) {
                Ok(data) => Some(data),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let mut connection = self.lock();
            let tx = connection.transaction()?;
            if let Some(data) = legacy {
                backup_legacy_file(legacy_path)?;
                replace_dispatch_snapshot(&tx, &data)?;
            }
            record_legacy_import(&tx, "discord-queue", legacy_path)?;
            tx.commit()?;
        }
        dispatch_snapshot(&self.lock())
    }

    pub fn replace_dispatch_queue(&self, snapshot: &[u8]) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_dispatch_snapshot(&tx, snapshot)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_or_import_cron_toggles(
        &self,
        legacy_path: &Path,
    ) -> anyhow::Result<HashMap<String, bool>> {
        self.import_legacy_map_if_needed("cron-toggles", legacy_path, replace_cron_toggles)?;
        let connection = self.lock();
        let mut statement = connection.prepare("SELECT job_id, enabled FROM cron_toggles")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?;
        Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
    }

    pub fn replace_cron_toggles(&self, entries: &HashMap<String, bool>) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_cron_toggles(&tx, entries)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_or_import_cron_threads(
        &self,
        legacy_path: &Path,
    ) -> anyhow::Result<HashMap<String, String>> {
        self.import_legacy_map_if_needed("cron-threads", legacy_path, replace_cron_threads)?;
        self.read_string_map("SELECT job_id, thread_id FROM cron_threads")
    }

    pub fn replace_cron_threads(&self, entries: &HashMap<String, String>) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_cron_threads(&tx, entries)?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_or_import_reminders(&self, legacy_path: &Path) -> anyhow::Result<Vec<Reminder>> {
        self.import_legacy_if_needed("reminders", legacy_path, replace_reminders)?;
        let connection = self.lock();
        read_payloads(
            &connection,
            "SELECT payload FROM reminders ORDER BY fire_at, id",
        )
    }

    pub fn replace_reminders(&self, entries: &[Reminder]) -> anyhow::Result<()> {
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        replace_reminders(&tx, entries)?;
        tx.commit()?;
        Ok(())
    }

    fn import_string_map_if_needed(
        &self,
        source: &str,
        legacy_path: &Path,
        replace: impl FnOnce(&Transaction<'_>, &HashMap<String, String>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.import_legacy_map_if_needed(source, legacy_path, replace)
    }

    fn import_legacy_map_if_needed<V: DeserializeOwned>(
        &self,
        source: &str,
        legacy_path: &Path,
        replace: impl FnOnce(&Transaction<'_>, &HashMap<String, V>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        harden_legacy_backup(legacy_path)?;
        if self.legacy_imported(source)? {
            return Ok(());
        }
        let legacy = read_legacy::<HashMap<String, V>>(legacy_path)?;
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        if let Some(entries) = legacy {
            backup_legacy_file(legacy_path)?;
            replace(&tx, &entries)?;
        }
        record_legacy_import(&tx, source, legacy_path)?;
        tx.commit()?;
        Ok(())
    }

    fn read_string_map(&self, sql: &str) -> anyhow::Result<HashMap<String, String>> {
        let connection = self.lock();
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<HashMap<_, _>, _>>()?)
    }

    fn import_legacy_if_needed<T>(
        &self,
        source: &str,
        legacy_path: &Path,
        replace: impl FnOnce(&Transaction<'_>, &[T]) -> anyhow::Result<()>,
    ) -> anyhow::Result<()>
    where
        T: DeserializeOwned,
    {
        harden_legacy_backup(legacy_path)?;
        if self.legacy_imported(source)? {
            return Ok(());
        }
        let legacy = read_legacy::<Vec<T>>(legacy_path)?;
        let mut connection = self.lock();
        let tx = connection.transaction()?;
        if let Some(entries) = legacy {
            backup_legacy_file(legacy_path)?;
            replace(&tx, &entries)?;
        }
        record_legacy_import(&tx, source, legacy_path)?;
        tx.commit()?;
        Ok(())
    }

    fn legacy_imported(&self, source: &str) -> anyhow::Result<bool> {
        let connection = self.lock();
        Ok(connection
            .query_row(
                "SELECT 1 FROM legacy_imports WHERE source = ?1",
                [source],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn read_legacy<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data)
            .with_context(|| format!("failed to parse legacy state at {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read legacy state at {}", path.display()))
        }
    }
}

fn backup_legacy_file(path: &Path) -> anyhow::Result<()> {
    let backup = legacy_backup_path(path);
    if !backup.exists() {
        std::fs::copy(path, &backup).with_context(|| {
            format!(
                "failed to back up legacy state from {} to {}",
                path.display(),
                backup.display()
            )
        })?;
    }
    set_private_permissions(&backup)?;
    Ok(())
}

fn harden_legacy_backup(path: &Path) -> anyhow::Result<()> {
    let backup = legacy_backup_path(path);
    if backup.exists() {
        set_private_permissions(&backup)?;
    }
    Ok(())
}

fn legacy_backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".control-db-v1.bak");
    PathBuf::from(name)
}

fn set_private_permissions(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn record_legacy_import(tx: &Transaction<'_>, source: &str, path: &Path) -> anyhow::Result<()> {
    tx.execute(
        "INSERT INTO legacy_imports(source, legacy_path) VALUES (?1, ?2)",
        params![source, path.to_string_lossy()],
    )?;
    Ok(())
}

fn replace_projects(tx: &Transaction<'_>, entries: &[ProjectBinding]) -> anyhow::Result<()> {
    tx.execute("DELETE FROM project_access_users", [])?;
    tx.execute("DELETE FROM project_access_roles", [])?;
    tx.execute("DELETE FROM project_bindings", [])?;
    for entry in entries {
        let channel_id = entry.channel_id.to_string();
        tx.execute(
            "INSERT INTO project_bindings(channel_id, guild_id, workspace_alias, created_by, home_message_id, created_at, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![channel_id, entry.guild_id.to_string(), entry.workspace_alias, entry.created_by.to_string(), entry.home_message_id.map(|id| id.to_string()), entry.created_at.to_rfc3339(), serde_json::to_string(entry)?],
        )?;
        for user_id in &entry.access_user_ids {
            tx.execute(
                "INSERT INTO project_access_users(channel_id, user_id) VALUES (?1, ?2)",
                params![entry.channel_id.to_string(), user_id.to_string()],
            )?;
        }
        for role_id in &entry.access_role_ids {
            tx.execute(
                "INSERT INTO project_access_roles(channel_id, role_id) VALUES (?1, ?2)",
                params![entry.channel_id.to_string(), role_id.to_string()],
            )?;
        }
    }
    Ok(())
}

fn replace_tasks(tx: &Transaction<'_>, entries: &[TaskRecord]) -> anyhow::Result<()> {
    tx.execute("DELETE FROM tasks", [])?;
    for entry in entries {
        tx.execute(
            "INSERT INTO tasks(thread_id, guild_id, project_channel_id, workspace_alias, title, created_by, status_message_id, state, queued_messages, created_at, updated_at, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![entry.thread_id.to_string(), entry.guild_id.to_string(), entry.project_channel_id.to_string(), entry.workspace_alias, entry.title, entry.created_by.to_string(), entry.status_message_id.map(|id| id.to_string()), format!("{:?}", entry.state).to_ascii_lowercase(), i64::try_from(entry.queued_messages)?, entry.created_at.to_rfc3339(), entry.updated_at.to_rfc3339(), serde_json::to_string(entry)?],
        )?;
    }
    Ok(())
}

fn replace_repository_jobs(
    tx: &Transaction<'_>,
    next_id: u64,
    jobs: &[RepositoryCommandJob],
) -> anyhow::Result<()> {
    tx.execute("DELETE FROM repository_command_jobs", [])?;
    tx.execute(
        "UPDATE repository_command_state SET next_id = ?1 WHERE singleton = 1",
        [i64::try_from(next_id)?],
    )?;
    for (queue_order, job) in jobs.iter().enumerate() {
        tx.execute(
            "INSERT INTO repository_command_jobs(id, queue_order, workspace_alias, project_channel_id, requested_by, command_id, book_slug, state, created_at, started_at, finished_at, recovered_from_active, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![i64::try_from(job.id)?, i64::try_from(queue_order)?, job.workspace_alias, job.project_channel_id.to_string(), job.requested_by.to_string(), job.command_id, job.book_slug, format!("{:?}", job.state).to_ascii_lowercase(), job.created_at.to_rfc3339(), job.started_at.map(|value| value.to_rfc3339()), job.finished_at.map(|value| value.to_rfc3339()), job.recovered_from_active, serde_json::to_string(job)?],
        )?;
    }
    Ok(())
}

fn replace_session_column(
    tx: &Transaction<'_>,
    column: &str,
    entries: &HashMap<String, String>,
) -> anyhow::Result<()> {
    if !matches!(column, "session_id" | "workdir") {
        anyhow::bail!("unsupported session record column");
    }
    let other = if column == "session_id" {
        "workdir"
    } else {
        "session_id"
    };
    tx.execute(
        &format!("DELETE FROM session_records WHERE {other} IS NULL"),
        [],
    )?;
    tx.execute(
        &format!("UPDATE session_records SET {column} = NULL WHERE {other} IS NOT NULL"),
        [],
    )?;
    let sql = format!(
        "INSERT INTO session_records(thread_key, {column}) VALUES (?1, ?2) \
         ON CONFLICT(thread_key) DO UPDATE SET {column} = excluded.{column}"
    );
    for (thread_key, value) in entries {
        tx.execute(&sql, params![thread_key, value])?;
    }
    Ok(())
}

fn replace_dispatch_snapshot(tx: &Transaction<'_>, snapshot: &[u8]) -> anyhow::Result<()> {
    let file: serde_json::Value = serde_json::from_slice(snapshot)?;
    let version = file
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if version != 1 {
        anyhow::bail!("unsupported dispatch queue version {version}");
    }
    let next_message_id = file
        .get("next_message_id")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let lanes = file
        .get("lanes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("dispatch queue lanes must be an array"))?;

    tx.execute("DELETE FROM dispatch_messages", [])?;
    tx.execute("DELETE FROM dispatch_lanes", [])?;
    tx.execute(
        "UPDATE dispatch_queue_state SET next_message_id = ?1 WHERE singleton = 1",
        [i64::try_from(next_message_id)?],
    )?;
    for lane in lanes {
        let key = required_json_string(lane, "key")?.to_string();
        let adapter_kind = required_json_string(lane, "adapter_kind")?;
        let channel = lane
            .get("thread_channel")
            .ok_or_else(|| anyhow::anyhow!("dispatch lane is missing thread_channel"))?;
        let platform = required_json_string(channel, "platform")?;
        let channel_id = required_json_string(channel, "channel_id")?;
        let thread_id = channel.get("thread_id").and_then(serde_json::Value::as_str);
        let mut lane_metadata = lane.clone();
        lane_metadata["pending"] = serde_json::Value::Array(Vec::new());
        lane_metadata["active"] = serde_json::Value::Array(Vec::new());
        tx.execute(
            "INSERT INTO dispatch_lanes(lane_key, adapter_kind, platform, channel_id, thread_id, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![key, adapter_kind, platform, channel_id, thread_id, serde_json::to_string(&lane_metadata)?],
        )?;
        for (state, field) in [("pending", "pending"), ("active", "active")] {
            let messages = lane
                .get(field)
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("dispatch lane {field} must be an array"))?;
            for (queue_order, message) in messages.iter().enumerate() {
                let id = message
                    .get("id")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| anyhow::anyhow!("dispatch message is missing id"))?;
                tx.execute(
                    "INSERT INTO dispatch_messages(id, lane_key, queue_state, queue_order, sender_name, queued_at_unix_ms, recovered_from_active, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![i64::try_from(id)?, key, state, i64::try_from(queue_order)?, message.get("sender_name").and_then(serde_json::Value::as_str).unwrap_or_default(), i64::try_from(message.get("queued_at_unix_ms").and_then(serde_json::Value::as_u64).unwrap_or(0))?, message.get("recovered_from_active").and_then(serde_json::Value::as_bool).unwrap_or(false), serde_json::to_string(message)?],
                )?;
            }
        }
    }
    Ok(())
}

fn dispatch_snapshot(connection: &Connection) -> anyhow::Result<Vec<u8>> {
    let next_message_id = connection.query_row(
        "SELECT next_message_id FROM dispatch_queue_state WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let mut lanes = read_payloads::<serde_json::Value>(
        connection,
        "SELECT payload FROM dispatch_lanes ORDER BY lane_key",
    )?;
    for lane in &mut lanes {
        let key = required_json_string(lane, "key")?.to_string();
        for state in ["pending", "active"] {
            let mut statement = connection.prepare(
                "SELECT payload FROM dispatch_messages WHERE lane_key = ?1 AND queue_state = ?2 ORDER BY queue_order, id",
            )?;
            let rows = statement.query_map(params![key, state], |row| row.get::<_, String>(0))?;
            let mut messages = Vec::new();
            for row in rows {
                messages.push(serde_json::from_str(&row?)?);
            }
            lane[state] = serde_json::Value::Array(messages);
        }
    }
    Ok(serde_json::to_vec_pretty(&serde_json::json!({
        "version": 1,
        "next_message_id": next_message_id.max(1),
        "lanes": lanes,
    }))?)
}

fn required_json_string<'a>(value: &'a serde_json::Value, field: &str) -> anyhow::Result<&'a str> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing dispatch queue field {field}"))
}

fn replace_cron_toggles(
    tx: &Transaction<'_>,
    entries: &HashMap<String, bool>,
) -> anyhow::Result<()> {
    tx.execute("DELETE FROM cron_toggles", [])?;
    for (job_id, enabled) in entries {
        tx.execute(
            "INSERT INTO cron_toggles(job_id, enabled) VALUES (?1, ?2)",
            params![job_id, enabled],
        )?;
    }
    Ok(())
}

fn replace_cron_threads(
    tx: &Transaction<'_>,
    entries: &HashMap<String, String>,
) -> anyhow::Result<()> {
    tx.execute("DELETE FROM cron_threads", [])?;
    for (job_id, thread_id) in entries {
        tx.execute(
            "INSERT INTO cron_threads(job_id, thread_id) VALUES (?1, ?2)",
            params![job_id, thread_id],
        )?;
    }
    Ok(())
}

fn replace_reminders(tx: &Transaction<'_>, entries: &[Reminder]) -> anyhow::Result<()> {
    tx.execute("DELETE FROM reminders", [])?;
    for reminder in entries {
        tx.execute(
            "INSERT INTO reminders(id, channel_id, sender_id, fire_at, created_at, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![reminder.id, reminder.channel_id.to_string(), reminder.sender_id.to_string(), reminder.fire_at.to_rfc3339(), reminder.created_at.to_rfc3339(), serde_json::to_string(reminder)?],
        )?;
    }
    Ok(())
}

fn read_payloads<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
) -> anyhow::Result<Vec<T>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut values = Vec::new();
    for row in rows {
        values.push(serde_json::from_str(&row?)?);
    }
    Ok(values)
}

fn seed_ui_catalog(connection: &Connection) -> anyhow::Result<()> {
    for (surface, title, accent, empty_text) in [
        (
            "project_home",
            "Project Home",
            0x5865f2_i64,
            "目前沒有進行中的任務。",
        ),
        (
            "task_status",
            "Task Status",
            0x57f287_i64,
            "目前沒有排隊中的訊息。",
        ),
        (
            "repository_queue",
            "Repository Command Queue",
            0xfee75c_i64,
            "目前沒有排隊中的指令。",
        ),
    ] {
        connection.execute(
            "INSERT OR IGNORE INTO ui_surfaces(surface_id, title, accent_color, empty_text) VALUES (?1, ?2, ?3, ?4)",
            params![surface, title, accent, empty_text],
        )?;
    }
    Ok(())
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS control_schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS legacy_imports (
    source TEXT PRIMARY KEY,
    legacy_path TEXT NOT NULL,
    imported_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS project_bindings (
    channel_id TEXT PRIMARY KEY,
    guild_id TEXT NOT NULL,
    workspace_alias TEXT NOT NULL,
    created_by TEXT NOT NULL,
    home_message_id TEXT,
    created_at TEXT NOT NULL,
    payload TEXT NOT NULL,
    UNIQUE(guild_id, workspace_alias)
);
CREATE TABLE IF NOT EXISTS project_access_users (
    channel_id TEXT NOT NULL REFERENCES project_bindings(channel_id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    PRIMARY KEY(channel_id, user_id)
);
CREATE TABLE IF NOT EXISTS project_access_roles (
    channel_id TEXT NOT NULL REFERENCES project_bindings(channel_id) ON DELETE CASCADE,
    role_id TEXT NOT NULL,
    PRIMARY KEY(channel_id, role_id)
);
CREATE TABLE IF NOT EXISTS tasks (
    thread_id TEXT PRIMARY KEY,
    guild_id TEXT NOT NULL,
    project_channel_id TEXT NOT NULL,
    workspace_alias TEXT NOT NULL,
    title TEXT NOT NULL,
    created_by TEXT NOT NULL,
    status_message_id TEXT,
    state TEXT NOT NULL,
    queued_messages INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_project_updated ON tasks(project_channel_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state);
CREATE TABLE IF NOT EXISTS repository_command_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    next_id INTEGER NOT NULL
);
INSERT OR IGNORE INTO repository_command_state(singleton, next_id) VALUES (1, 1);
CREATE TABLE IF NOT EXISTS repository_command_jobs (
    id INTEGER PRIMARY KEY,
    queue_order INTEGER NOT NULL,
    workspace_alias TEXT NOT NULL,
    project_channel_id TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    command_id TEXT NOT NULL,
    book_slug TEXT,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    recovered_from_active INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_repository_jobs_fifo ON repository_command_jobs(workspace_alias, state, queue_order);
CREATE TABLE IF NOT EXISTS ui_surfaces (
    surface_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    accent_color INTEGER NOT NULL,
    empty_text TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS ui_actions (
    surface_id TEXT NOT NULL REFERENCES ui_surfaces(surface_id) ON DELETE CASCADE,
    action_id TEXT NOT NULL,
    label TEXT NOT NULL,
    style TEXT NOT NULL,
    sort_order INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY(surface_id, action_id)
);
CREATE TABLE IF NOT EXISTS session_records (
    thread_key TEXT PRIMARY KEY,
    session_id TEXT,
    workdir TEXT,
    CHECK(session_id IS NOT NULL OR workdir IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS idx_session_records_session ON session_records(session_id);
CREATE TABLE IF NOT EXISTS dispatch_queue_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    next_message_id INTEGER NOT NULL
);
INSERT OR IGNORE INTO dispatch_queue_state(singleton, next_message_id) VALUES (1, 1);
CREATE TABLE IF NOT EXISTS dispatch_lanes (
    lane_key TEXT PRIMARY KEY,
    adapter_kind TEXT NOT NULL,
    platform TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    thread_id TEXT,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_dispatch_lanes_thread ON dispatch_lanes(platform, thread_id);
CREATE TABLE IF NOT EXISTS dispatch_messages (
    id INTEGER PRIMARY KEY,
    lane_key TEXT NOT NULL REFERENCES dispatch_lanes(lane_key) ON DELETE CASCADE,
    queue_state TEXT NOT NULL CHECK(queue_state IN ('pending', 'active')),
    queue_order INTEGER NOT NULL,
    sender_name TEXT NOT NULL,
    queued_at_unix_ms INTEGER NOT NULL,
    recovered_from_active INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_dispatch_messages_fifo ON dispatch_messages(lane_key, queue_state, queue_order);
CREATE TABLE IF NOT EXISTS cron_toggles (
    job_id TEXT PRIMARY KEY,
    enabled INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS cron_threads (
    job_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS reminders (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    fire_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reminders_fire_at ON reminders(fire_at);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn imports_projects_once_and_keeps_backup() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("discord-projects.json");
        let binding = ProjectBinding {
            guild_id: 1,
            channel_id: 2,
            workspace_alias: "openab".into(),
            created_by: 3,
            access_role_id: None,
            access_user_ids: vec![4],
            access_role_ids: vec![5],
            home_message_id: None,
            created_at: Utc::now(),
        };
        std::fs::write(&legacy, serde_json::to_vec(&vec![binding.clone()]).unwrap()).unwrap();
        let db = ControlDb::open(dir.path().join("control.db")).unwrap();
        assert_eq!(db.load_or_import_projects(&legacy).unwrap(), vec![binding]);
        assert!(dir
            .path()
            .join("discord-projects.json.control-db-v1.bak")
            .exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let backup = dir.path().join("discord-projects.json.control-db-v1.bak");
            for path in [dir.path().join("control.db"), backup.clone()] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o644)).unwrap();
            db.load_or_import_projects(&legacy).unwrap();
            assert_eq!(
                std::fs::metadata(backup).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&legacy, "[]").unwrap();
        assert_eq!(db.load_or_import_projects(&legacy).unwrap().len(), 1);
    }

    #[test]
    fn seeds_cursor_ui_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let db = ControlDb::open(dir.path().join("control.db")).unwrap();
        let connection = db.lock();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM ui_surfaces", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn session_maps_share_rows_without_erasing_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let mapping_path = dir.path().join("thread_map.json");
        let meta_path = dir.path().join("session_meta.json");
        std::fs::write(&mapping_path, r#"{"discord:1":"chat-1"}"#).unwrap();
        std::fs::write(
            &meta_path,
            r#"{"discord:1":"/work/one","discord:2":"/work/two"}"#,
        )
        .unwrap();
        let db = ControlDb::open(dir.path().join("control.db")).unwrap();

        assert_eq!(
            db.load_or_import_session_mappings(&mapping_path).unwrap()["discord:1"],
            "chat-1"
        );
        assert_eq!(
            db.load_or_import_session_workdirs(&meta_path)
                .unwrap()
                .len(),
            2
        );
        db.replace_session_mappings(&HashMap::from([("discord:2".into(), "chat-2".into())]))
            .unwrap();

        assert_eq!(
            db.load_or_import_session_workdirs(&meta_path).unwrap()["discord:1"],
            "/work/one"
        );
        assert_eq!(
            db.load_or_import_session_mappings(&mapping_path).unwrap()["discord:2"],
            "chat-2"
        );
    }

    #[test]
    fn dispatch_snapshot_is_structured_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("discord-queue.json");
        let snapshot = serde_json::json!({
            "version": 1,
            "next_message_id": 8,
            "lanes": [{
                "key": "discord:thread:1",
                "adapter_kind": "discord",
                "thread_channel": {
                    "platform": "discord",
                    "channel_id": "10",
                    "thread_id": "11",
                    "parent_id": "10",
                    "origin_event_id": null
                },
                "pending": [{
                    "id": 7,
                    "sender_json": "{}",
                    "sender_name": "Example User",
                    "prompt": "status",
                    "extra_blocks": [],
                    "trigger_msg": {
                        "channel": {
                            "platform": "discord",
                            "channel_id": "10",
                            "thread_id": "11",
                            "parent_id": "10",
                            "origin_event_id": null
                        },
                        "message_id": "12"
                    },
                    "queued_at_unix_ms": 100,
                    "other_bot_present": false,
                    "recipient": null,
                    "recovered_from_active": false
                }],
                "active": []
            }]
        });
        std::fs::write(&legacy, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let db = ControlDb::open(dir.path().join("control.db")).unwrap();
        let restored: serde_json::Value =
            serde_json::from_slice(&db.load_or_import_dispatch_queue(&legacy).unwrap()).unwrap();
        assert_eq!(restored["next_message_id"], 8);
        assert_eq!(restored["lanes"][0]["pending"][0]["id"], 7);
        let connection = db.lock();
        let counts = (
            connection
                .query_row("SELECT COUNT(*) FROM dispatch_lanes", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            connection
                .query_row("SELECT COUNT(*) FROM dispatch_messages", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
        );
        assert_eq!(counts, (1, 1));
    }

    #[test]
    fn cron_and_reminder_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let db = ControlDb::open(dir.path().join("control.db")).unwrap();
        let toggles_path = dir.path().join("cron-toggles.json");
        let threads_path = dir.path().join("cron-threads.json");
        let reminders_path = dir.path().join("reminders.json");
        std::fs::write(&toggles_path, r#"{"daily":true}"#).unwrap();
        std::fs::write(&threads_path, r#"{"daily":"thread-1"}"#).unwrap();
        let now = Utc::now();
        let reminder = Reminder {
            id: "reminder-1".into(),
            channel_id: 1,
            sender_id: 2,
            targets: vec!["<@2>".into()],
            message: "follow up".into(),
            fire_at: now,
            created_at: now,
        };
        std::fs::write(
            &reminders_path,
            serde_json::to_vec(&vec![reminder.clone()]).unwrap(),
        )
        .unwrap();

        assert!(db.load_or_import_cron_toggles(&toggles_path).unwrap()["daily"]);
        assert_eq!(
            db.load_or_import_cron_threads(&threads_path).unwrap()["daily"],
            "thread-1"
        );
        let restored = db.load_or_import_reminders(&reminders_path).unwrap();
        assert_eq!(restored[0].id, reminder.id);
        assert_eq!(restored[0].message, reminder.message);
    }
}
