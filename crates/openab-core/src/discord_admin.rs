//! Client for the dedicated Discord Admin Bot's private control plane.

use crate::config::DiscordAdminControlConfig;
use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

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

impl DiscordAdminClient {
    pub fn from_config(config: &DiscordAdminControlConfig) -> Result<Self> {
        let token = match (&config.token_file, &config.token_env) {
            (Some(path), None) => {
                let token_path = Path::new(path.trim());
                std::fs::read_to_string(token_path)
                    .with_context(|| {
                        format!(
                            "read Discord Admin control token {}",
                            token_path.display()
                        )
                    })?
                    .trim()
                    .to_string()
            }
            (None, Some(name)) => std::env::var(name.trim())
                .with_context(|| format!("read Discord Admin control token from {name}"))?
                .trim()
                .to_string(),
            _ => anyhow::bail!(
                "Discord Admin control must configure exactly one token source"
            ),
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

    pub async fn cleanup(
        &self,
        actor_user_id: u64,
        guild_id: u64,
    ) -> Result<CleanupCandidates> {
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
        };

        let error = DiscordAdminClient::from_config(&config)
            .err()
            .expect("short token should fail");

        assert!(error.to_string().contains("at least 32"));
    }
}
