//! Client for the dedicated Discord Admin Bot's private control plane.

use crate::config::DiscordAdminControlConfig;
use anyhow::{Context, Result};
use chrono::{Datelike, Timelike, Weekday};
use reqwest::StatusCode;
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static REPORT_EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
pub const KNOWLEDGE_WEEKLY_MARKER: &str = "OPENAB_KNOWLEDGE_WEEKLY_V1";

#[derive(Clone)]
pub struct DiscordAdminReporter {
    http: reqwest::Client,
    base_url: String,
    token: String,
    source_id: String,
    include_title: bool,
    include_workspace_alias: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeWeeklyAudit {
    pub window_start: String,
    pub window_end: String,
    pub queried_at: String,
    pub sources: Vec<KnowledgeWeeklySource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeWeeklySource {
    pub source_id: String,
    pub title: String,
    pub url: String,
    pub status: KnowledgeWeeklyStatus,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub error: String,
    #[serde(default)]
    pub items: Vec<KnowledgeWeeklyItem>,
}

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeWeeklyStatus {
    Updated,
    NoUpdates,
    Partial,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeWeeklyItem {
    pub page_id: String,
    pub title: String,
    pub url: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionInventoryItem {
    pub thread_id: u64,
    pub state: String,
    pub last_activity_at: String,
    pub queued_messages: usize,
    pub prompt_in_flight: bool,
    pub externally_detached: bool,
    pub title: String,
    pub workspace_alias: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledJobRun {
    pub job_id: String,
    pub run_id: String,
    pub started_at: String,
    pub finished_at: String,
    pub status: String,
    pub metrics: RetentionJobMetrics,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionJobMetrics {
    pub sources_scanned: u32,
    pub items_scanned: u32,
    pub protected_items: u32,
    pub enqueued_items: u32,
    pub pending_items: u32,
    pub trash_due_items: u32,
    pub trashed_items: u32,
    pub failed_items: u32,
}

/// The three fixed scheduled sources whose articles may be mirrored.
const SCHEDULED_ARTICLE_SOURCE_IDS: [&str; 3] = [
    "github_ai_data_weekly",
    "world_stories",
    "weekly_reading_digest",
];
const SCHEDULED_ARTICLE_BATCH_MAX: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledArticle {
    pub source_id: String,
    pub page_id: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub summary: String,
}

#[derive(Serialize)]
struct ScheduledArticlesRequest<'a> {
    source_id: &'a str,
    job_id: &'a str,
    run_id: &'a str,
    articles: &'a [ScheduledArticle],
}

#[derive(Serialize)]
struct ScheduledJobRunRequest<'a> {
    source_id: &'a str,
    job_id: &'a str,
    run_id: &'a str,
    started_at: &'a str,
    finished_at: &'a str,
    status: &'a str,
    metrics: &'a RetentionJobMetrics,
    note: &'a str,
}

#[derive(Serialize)]
struct SessionInventoryRequest {
    source_id: String,
    snapshot_id: String,
    generated_at: String,
    complete: bool,
    error: String,
    sessions: Vec<SessionInventoryItem>,
}

#[derive(Serialize)]
struct KnowledgeWeeklyRequest<'a> {
    reporter_source_id: &'a str,
    window_start: &'a str,
    window_end: &'a str,
    queried_at: &'a str,
    sources: &'a [KnowledgeWeeklySource],
}

#[derive(Serialize)]
struct SessionEventEnvelope<'a> {
    events: [SessionEventRequest<'a>; 1],
}

#[derive(Serialize)]
struct SessionEventRequest<'a> {
    source_id: &'a str,
    event_id: String,
    thread_id: u64,
    event_type: &'a str,
    occurred_at: String,
    title: &'a str,
    workspace_alias: &'a str,
}

impl DiscordAdminReporter {
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn from_env() -> Result<Option<Self>> {
        let token = match std::env::var("DISCORD_ADMIN_REPORT_TOKEN") {
            Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => return Ok(None),
        };
        anyhow::ensure!(
            token.len() >= 32,
            "Discord Admin report token must contain at least 32 characters"
        );
        let source_id = std::env::var("OPENAB_REPORT_SOURCE_ID")
            .or_else(|_| std::env::var("OPENAB_REPORT_SOURCE"))
            .unwrap_or_else(|_| "openab".into());
        anyhow::ensure!(
            !source_id.is_empty()
                && source_id.len() <= 64
                && source_id
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric()
                        || matches!(character, '-' | '_')),
            "OPENAB_REPORT_SOURCE_ID must contain 1-64 ASCII letters, numbers, '-' or '_'"
        );
        let base = std::env::var("DISCORD_ADMIN_REPORT_URL")
            .context("DISCORD_ADMIN_REPORT_URL is required when session reporting is enabled")?;
        validate_service_url(
            &base,
            env_flag("DISCORD_ADMIN_REPORT_ALLOW_INSECURE_HTTP"),
            "DISCORD_ADMIN_REPORT_URL",
        )?;
        Ok(Some(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?,
            base_url: base.trim().trim_end_matches('/').to_string(),
            token,
            source_id,
            include_title: env_flag("OPENAB_REPORT_INCLUDE_TITLE"),
            include_workspace_alias: env_flag("OPENAB_REPORT_INCLUDE_WORKSPACE_ALIAS"),
        }))
    }

