//! Safe execution of administrator-configured repository commands.

use crate::config::{
    DiscordProjectCommandConfig, PROJECT_COMMAND_BOOK_PLACEHOLDER,
    PROJECT_COMMAND_BOOK_SLUG_MAX_LEN,
};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};

const CAPTURE_LIMIT_BYTES: usize = 32 * 1024;
/// Discord String Select hard limit.
pub const BOOK_SELECT_MAX_OPTIONS: usize = 25;

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

/// Whether `slug` is a safe book directory name (no path traversal).
pub fn is_valid_book_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= PROJECT_COMMAND_BOOK_SLUG_MAX_LEN
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// List book directory names under `workspace/books/`, sorted.
///
/// Returns at most [`BOOK_SELECT_MAX_OPTIONS`] entries plus the total count so
/// callers can show a truncation note.
pub fn list_workspace_books(workspace: &Path) -> Result<(Vec<String>, usize)> {
    let books_root = workspace.join("books");
    if !books_root.is_dir() {
        bail!("workspace has no books/ directory");
    }
    let mut slugs = Vec::new();
    for entry in fs::read_dir(&books_root).context("could not read books/ directory")? {
        let entry = entry.context("could not read books/ entry")?;
        let file_type = entry.file_type().context("could not stat books/ entry")?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(slug) = name.to_str() else {
            continue;
        };
        if !is_valid_book_slug(slug) {
            continue;
        }
        slugs.push(slug.to_string());
    }
    slugs.sort();
    let total = slugs.len();
    if slugs.len() > BOOK_SELECT_MAX_OPTIONS {
        slugs.truncate(BOOK_SELECT_MAX_OPTIONS);
    }
    Ok((slugs, total))
}

/// Confirm `slug` names an existing directory under `workspace/books/`.
pub fn validate_workspace_book(workspace: &Path, slug: &str) -> Result<PathBuf> {
    if !is_valid_book_slug(slug) {
        bail!("invalid book slug");
    }
    let book_dir = workspace.join("books").join(slug);
    let canonical_books = workspace
        .join("books")
        .canonicalize()
        .context("books/ directory is unavailable")?;
    let canonical_book = book_dir
        .canonicalize()
        .context("selected book directory is unavailable")?;
    if !canonical_book.starts_with(&canonical_books) || !canonical_book.is_dir() {
        bail!("selected book is outside books/");
    }
    Ok(canonical_book)
}

/// Replace the single `{{book}}` placeholder when `book_select` is enabled.
pub fn resolve_project_command_args(
    command: &DiscordProjectCommandConfig,
    book_slug: Option<&str>,
) -> Result<Vec<String>> {
    if command.book_select {
        let slug = book_slug.context("book_select command requires a selected book")?;
        if !is_valid_book_slug(slug) {
            bail!("invalid book slug");
        }
        let mut replaced = false;
        let args = command
            .args
            .iter()
            .map(|arg| {
                if arg.contains(PROJECT_COMMAND_BOOK_PLACEHOLDER) {
                    replaced = true;
                    arg.replace(PROJECT_COMMAND_BOOK_PLACEHOLDER, slug)
                } else {
                    arg.clone()
                }
            })
            .collect();
        if !replaced {
            bail!("book_select command is missing the {PROJECT_COMMAND_BOOK_PLACEHOLDER} placeholder");
        }
        return Ok(args);
    }
    if book_slug.is_some() {
        bail!("command does not accept a book selection");
    }
    if command
        .args
        .iter()
        .any(|arg| arg.contains(PROJECT_COMMAND_BOOK_PLACEHOLDER))
    {
        bail!("command args still contain an unresolved book placeholder");
    }
    Ok(command.args.clone())
}

