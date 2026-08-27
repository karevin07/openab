use crate::acp::connection::{AcpConnection, SessionActivity};
use crate::acp::protocol::ConfigOption;
use crate::config::AgentConfig;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use tracing::{info, warn};

/// Combined state protected by a single lock to prevent deadlocks.
/// Lock ordering: never await a per-connection mutex while holding `state`.
struct PoolState {
    /// Active connections: thread_key → AcpConnection handle.
    active: HashMap<String, Arc<Mutex<AcpConnection>>>,
    /// Lock-free cancel handles: thread_key → (stdin, session_id).
    /// Stored separately so cancel can work without locking the connection.
    cancel_handles: HashMap<String, CancelHandle>,
    /// Lock-free facade tokens: thread_key → the exact `OPENAB_SESSION_TOKEN` minted for the
    /// connection currently under that key. Stored here, not just inside the connection, so hung
    /// eviction can revoke the exact token **synchronously** — the `AcpConnection` DropGuard that
    /// normally revokes it cannot fire while a hung streaming task still holds an Arc of the
    /// connection, and `AcpTunnelSource` authorizes by channel alone, so an un-revoked predecessor
    /// token would keep reaching whatever tunnel a successor registers for that channel (F3).
    #[cfg(feature = "acp-mcp")]
    facade_tokens: HashMap<String, String>,
    /// Lock-free activity handles for hung-session detection without the connection mutex.
    activity: HashMap<String, Arc<SessionActivity>>,
    /// Child process-group ids, captured at insert time so hung eviction can
    /// kill the agent process without ever locking the connection.
    pgids: HashMap<String, i32>,
    /// Suspended sessions: thread_key → ACP sessionId.
    /// Used at runtime to decide which thread can be resumed via `session/load`
    /// because it no longer has a live in-memory connection.
    suspended: HashMap<String, String>,
    /// Persisted resumable sessions: thread_key → ACP sessionId.
    /// Includes both suspended sessions and active sessions so a process restart
    /// can recover any live thread via `session/load`.
    persisted: HashMap<String, String>,
    /// Serializes create/resume work per thread so rapid same-thread requests
    /// cannot race each other into duplicate `session/load` attempts.
    creating: HashMap<String, Arc<Mutex<()>>>,
    /// Per-session working directory overrides (from control directives).
    /// thread_key → canonical workspace path.
    session_workdirs: HashMap<String, String>,
    /// Reset counter per thread, bumped by [`SessionPool::reset_session`].
    ///
    /// `get_or_create` reads it before its slow section and again under the
    /// final write lock. A reset that lands in between has already cleared the
    /// maps and told the user so; without this the creation would write a
    /// session straight back and — when it was resuming rather than creating —
    /// restore the very history the reset discarded.
    ///
    /// Deliberately not a lock: making `reset_session` wait for the creating
    /// gate would park it behind a `session/new` that can take two minutes,
    /// and a stuck session is exactly when someone reaches for `/reset`.
    reset_generations: HashMap<String, u64>,
}

pub struct SessionPool {
    state: RwLock<PoolState>,
    config: AgentConfig,
    max_sessions: usize,
    /// Force-evict sessions stuck in-flight longer than this threshold
    /// (`prompt_hard_timeout_secs + hung_grace_secs`, wired in main.rs).
    hung_threshold_secs: u64,
    mapping_path: PathBuf,
    meta_path: PathBuf,
    #[cfg(feature = "discord")]
    control_db: Option<crate::control_db::ControlDb>,
    handoff_dir: PathBuf,
    default_config_options: HashMap<String, String>,
    #[cfg(feature = "acp-mcp")]
    session_registrar: Option<Arc<dyn crate::acp_mcp::SessionTokenRegistrar>>,
    #[cfg(feature = "acp-mcp")]
    facade_url: Option<String>,
}

type CancelHandle = (Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>, String);

/// User-facing lifecycle state for a thread session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Suspended,
    Persisted,
    None,
}

/// Read-only session metadata used by platform status commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub state: SessionState,
    pub working_dir: Option<String>,
    /// True while an external ACP client owns this session through the
    /// handoff marker. Status UIs use this to prevent conflicting actions.
    pub externally_detached: bool,
}
type ActiveSnapshot = Vec<(String, Arc<Mutex<AcpConnection>>)>;
type EvictionCandidate = (String, Arc<Mutex<AcpConnection>>, Instant, Option<String>);

fn remove_if_same_handle<T>(
    map: &mut HashMap<String, Arc<Mutex<T>>>,
    key: &str,
    expected: &Arc<Mutex<T>>,
) -> Option<Arc<Mutex<T>>> {
    let should_remove = map
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, expected));
    if should_remove {
        map.remove(key)
    } else {
        None
    }
}

fn get_or_insert_gate(map: &mut HashMap<String, Arc<Mutex<()>>>, key: &str) -> Arc<Mutex<()>> {
    map.entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn session_state(state: &PoolState, key: &str) -> SessionState {
    if state.active.contains_key(key) {
        SessionState::Active
    } else if state.suspended.contains_key(key) {
        SessionState::Suspended
    } else if state.persisted.contains_key(key) {
        SessionState::Persisted
    } else {
        SessionState::None
    }
}

fn has_session_state(state: &PoolState, key: &str) -> bool {
    session_state(state, key) != SessionState::None || state.session_workdirs.contains_key(key)
}

/// Returns true when a session should be treated as stale during idle cleanup.
fn classify_idle(last_active: Instant, alive: bool, cutoff: Instant) -> bool {
    last_active < cutoff || !alive
}

/// Returns true when a locked, in-flight session has exceeded the hung threshold.
fn classify_hung(
    in_flight: bool,
    last_active_age: std::time::Duration,
    threshold: std::time::Duration,
) -> bool {
    in_flight && last_active_age > threshold
}

/// Emit the force-evict warning with **both** ids redacted.
///
/// `key` is a pool key `<platform>:<channel_id>` (`acp_<uuid>`) and `session_id` is `sess_<uuid>`;
/// either resumes the session, so both are credentials. Extracted from the loop in `cleanup_idle`
/// so the redaction can be exercised by a test for real — R1 redacted the sites it enumerated and
/// this force-evict site was outside that list, logging both ids raw.
fn warn_force_evicting_hung(
    key: &str,
    session_id: Option<&str>,
    age_secs: u64,
    threshold_secs: u64,
) {
    warn!(
        thread_id = %crate::redact::redact_session_ids(key),
        session_id = %session_id.map(crate::redact::redact_session_ids).unwrap_or_default(),
        age_secs,
        threshold_secs,
        "force-evicting hung session"
    );
}

/// Returns true when `candidate_last_active` is a better eviction target than `current_oldest`.
fn better_candidate(current_oldest: Option<Instant>, candidate_last_active: Instant) -> bool {
    match current_oldest {
        Some(oldest) => candidate_last_active < oldest,
        None => true,
    }
}

/// Prepare facade browser capabilities for one session: write the agent's facade MCP entry, and
/// mint its session token **only if that write succeeded**.
///
/// The token is useless without the config. The file carries
/// `Authorization: Bearer ${OPENAB_SESSION_TOKEN}`, and it is the artifact the OPERATOR wires in
/// — since D-15 openab writes only `.openab/mcp-facade.json`, which no agent reads on its own, so
/// the import or `--mcp-config` flag is what actually points the agent at the facade. The ordering
/// still holds for a narrower reason: if openab cannot even author that file, the session has no
/// path to the facade it could be wired to, and minting regardless would register a live
/// credential for a session that cannot use it and leave it valid until eviction, while the
/// failure showed up only as a warning. Returning `None` keeps the session running without
/// browser capabilities, which is the honest description of what actually happened.
#[cfg(feature = "acp-mcp")]
async fn setup_facade_session(
    workdir: &str,
    facade_url: &str,
    channel_id: &str,
    registrar: &Arc<dyn crate::acp_mcp::SessionTokenRegistrar>,
) -> Option<String> {
    match crate::acp_mcp::write_facade_mcp_config(workdir, facade_url).await {
        Ok(()) => Some(registrar.mint(channel_id)),
        Err(e) => {
            tracing::error!(
                workdir, error = %e,
                "facade mcp config write failed — starting this session WITHOUT browser \
                 capabilities and not minting a session token that could never be presented"
            );
            None
        }
    }
}

/// Remove every non-`active` pool entry for `key`.
///
/// The single implementation for both hung eviction and [`SessionPool::reset_session`]; the latter
/// removes `active` itself and then calls this. It used to be a second copy of the same list, which
/// is how the two could drift — and the line most likely to be lost from a copy is the one below
/// about the creating gate, because it says *not* to remove something.
///
/// Hung eviction must NOT leave the session resumable: the old streaming task still holds an Arc
/// clone of the connection, so the agent process may be alive and mid-turn. If the session id
/// stayed in `suspended`/`persisted`, the next message would `session/load` the same session while
/// the old process still owns an in-flight turn.
fn purge_session_entries(state: &mut PoolState, key: &str) {
    state.cancel_handles.remove(key);
    state.activity.remove(key);
    state.pgids.remove(key);
    state.suspended.remove(key);
    state.persisted.remove(key);
    // Do NOT remove the creating gate: it is concurrency control, not session
    // state. Removing it while a holder still owns the old gate Arc would let
    // a concurrent get_or_create mint a fresh gate and run two creations for
    // the same key. The reset generation is the same kind of thing — dropping
    // it would reset the counter an in-flight creation is comparing against.
    state.session_workdirs.remove(key);
}

/// Escalating kill for a hung agent's process group: wait 10s after the
/// session/cancel attempt, SIGTERM, wait 2s, SIGKILL. Mirrors
/// `AcpConnection::kill_process_group`, which cannot run here because the
/// hung task never drops its connection Arc.
async fn kill_pgid_after_grace(pgid: Option<i32>) {
    let Some(pgid) = pgid.filter(|p| *p > 0) else {
        return;
    };
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        // No process-group kill on non-unix; rely on AcpConnection::Drop's
        // Windows handling if/when the hung task eventually unwinds.
        let _ = pgid;
    }
}

