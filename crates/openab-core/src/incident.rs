//! Structured failure records, and the policy deciding whether one is worth
//! waking an agent for.
//!
//! Platform-agnostic on purpose. `adapter.rs` is shared with Slack, so the
//! emission side only ever writes a record; opening a Discord thread and
//! prompting an agent lives in the Discord layer.
//!
//! Records land in a spool directory rather than only an in-process channel so
//! that two things work: failures survive a restart, and the knowledge bot —
//! which has no repository access and therefore never runs triage itself — can
//! hand its failures to the coding bot through a shared mount.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config::DiscordIncidentTriageConfig;
use crate::error_display::{error_class, ErrorClass};

pub const INCIDENT_SCHEMA: &str = "openab.incident.v1";

/// Longest `detail` we keep. Enough for a stack-ish error body, short enough
/// that a pathological agent response cannot fill the disk or the prompt.
const MAX_DETAIL_CHARS: usize = 4000;
const MAX_SUMMARY_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentKind {
    /// The agent answered `session/prompt` with a JSON-RPC error.
    AgentTurnError,
    /// The agent process exited or stopped responding mid-turn.
    AgentDied,
    /// The turn passed the hard timeout and was abandoned.
    PromptTimeout,
    /// The session could not be created at all.
    SessionCreateFailed,
    /// A structured payload the model emitted failed schema validation.
    PayloadRejected,
    /// A scheduled job could not run, or errored while running.
    CronFailure,
    /// Reporting telemetry to the Admin Bot failed.
    TelemetryFailed,
}

impl IncidentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentTurnError => "agent_turn_error",
            Self::AgentDied => "agent_died",
            Self::PromptTimeout => "prompt_timeout",
            Self::SessionCreateFailed => "session_create_failed",
            Self::PayloadRejected => "payload_rejected",
            Self::CronFailure => "cron_failure",
            Self::TelemetryFailed => "telemetry_failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "agent_turn_error" => Some(Self::AgentTurnError),
            "agent_died" => Some(Self::AgentDied),
            "prompt_timeout" => Some(Self::PromptTimeout),
            "session_create_failed" => Some(Self::SessionCreateFailed),
            "payload_rejected" => Some(Self::PayloadRejected),
            "cron_failure" => Some(Self::CronFailure),
            "telemetry_failed" => Some(Self::TelemetryFailed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    #[serde(default = "default_schema")]
    pub schema: String,
    pub id: String,
    /// Which runtime recorded this — `coding` or `knowledge`.
    pub source_id: String,
    pub kind: IncidentKind,
    pub detected_at: DateTime<Utc>,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub workspace_alias: Option<String>,
    /// One redacted line naming what failed.
    pub summary: String,
    /// The raw error, including the `error.data` payload that used to be
    /// dropped on the floor at the ACP boundary.
    #[serde(default)]
    pub detail: String,
    /// Cleared when the failure came out of a triage session itself, so a
    /// diagnosis that fails can never ask for a diagnosis of the diagnosis.
    #[serde(default = "default_true")]
    pub triage_eligible: bool,
}

fn default_schema() -> String {
    INCIDENT_SCHEMA.to_string()
}

fn default_true() -> bool {
    true
}

impl Incident {
    /// `source_id` is left blank deliberately: the sink stamps it on the way
    /// out, so no call site has to know or repeat which runtime it is in.
    pub fn new(
        kind: IncidentKind,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let detected_at = Utc::now();
        Self {
            schema: INCIDENT_SCHEMA.to_string(),
            id: format!(
                "{}-{}",
                detected_at.timestamp_millis(),
                uuid::Uuid::new_v4().simple()
            ),
            source_id: String::new(),
            kind,
            detected_at,
            platform: String::new(),
            channel_id: None,
            thread_id: None,
            workspace_alias: None,
            summary: truncate_chars(&crate::redact::redact_session_ids(&summary.into()), MAX_SUMMARY_CHARS),
            detail: truncate_chars(&crate::redact::redact_session_ids(&detail.into()), MAX_DETAIL_CHARS),
            triage_eligible: true,
        }
    }

    pub fn with_location(
        mut self,
        platform: impl Into<String>,
        channel_id: Option<String>,
        thread_id: Option<String>,
    ) -> Self {
        self.platform = platform.into();
        self.channel_id = channel_id;
        self.thread_id = thread_id;
        self
    }

    pub fn with_workspace_alias(mut self, alias: Option<String>) -> Self {
        self.workspace_alias = alias.filter(|value| !value.trim().is_empty());
        self
    }

