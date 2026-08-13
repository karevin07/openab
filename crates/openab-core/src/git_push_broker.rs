//! Client for the private Git push broker.

use crate::config::DiscordGitPushBrokerConfig;
use crate::project_command::ProjectCommandOutput;
use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

#[derive(Clone)]
pub struct GitPushBrokerClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

#[derive(Serialize)]
struct PushRequest<'a> {
    workspace_alias: &'a str,
}

#[derive(Deserialize)]
struct PushResponse {
    exit_code: i32,
    stdout: String,
    stderr: String,
    truncated: bool,
    elapsed_ms: u64,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: ApiError,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

impl GitPushBrokerClient {
    pub fn from_config(config: &DiscordGitPushBrokerConfig) -> Result<Self> {
        let token = match (&config.token_file, &config.token_env) {
            (Some(path), None) => {
                let token_path = Path::new(path.trim());
                std::fs::read_to_string(token_path)
                    .with_context(|| format!("read Git push broker token {}", token_path.display()))?
                    .trim()
                    .to_string()
            }
            (None, Some(name)) => std::env::var(name.trim())
                .with_context(|| format!("read Git push broker token from {name}"))?
                .trim()
                .to_string(),
            _ => anyhow::bail!("Git push broker must configure exactly one token source"),
        };
        anyhow::ensure!(
            token.len() >= 32,
            "Git push broker token must contain at least 32 characters"
        );
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(130))
            .build()
            .context("build Git push broker client")?;
        Ok(Self {
            http,
            base_url: config.url.trim().trim_end_matches('/').to_string(),
            token,
        })
    }

    pub async fn push(&self, workspace_alias: &str) -> Result<ProjectCommandOutput> {
        let response = self
            .http
            .post(format!("{}/v1/push", self.base_url))
            .bearer_auth(&self.token)
            .json(&PushRequest { workspace_alias })
            .send()
            .await
            .context("Git push broker request")?;
        let status = response.status();
        if status.is_success() {
            let output = response
                .json::<PushResponse>()
                .await
                .context("decode Git push broker response")?;
            return Ok(ProjectCommandOutput {
                exit_code: Some(output.exit_code),
                timed_out: false,
                stdout: output.stdout,
                stderr: output.stderr,
                truncated: output.truncated,
                elapsed: Duration::from_millis(output.elapsed_ms),
            });
        }
        let fallback = format!("Git push broker returned HTTP {status}");
        let error = response.json::<ApiErrorEnvelope>().await.ok();
        let message = error
            .map(|value| format!("{}: {}", value.error.code, value.error.message))
            .unwrap_or(fallback);
        if status == StatusCode::UNAUTHORIZED {
            anyhow::bail!("Git push broker authentication failed")
        }
        anyhow::bail!(message)
    }
}