/// Remove a hung session from all pool maps. Returns true if the exact
/// connection captured at classification time was still registered; when a
/// fresh replacement exists for the key, nothing is touched.
fn apply_hung_eviction(
    state: &mut PoolState,
    key: &str,
    expected: &Arc<Mutex<AcpConnection>>,
) -> bool {
    if remove_if_same_handle(&mut state.active, key, expected).is_none() {
        return false;
    }
    purge_session_entries(state, key);
    true
}

/// Record `token` as the facade token for `key`, revoking whatever token it supersedes.
///
/// A superseded token belongs to a predecessor connection under the same key. Its `AcpConnection`
/// DropGuard normally revokes it, but if that predecessor is hung (a stuck streaming task still
/// holds an Arc) the guard never fires — so revoking the superseded token here is what stops it
/// staying valid for the channel after a successor takes over (F3). Revocation is by exact token
/// and idempotent, so overlapping with the guard on a clean replacement is harmless.
#[cfg(feature = "acp-mcp")]
fn install_facade_token(
    state: &mut PoolState,
    key: &str,
    token: String,
    registrar: Option<&Arc<dyn crate::acp_mcp::SessionTokenRegistrar>>,
) {
    if let Some(superseded) = state.facade_tokens.insert(key.to_string(), token) {
        if let Some(registrar) = registrar {
            registrar.revoke(&superseded);
        }
    }
}

/// Revoke and forget the facade token recorded for `key`, if any.
///
/// Called from every path that removes a connection from `active` (hung eviction, idle eviction,
/// reset, suspend). On the clean paths the connection also drops and its guard revokes the same
/// token — idempotent — but the hung path is the one that needs this: the guard cannot fire while
/// the hung task holds an Arc, so without a synchronous revoke here the token outlives the eviction
/// and `AcpTunnelSource` (channel-only authorization) would let the hung predecessor reach a
/// successor's tunnel (F3). `purge_session_entries` deliberately does NOT touch `facade_tokens`, so
/// this can run *after* `apply_hung_eviction` and still find the token to revoke.
#[cfg(feature = "acp-mcp")]
fn revoke_facade_token_for_key(
    state: &mut PoolState,
    key: &str,
    registrar: Option<&Arc<dyn crate::acp_mcp::SessionTokenRegistrar>>,
) {
    if let Some(token) = state.facade_tokens.remove(key) {
        if let Some(registrar) = registrar {
            registrar.revoke(&token);
        }
    }
}

impl SessionPool {
    pub fn new(
        config: AgentConfig,
        max_sessions: usize,
        hung_threshold_secs: u64,
        default_config_options: HashMap<String, String>,
    ) -> Self {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"));
        Self::new_in(
            &home,
            config,
            max_sessions,
            hung_threshold_secs,
            default_config_options,
        )
    }

    /// Same as [`SessionPool::new`] with the state directory given explicitly.
    ///
    /// `new` derives it from `HOME`, which makes a pool impossible to construct
    /// in a test without two tests fighting over the same `thread_map.json`.
    pub(crate) fn new_in(
        home: &Path,
        config: AgentConfig,
        max_sessions: usize,
        hung_threshold_secs: u64,
        default_config_options: HashMap<String, String>,
    ) -> Self {
        let openab_dir = home.join(".openab");
        let _ = std::fs::create_dir_all(&openab_dir);
        let mapping_path = openab_dir.join("thread_map.json");
        let meta_path = openab_dir.join("session_meta.json");
        let handoff_dir = openab_dir.join("external-handoffs");
        let _ = std::fs::create_dir_all(&handoff_dir);
        let suspended = Self::load_mapping(&mapping_path);
        let session_workdirs = Self::load_mapping(&meta_path);
        Self {
            state: RwLock::new(PoolState {
                active: HashMap::new(),
                cancel_handles: HashMap::new(),
                #[cfg(feature = "acp-mcp")]
                facade_tokens: HashMap::new(),
                activity: HashMap::new(),
                pgids: HashMap::new(),
                persisted: suspended.clone(),
                suspended,
                creating: HashMap::new(),
                session_workdirs,
                reset_generations: HashMap::new(),
            }),
            config,
            max_sessions,
            hung_threshold_secs,
            mapping_path,
            meta_path,
            #[cfg(feature = "discord")]
            control_db: None,
            handoff_dir,
            default_config_options,
            #[cfg(feature = "acp-mcp")]
            session_registrar: None,
            #[cfg(feature = "acp-mcp")]
            facade_url: None,
        }
    }

    #[cfg(feature = "discord")]
    pub fn with_control_db(mut self, db: crate::control_db::ControlDb) -> anyhow::Result<Self> {
        let persisted = db.load_or_import_session_mappings(&self.mapping_path)?;
        let session_workdirs = db.load_or_import_session_workdirs(&self.meta_path)?;
        let state = self.state.get_mut();
        state.suspended = persisted.clone();
        state.persisted = persisted;
        state.session_workdirs = session_workdirs;
        self.control_db = Some(db);
        Ok(self)
    }

    /// Wire the facade session-token registrar + facade URL, set by the root
    /// when `[mcp]` is running. With both present the pool does its half: mints
    /// one token per session, injects it as `OPENAB_SESSION_TOKEN` in the agent
    /// process env, and writes the static facade MCP entry once per workdir.
    ///
    /// That is necessary but NOT sufficient for browser capabilities to route
    /// through the facade. The operator must still put the written entry in front
    /// of the agent, and a `type:acp` server must actually attach over `/acp` —
    /// admission is that transport auth, not a config allowlist (D-29 removed
    /// `[[mcp.acp_servers]]`, reversing D-20).
    #[cfg(feature = "acp-mcp")]
    pub fn with_facade_sessions(
        mut self,
        registrar: Option<Arc<dyn crate::acp_mcp::SessionTokenRegistrar>>,
        facade_url: Option<String>,
    ) -> Self {
        self.session_registrar = registrar;
        self.facade_url = facade_url;
        self
    }

    fn load_mapping(path: &Path) -> HashMap<String, String> {
        match std::fs::read_to_string(path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                warn!(path = %path.display(), error = %e, "corrupt mapping file, starting fresh");
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        }
    }