    /// Stable across restarts and across both containers: no timestamps, no
    /// session ids, no varying numbers. Two occurrences of the same underlying
    /// failure collapse onto one key so the cooldown actually holds.
    pub fn dedup_key(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.kind.as_str().as_bytes());
        hasher.update([0u8]);
        hasher.update(self.workspace_alias.as_deref().unwrap_or("").as_bytes());
        hasher.update([0u8]);
        hasher.update(normalize_detail(&self.summary).as_bytes());
        let digest = hasher.finalize();
        digest[..8].iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// Collapse the parts of an error string that vary between otherwise identical
/// failures, so they hash to one dedup key.
pub fn normalize_detail(raw: &str) -> String {
    let redacted = crate::redact::redact_session_ids(raw);
    let mut out = String::with_capacity(redacted.len());
    let mut digits = false;
    for ch in redacted.chars() {
        if ch.is_ascii_digit() {
            if !digits {
                out.push('#');
                digits = true;
            }
            continue;
        }
        digits = false;
        out.push(ch.to_ascii_lowercase());
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    trimmed.chars().take(limit).collect::<String>() + "…"
}

// ─── Spool ────────────────────────────────────────────────────────────────────

/// Records a failure to the shared spool and, when the consumer lives in this
/// same process, nudges it immediately instead of waiting for the next sweep.
pub struct IncidentSink {
    spool: PathBuf,
    source_id: String,
    tx: Option<tokio::sync::mpsc::UnboundedSender<Incident>>,
}

impl IncidentSink {
    pub fn new(spool: impl Into<PathBuf>, source_id: impl Into<String>) -> Self {
        Self {
            spool: spool.into(),
            source_id: source_id.into(),
            tx: None,
        }
    }

    pub fn with_channel(mut self, tx: tokio::sync::mpsc::UnboundedSender<Incident>) -> Self {
        self.tx = Some(tx);
        self
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn spool(&self) -> &Path {
        &self.spool
    }

    /// Never fails the caller: a failure to record a failure must not take down
    /// the path that was already handling an error.
    pub fn record(&self, incident: Incident) {
        let mut incident = incident;
        incident.source_id = self.source_id.clone();
        if let Err(error) = write_incident(&self.spool, &incident) {
            warn!(%error, id = %incident.id, "failed to write incident to the spool");
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(incident);
        }
    }
}

pub fn inbox_dir(spool: &Path) -> PathBuf {
    spool.join("inbox")
}

pub fn claimed_dir(spool: &Path) -> PathBuf {
    spool.join("claimed")
}

pub fn done_dir(spool: &Path) -> PathBuf {
    spool.join("done")
}

fn write_incident(spool: &Path, incident: &Incident) -> std::io::Result<()> {
    let inbox = inbox_dir(spool);
    std::fs::create_dir_all(&inbox)?;
    let path = inbox.join(format!("{}.json", incident.id));
    write_json_0600(&path, incident)
}

pub(crate) fn write_json_0600<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let data = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
}

/// Take ownership of one spooled incident.
///
/// The move is a same-filesystem rename, which is atomic, so when both runtimes
/// share the mount exactly one of them can ever claim a given record.
pub fn claim_next(spool: &Path, claimer: &str) -> Option<Incident> {
    let inbox = inbox_dir(spool);
    let claimed = claimed_dir(spool);
    if std::fs::create_dir_all(&claimed).is_err() {
        return None;
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&inbox)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    entries.sort();
    for path in entries {
        let file_name = path.file_name()?.to_string_lossy().to_string();
        let target = claimed.join(format!("{claimer}-{file_name}"));
        if std::fs::rename(&path, &target).is_err() {
            // Lost the race to the other runtime, or the file vanished.
            continue;
        }
        match std::fs::read_to_string(&target)
            .ok()
            .and_then(|body| serde_json::from_str::<Incident>(&body).ok())
        {
            Some(incident) => return Some(incident),
            None => {
                warn!(path = %target.display(), "discarding unreadable incident record");
                let _ = std::fs::remove_file(&target);
            }
        }
    }
    None
}

/// Retire a record so the spool does not grow without bound.
///
/// Handles both routes in: a record taken by [`claim_next`] sits in `claimed/`,
/// while one delivered over the in-process channel was never claimed and is
/// still in `inbox/`. Retiring both means an in-process delivery is not picked
/// up a second time by the next sweep.
pub fn mark_done(spool: &Path, claimer: &str, incident_id: &str) {
    let done = done_dir(spool);
    if std::fs::create_dir_all(&done).is_err() {
        return;
    }
    let target = done.join(format!("{claimer}-{incident_id}.json"));
    let claimed = claimed_dir(spool).join(format!("{claimer}-{incident_id}.json"));
    if std::fs::rename(&claimed, &target).is_ok() {
        return;
    }
    let _ = std::fs::rename(inbox_dir(spool).join(format!("{incident_id}.json")), &target);
}

// ─── Triage policy ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageDecision {
    /// Open a diagnostic session.
    Fire,
    /// Post a plain notification instead — a session would be pointless or unsafe.
    NotifyOnly(SkipReason),
    /// Record only; say nothing.
    Skip(SkipReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Disabled,
    KindFiltered,
    ReEntrant,
    Cooldown { remaining_secs: i64 },
    DailyCap,
    NotifyOnlyMode,
    /// The agent runtime itself is broken, so a diagnostic session would just
    /// fail the same way the thing it was sent to diagnose did.
    RuntimeDead,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::KindFiltered => "kind_filtered",
            Self::ReEntrant => "re_entrant",
            Self::Cooldown { .. } => "cooldown",
            Self::DailyCap => "daily_cap",
            Self::NotifyOnlyMode => "notify_only",
            Self::RuntimeDead => "runtime_dead",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriageState {
    /// `YYYY-MM-DD` in UTC that `fired_today` refers to.
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub fired_today: u32,
    /// dedup key → unix seconds of the last session opened for it.
    #[serde(default)]
    pub cooldowns: HashMap<String, i64>,
    /// Threads hosting a diagnostic session. Failures inside these never
    /// trigger another one.
    #[serde(default)]
    pub triage_threads: HashSet<String>,
}

impl TriageState {
    pub fn fired_today_on(&self, now: DateTime<Utc>) -> u32 {
        if self.day == day_key(now) {
            self.fired_today
        } else {
            0
        }
    }

    pub fn record_fired(&mut self, dedup_key: &str, thread_id: Option<&str>, now: DateTime<Utc>) {
        let today = day_key(now);
        if self.day != today {
            self.day = today;
            self.fired_today = 0;
        }
        self.fired_today = self.fired_today.saturating_add(1);
        self.cooldowns
            .insert(dedup_key.to_string(), now.timestamp());
        if let Some(thread_id) = thread_id {
            self.triage_threads.insert(thread_id.to_string());
        }
    }

    /// Keep the cooldown map from growing forever; anything past its window is
    /// no longer load-bearing.
    pub fn prune(&mut self, cooldown_secs: u64, now: DateTime<Utc>) {
        let cutoff = now.timestamp() - cooldown_secs as i64;
        self.cooldowns.retain(|_, last| *last >= cutoff);
    }
}

pub fn day_key(now: DateTime<Utc>) -> String {
    now.format("%Y-%m-%d").to_string()
}

/// Pure policy: everything that decides whether a failure earns a session.
pub fn decide_triage(
    config: &DiscordIncidentTriageConfig,
    state: &TriageState,
    incident: &Incident,
    now: DateTime<Utc>,
) -> TriageDecision {
    if !config.enabled {
        return TriageDecision::Skip(SkipReason::Disabled);
    }
    if !incident.triage_eligible {
        return TriageDecision::Skip(SkipReason::ReEntrant);
    }
    if let Some(thread_id) = &incident.thread_id {
        if state.triage_threads.contains(thread_id) {
            return TriageDecision::Skip(SkipReason::ReEntrant);
        }
    }
    if !config
        .kinds
        .iter()
        .any(|kind| IncidentKind::parse(kind) == Some(incident.kind))
    {
        return TriageDecision::Skip(SkipReason::KindFiltered);
    }
    if let Some(last) = state.cooldowns.get(&incident.dedup_key()) {
        let elapsed = now.timestamp() - last;
        if elapsed < config.cooldown_secs as i64 {
            return TriageDecision::Skip(SkipReason::Cooldown {
                remaining_secs: config.cooldown_secs as i64 - elapsed,
            });
        }
    }
    if state.fired_today_on(now) >= config.max_per_day {
        return TriageDecision::Skip(SkipReason::DailyCap);
    }
    if config.notify_only {
        return TriageDecision::NotifyOnly(SkipReason::NotifyOnlyMode);
    }
    if runtime_is_dead(incident) {
        return TriageDecision::NotifyOnly(SkipReason::RuntimeDead);
    }
    TriageDecision::Fire
}

/// A failure the agent runtime cannot itself recover from — an expired token, a
/// missing binary, a dead process. Sending a diagnostic prompt into that would
/// just produce a second identical failure.
pub fn runtime_is_dead(incident: &Incident) -> bool {
    if matches!(incident.kind, IncidentKind::AgentDied) {
        return true;
    }
    matches!(
        error_class(&format!("{} {}", incident.summary, incident.detail)),
        ErrorClass::AuthExpired | ErrorClass::AgentMissing | ErrorClass::ProcessDead
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DiscordIncidentTriageConfig {
        DiscordIncidentTriageConfig {
            enabled: true,
            channel_id: "123".into(),
            spool_dir: "/tmp/does-not-matter".into(),
            kinds: vec!["agent_turn_error".into(), "payload_rejected".into()],
            cooldown_secs: 3600,
            max_per_day: 6,
            notify_only: false,
        }
    }

    fn incident(kind: IncidentKind, summary: &str) -> Incident {
        Incident::new(kind, summary, "detail body")
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-19T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn dedup_key_ignores_timestamps_session_ids_and_counters() {
        let first = incident(
            IncidentKind::AgentTurnError,
            "prompt failed after 42 seconds for request 8891",
        );
        let second = incident(
            IncidentKind::AgentTurnError,
            "prompt failed after 7 seconds for request 10233",
        );
        assert_eq!(first.dedup_key(), second.dedup_key());
    }

    #[test]
    fn dedup_key_separates_different_kinds_and_workspaces() {
        let base = incident(IncidentKind::AgentTurnError, "same text");
        let other_kind = incident(IncidentKind::PayloadRejected, "same text");
        let other_workspace = incident(IncidentKind::AgentTurnError, "same text")
            .with_workspace_alias(Some("novel-vault".into()));
        assert_ne!(base.dedup_key(), other_kind.dedup_key());
        assert_ne!(base.dedup_key(), other_workspace.dedup_key());
    }

    #[test]
    fn a_failure_inside_a_triage_thread_never_triggers_another_triage() {
        let mut state = TriageState::default();
        state.triage_threads.insert("999".into());
        let from_triage_thread = incident(IncidentKind::AgentTurnError, "boom")
            .with_location("discord", None, Some("999".into()));
        assert_eq!(
            decide_triage(&config(), &state, &from_triage_thread, now()),
            TriageDecision::Skip(SkipReason::ReEntrant)
        );

        // The independent second layer: the sink can also mark a record itself.
        let mut stamped = incident(IncidentKind::AgentTurnError, "boom");
        stamped.triage_eligible = false;
        assert_eq!(
            decide_triage(&config(), &TriageState::default(), &stamped, now()),
            TriageDecision::Skip(SkipReason::ReEntrant)
        );
    }

    #[test]
    fn cooldown_holds_then_releases() {
        let incident = incident(IncidentKind::AgentTurnError, "repeat offender");
        let mut state = TriageState::default();
        state.record_fired(&incident.dedup_key(), None, now());

        let during = now() + chrono::Duration::seconds(600);
        assert!(matches!(
            decide_triage(&config(), &state, &incident, during),
            TriageDecision::Skip(SkipReason::Cooldown { .. })
        ));

        let after = now() + chrono::Duration::seconds(3601);
        assert_eq!(
            decide_triage(&config(), &state, &incident, after),
            TriageDecision::Fire
        );
    }

    #[test]
    fn daily_cap_blocks_and_resets_the_next_day() {
        let incident = incident(IncidentKind::AgentTurnError, "noisy");
        let mut state = TriageState::default();
        state.day = day_key(now());
        state.fired_today = 6;
        assert_eq!(
            decide_triage(&config(), &state, &incident, now()),
            TriageDecision::Skip(SkipReason::DailyCap)
        );

        let tomorrow = now() + chrono::Duration::days(1);
        assert_eq!(
            decide_triage(&config(), &state, &incident, tomorrow),
            TriageDecision::Fire
        );
    }

    #[test]
    fn unlisted_kinds_are_recorded_but_never_fire() {
        let cron = incident(IncidentKind::CronFailure, "channel unresolved");
        assert_eq!(
            decide_triage(&config(), &TriageState::default(), &cron, now()),
            TriageDecision::Skip(SkipReason::KindFiltered)
        );
    }

    #[test]
    fn disabling_the_feature_stops_everything() {
        let mut config = config();
        config.enabled = false;
        let incident = incident(IncidentKind::AgentTurnError, "boom");
        assert_eq!(
            decide_triage(&config, &TriageState::default(), &incident, now()),
            TriageDecision::Skip(SkipReason::Disabled)
        );
    }

    #[test]
    fn a_dead_runtime_degrades_to_a_notification_instead_of_a_doomed_session() {
        // The real incident this feature was built for: an expired Cursor token
        // means the diagnostic session cannot run either.
        let auth = incident(
            IncidentKind::AgentTurnError,
            "**Unauthorized** (code: 401) invalid api key",
        );
        assert_eq!(
            decide_triage(&config(), &TriageState::default(), &auth, now()),
            TriageDecision::NotifyOnly(SkipReason::RuntimeDead)
        );

        let died = incident(IncidentKind::AgentDied, "Agent process died");
        let mut config_with_kind = config();
        config_with_kind.kinds.push("agent_died".into());
        assert_eq!(
            decide_triage(&config_with_kind, &TriageState::default(), &died, now()),
            TriageDecision::NotifyOnly(SkipReason::RuntimeDead)
        );
    }

    #[test]
    fn notify_only_mode_suppresses_sessions_without_hiding_the_failure() {
        let mut config = config();
        config.notify_only = true;
        let incident = incident(IncidentKind::PayloadRejected, "schema mismatch");
        assert_eq!(
            decide_triage(&config, &TriageState::default(), &incident, now()),
            TriageDecision::NotifyOnly(SkipReason::NotifyOnlyMode)
        );
    }

    #[test]
    fn incident_survives_a_serde_round_trip() {
        let original = incident(IncidentKind::PayloadRejected, "bad json")
            .with_location("discord", Some("111".into()), Some("222".into()))
            .with_workspace_alias(Some("openab".into()));
        let encoded = serde_json::to_string(&original).unwrap();
        let decoded: Incident = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.id, original.id);
        assert_eq!(decoded.kind, original.kind);
        assert_eq!(decoded.dedup_key(), original.dedup_key());
        assert_eq!(decoded.thread_id.as_deref(), Some("222"));
    }

    #[test]
    fn only_one_claimer_can_win_a_record() {
        let dir = tempdir();
        let sink = IncidentSink::new(&dir, "knowledge");
        sink.record(incident(IncidentKind::PayloadRejected, "written by knowledge"));

        let first = claim_next(&dir, "coding");
        let second = claim_next(&dir, "coding");
        assert!(first.is_some(), "the only record should be claimable once");
        assert!(second.is_none(), "a claimed record must not be handed out twice");

        let claimed = first.unwrap();
        assert_eq!(claimed.source_id, "knowledge");
        mark_done(&dir, "coding", &claimed.id);
        assert!(done_dir(&dir).join(format!("coding-{}.json", claimed.id)).exists());
    }

    #[test]
    fn an_in_process_delivery_is_retired_without_having_been_claimed() {
        // Records reach the watcher two ways. The in-process channel hands over a
        // copy that was never claimed, so retiring it has to clear the inbox
        // entry too — otherwise the next sweep processes the same failure again.
        let dir = tempdir();
        let sink = IncidentSink::new(&dir, "coding");
        let recorded = incident(IncidentKind::AgentTurnError, "delivered in process");
        sink.record(recorded.clone());

        mark_done(&dir, "coding", &recorded.id);

        assert!(
            claim_next(&dir, "coding").is_none(),
            "a retired record must not be swept up again"
        );
        assert!(done_dir(&dir)
            .join(format!("coding-{}.json", recorded.id))
            .exists());
    }

    #[test]
    fn recording_never_panics_when_the_spool_is_unwritable() {
        // Recording a failure must not itself become a failure.
        let sink = IncidentSink::new("/proc/definitely/not/writable", "coding");
        sink.record(incident(IncidentKind::AgentTurnError, "boom"));
    }

    #[test]
    fn pruning_drops_only_expired_cooldowns() {
        let mut state = TriageState::default();
        state.cooldowns.insert("fresh".into(), now().timestamp());
        state
            .cooldowns
            .insert("stale".into(), now().timestamp() - 7200);
        state.prune(3600, now());
        assert!(state.cooldowns.contains_key("fresh"));
        assert!(!state.cooldowns.contains_key("stale"));
    }

    fn tempdir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("openab-incident-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