/// Display string for confirmation cards after optional book substitution.
pub fn project_command_argv_display(
    command: &DiscordProjectCommandConfig,
    book_slug: Option<&str>,
) -> Result<String> {
    let args = resolve_project_command_args(command, book_slug)?;
    Ok(std::iter::once(command.program.as_str())
        .chain(args.iter().map(String::as_str))
        .map(|value| {
            if value.chars().any(char::is_whitespace) {
                format!("{value:?}")
            } else {
                value.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" "))
}

/// Execute one pre-validated command directly inside a repository workspace.
///
/// No shell is involved, arguments are literal, and the inherited environment
/// is cleared so Discord/OpenAB credentials cannot reach the child process.
pub async fn run_project_command(
    command: &DiscordProjectCommandConfig,
    workspace: &Path,
    book_slug: Option<&str>,
) -> Result<ProjectCommandOutput> {
    let workspace = workspace
        .canonicalize()
        .context("repository workspace is unavailable")?;
    if !workspace.is_dir() || !workspace.join(".git").exists() {
        bail!("repository command workspace is not a Git repository");
    }

    if command.book_select {
        let slug = book_slug.context("book_select command requires a selected book")?;
        validate_workspace_book(&workspace, slug)?;
    }

    let args = resolve_project_command_args(command, book_slug)?;

    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&args)
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
            book_select: false,
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
            None,
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
        let output = run_project_command(&config, dir.path(), None)
            .await
            .unwrap();

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
        let output = run_project_command(&config, dir.path(), None)
            .await
            .unwrap();
        assert!(!output.stdout.contains("OPENAB_TEST_DEFINITELY_UNSET"));
    }

    #[tokio::test]
    async fn rejects_non_repository_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let error = run_project_command(&command("git", &["status"]), workspace.path(), None)
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

        let output = run_project_command(&configured, workspace.path(), None)
            .await
            .unwrap();

        assert!(output.timed_out);
        assert_eq!(output.exit_code, None);
        assert!(output.elapsed < Duration::from_secs(3));
    }

    #[test]
    fn resolves_book_placeholder_only_when_book_select_is_enabled() {
        let mut configured = command("python3", &["sync.py", "--book", "{{book}}", "--force"]);
        configured.book_select = true;
        configured.requires_confirmation = true;
        let args = resolve_project_command_args(&configured, Some("heshi-mentu")).unwrap();
        assert_eq!(args, ["sync.py", "--book", "heshi-mentu", "--force"]);

        let err = resolve_project_command_args(&configured, None).unwrap_err();
        assert!(err.to_string().contains("requires a selected book"));

        let plain = command("python3", &["sync.py", "--yes"]);
        let err = resolve_project_command_args(&plain, Some("heshi-mentu")).unwrap_err();
        assert!(err.to_string().contains("does not accept a book selection"));
    }

    #[test]
    fn resolves_a_placeholder_embedded_in_a_make_variable() {
        let mut configured = command("make", &["epub", "BOOK={{book}}", "UPLOAD=gdrive"]);
        configured.book_select = true;
        configured.requires_confirmation = true;
        let args = resolve_project_command_args(&configured, Some("heshi-mentu")).unwrap();
        assert_eq!(args, ["epub", "BOOK=heshi-mentu", "UPLOAD=gdrive"]);
    }

    #[test]
    fn lists_and_validates_book_directories() {
        let workspace = repository_workspace();
        let books = workspace.path().join("books");
        fs::create_dir_all(books.join("heshi-mentu")).unwrap();
        fs::create_dir_all(books.join("blood-chalice")).unwrap();
        fs::write(books.join("readme.txt"), "no").unwrap();
        fs::create_dir_all(books.join("bad_name")).unwrap();

        let (slugs, total) = list_workspace_books(workspace.path()).unwrap();
        assert_eq!(total, 2);
        assert_eq!(slugs, ["blood-chalice", "heshi-mentu"]);
        assert!(validate_workspace_book(workspace.path(), "heshi-mentu").is_ok());
        assert!(validate_workspace_book(workspace.path(), "../etc").is_err());
        assert!(validate_workspace_book(workspace.path(), "missing-book").is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn substitutes_selected_book_into_argv() {
        let workspace = repository_workspace();
        fs::create_dir_all(workspace.path().join("books/heshi-mentu")).unwrap();
        let mut configured = command("printf", &["%s", "{{book}}"]);
        configured.book_select = true;
        configured.requires_confirmation = true;

        let output = run_project_command(&configured, workspace.path(), Some("heshi-mentu"))
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(0));
        assert_eq!(output.stdout, "heshi-mentu");
    }
}