    fn save_mapping(&self, persisted: &HashMap<String, String>) {
        #[cfg(feature = "discord")]
        if let Some(db) = &self.control_db {
            if let Err(error) = db.replace_session_mappings(persisted) {
                warn!(%error, "failed to persist thread mapping to control database");
            }
            return;
        }
        let data = match serde_json::to_string_pretty(persisted) {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "failed to serialize thread mapping");
                return;
            }
        };
        let tmp = self.mapping_path.with_extension("json.tmp");
        if let Err(e) =
            std::fs::write(&tmp, &data).and_then(|_| std::fs::rename(&tmp, &self.mapping_path))
        {
            warn!(path = %self.mapping_path.display(), error = %e, "failed to persist thread mapping");
        }
    }

    fn save_meta(&self, workdirs: &HashMap<String, String>) {
        #[cfg(feature = "discord")]
        if let Some(db) = &self.control_db {
            if let Err(error) = db.replace_session_workdirs(workdirs) {
                warn!(%error, "failed to persist session metadata to control database");
            }
            return;
        }
        let data = match serde_json::to_string_pretty(workdirs) {
            Ok(d) => d,
            Err(e) => {
                warn!(error = %e, "failed to serialize session metadata");
                return;
            }
        };
        let tmp = self.meta_path.with_extension("json.tmp");
        if let Err(e) =
            std::fs::write(&tmp, &data).and_then(|_| std::fs::rename(&tmp, &self.meta_path))
        {
            warn!(path = %self.meta_path.display(), error = %e, "failed to persist session metadata");
        }
    }

    fn handoff_marker_path(&self, thread_id: &str) -> PathBuf {
        let filename: String = thread_id
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        self.handoff_dir.join(filename)
    }

    /// Check if session state exists for this thread (active, suspended, or persisted).
    #[allow(dead_code)]
    pub async fn has_active_session(&self, thread_id: &str) -> bool {
        let state = self.state.read().await;
        // Any of these means the thread already has session state.
        if state.suspended.contains_key(thread_id) || state.persisted.contains_key(thread_id) {
            return true;
        }
        if let Some(conn) = state.active.get(thread_id) {
            match conn.try_lock() {
                Ok(c) => return c.alive(),
                Err(_) => return true, // lock held = connection busy streaming = alive
            }
        }
        false
    }

    /// Return a read-only lifecycle/workspace snapshot for platform UX.
    pub async fn session_snapshot(&self, thread_id: &str) -> SessionSnapshot {
        let state = self.state.read().await;
        let lifecycle = session_state(&state, thread_id);
        let working_dir =
            state.session_workdirs.get(thread_id).cloned().or_else(|| {
                (lifecycle != SessionState::None).then(|| self.config.working_dir.clone())
            });
        SessionSnapshot {
            state: lifecycle,
            working_dir,
            externally_detached: self.handoff_marker_path(thread_id).is_file(),
        }
    }

    /// True when this thread's ACP connection currently holds its mutex (prompt in flight).
    pub async fn prompt_in_flight(&self, thread_id: &str) -> bool {
        let state = self.state.read().await;
        match state.active.get(thread_id) {
            Some(conn) => conn.try_lock().is_err(),
            None => false,
        }
    }

    /// Reject a chat that already belongs to a Discord thread before creating UI for it.
    pub async fn ensure_external_session_available(&self, session_id: &str) -> Result<()> {
        if !self.external_session_is_available(session_id).await {
            return Err(anyhow!(
                "Cursor chat is already attached to a Discord thread"
            ));
        }
        Ok(())
    }

    pub async fn external_session_is_available(&self, session_id: &str) -> bool {
        let state = self.state.read().await;
        !state
            .persisted
            .values()
            .any(|current| current == session_id)
    }

    /// Attach an existing external ACP session to an idle thread without spawning an agent.
    /// The next message loads `session_id` through the normal `session/load` path.
    pub async fn attach_external_session(
        &self,
        thread_id: &str,
        session_id: &str,
        working_dir: &str,
    ) -> Result<()> {
        if thread_id.is_empty() || session_id.is_empty() {
            return Err(anyhow!("thread ID and session ID are required"));
        }
        let working_dir = Path::new(working_dir)
            .canonicalize()
            .map_err(|error| anyhow!("invalid session workspace: {error}"))?
            .to_string_lossy()
            .to_string();
        let create_gate = {
            let mut state = self.state.write().await;
            get_or_insert_gate(&mut state.creating, thread_id)
        };
        let _create_guard = create_gate.lock().await;

        let mut state = self.state.write().await;
        attach_external_session_state(&mut state, thread_id, session_id, &working_dir)?;
        self.save_mapping(&state.persisted);
        self.save_meta(&state.session_workdirs);
        let _ = std::fs::remove_file(self.handoff_marker_path(thread_id));
        info!(
            thread_id = %crate::redact::redact_session_ids(thread_id),
            session_id = %crate::redact::redact_session_ids(session_id),
            "external session attached"
        );
        Ok(())
    }

    pub async fn get_or_create(
        &self,
        thread_id: &str,
        working_dir_override: Option<&str>,
    ) -> Result<bool> {
        let create_gate = {
            let mut state = self.state.write().await;
            get_or_insert_gate(&mut state.creating, thread_id)
        };
        let _create_guard = create_gate.lock().await;

        if self.handoff_marker_path(thread_id).is_file() {
            return Err(anyhow!(
                "session is detached to an external ACP client; close or release it first"
            ));
        }

        let (existing, saved_session_id, reset_generation) = {
            let state = self.state.read().await;
            (
                state.active.get(thread_id).cloned(),
                state.suspended.get(thread_id).cloned(),
                state.reset_generations.get(thread_id).copied().unwrap_or(0),
            )
        };

        let had_existing = existing.is_some();
        let mut saved_session_id = saved_session_id;
        if let Some(conn) = existing.clone() {
            // Never await the existing connection's mutex here: we hold the
            // per-thread creating gate, so blocking on a hung connection would
            // permanently jam ALL future messages for this thread_id (F1).
            // Lock held = busy streaming = alive (same convention as
            // has_active_session); cleanup_idle owns hung recovery.
            let Ok(conn) = conn.try_lock() else {
                return Ok(false);
            };
            if conn.alive() {
                return Ok(false);
            }
            if saved_session_id.is_none() {
                saved_session_id = conn.acp_session_id.clone();
            }
        }

        // Snapshot active handles so we can inspect them outside the state lock.
        let snapshot: Vec<(String, Arc<Mutex<AcpConnection>>)> = {
            let state = self.state.read().await;
            state
                .active
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };

        let mut eviction_candidate: Option<EvictionCandidate> = None;
        let mut skipped_locked_candidates = 0usize;
        for (key, conn) in snapshot {
            if key == thread_id {
                continue;
            }
            let conn_handle = Arc::clone(&conn);
            let Ok(conn) = conn.try_lock() else {
                skipped_locked_candidates += 1;
                continue;
            };
            let candidate = (
                key,
                conn_handle,
                conn.last_active,
                conn.acp_session_id.clone(),
            );
            if better_candidate(
                eviction_candidate.as_ref().map(|(_, _, t, _)| *t),
                candidate.2,
            ) {
                eviction_candidate = Some(candidate);
            }
        }

        // Resolve effective working directory: stored per-session > explicit override > global config.
        // Stored value has highest priority to enforce immutability (ADR §4.5).
        let stored_workdir = {
            let state = self.state.read().await;
            state.session_workdirs.get(thread_id).cloned()
        };

        let effective_workdir = if let Some(stored) = stored_workdir {
            stored
        } else if let Some(wd) = working_dir_override {
            wd.to_string()
        } else {
            self.config.working_dir.clone()
        };

        // Browser capabilities for an `acp:` session come from the OAB MCP Facade and nowhere
        // else: mint a per-session token (it rides the agent spawn below as OPENAB_SESSION_TOKEN)
        // and write the static facade entry before the agent boots. The returned guard revokes
        // that token when this connection is dropped, on any evict path.
        //
        // There is no transport fallback. Without `[mcp]` the root wires no registrar, and the
        // session simply starts without browser capabilities — which is the honest outcome and is
        // reported once at startup rather than being silently substituted per session.
        #[cfg(feature = "acp-mcp")]
        let mut session_token: Option<String> = None;
        #[cfg(feature = "acp-mcp")]
        let facade_token_guard: Option<tokio_util::sync::DropGuard> = match (
            thread_id.strip_prefix("acp:"),
            self.session_registrar.as_ref(),
            self.facade_url.as_ref(),
        ) {
            (Some(channel_id), Some(registrar), Some(facade_url)) => {
                match setup_facade_session(&effective_workdir, facade_url, channel_id, registrar)
                    .await
                {
                    Some(token) => {
                        session_token = Some(token.clone());
                        info!(thread_id = %crate::redact::redact_session_ids(thread_id), "session token minted for facade browser capabilities");
                        // The guard carries the TOKEN it minted, not the channel. A replaced
                        // session's teardown runs after its successor has already re-minted for
                        // the same channel, so revoking by channel would strip the live token and
                        // silently cut the new agent off from the facade; revoking this exact
                        // token is a no-op by then (R1).
                        let ct = tokio_util::sync::CancellationToken::new();
                        let child = ct.child_token();
                        let registrar = registrar.clone();
                        tokio::spawn(async move {
                            child.cancelled().await;
                            registrar.revoke(&token);
                        });
                        Some(ct.drop_guard())
                    }
                    // No config, so no token and no revoke guard to arm. The session still
                    // starts — it simply has no browser capabilities.
                    None => None,
                }
            }
            _ => None,
        };

        // Build the replacement connection outside the state lock so one stuck
        // initialization does not block all unrelated sessions.
        #[cfg(feature = "acp-mcp")]
        let spawn_env: std::collections::HashMap<String, String> = {
            let mut env = self.config.env.clone();
            if let Some(tok) = &session_token {
                // The static facade MCP entry references ${OPENAB_SESSION_TOKEN};
                // the value lives only in this agent process's environment.
                env.insert("OPENAB_SESSION_TOKEN".to_string(), tok.clone());
            }
            env
        };
        #[cfg(not(feature = "acp-mcp"))]
        let spawn_env = self.config.env.clone();
        let mut new_conn = AcpConnection::spawn(
            &self.config.command,
            &self.config.args,
            &effective_workdir,
            &spawn_env,
            &self.config.inherit_env,
        )
        .await?;

        new_conn.initialize().await?;

        let mut resumed = false;
        let mut load_failed: Option<&str> = None;
        if let Some(ref sid) = saved_session_id {
            if new_conn.supports_load_session {
                match new_conn.session_load(sid, &effective_workdir).await {
                    Ok(()) => {
                        info!(thread_id = %crate::redact::redact_session_ids(thread_id), session_id = %crate::redact::redact_session_ids(sid), "session resumed via session/load");
                        resumed = true;
                    }
                    Err(e) => {
                        // `AcpRequestError` classifies this; the pool no longer
                        // reads the message text to decide whether the session
                        // is still worth resuming.
                        if e.is_transient() {
                            warn!(thread_id = %crate::redact::redact_session_ids(thread_id), session_id = %crate::redact::redact_session_ids(sid), error = %e,
                                "session/load failed transiently, preserving session ID for retry");
                            load_failed = Some(e.user_reason());
                        } else {
                            warn!(thread_id = %crate::redact::redact_session_ids(thread_id), session_id = %crate::redact::redact_session_ids(sid), error = %e,
                                "session/load failed, creating new session");
                        }
                    }
                }
            }
        }

        if let Some(reason) = load_failed {
            // session/load failed transiently. The original session ID is already
            // in state.persisted (we haven't touched it), so the next message will
            // retry session/load automatically. Return an error so the current message
            // is not processed against a context-free session.
            return Err(anyhow!(
                "session load {reason}: could not restore previous session"
            ));
        }

        if !resumed {
            new_conn.session_new(&effective_workdir).await?;

            // Apply default config options (e.g. mode=bypass, model=swe-1-6)
            for (config_id, value) in &self.default_config_options {
                if let Err(e) = new_conn.set_config_option(config_id, value).await {
                    warn!(config_id, value, error = %e, "failed to set default config option");
                }
            }

            // Surface the reset banner both for restored sessions and for stale
            // live entries that died before we could recover a resumable
            // session id. In both cases the caller is continuing after an
            // unexpected session loss.
            if had_existing || saved_session_id.is_some() {
                new_conn.session_reset = true;
            }
        }

        let cancel_handle = new_conn.cancel_handle();
        let activity_handle = new_conn.activity_handle();
        let child_pgid = new_conn.child_pgid();
        let cancel_session_id = new_conn.acp_session_id.clone().unwrap_or_default();
        #[cfg(feature = "acp-mcp")]
        new_conn.set_facade_token_guard(facade_token_guard);
        let new_conn = Arc::new(Mutex::new(new_conn));

        let mut state = self.state.write().await;

        // A reset landed while this connection was being built. It has already
        // cleared the maps and reported success; writing now would silently undo
        // it. Return instead and let `new_conn` drop — its `Drop` kills the agent
        // process group, and the next message starts the clean session the user
        // asked for.
        if state.reset_generations.get(thread_id).copied().unwrap_or(0) != reset_generation {
            info!(
                thread_id = %crate::redact::redact_session_ids(thread_id),
                "session was reset while it was being created; discarding the new connection"
            );
            return Err(anyhow!(
                "session was reset while it was being created; send the request again"
            ));
        }

        // Another task may have created a healthy connection while we were
        // initializing this one.
        if let Some(existing) = state.active.get(thread_id).cloned() {
            let Ok(existing) = existing.try_lock() else {
                return Ok(false);
            };
            if existing.alive() {
                return Ok(false);
            }
            warn!(thread_id = %crate::redact::redact_session_ids(thread_id), "stale connection, rebuilding");
            drop(existing);
            state.active.remove(thread_id);
            state.cancel_handles.remove(thread_id);
            state.activity.remove(thread_id);
            state.pgids.remove(thread_id);
        }

        if state.active.len() >= self.max_sessions {
            if let Some((key, expected_conn, _, sid)) = eviction_candidate {
                if remove_if_same_handle(&mut state.active, &key, &expected_conn).is_some() {
                    state.cancel_handles.remove(&key);
                    state.activity.remove(&key);
                    state.pgids.remove(&key);
                    #[cfg(feature = "acp-mcp")]
                    revoke_facade_token_for_key(&mut state, &key, self.session_registrar.as_ref());
                    info!(evicted = %crate::redact::redact_session_ids(&key), "pool full, suspending oldest idle session");
                    if let Some(sid) = sid {
                        state.persisted.insert(key.clone(), sid.clone());
                        state.suspended.insert(key, sid);
                    } else {
                        state.persisted.remove(&key);
                    }
                } else {
                    warn!(evicted = %crate::redact::redact_session_ids(&key), "pool full but eviction candidate changed before removal");
                }
            } else if skipped_locked_candidates > 0 {
                warn!(
                    max_sessions = self.max_sessions,
                    skipped_locked_candidates,
                    "pool full but all other sessions were busy during eviction scan"
                );
            }
        }

        if state.active.len() >= self.max_sessions {
            return Err(anyhow!("pool exhausted ({} sessions)", self.max_sessions));
        }

        if cancel_session_id.is_empty() {
            state.persisted.remove(thread_id);
        } else {
            state
                .persisted
                .insert(thread_id.to_string(), cancel_session_id.clone());
        }
        state.suspended.remove(thread_id);
        state.active.insert(thread_id.to_string(), new_conn);
        state
            .activity
            .insert(thread_id.to_string(), activity_handle);
        if let Some(pgid) = child_pgid {
            state.pgids.insert(thread_id.to_string(), pgid);
        }
        if !cancel_session_id.is_empty() {
            state
                .cancel_handles
                .insert(thread_id.to_string(), (cancel_handle, cancel_session_id));
        }
        // Record this connection's exact token lock-free, revoking any predecessor token it
        // supersedes under the same key (its guard cannot fire if that predecessor is hung). F3.
        #[cfg(feature = "acp-mcp")]
        if let Some(token) = session_token {
            install_facade_token(
                &mut state,
                thread_id,
                token,
                self.session_registrar.as_ref(),
            );
        }
        self.save_mapping(&state.persisted);

        // Persist workspace override only after session spawn succeeded (口渡 F2).
        if working_dir_override.is_some() {
            state
                .session_workdirs
                .entry(thread_id.to_string())
                .or_insert_with(|| effective_workdir.clone());
            self.save_meta(&state.session_workdirs);
        }

        // Return true only for genuinely new sessions — not resumed or reconnected ones.
        // A session with prior state (saved_session_id or had_existing) is a resume,
        // even if we had to spawn a new ACP process. ADR §2.2: directives are first-message-only.
        let is_fresh = !had_existing && saved_session_id.is_none();
        Ok(is_fresh)
    }

    /// Get mutable access to a connection. Caller must have called get_or_create first.
    ///
    /// Only the per-connection `Mutex` is held during `f`; the pool-level
    /// `RwLock` is acquired briefly (read-only) to look up the `Arc` and then
    /// released, so other connections can be used concurrently.
    pub async fn with_connection<F, R>(&self, thread_id: &str, f: F) -> Result<R>
    where
        F: for<'a> FnOnce(
            &'a mut AcpConnection,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<R>> + Send + 'a>,
        >,
    {
        let conn = {
            let state = self.state.read().await;
            state.active.get(thread_id).cloned().ok_or_else(|| {
                anyhow!(
                    "no connection for thread {}",
                    crate::redact::redact_session_ids(thread_id)
                )
            })?
        };

        let mut conn = conn.lock().await;
        f(&mut conn).await
    }

    /// Get cached configOptions for a session (e.g. available models).
    pub async fn get_config_options(&self, thread_id: &str) -> Vec<ConfigOption> {
        let state = self.state.read().await;
        let conn = match state.active.get(thread_id) {
            Some(c) => c.clone(),
            None => return Vec::new(),
        };
        drop(state);
        let conn = conn.lock().await;
        conn.config_options.clone()
    }

    /// Set a config option (e.g. model) via ACP and return updated options.
    pub async fn set_config_option(
        &self,
        thread_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<Vec<ConfigOption>> {
        let conn = {
            let state = self.state.read().await;
            state.active.get(thread_id).cloned().ok_or_else(|| {
                anyhow!(
                    "no connection for thread {}",
                    crate::redact::redact_session_ids(thread_id)
                )
            })?
        };
        let mut conn = conn.lock().await;
        conn.set_config_option(config_id, value).await
    }

    /// Query account-level usage/billing from the backend agent for a session
    /// (kiro-cli extension). Fails when there is no active session for the
    /// thread or the backend does not support usage queries.
    pub async fn get_usage(&self, thread_id: &str) -> Result<crate::acp::protocol::UsageReport> {
        let conn = {
            let state = self.state.read().await;
            state.active.get(thread_id).cloned().ok_or_else(|| {
                anyhow!(
                    "no connection for thread {}",
                    crate::redact::redact_session_ids(thread_id)
                )
            })?
        };
        let mut conn = conn.lock().await;
        conn.get_usage().await
    }

    /// Cancel the current in-flight operation for a session.
    /// Uses pre-stored cancel handles to avoid locking the connection (which is held during streaming).
    pub async fn cancel_session(&self, thread_id: &str) -> Result<()> {
        let (stdin, session_id) = {
            let state = self.state.read().await;
            state
                .cancel_handles
                .get(thread_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "no session for thread {}",
                        crate::redact::redact_session_ids(thread_id)
                    )
                })?
        };
        let data = serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session_id}
        }))?;
        tracing::info!(session_id = %crate::redact::redact_session_ids(&session_id), "sending session/cancel");
        use tokio::io::AsyncWriteExt;
        let mut w = stdin.lock().await;
        w.write_all(data.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    /// Detach an idle live ACP process while preserving its resumable session ID
    /// and workspace metadata. This is the hand-off point for another ACP client
    /// (for example a local terminal client) to load the same checkpoint.
    ///
    /// A prompt must not be in flight: two ACP processes writing the same session
    /// store concurrently can corrupt or lose conversation state.
    pub async fn detach_session(&self, thread_id: &str) -> Result<()> {
        let create_gate = {
            let mut state = self.state.write().await;
            get_or_insert_gate(&mut state.creating, thread_id)
        };
        let _create_guard = create_gate.lock().await;

        let active = {
            let state = self.state.read().await;
            if let Some(activity) = state.activity.get(thread_id) {
                if activity.in_flight() {
                    return Err(anyhow!(
                        "session is busy; wait for the current reply or cancel it first"
                    ));
                }
            }
            state.active.get(thread_id).cloned()
        };

        let Some(active) = active else {
            let state = self.state.read().await;
            if !state.suspended.contains_key(thread_id) && !state.persisted.contains_key(thread_id)
            {
                return Err(anyhow!(
                    "no resumable session for thread {}",
                    crate::redact::redact_session_ids(thread_id)
                ));
            }
            drop(state);
            std::fs::write(self.handoff_marker_path(thread_id), b"detached\n")?;
            return Ok(());
        };

        // A held connection lock means a turn or another connection operation is
        // active even if its activity flag has not been updated yet.
        let session_id = {
            let conn = active.try_lock().map_err(|_| {
                anyhow!("session is busy; wait for the current reply or cancel it first")
            })?;
            conn.acp_session_id
                .clone()
                .ok_or_else(|| anyhow!("session has no resumable ACP session ID yet"))?
        };

        let marker_path = self.handoff_marker_path(thread_id);
        std::fs::write(&marker_path, b"detached\n")?;
        let mut state = self.state.write().await;
        if remove_if_same_handle(&mut state.active, thread_id, &active).is_none() {
            let _ = std::fs::remove_file(marker_path);
            return Err(anyhow!(
                "session changed while preparing the hand-off; try again"
            ));
        }
        state.cancel_handles.remove(thread_id);
        state.activity.remove(thread_id);
        state.pgids.remove(thread_id);
        state
            .persisted
            .insert(thread_id.to_string(), session_id.clone());
        state.suspended.insert(thread_id.to_string(), session_id);
        #[cfg(feature = "acp-mcp")]
        revoke_facade_token_for_key(&mut state, thread_id, self.session_registrar.as_ref());
        self.save_mapping(&state.persisted);
        info!(
            thread_id = %crate::redact::redact_session_ids(thread_id),
            "session detached for external ACP hand-off"
        );
        Ok(())
    }

    /// Reset a session: cancel any in-flight operation, remove the active connection,
    /// and clear all suspended state. The ACP process will be killed once the last
    /// Arc reference is dropped (after streaming finishes). The next message will
    /// trigger a fresh `get_or_create` with a new ACP session.
    pub async fn reset_session(&self, thread_id: &str) -> Result<()> {
        // Send session/cancel via the lock-free stdin handle first.
        // This stops in-flight streaming even while with_connection() holds the
        // connection mutex, so the old process finishes promptly.
        if let Some((stdin, session_id)) = {
            let state = self.state.read().await;
            state.cancel_handles.get(thread_id).cloned()
        } {
            let data = serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": {"sessionId": session_id}
            }))?;
            tracing::info!(session_id = %crate::redact::redact_session_ids(&session_id), "reset: sending session/cancel");
            use tokio::io::AsyncWriteExt;
            let mut w = stdin.lock().await;
            let _ = w.write_all(data.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
        }

        let mut state = self.state.write().await;
        let had_state = has_session_state(&state, thread_id);
        // Bumped before anything is cleared, so a creation that re-acquires the
        // write lock after this point sees a changed generation and stands down.
        let generation = state
            .reset_generations
            .entry(thread_id.to_string())
            .or_insert(0);
        *generation = generation.saturating_add(1);
        state.active.remove(thread_id);
        // Everything else a reset clears is exactly what hung eviction clears, including the rule
        // that the creating gate survives. Call the one implementation rather than keeping a second
        // copy of the list: the copies are what let the two drift, and the gate rule is precisely
        // the kind of line that gets dropped from a duplicate without anyone noticing.
        purge_session_entries(&mut state, thread_id);
        // Resetting a hung session drops the map's Arc but not the one the stuck task holds, so the
        // guard cannot revoke — do it synchronously here too (F3).
        #[cfg(feature = "acp-mcp")]
        revoke_facade_token_for_key(&mut state, thread_id, self.session_registrar.as_ref());
        self.save_mapping(&state.persisted);
        self.save_meta(&state.session_workdirs);
        let _ = std::fs::remove_file(self.handoff_marker_path(thread_id));
        if had_state {
            info!(thread_id = %crate::redact::redact_session_ids(thread_id), "session reset");
            Ok(())
        } else {
            Err(anyhow!(
                "no session for thread {}",
                crate::redact::redact_session_ids(thread_id)
            ))
        }
    }

    pub async fn cleanup_idle(&self, ttl_secs: u64) {
        let cutoff = Instant::now() - std::time::Duration::from_secs(ttl_secs);
        let hung_threshold = std::time::Duration::from_secs(self.hung_threshold_secs);

        let (snapshot, activity_map, cancel_map, pgid_map) = {
            let state = self.state.read().await;
            let snapshot: ActiveSnapshot = state
                .active
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect();
            (
                snapshot,
                state.activity.clone(),
                state.cancel_handles.clone(),
                state.pgids.clone(),
            )
        };

        let mut stale = Vec::new();
        let mut hung: Vec<(String, Arc<Mutex<AcpConnection>>)> = Vec::new();
        for (key, conn) in snapshot {
            // Skip active sessions for this cleanup round instead of waiting on
            // their per-connection mutex. A busy session is not idle unless hung.
            let conn_handle = Arc::clone(&conn);
            let Ok(conn) = conn.try_lock() else {
                if let Some(activity) = activity_map.get(&key) {
                    if classify_hung(activity.in_flight(), activity.age(), hung_threshold) {
                        let session_id = cancel_map.get(&key).map(|(_, sid)| sid.clone());
                        warn_force_evicting_hung(
                            &key,
                            session_id.as_deref(),
                            activity.age().as_secs(),
                            self.hung_threshold_secs,
                        );
                        // Best-effort session/cancel via the lock-free stdin
                        // handle, detached so a wedged stdin can never block
                        // cleanup (and never while holding `state`). The hung
                        // task never unwinds, so AcpConnection::Drop never
                        // fires; after the cancel attempt, kill the child
                        // process group directly or the agent leaks forever (F4).
                        let stdin_handle = cancel_map.get(&key).map(|(stdin, _)| Arc::clone(stdin));
                        let pgid = pgid_map.get(&key).copied();
                        tokio::spawn(async move {
                            if let (Some(stdin), Some(session_id)) = (stdin_handle, session_id) {
                                let _ = tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    async move {
                                        if let Ok(data) =
                                            serde_json::to_string(&serde_json::json!({
                                                "jsonrpc": "2.0",
                                                "method": "session/cancel",
                                                "params": {"sessionId": session_id}
                                            }))
                                        {
                                            use tokio::io::AsyncWriteExt;
                                            let mut w = stdin.lock().await;
                                            let _ = w.write_all(data.as_bytes()).await;
                                            let _ = w.write_all(b"\n").await;
                                            let _ = w.flush().await;
                                        }
                                    },
                                )
                                .await;
                            }
                            kill_pgid_after_grace(pgid).await;
                        });
                        hung.push((key, conn_handle));
                    }
                }
                continue;
            };
            // try_lock success means no turn is streaming under
            // with_connection, so a true in_flight flag is stale (the turn
            // aborted without prompt_done). Self-heal it so the session can
            // never be falsely classified as hung later.
            if let Some(activity) = activity_map.get(&key) {
                if activity.in_flight() {
                    activity.set_in_flight(false);
                    activity.touch();
                }
            }
            if classify_idle(conn.last_active, conn.alive(), cutoff) {
                stale.push((key, conn_handle, conn.acp_session_id.clone()));
            }
        }

        if stale.is_empty() && hung.is_empty() {
            return;
        }

        let mut state = self.state.write().await;
        for (key, expected_conn, sid) in stale {
            if remove_if_same_handle(&mut state.active, &key, &expected_conn).is_some() {
                info!(thread_id = %crate::redact::redact_session_ids(&key), "cleaning up idle session");
                state.cancel_handles.remove(&key);
                state.activity.remove(&key);
                state.pgids.remove(&key);
                #[cfg(feature = "acp-mcp")]
                revoke_facade_token_for_key(&mut state, &key, self.session_registrar.as_ref());
                if let Some(sid) = sid {
                    state.persisted.insert(key.clone(), sid.clone());
                    state.suspended.insert(key, sid);
                } else {
                    state.persisted.remove(&key);
                    state.session_workdirs.remove(&key);
                }
            }
        }
        for (key, expected_conn) in hung {
            if apply_hung_eviction(&mut state, &key, &expected_conn) {
                // The DropGuard cannot fire — the hung streaming task still holds an Arc, so the
                // connection never drops. Revoke the exact token synchronously, or it keeps
                // resolving to the channel and a successor's tunnel becomes reachable by the hung
                // predecessor (F3). Safe after `apply_hung_eviction`: its `purge_session_entries`
                // leaves `facade_tokens` alone.
                #[cfg(feature = "acp-mcp")]
                revoke_facade_token_for_key(&mut state, &key, self.session_registrar.as_ref());
            } else {
                warn!(thread_id = %crate::redact::redact_session_ids(&key), "hung session was replaced before eviction; maps untouched");
            }
        }
        self.save_mapping(&state.persisted);
        self.save_meta(&state.session_workdirs);
    }

    pub async fn shutdown(&self) {
        // Snapshot active handles, then drop state lock before awaiting
        // per-connection mutexes (lock ordering: never hold state while
        // awaiting a connection lock).
        let snapshot: Vec<(String, Arc<Mutex<AcpConnection>>)> = {
            let state = self.state.read().await;
            state
                .active
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };

        let mut session_ids: Vec<(String, String)> = Vec::new();
        for (key, conn) in snapshot {
            let conn = conn.lock().await;
            if let Some(sid) = conn.acp_session_id.clone() {
                session_ids.push((key, sid));
            }
        }

        let mut state = self.state.write().await;
        for (key, sid) in session_ids {
            state.persisted.insert(key.clone(), sid.clone());
            state.suspended.insert(key, sid);
        }
        self.save_mapping(&state.persisted);
        let count = state.active.len();
        state.active.clear();
        state.cancel_handles.clear();
        state.activity.clear();
        state.pgids.clear();
        info!(count, "pool shutdown complete");
    }
}

