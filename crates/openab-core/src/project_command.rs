//! Safe execution of administrator-configured repository commands.

use crate::config::DiscordProjectCommandConfig;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};

const CAPTURE_LIMIT_BYTES: usize = 32 * 1024;

#[derive(Debug)]
pub struct ProjectCommandOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub elapsed: Duration,
}

async fn read_capped<R>(mut reader: R) -> (Vec<u8>, bool)
where
    R: AsyncRead + Unpin,
{
    let mut captured = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = CAPTURE_LIMIT_BYTES.saturating_sub(captured.len());
                let keep = remaining.min(read);
                captured.extend_from_slice(&chunk[..keep]);
                truncated |= keep < read;
            }
        }
    }
    (captured, truncated)
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid.filter(|pid| *pid <= i32::MAX as u32) {
        // SAFETY: the child is spawned as its own process-group leader below.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

/// Execute one pre-validated command directly inside a repository workspace.
///
/// No shell is involved, arguments are literal, and the inherited environment
/// is cleared so Discord/OpenAB credentials cannot reach the child process.
pub async fn run_project_command(
    command: &DiscordProjectCommandConfig,
    workspace: &Path,
) -> Result<ProjectCommandOutput> {
    let workspace = workspace
        .canonicalize()
        .context("repository workspace is unavailable")?;
    if !workspace.is_dir() || !workspace.join(".git").exists() {
        bail!("repository command workspace is not a Git repository");
    }

    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&command.args)
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env_clear()
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8");
    if let Some(value) = std::env::var_os("HOME") {
        process.env("HOME", value);
    }
    if let Some(value) = std::env::var_os("PATH") {
        process.env("PATH", value);
    }
    #[cfg(unix)]
    if let Some(value) = std::env::var_os("USER") {
        process.env("USER", value);
    }
    #[cfg(windows)]
    for key in ["USERPROFILE", "USERNAME", "SystemRoot", "SystemDrive"] {
        if let Some(value) = std::env::var_os(key) {
            process.env(key, value);
        }
    }
    // Named by the operator in `env_passthrough`, copied from OpenAB's own
    // environment. A name the operator asked for but that is unset here is
    // skipped silently: the command reports its own missing-credential error,
    // which says more than a failure to start would.
    for key in &command.env_passthrough {
        if let Some(value) = std::env::var_os(key) {
            process.env(key, value);
        }
    }

    // Give every command its own process group so timeout termination also
    // reaches children spawned by make, npm, and similar task runners.
    #[cfg(unix)]
    unsafe {
        process.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let started = Instant::now();
    let mut child = process.spawn().with_context(|| {
        format!(
            "could not start configured executable `{}`",
            command.program
        )
    })?;
    #[cfg(unix)]
    let child_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .context("configured command stdout is unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("configured command stderr is unavailable")?;
    let stdout_task = tokio::spawn(read_capped(stdout));
    let stderr_task = tokio::spawn(read_capped(stderr));

    let timeout = Duration::from_secs(command.timeout_seconds);
    let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => (
            Some(status.context("could not wait for configured command")?),
            false,
        ),
        Err(_) => {
            #[cfg(unix)]
            kill_process_group(child_pid);
            #[cfg(not(unix))]
            let _ = child.kill().await;
            let _ = child.wait().await;
            (None, true)
        }
    };

    let (stdout, stdout_truncated) = stdout_task.await.unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_task.await.unwrap_or_default();
    Ok(ProjectCommandOutput {
        exit_code: status.and_then(|status| status.code()),
        timed_out,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated: stdout_truncated || stderr_truncated,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn command(program: &str, args: &[&str]) -> DiscordProjectCommandConfig {
        DiscordProjectCommandConfig {
            workspace_alias: "repo".into(),
            id: "test".into(),
            label: "Test".into(),
            description: String::new(),
            runner: crate::config::DiscordProjectCommandRunner::Local,
            program: program.into(),
            args: args.iter().map(|value| (*value).into()).collect(),
            timeout_seconds: 5,
            requires_confirmation: false,
            env_passthrough: Vec::new(),
        }
    }

    fn repository_workspace() -> tempfile::TempDir {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join(".git")).unwrap();
        workspace
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn executes_literal_arguments_in_repository() {
        let workspace = repository_workspace();
        let output = run_project_command(
            &command("printf", &["literal;not-shell-expanded"]),
            workspace.path(),
        )
        .await
        .unwrap();

        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, "literal;not-shell-expanded");
        assert!(output.stderr.is_empty());
        assert!(!output.timed_out);
    }

    /// The bridge scripts need the bot token, and the whole point of handing it
    /// over here is that the `.env` beside them can stop being mounted — the
    /// agent runs as the same uid and would otherwise just read the file.
    #[tokio::test]
    async fn passes_through_only_the_named_variables() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::env::set_var("OPENAB_TEST_WANTED", "wanted-value");
        std::env::set_var("OPENAB_TEST_UNWANTED", "secret");

        let mut config = command("printenv", &[]);
        config.env_passthrough = vec!["OPENAB_TEST_WANTED".into()];
        let output = run_project_command(&config, dir.path()).await.unwrap();

        std::env::remove_var("OPENAB_TEST_WANTED");
        std::env::remove_var("OPENAB_TEST_UNWANTED");
        assert!(output.stdout.contains("OPENAB_TEST_WANTED=wanted-value"));
        // Everything else stays cleared: passthrough is an allowlist, not a door.
        assert!(!output.stdout.contains("OPENAB_TEST_UNWANTED"));
    }

    /// An operator can name a variable that is not set here — a deployment
    /// without the admin bot, say. Starting anyway lets the command report its
    /// own missing-credential message instead of failing to spawn.
    #[tokio::test]
    async fn a_missing_named_variable_does_not_stop_the_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let mut config = command("printenv", &[]);
        config.env_passthrough = vec!["OPENAB_TEST_DEFINITELY_UNSET".into()];
        let output = run_project_command(&config, dir.path()).await.unwrap();
        assert!(!output.stdout.contains("OPENAB_TEST_DEFINITELY_UNSET"));
    }

    #[tokio::test]
    async fn rejects_non_repository_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let error = run_project_command(&command("git", &["status"]), workspace.path())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not a Git repository"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminates_command_after_timeout() {
        let workspace = repository_workspace();
        let mut configured = command("sleep", &["5"]);
        configured.timeout_seconds = 1;

        let output = run_project_command(&configured, workspace.path())
            .await
            .unwrap();

        assert!(output.timed_out);
        assert_eq!(output.exit_code, None);
        assert!(output.elapsed < Duration::from_secs(3));
    }
}