    pub async fn report(
        &self,
        thread_id: u64,
        event_type: &str,
        title: &str,
        workspace_alias: &str,
    ) -> Result<()> {
        let occurred_at = chrono::Utc::now().to_rfc3339();
        let sequence = REPORT_EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let body = SessionEventEnvelope {
            events: [SessionEventRequest {
                source_id: &self.source_id,
                event_id: format!("{}:{thread_id}:{event_type}:{sequence}", occurred_at),
                thread_id,
                event_type,
                occurred_at,
                title: if self.include_title { title } else { "" },
                workspace_alias: if self.include_workspace_alias {
                    workspace_alias
                } else {
                    ""
                },
            }],
        };
        let response = self
            .http
            .post(format!("{}/v1/telemetry/session-events", self.base_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "Discord Admin report endpoint returned {}",
            response.status()
        );
        Ok(())
    }

    pub async fn report_knowledge_weekly(&self, audit: &KnowledgeWeeklyAudit) -> Result<()> {
        anyhow::ensure!(
            self.source_id == "knowledge",
            "only the knowledge reporter may submit weekly source audits"
        );
        let body = KnowledgeWeeklyRequest {
            reporter_source_id: &self.source_id,
            window_start: &audit.window_start,
            window_end: &audit.window_end,
            queried_at: &audit.queried_at,
            sources: &audit.sources,
        };
        let response = self
            .http
            .post(format!("{}/v1/telemetry/knowledge-weekly", self.base_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "Discord Admin knowledge report endpoint returned {}",
            response.status()
        );
        Ok(())
    }

    pub async fn report_session_inventory(
        &self,
        mut sessions: Vec<SessionInventoryItem>,
        complete: bool,
        error: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(self.source_id.as_str(), "coding" | "knowledge"),
            "session inventory source must be coding or knowledge"
        );
        anyhow::ensure!(sessions.len() <= 500, "session inventory is too large");
        anyhow::ensure!(
            (complete && error.trim().is_empty()) || (!complete && !error.trim().is_empty()),
            "session inventory completeness and error are inconsistent"
        );
        for session in &mut sessions {
            if !self.include_title {
                session.title.clear();
            }
            if !self.include_workspace_alias {
                session.workspace_alias.clear();
            }
        }
        let generated_at = chrono::Utc::now().to_rfc3339();
        let sequence = REPORT_EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let body = SessionInventoryRequest {
            source_id: self.source_id.clone(),
            snapshot_id: format!("{}:{generated_at}:{sequence}", self.source_id),
            generated_at,
            complete,
            error: error.trim().chars().take(300).collect(),
            sessions,
        };
        let response = self
            .http
            .post(format!("{}/v1/telemetry/session-inventory", self.base_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "Discord Admin session inventory endpoint returned {}",
            response.status()
        );
        Ok(())
    }

    pub async fn report_scheduled_job_run(&self, run: &ScheduledJobRun) -> Result<()> {
        anyhow::ensure!(
            self.source_id == "knowledge",
            "only the knowledge reporter may submit scheduled job runs"
        );
        validate_scheduled_job_run(run)?;
        let body = ScheduledJobRunRequest {
            source_id: &self.source_id,
            job_id: &run.job_id,
            run_id: &run.run_id,
            started_at: &run.started_at,
            finished_at: &run.finished_at,
            status: &run.status,
            metrics: &run.metrics,
            note: &run.note,
        };
        let response = self
            .http
            .post(format!("{}/v1/telemetry/scheduled-job-runs", self.base_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "Discord Admin scheduled job endpoint returned {}",
            response.status()
        );
        Ok(())
    }

    pub async fn report_scheduled_articles(
        &self,
        job_id: &str,
        run_id: &str,
        articles: &[ScheduledArticle],
    ) -> Result<()> {
        anyhow::ensure!(
            self.source_id == "knowledge",
            "only the knowledge reporter may submit scheduled articles"
        );
        validate_scheduled_articles(articles)?;
        let body = ScheduledArticlesRequest {
            source_id: &self.source_id,
            job_id,
            run_id,
            articles,
        };
        let response = self
            .http
            .post(format!("{}/v1/telemetry/scheduled-articles", self.base_url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        anyhow::ensure!(
            response.status().is_success(),
            "Discord Admin scheduled articles endpoint returned {}",
            response.status()
        );
        Ok(())
    }
}

pub fn validate_scheduled_job_run(run: &ScheduledJobRun) -> Result<()> {
    anyhow::ensure!(
        run.job_id == "opencode-scheduled-source-retention",
        "unsupported scheduled job ID"
    );
    anyhow::ensure!(
        !run.run_id.trim().is_empty() && run.run_id.len() <= 160,
        "invalid scheduled job run ID"
    );
    let started = chrono::DateTime::parse_from_rfc3339(&run.started_at)
        .context("invalid scheduled job started_at")?;
    let finished = chrono::DateTime::parse_from_rfc3339(&run.finished_at)
        .context("invalid scheduled job finished_at")?;
    anyhow::ensure!(
        finished >= started && finished.signed_duration_since(started) <= chrono::Duration::hours(6),
        "invalid scheduled job duration"
    );
    anyhow::ensure!(
        matches!(run.status.as_str(), "success" | "partial" | "failed"),
        "invalid scheduled job status"
    );
    anyhow::ensure!(
        run.note.chars().count() <= 300,
        "scheduled job note is too long"
    );
    anyhow::ensure!(
        run.metrics.sources_scanned <= 3,
        "scheduled retention job supports exactly three sources"
    );
    anyhow::ensure!(
        run.status != "success" || run.metrics.sources_scanned == 3,
        "successful retention job must scan all three sources"
    );
    anyhow::ensure!(
        run.status != "success" || run.metrics.failed_items == 0,
        "successful scheduled job cannot contain failed items"
    );
    anyhow::ensure!(
        run.status == "success" || !run.note.trim().is_empty(),
        "partial or failed scheduled job requires a note"
    );
    Ok(())
}

pub fn validate_scheduled_articles(articles: &[ScheduledArticle]) -> Result<()> {
    anyhow::ensure!(!articles.is_empty(), "scheduled article batch is empty");
    anyhow::ensure!(
        articles.len() <= SCHEDULED_ARTICLE_BATCH_MAX,
        "scheduled article batch is too large"
    );
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    for article in articles {
        anyhow::ensure!(
            SCHEDULED_ARTICLE_SOURCE_IDS.contains(&article.source_id.as_str()),
            "scheduled article source_id is not a scheduled source"
        );
        let page_id = article.page_id.trim();
        anyhow::ensure!(
            (8..=64).contains(&page_id.len())
                && page_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "invalid scheduled article page ID"
        );
        anyhow::ensure!(
            seen.insert((article.source_id.as_str(), page_id)),
            "duplicate scheduled article in batch"
        );
        let title = article.title.trim();
        anyhow::ensure!(
            !title.is_empty() && title.chars().count() <= 300,
            "invalid scheduled article title"
        );
        let url = article.url.trim();
        anyhow::ensure!(
            url.starts_with("https://")
                && url.len() <= 1000
                && !url.chars().any(char::is_whitespace),
            "scheduled article URL must be https"
        );
        let published_at = article.published_at.trim();
        if !published_at.is_empty() {
            chrono::DateTime::parse_from_rfc3339(published_at)
                .context("invalid scheduled article published_at")?;
        }
        anyhow::ensure!(
            article.summary.chars().count() <= 1000,
            "scheduled article summary is too long"
        );
    }
    Ok(())
}

pub fn knowledge_weekly_marker_present(content: &str) -> bool {
    knowledge_weekly_payload(content).is_some()
}

pub fn parse_knowledge_weekly_audit(content: &str) -> Result<Option<KnowledgeWeeklyAudit>> {
    let Some(payload) = knowledge_weekly_payload(content) else {
        return Ok(None);
    };
    anyhow::ensure!(
        content.len() <= 128 * 1024,
        "knowledge weekly payload is too large"
    );
    let mut payload = payload.trim();
    if let Some(stripped) = payload.strip_suffix("```") {
        payload = stripped.trim_end();
    }
    let audit: KnowledgeWeeklyAudit =
        serde_json::from_str(payload).context("parse knowledge weekly payload")?;
    validate_knowledge_weekly_audit(&audit)?;
    Ok(Some(audit))
}

fn knowledge_weekly_payload(content: &str) -> Option<&str> {
    let mut offset = 0;
    for line in content.split_inclusive('\n') {
        let marker = line.trim_end_matches(['\r', '\n']).trim();
        if marker == KNOWLEDGE_WEEKLY_MARKER {
            return Some(&content[offset + line.len()..]);
        }
        offset += line.len();
    }
    None
}

fn validate_knowledge_weekly_audit(audit: &KnowledgeWeeklyAudit) -> Result<()> {
    let start = chrono::DateTime::parse_from_rfc3339(&audit.window_start)
        .context("invalid knowledge weekly window_start")?;
    let end = chrono::DateTime::parse_from_rfc3339(&audit.window_end)
        .context("invalid knowledge weekly window_end")?;
    let queried_at = chrono::DateTime::parse_from_rfc3339(&audit.queried_at)
        .context("invalid knowledge weekly queried_at")?;
    let span = end.signed_duration_since(start);
    anyhow::ensure!(
        start.offset().local_minus_utc() == 8 * 60 * 60
            && end.offset().local_minus_utc() == 8 * 60 * 60
            && start.weekday() == Weekday::Tue
            && start.hour() == 0
            && start.minute() == 0
            && start.second() == 0
            && start.nanosecond() == 0
            && span == chrono::Duration::days(7),
        "knowledge weekly window must be Tuesday 00:00 to Tuesday 00:00 in Asia/Taipei"
    );
    anyhow::ensure!(queried_at >= end, "queried_at must not precede window_end");
    anyhow::ensure!(
        audit.sources.len() == 3,
        "knowledge weekly payload must contain exactly three sources"
    );
    let mut source_ids = HashSet::new();
    for source in &audit.sources {
        anyhow::ensure!(
            !source.source_id.is_empty()
                && source.source_id.len() <= 64
                && source
                    .source_id
                    .chars()
                    .all(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_')),
            "invalid knowledge weekly source_id"
        );
        anyhow::ensure!(
            source_ids.insert(source.source_id.as_str()),
            "knowledge weekly source IDs must be unique"
        );
        anyhow::ensure!(
            !source.title.trim().is_empty() && source.title.chars().count() <= 100,
            "invalid knowledge weekly source title"
        );
        validate_public_url(&source.url, "knowledge weekly source URL")?;
        anyhow::ensure!(source.items.len() <= 200, "too many knowledge weekly items");
        match source.status {
            KnowledgeWeeklyStatus::Updated => anyhow::ensure!(
                !source.items.is_empty() && source.error.trim().is_empty(),
                "updated sources require items and no error"
            ),
            KnowledgeWeeklyStatus::NoUpdates => anyhow::ensure!(
                source.items.is_empty() && source.error.trim().is_empty(),
                "no_updates sources cannot contain items or errors"
            ),
            KnowledgeWeeklyStatus::Partial => anyhow::ensure!(
                !source.error.trim().is_empty(),
                "partial sources require an error"
            ),
            KnowledgeWeeklyStatus::Failed => anyhow::ensure!(
                source.items.is_empty() && !source.error.trim().is_empty(),
                "failed sources require an error and no items"
            ),
        }
        anyhow::ensure!(
            source.error.chars().count() <= 300,
            "knowledge weekly error is too long"
        );
        let mut page_ids = HashSet::new();
        for item in &source.items {
            anyhow::ensure!(
                !item.page_id.trim().is_empty() && item.page_id.len() <= 128,
                "invalid knowledge weekly page_id"
            );
            anyhow::ensure!(
                page_ids.insert(item.page_id.as_str()),
                "knowledge weekly page IDs must be unique per source"
            );
            anyhow::ensure!(
                !item.title.trim().is_empty() && item.title.chars().count() <= 200,
                "invalid knowledge weekly item title"
            );
            validate_public_url(&item.url, "knowledge weekly item URL")?;
            let created = chrono::DateTime::parse_from_rfc3339(&item.created_at)
                .context("invalid knowledge weekly item created_at")?;
            anyhow::ensure!(
                created >= start && created < end,
                "knowledge weekly item created_at is outside the report window"
            );
        }
    }
    Ok(())
}

fn validate_public_url(value: &str, name: &str) -> Result<()> {
    let url = reqwest::Url::parse(value.trim()).with_context(|| format!("invalid {name}"))?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "{name} must use http or https"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "{name} must not contain credentials"
    );
    Ok(())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn validate_service_url(value: &str, allow_insecure_http: bool, name: &str) -> Result<()> {
    let url = reqwest::Url::parse(value.trim()).with_context(|| format!("invalid {name}"))?;
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "{name} must not contain credentials"
    );
    anyhow::ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && allow_insecure_http),
        "{name} must use https unless insecure HTTP is explicitly enabled for a trusted private network"
    );
    Ok(())
}

#[derive(Clone)]
pub struct DiscordAdminClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminStatus {
    pub server: AdminIdentity,
    pub bot: AdminIdentity,
    pub permissions: AdminPermissions,
    pub counts: AdminCounts,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminIdentity {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminPermissions {
    pub manage_guild: bool,
    pub manage_channels: bool,
    pub manage_roles: bool,
    pub view_channel: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminCounts {
    pub categories: usize,
    pub text_channels: usize,
    pub forums: usize,
    pub voice_channels: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminInventory {
    pub categories: Vec<AdminCategory>,
    pub uncategorized: Vec<AdminChannel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminCategory {
    pub id: String,
    pub name: String,
    pub position: i64,
    pub channels: Vec<AdminChannel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminChannel {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub position: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryPreview {
    pub plan: CategoryPlan,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryPlan {
    pub id: String,
    pub operation: String,
    pub name: String,
    pub position: Option<i64>,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryResult {
    pub category: CreatedCategory,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatedCategory {
    pub id: String,
    pub name: String,
    pub position: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelPreview {
    pub plan: ChannelPlan,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelPlan {
    pub id: String,
    pub operation: String,
    pub name: String,
    pub topic: Option<String>,
    pub category: ChannelCategory,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelCategory {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelResult {
    pub channel: CreatedTextChannel,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatedTextChannel {
    pub id: String,
    pub name: String,
    pub topic: Option<String>,
    pub category: ChannelCategory,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CleanupCandidates {
    pub minimum_age_hours: u64,
    pub candidates: Vec<CleanupCandidate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CleanupCandidate {
    pub id: String,
    pub name: String,
    pub target_type: String,
    pub category_name: Option<String>,
    pub age_hours: u64,
    pub created_at: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeletionPreview {
    pub plan: DeletionPlan,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeletionPlan {
    pub id: String,
    pub operation: String,
    pub target_type: String,
    pub target_id: String,
    pub name: String,
    pub age_hours: u64,
    pub reason: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeletionResult {
    pub deleted: DeletedResource,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeletedResource {
    pub id: String,
    pub name: String,
    pub target_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MutationPlan {
    pub id: String,
    pub operation: String,
    pub summary: String,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MutationPreview {
    pub plan: MutationPlan,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MutationOutcome {
    pub id: String,
    pub name: String,
    pub operation: String,
    pub summary: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MutationResult {
    pub result: MutationOutcome,
}

#[derive(Debug, Deserialize)]
struct ApiErrorEnvelope {
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

#[derive(Serialize)]
struct ActorRequest {
    actor_user_id: u64,
    guild_id: u64,
}

#[derive(Serialize)]
struct PreviewRequest<'a> {
    actor_user_id: u64,
    guild_id: u64,
    name: &'a str,
    position: Option<i64>,
}

#[derive(Serialize)]
struct ApplyRequest<'a> {
    actor_user_id: u64,
    guild_id: u64,
    plan_id: &'a str,
}

#[derive(Serialize)]
struct DeletionPreviewRequest<'a> {
    actor_user_id: u64,
    guild_id: u64,
    target_type: &'a str,
    target_id: u64,
}

#[derive(Serialize)]
struct ChannelPreviewRequest<'a> {
    actor_user_id: u64,
    guild_id: u64,
    category_id: u64,
    name: &'a str,
    topic: Option<&'a str>,
}

#[derive(Serialize)]
struct RenamePreviewRequest<'a> {
    actor_user_id: u64,
    guild_id: u64,
    target_type: &'a str,
    target_id: u64,
    name: &'a str,
}

#[derive(Serialize)]
struct MovePreviewRequest {
    actor_user_id: u64,
    guild_id: u64,
    channel_id: u64,
    category_id: u64,
}

#[derive(Serialize)]
struct PermissionPreviewRequest<'a> {
    actor_user_id: u64,
    guild_id: u64,
    channel_id: u64,
    template: &'a str,
    role_id: Option<u64>,
}

#[derive(Serialize)]
struct StructurePreviewRequest<'a> {
    actor_user_id: u64,
    guild_id: u64,
    blueprint: &'a str,
    template: &'a str,
    role_id: Option<u64>,
}

impl DiscordAdminClient {
    pub fn from_config(config: &DiscordAdminControlConfig) -> Result<Self> {
        validate_service_url(
            &config.url,
            config.allow_insecure_http,
            "discord.admin_control.url",
        )?;
        let token = match (&config.token_file, &config.token_env) {
            (Some(path), None) => {
                let token_path = Path::new(path.trim());
                std::fs::read_to_string(token_path)
                    .with_context(|| {
                        format!("read Discord Admin control token {}", token_path.display())
                    })?
                    .trim()
                    .to_string()
            }
            (None, Some(name)) => std::env::var(name.trim())
                .with_context(|| format!("read Discord Admin control token from {name}"))?
                .trim()
                .to_string(),
            _ => anyhow::bail!("Discord Admin control must configure exactly one token source"),
        };
        anyhow::ensure!(
            token.len() >= 32,
            "Discord Admin control token must contain at least 32 characters"
        );
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("build Discord Admin control client")?;
        Ok(Self {
            http,
            base_url: config.url.trim().trim_end_matches('/').to_string(),
            token,
        })
    }

    async fn post<T: DeserializeOwned, B: Serialize>(&self, path: &str, body: &B) -> Result<T> {
        let response = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("Discord Admin control request {path}"))?;
        let status = response.status();
        if status.is_success() {
            return response
                .json::<T>()
                .await
                .with_context(|| format!("decode Discord Admin response {path}"));
        }
        let fallback = format!("Discord Admin returned HTTP {status}");
        let error = response.json::<ApiErrorEnvelope>().await.ok();
        let message = error
            .map(|value| format!("{}: {}", value.error.code, value.error.message))
            .unwrap_or(fallback);
        if status == StatusCode::UNAUTHORIZED {
            anyhow::bail!("Discord Admin authentication failed")
        }
        anyhow::bail!(message)
    }

    pub async fn status(&self, actor_user_id: u64, guild_id: u64) -> Result<AdminStatus> {
        self.post(
            "/v1/status",
            &ActorRequest {
                actor_user_id,
                guild_id,
            },
        )
        .await
    }

    pub async fn inventory(&self, actor_user_id: u64, guild_id: u64) -> Result<AdminInventory> {
        self.post(
            "/v1/inventory",
            &ActorRequest {
                actor_user_id,
                guild_id,
            },
        )
        .await
    }

    pub async fn cleanup(&self, actor_user_id: u64, guild_id: u64) -> Result<CleanupCandidates> {
        self.post(
            "/v1/cleanup",
            &ActorRequest {
                actor_user_id,
                guild_id,
            },
        )
        .await
    }

    pub async fn preview_category(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        name: &str,
        position: Option<i64>,
    ) -> Result<CategoryPreview> {
        self.post(
            "/v1/categories/preview",
            &PreviewRequest {
                actor_user_id,
                guild_id,
                name,
                position,
            },
        )
        .await
    }

    pub async fn apply_category(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        plan_id: &str,
    ) -> Result<CategoryResult> {
        self.post(
            "/v1/categories/apply",
            &ApplyRequest {
                actor_user_id,
                guild_id,
                plan_id,
            },
        )
        .await
    }

    pub async fn preview_channel(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        category_id: u64,
        name: &str,
        topic: Option<&str>,
    ) -> Result<ChannelPreview> {
        self.post(
            "/v1/channels/preview",
            &ChannelPreviewRequest {
                actor_user_id,
                guild_id,
                category_id,
                name,
                topic,
            },
        )
        .await
    }

    pub async fn apply_channel(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        plan_id: &str,
    ) -> Result<ChannelResult> {
        self.post(
            "/v1/channels/apply",
            &ApplyRequest {
                actor_user_id,
                guild_id,
                plan_id,
            },
        )
        .await
    }

    pub async fn preview_deletion(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        target_type: &str,
        target_id: u64,
    ) -> Result<DeletionPreview> {
        self.post(
            "/v1/deletions/preview",
            &DeletionPreviewRequest {
                actor_user_id,
                guild_id,
                target_type,
                target_id,
            },
        )
        .await
    }

    pub async fn apply_deletion(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        plan_id: &str,
    ) -> Result<DeletionResult> {
        self.post(
            "/v1/deletions/apply",
            &ApplyRequest {
                actor_user_id,
                guild_id,
                plan_id,
            },
        )
        .await
    }

    pub async fn preview_rename(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        target_type: &str,
        target_id: u64,
        name: &str,
    ) -> Result<MutationPreview> {
        self.post(
            "/v1/renames/preview",
            &RenamePreviewRequest {
                actor_user_id,
                guild_id,
                target_type,
                target_id,
                name,
            },
        )
        .await
    }

    pub async fn apply_rename(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        plan_id: &str,
    ) -> Result<MutationResult> {
        self.post(
            "/v1/renames/apply",
            &ApplyRequest {
                actor_user_id,
                guild_id,
                plan_id,
            },
        )
        .await
    }

    pub async fn preview_move(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        channel_id: u64,
        category_id: u64,
    ) -> Result<MutationPreview> {
        self.post(
            "/v1/moves/preview",
            &MovePreviewRequest {
                actor_user_id,
                guild_id,
                channel_id,
                category_id,
            },
        )
        .await
    }

    pub async fn apply_move(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        plan_id: &str,
    ) -> Result<MutationResult> {
        self.post(
            "/v1/moves/apply",
            &ApplyRequest {
                actor_user_id,
                guild_id,
                plan_id,
            },
        )
        .await
    }

    pub async fn preview_permission(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        channel_id: u64,
        template: &str,
        role_id: Option<u64>,
    ) -> Result<MutationPreview> {
        self.post(
            "/v1/permissions/preview",
            &PermissionPreviewRequest {
                actor_user_id,
                guild_id,
                channel_id,
                template,
                role_id,
            },
        )
        .await
    }

    pub async fn apply_permission(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        plan_id: &str,
    ) -> Result<MutationResult> {
        self.post(
            "/v1/permissions/apply",
            &ApplyRequest {
                actor_user_id,
                guild_id,
                plan_id,
            },
        )
        .await
    }

    pub async fn preview_structure(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        blueprint: &str,
        template: &str,
        role_id: Option<u64>,
    ) -> Result<MutationPreview> {
        self.post(
            "/v1/structures/preview",
            &StructurePreviewRequest {
                actor_user_id,
                guild_id,
                blueprint,
                template,
                role_id,
            },
        )
        .await
    }

    pub async fn apply_structure(
        &self,
        actor_user_id: u64,
        guild_id: u64,
        plan_id: &str,
    ) -> Result<MutationResult> {
        self.post(
            "/v1/structures/apply",
            &ApplyRequest {
                actor_user_id,
                guild_id,
                plan_id,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_rejects_short_token_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, "short").unwrap();
        let config = DiscordAdminControlConfig {
            url: "http://discord-admin:8787".into(),
            token_file: Some(path.display().to_string()),
            token_env: None,
            allow_insecure_http: true,
        };

        let error = DiscordAdminClient::from_config(&config)
            .err()
            .expect("short token should fail");

        assert!(error.to_string().contains("at least 32"));
    }

    #[test]
    fn service_url_requires_https_or_private_network_opt_in() {
        assert!(validate_service_url("https://admin.example.test", false, "test URL").is_ok());
        assert!(validate_service_url("http://discord-admin:8787", true, "test URL").is_ok());
        assert!(validate_service_url("http://admin.example.test", false, "test URL").is_err());
        assert!(
            validate_service_url("https://user:secret@admin.example.test", false, "test URL")
                .is_err()
        );
    }

    #[test]
    fn knowledge_weekly_contract_validates_three_sources_and_window() {
        let payload = r#"
OPENAB_KNOWLEDGE_WEEKLY_V1
{"window_start":"2026-08-18T00:00:00+08:00","window_end":"2026-08-25T00:00:00+08:00","queried_at":"2026-08-25T08:40:00+08:00","sources":[{"source_id":"source_a","title":"Source A","url":"https://www.notion.so/source-a","status":"updated","items":[{"page_id":"page-a","title":"Article A","url":"https://www.notion.so/article-a","created_at":"2026-08-20T12:00:00+08:00"}]},{"source_id":"source_b","title":"Source B","url":"https://www.notion.so/source-b","status":"no_updates","items":[]},{"source_id":"source_c","title":"Source C","url":"https://www.notion.so/source-c","status":"failed","error":"connector unavailable","items":[]}]}
"#;
        let audit = parse_knowledge_weekly_audit(payload).unwrap().unwrap();

        assert_eq!(audit.sources.len(), 3);
        assert_eq!(audit.sources[0].items.len(), 1);
        assert_eq!(audit.sources[2].status, KnowledgeWeeklyStatus::Failed);
    }

    #[test]
    fn knowledge_weekly_contract_normalizes_null_error_for_success() {
        let payload = r#"
OPENAB_KNOWLEDGE_WEEKLY_V1
{"window_start":"2026-08-18T00:00:00+08:00","window_end":"2026-08-25T00:00:00+08:00","queried_at":"2026-08-25T08:40:00+08:00","sources":[{"source_id":"source_a","title":"Source A","url":"https://www.notion.so/source-a","status":"updated","error":null,"items":[{"page_id":"page-a","title":"Article A","url":"https://www.notion.so/article-a","created_at":"2026-08-20T12:00:00+08:00"}]},{"source_id":"source_b","title":"Source B","url":"https://www.notion.so/source-b","status":"no_updates","error":null,"items":[]},{"source_id":"source_c","title":"Source C","url":"https://www.notion.so/source-c","status":"failed","error":"connector unavailable","items":[]}]}
"#;

        let audit = parse_knowledge_weekly_audit(payload).unwrap().unwrap();

        assert!(audit.sources[0].error.is_empty());
        assert!(audit.sources[1].error.is_empty());
    }

    #[test]
    fn knowledge_weekly_marker_must_be_on_its_own_line() {
        let prompt = "最後只能輸出 OPENAB_KNOWLEDGE_WEEKLY_V1 換行後的一個 JSON object";

        assert!(!knowledge_weekly_marker_present(prompt));
        assert!(parse_knowledge_weekly_audit(prompt).unwrap().is_none());
    }

    #[test]
    fn knowledge_weekly_contract_rejects_out_of_window_item() {
        let payload = r#"
OPENAB_KNOWLEDGE_WEEKLY_V1
{"window_start":"2026-08-18T00:00:00+08:00","window_end":"2026-08-25T00:00:00+08:00","queried_at":"2026-08-25T08:40:00+08:00","sources":[{"source_id":"source_a","title":"Source A","url":"https://www.notion.so/source-a","status":"updated","items":[{"page_id":"page-a","title":"Article A","url":"https://www.notion.so/article-a","created_at":"2026-08-25T00:00:00+08:00"}]},{"source_id":"source_b","title":"Source B","url":"https://www.notion.so/source-b","status":"no_updates","items":[]},{"source_id":"source_c","title":"Source C","url":"https://www.notion.so/source-c","status":"no_updates","items":[]}]}
"#;

        assert!(parse_knowledge_weekly_audit(payload).is_err());
    }

    #[test]
    fn scheduled_job_contract_requires_truthful_status_and_note() {
        let mut run = ScheduledJobRun {
            job_id: "opencode-scheduled-source-retention".into(),
            run_id: "retention-2026-08-30".into(),
            started_at: "2026-08-30T03:00:00+08:00".into(),
            finished_at: "2026-08-30T03:12:00+08:00".into(),
            status: "success".into(),
            metrics: RetentionJobMetrics {
                sources_scanned: 3,
                items_scanned: 120,
                protected_items: 4,
                enqueued_items: 5,
                pending_items: 8,
                trash_due_items: 2,
                trashed_items: 0,
                failed_items: 0,
            },
            note: "Trash API unavailable; due targets were left unchanged.".into(),
        };
        assert!(validate_scheduled_job_run(&run).is_ok());

        run.metrics.failed_items = 1;
        assert!(validate_scheduled_job_run(&run).is_err());
        run.status = "partial".into();
        run.note.clear();
        assert!(validate_scheduled_job_run(&run).is_err());

        run.status = "success".into();
        run.metrics.failed_items = 0;
        run.metrics.sources_scanned = 2;
        run.note = "one source was skipped".into();
        assert!(validate_scheduled_job_run(&run).is_err());
    }
}