fn attach_external_session_state(
    state: &mut PoolState,
    thread_id: &str,
    session_id: &str,
    working_dir: &str,
) -> Result<()> {
    if has_session_state(state, thread_id) {
        let same_mapping = state.persisted.get(thread_id).map(String::as_str) == Some(session_id)
            && state.session_workdirs.get(thread_id).map(String::as_str) == Some(working_dir);
        if same_mapping {
            return Ok(());
        }
        return Err(anyhow!("Discord thread already has a session"));
    }
    if state
        .persisted
        .iter()
        .any(|(key, current)| key != thread_id && current == session_id)
    {
        return Err(anyhow!("Cursor chat is already attached to another thread"));
    }
    state
        .persisted
        .insert(thread_id.to_string(), session_id.to_string());
    state
        .suspended
        .insert(thread_id.to_string(), session_id.to_string());
    state
        .session_workdirs
        .insert(thread_id.to_string(), working_dir.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        attach_external_session_state, better_candidate, classify_hung, classify_idle,
        get_or_insert_gate, has_session_state, purge_session_entries, remove_if_same_handle,
        session_state, PoolState, SessionPool, SessionState,
    };
    use crate::acp::connection::SessionActivity;
    use crate::config::AgentConfig;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;
    use tokio::time::Instant;

    /// Registrar double that records every mint, so a test can assert one never happened.
    #[cfg(feature = "acp-mcp")]
    #[derive(Default)]
    struct CountingRegistrar {
        minted: std::sync::Mutex<Vec<String>>,
        revoked: std::sync::Mutex<Vec<String>>,
    }

    #[cfg(feature = "acp-mcp")]
    impl CountingRegistrar {
        fn revoked(&self) -> Vec<String> {
            self.revoked.lock().unwrap().clone()
        }
    }

    #[cfg(feature = "acp-mcp")]
    impl crate::acp_mcp::SessionTokenRegistrar for CountingRegistrar {
        fn mint(&self, channel_id: &str) -> String {
            self.minted.lock().unwrap().push(channel_id.to_string());
            "token-xyz".to_string()
        }
        fn revoke(&self, token: &str) {
            self.revoked.lock().unwrap().push(token.to_string());
        }
    }

    /// Build an empty `PoolState` for a helper-level test.
    fn empty_pool_state() -> super::PoolState {
        super::PoolState {
            active: HashMap::new(),
            cancel_handles: HashMap::new(),
            #[cfg(feature = "acp-mcp")]
            facade_tokens: HashMap::new(),
            activity: HashMap::new(),
            pgids: HashMap::new(),
            suspended: HashMap::new(),
            persisted: HashMap::new(),
            creating: HashMap::new(),
            session_workdirs: HashMap::new(),
            reset_generations: HashMap::new(),
        }
    }

    #[test]
    fn session_state_reports_suspended_persisted_and_none() {
        let mut state = empty_pool_state();
        state
            .persisted
            .insert("persisted".into(), "session-p".into());
        state
            .suspended
            .insert("suspended".into(), "session-s".into());
        state
            .persisted
            .insert("suspended".into(), "session-s".into());

        assert_eq!(session_state(&state, "suspended"), SessionState::Suspended);
        assert_eq!(session_state(&state, "persisted"), SessionState::Persisted);
        assert_eq!(session_state(&state, "missing"), SessionState::None);
    }

    #[test]
    fn workspace_metadata_counts_as_closable_session_state() {
        let mut state = empty_pool_state();
        state
            .session_workdirs
            .insert("metadata-only".into(), "/tmp/repo".into());

        assert!(has_session_state(&state, "metadata-only"));
        assert!(!has_session_state(&state, "missing"));
    }

    #[test]
    fn external_session_attach_is_idempotent_and_unique() {
        let mut state = empty_pool_state();
        attach_external_session_state(&mut state, "discord:1", "chat-a", "/tmp/repo").unwrap();
        attach_external_session_state(&mut state, "discord:1", "chat-a", "/tmp/repo").unwrap();

        assert_eq!(
            state.persisted.get("discord:1").map(String::as_str),
            Some("chat-a")
        );
        assert_eq!(
            state.suspended.get("discord:1").map(String::as_str),
            Some("chat-a")
        );
        assert_eq!(
            state.session_workdirs.get("discord:1").map(String::as_str),
            Some("/tmp/repo")
        );
        assert!(
            attach_external_session_state(&mut state, "discord:2", "chat-a", "/tmp/repo").is_err()
        );
        assert!(
            attach_external_session_state(&mut state, "discord:1", "chat-b", "/tmp/repo").is_err()
        );
    }

    /// F3: replacing a hung predecessor's token revokes the predecessor's EXACT token and leaves
    /// the successor's standing. Without the revoke the predecessor token keeps resolving to the
    /// channel and — since `AcpTunnelSource` authorizes by channel — could reach the successor's
    /// tunnel. Exercises the production `install_facade_token`.
    #[cfg(feature = "acp-mcp")]
    #[test]
    fn installing_a_successor_token_revokes_only_the_superseded_predecessor() {
        let reg = Arc::new(CountingRegistrar::default());
        let registrar: Arc<dyn crate::acp_mcp::SessionTokenRegistrar> = reg.clone();
        let mut state = empty_pool_state();

        // Predecessor registers, then a successor takes over the SAME key.
        super::install_facade_token(
            &mut state,
            "discord:acp_x",
            "T_pred".into(),
            Some(&registrar),
        );
        assert!(
            reg.revoked().is_empty(),
            "nothing to revoke on the first install"
        );
        super::install_facade_token(
            &mut state,
            "discord:acp_x",
            "T_succ".into(),
            Some(&registrar),
        );

        assert_eq!(
            reg.revoked(),
            vec!["T_pred"],
            "the predecessor token must be revoked"
        );
        assert_eq!(
            state.facade_tokens.get("discord:acp_x").map(String::as_str),
            Some("T_succ"),
            "the successor's token stands"
        );
    }

    /// F3: hung eviction revokes the exact facade token synchronously (the DropGuard cannot fire
    /// while the hung task holds an Arc). Exercises the production `revoke_facade_token_for_key`,
    /// which the hung-eviction loop calls after `apply_hung_eviction`.
    #[cfg(feature = "acp-mcp")]
    #[test]
    fn hung_eviction_revokes_the_exact_facade_token_and_forgets_it() {
        let reg = Arc::new(CountingRegistrar::default());
        let registrar: Arc<dyn crate::acp_mcp::SessionTokenRegistrar> = reg.clone();
        let mut state = empty_pool_state();
        state
            .facade_tokens
            .insert("discord:acp_x".into(), "T_hung".into());
        // A different session's token must be untouched.
        state
            .facade_tokens
            .insert("discord:acp_y".into(), "T_other".into());

        super::revoke_facade_token_for_key(&mut state, "discord:acp_x", Some(&registrar));

        assert_eq!(
            reg.revoked(),
            vec!["T_hung"],
            "only the evicted session's token is revoked"
        );
        assert!(
            !state.facade_tokens.contains_key("discord:acp_x"),
            "and it is forgotten"
        );
        assert_eq!(
            state.facade_tokens.get("discord:acp_y").map(String::as_str),
            Some("T_other"),
            "an unrelated session's token is untouched"
        );
    }

    /// A failed facade config write must not mint a token. The agent has no `openab` entry, so it
    /// can never present one; minting anyway would leave a live credential registered for a
    /// session that cannot use it until eviction.
    #[cfg(feature = "acp-mcp")]
    #[tokio::test]
    async fn no_token_is_minted_when_the_facade_config_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        // Make `<workdir>/.openab` a FILE, so `create_dir_all` inside the writer fails.
        //
        // This used to block on `.cursor`, which openab no longer creates: since D-15 it authors
        // only `.openab/mcp-facade.json` and never touches a vendor directory. Left pointing at
        // `.cursor` the write would SUCCEED, the test would fail, and — worse if it had been
        // written the other way round — a test asserting "no mint on failure" would have been
        // passing against a call that never failed.
        std::fs::write(dir.path().join(".openab"), b"not a directory").unwrap();

        let counting = Arc::new(CountingRegistrar::default());
        let registrar: Arc<dyn crate::acp_mcp::SessionTokenRegistrar> = counting.clone();
        let token = super::setup_facade_session(
            dir.path().to_str().unwrap(),
            "http://127.0.0.1:8848/mcp",
            "acp_x",
            &registrar,
        )
        .await;

        assert!(token.is_none(), "a failed config write must yield no token");
        assert!(
            counting.minted.lock().unwrap().is_empty(),
            "the registrar must never be asked to mint when the config could not be written"
        );
    }

    /// The happy path still mints exactly once, for the right channel.
    #[cfg(feature = "acp-mcp")]
    #[tokio::test]
    async fn a_successful_facade_config_write_mints_one_token() {
        let dir = tempfile::tempdir().unwrap();
        let counting = Arc::new(CountingRegistrar::default());
        let registrar: Arc<dyn crate::acp_mcp::SessionTokenRegistrar> = counting.clone();
        let token = super::setup_facade_session(
            dir.path().to_str().unwrap(),
            "http://127.0.0.1:8848/mcp",
            "acp_x",
            &registrar,
        )
        .await;

        assert_eq!(token.as_deref(), Some("token-xyz"));
        assert_eq!(counting.minted.lock().unwrap().as_slice(), ["acp_x"]);
    }

    #[test]
    fn remove_if_same_handle_removes_matching_entry() {
        let expected = Arc::new(Mutex::new(1_u8));
        let mut map = HashMap::from([("thread".to_string(), Arc::clone(&expected))]);

        let removed = remove_if_same_handle(&mut map, "thread", &expected);

        assert!(removed.is_some());
        assert!(map.is_empty());
    }

    #[test]
    fn remove_if_same_handle_keeps_replaced_entry() {
        let stale = Arc::new(Mutex::new(1_u8));
        let fresh = Arc::new(Mutex::new(2_u8));
        let mut map = HashMap::from([("thread".to_string(), Arc::clone(&fresh))]);

        let removed = remove_if_same_handle(&mut map, "thread", &stale);

        assert!(removed.is_none());
        let current = map.get("thread").expect("entry should remain");
        assert!(Arc::ptr_eq(current, &fresh));
    }

    #[test]
    fn get_or_insert_gate_reuses_gate_for_same_thread() {
        let mut map = HashMap::new();

        let first = get_or_insert_gate(&mut map, "thread");
        let second = get_or_insert_gate(&mut map, "thread");

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(map.len(), 1);
    }

    /// `get_or_create` holds a per-thread gate from its first line until it has
    /// inserted the finished connection — across `spawn`, `initialize` and
    /// `session/load`. That is seconds of real time, and exactly the window in
    /// which someone reaches for `/reset` because the session feels stuck.
    ///
    /// `detach_session` and `attach_external_session` take that gate.
    /// `reset_session` deliberately does not, and this pins that: parking a
    /// reset behind a `session/new` that can take two minutes would break the
    /// one case it exists for, a session that is already stuck.
    ///
    /// Staying ungated means the creation has to notice on its own, which is
    /// what `reset_generations` is for — see the test below. Before that
    /// counter existed, a reset landing mid-creation was silently undone, and
    /// when the creation was resuming rather than creating, the thread came
    /// back holding the very history the reset was meant to discard.
    ///
    /// Both calls run under identical conditions here, so the difference
    /// between them is the code's and not this test's setup.
    #[tokio::test]
    async fn reset_session_is_not_serialised_against_session_creation() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SessionPool::new_in(dir.path(), AgentConfig::default(), 4, 600, HashMap::new());
        const KEY: &str = "discord:T1";

        {
            let mut state = pool.state.write().await;
            state
                .persisted
                .insert(KEY.to_string(), "sess_old".to_string());
            state
                .suspended
                .insert(KEY.to_string(), "sess_old".to_string());
        }

        // Stands in for an in-flight `get_or_create`, which owns this gate for
        // the whole of its slow section.
        let gate = {
            let mut state = pool.state.write().await;
            get_or_insert_gate(&mut state.creating, KEY)
        };
        let creating = gate.lock().await;

        // Ungated: runs to completion while a creation is in flight.
        tokio::time::timeout(Duration::from_secs(2), pool.reset_session(KEY))
            .await
            .expect(
                "reset_session waited for the creating gate — the gap is closed, update this test",
            )
            .expect("reset_session should clear an existing session");
        assert_eq!(pool.session_snapshot(KEY).await.state, SessionState::None);

        // Gated, under exactly the same conditions — so the difference above
        // belongs to the code and not to this test's setup.
        let detached =
            tokio::time::timeout(Duration::from_millis(300), pool.detach_session(KEY)).await;
        assert!(
            detached.is_err(),
            "detach_session returned without waiting for the gate; the contrast this test relies on is invalid"
        );

        drop(creating);
    }

    /// The other half of the same problem. `reset_session` stays ungated, so a
    /// creation already in flight has to detect the reset itself: it captures
    /// this counter before its slow section and compares again under the final
    /// write lock, standing down if it moved.
    #[tokio::test]
    async fn reset_bumps_the_generation_an_in_flight_creation_compares_against() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SessionPool::new_in(dir.path(), AgentConfig::default(), 4, 600, HashMap::new());
        const KEY: &str = "discord:T1";

        // An untouched thread has no entry, which a creation reads as 0.
        assert_eq!(
            pool.state.read().await.reset_generations.get(KEY).copied(),
            None
        );

        for expected in 1..=2u64 {
            {
                let mut state = pool.state.write().await;
                state
                    .persisted
                    .insert(KEY.to_string(), "sess_old".to_string());
                state
                    .suspended
                    .insert(KEY.to_string(), "sess_old".to_string());
            }
            pool.reset_session(KEY).await.expect("reset should succeed");
            assert_eq!(
                pool.state.read().await.reset_generations.get(KEY).copied(),
                Some(expected),
                "every reset must move the counter, or the second one goes unnoticed"
            );
        }

        // Per thread: a reset here cannot cancel a creation running elsewhere.
        assert_eq!(
            pool.state
                .read()
                .await
                .reset_generations
                .get("discord:T2")
                .copied(),
            None
        );
    }

    #[test]
    fn classify_idle_marks_stale_by_time() {
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs(60);
        let last_active = now - std::time::Duration::from_secs(120);
        assert!(classify_idle(last_active, true, cutoff));
    }

    #[test]
    fn classify_idle_marks_stale_by_death() {
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs(60);
        assert!(classify_idle(now, false, cutoff));
    }

    #[test]
    fn classify_idle_keeps_fresh_alive_sessions() {
        let now = Instant::now();
        let cutoff = now - std::time::Duration::from_secs(60);
        assert!(!classify_idle(now, true, cutoff));
    }

    #[test]
    fn better_candidate_prefers_empty_current() {
        assert!(better_candidate(None, Instant::now()));
    }

    #[test]
    fn better_candidate_prefers_older_last_active() {
        let older = Instant::now() - std::time::Duration::from_secs(120);
        let newer = Instant::now() - std::time::Duration::from_secs(30);
        assert!(better_candidate(Some(newer), older));
    }

    #[test]
    fn better_candidate_rejects_newer_last_active() {
        let older = Instant::now() - std::time::Duration::from_secs(120);
        let newer = Instant::now() - std::time::Duration::from_secs(30);
        assert!(!better_candidate(Some(older), newer));
    }

    #[test]
    fn classify_hung_detects_in_flight_session_past_threshold() {
        assert!(classify_hung(
            true,
            std::time::Duration::from_secs(200),
            std::time::Duration::from_secs(120),
        ));
    }

    #[test]
    fn classify_hung_ignores_in_flight_session_within_threshold() {
        assert!(!classify_hung(
            true,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(120),
        ));
    }

    #[test]
    fn classify_hung_never_marks_idle_sessions() {
        assert!(!classify_hung(
            false,
            std::time::Duration::from_secs(200),
            std::time::Duration::from_secs(120),
        ));
    }

    #[test]
    fn better_candidate_keeps_existing_on_equal_last_active() {
        let ts = Instant::now() - std::time::Duration::from_secs(60);
        assert!(!better_candidate(Some(ts), ts));
    }

    /// The force-evict warning must log NEITHER id raw — both the `acp_<uuid>` channel (inside the
    /// `<platform>:<channel_id>` pool key) and the `sess_<uuid>` session id resume the session. A
    /// capture subscriber exercises the real `warn!` macro, so a revert to raw fields fails here
    /// rather than silently shipping a credential to the logs (F6 / round 6).
    #[test]
    fn force_evict_warning_redacts_both_ids() {
        use std::io::Write;
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        #[derive(Clone)]
        struct Cap(StdArc<StdMutex<Vec<u8>>>);
        impl Write for Cap {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let uuid = "00000000-0000-0000-0000-000000000000";
        let buf = StdArc::new(StdMutex::new(Vec::new()));
        let cap = Cap(buf.clone());
        let sub = tracing_subscriber::fmt()
            .with_writer(move || cap.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(sub, || {
            super::warn_force_evicting_hung(
                &format!("discord:acp_{uuid}"),
                Some(&format!("sess_{uuid}")),
                999,
                600,
            );
        });

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("force-evicting hung session"),
            "the warning must fire: {out}"
        );
        assert!(!out.contains(uuid), "no raw uuid may reach the log: {out}");
        assert!(
            !out.contains("acp_") && !out.contains("sess_"),
            "no raw id prefix either: {out}"
        );
        assert!(
            out.contains('#'),
            "the redaction tag must be present: {out}"
        );
        assert!(
            out.contains("discord"),
            "the readable platform half must survive: {out}"
        );
    }

    #[test]
    fn purge_session_entries_drops_all_entries_for_evicted_key_only() {
        let mut state = PoolState {
            active: HashMap::new(),
            cancel_handles: HashMap::new(),
            #[cfg(feature = "acp-mcp")]
            facade_tokens: HashMap::new(),
            activity: HashMap::from([
                ("hung".to_string(), Arc::new(SessionActivity::new())),
                ("other".to_string(), Arc::new(SessionActivity::new())),
            ]),
            pgids: HashMap::from([("hung".to_string(), 1234), ("other".to_string(), 5678)]),
            suspended: HashMap::from([
                ("hung".to_string(), "session-hung".to_string()),
                ("other".to_string(), "session-other".to_string()),
            ]),
            persisted: HashMap::from([
                ("hung".to_string(), "session-hung".to_string()),
                ("other".to_string(), "session-other".to_string()),
            ]),
            creating: HashMap::from([("hung".to_string(), Arc::new(Mutex::new(())))]),
            session_workdirs: HashMap::from([("hung".to_string(), "/tmp/ws".to_string())]),
            reset_generations: HashMap::from([("hung".to_string(), 7)]),
        };

        purge_session_entries(&mut state, "hung");

        // Evicted key must not be resumable: no suspended/persisted entry left.
        assert!(!state.activity.contains_key("hung"));
        assert!(!state.cancel_handles.contains_key("hung"));
        assert!(!state.pgids.contains_key("hung"));
        assert!(!state.suspended.contains_key("hung"));
        assert!(!state.persisted.contains_key("hung"));
        assert!(!state.session_workdirs.contains_key("hung"));
        // The creating gate is concurrency control, not session state: it must
        // survive so an in-flight get_or_create holder stays serialized. The
        // reset generation is the same: dropping it would reset the counter an
        // in-flight creation is comparing itself against, and that creation
        // would then happily undo this eviction.
        assert!(state.creating.contains_key("hung"));
        assert_eq!(state.reset_generations.get("hung"), Some(&7));
        assert_eq!(state.pgids.get("other"), Some(&5678));
        // Other keys survive untouched.
        assert_eq!(
            state.persisted.get("other"),
            Some(&"session-other".to_string())
        );
        assert_eq!(
            state.suspended.get("other"),
            Some(&"session-other".to_string())
        );
        assert!(state.activity.contains_key("other"));
    }

    #[test]
    fn persisted_mapping_can_include_active_and_suspended_sessions() {
        let persisted = HashMap::from([
            ("active-thread".to_string(), "session-active".to_string()),
            (
                "suspended-thread".to_string(),
                "session-suspended".to_string(),
            ),
        ]);

        let serialized =
            serde_json::to_string_pretty(&persisted).expect("serialize persisted mapping");
        let roundtrip: HashMap<String, String> =
            serde_json::from_str(&serialized).expect("deserialize persisted mapping");

        assert_eq!(
            roundtrip.get("active-thread"),
            Some(&"session-active".to_string())
        );
        assert_eq!(
            roundtrip.get("suspended-thread"),
            Some(&"session-suspended".to_string())
        );
    }
}
