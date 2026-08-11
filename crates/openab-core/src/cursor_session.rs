//! Validation helpers for Cursor CLI chat checkpoints.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[cfg(feature = "discord")]
use crate::acp::SessionPool;
#[cfg(feature = "discord")]
use crate::adapter::{ChannelRef, ChatAdapter};
#[cfg(feature = "discord")]
use crate::project_registry::ProjectBinding;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorChatCheckpoint {
    pub session_id: String,
    pub working_dir: PathBuf,
}

pub fn cursor_chat_root() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join(".cursor")
        .join("chats")
}

pub fn validate_cursor_chat_id(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if !valid {
        bail!("invalid Cursor chat ID");
    }
    Ok(value.to_ascii_lowercase())
}

pub fn load_cursor_chat(session_id: &str) -> Result<CursorChatCheckpoint> {
    load_cursor_chat_from(&cursor_chat_root(), session_id)
}

pub fn load_cursor_chat_from(root: &Path, session_id: &str) -> Result<CursorChatCheckpoint> {
    let session_id = validate_cursor_chat_id(session_id)?;
    let mut matches = Vec::new();
    for workspace_entry in std::fs::read_dir(root)
        .with_context(|| format!("Cursor chat store is unavailable: {}", root.display()))?
    {
        let workspace_entry = workspace_entry?;
        if !workspace_entry.file_type()?.is_dir() {
            continue;
        }
        let chat_dir = workspace_entry.path().join(&session_id);
        if chat_dir.join("store.db").is_file() {
            matches.push(chat_dir);
        }
    }
    let chat_dir = match matches.as_slice() {
        [chat_dir] => chat_dir,
        [] => bail!("Cursor chat checkpoint was not found"),
        _ => bail!("Cursor chat ID appears in more than one workspace"),
    };

    let metadata_path = chat_dir.join("meta.json");
    let metadata: Value = serde_json::from_str(
        &std::fs::read_to_string(&metadata_path)
            .with_context(|| format!("cannot read {}", metadata_path.display()))?,
    )
    .with_context(|| format!("invalid Cursor metadata: {}", metadata_path.display()))?;
    let working_dir = metadata
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|value| Path::new(value).is_absolute())
        .ok_or_else(|| anyhow!("Cursor chat metadata has no absolute workspace"))?;
    let working_dir = Path::new(working_dir)
        .canonicalize()
        .with_context(|| format!("Cursor chat workspace is unavailable: {working_dir}"))?;

    Ok(CursorChatCheckpoint {
        session_id,
        working_dir,
    })
}

pub fn require_workspace(checkpoint: &CursorChatCheckpoint, expected: &Path) -> Result<PathBuf> {
    let expected = expected
        .canonicalize()
        .with_context(|| format!("project workspace is unavailable: {}", expected.display()))?;
    if checkpoint.working_dir != expected {
        bail!(
            "Cursor chat belongs to {}, not {}",
            checkpoint.working_dir.display(),
            expected.display()
        );
    }
    Ok(expected)
}

/// Best-effort guard against handing a chat to Discord while a Cursor CLI
/// process is still using it. SessionPool remains the authoritative guard
/// against duplicate Discord ownership.
pub fn cursor_chat_is_running(checkpoint: &CursorChatCheckpoint) -> bool {
    let session_id = checkpoint.session_id.to_ascii_lowercase();
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return false;
    };
    processes.filter_map(Result::ok).any(|process| {
        let Ok(cmdline) = std::fs::read(process.path().join("cmdline")) else {
            return false;
        };
        let cmdline = String::from_utf8_lossy(&cmdline).to_ascii_lowercase();
        if !cmdline.contains("cursor-agent") {
            return false;
        }
        let same_workspace = std::fs::read_link(process.path().join("cwd"))
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .is_some_and(|path| path == checkpoint.working_dir);
        cmdline.contains(&session_id) || same_workspace
    })
}

#[cfg(feature = "discord")]
pub async fn attach_cursor_chat(
    pool: &SessionPool,
    chat_id: &str,
    expected_workspace: &Path,
    discord_thread_id: &str,
) -> Result<CursorChatCheckpoint> {
    let checkpoint = load_cursor_chat(chat_id)?;
    let workspace = require_workspace(&checkpoint, expected_workspace)?;
    if cursor_chat_is_running(&checkpoint) {
        bail!("Cursor still owns this chat; exit the Cursor UI before attaching it");
    }
    let workspace = workspace
        .to_str()
        .ok_or_else(|| anyhow!("Cursor chat workspace is not valid UTF-8"))?;
    pool.attach_external_session(
        &format!("discord:{discord_thread_id}"),
        &checkpoint.session_id,
        workspace,
    )
    .await?;
    Ok(checkpoint)
}

#[cfg(feature = "discord")]
pub async fn publish_cursor_chat(
    adapter: &dyn ChatAdapter,
    pool: &SessionPool,
    binding: &ProjectBinding,
    expected_workspace: &Path,
    chat_id: &str,
    title: &str,
) -> Result<ChannelRef> {
    let checkpoint = load_cursor_chat(chat_id)?;
    require_workspace(&checkpoint, expected_workspace)?;
    if cursor_chat_is_running(&checkpoint) {
        bail!("Cursor still owns this chat; exit the Cursor UI before publishing it");
    }
    pool.ensure_external_session_available(&checkpoint.session_id)
        .await?;
    let parent = ChannelRef {
        platform: "discord".to_string(),
        channel_id: binding.channel_id.to_string(),
        thread_id: None,
        parent_id: None,
        origin_event_id: None,
    };
    let short_id = checkpoint
        .session_id
        .get(..8)
        .unwrap_or(&checkpoint.session_id);
    let trigger = adapter
        .send_message(
            &parent,
            &format!("📤 Publishing Cursor session `{short_id}` to Discord"),
        )
        .await?;
    let thread = adapter
        .create_thread(&parent, &trigger, &sanitize_publish_title(title))
        .await?;
    if let Err(error) = attach_cursor_chat(
        pool,
        &checkpoint.session_id,
        expected_workspace,
        &thread.channel_id,
    )
    .await
    {
        let _ = adapter
            .send_message(
                &thread,
                &format!("⚠️ Cursor session attach failed: {error}"),
            )
            .await;
        return Err(error);
    }
    adapter
        .send_message(
            &thread,
            &format!(
                "✅ Cursor session `{}` is attached to **@{}**. Send the next message here to continue the same chat.",
                checkpoint.session_id, binding.workspace_alias
            ),
        )
        .await?;
    Ok(thread)
}

#[cfg(feature = "discord")]
pub fn sanitize_publish_title(value: &str) -> String {
    let title: String = value.trim().chars().take(100).collect();
    if title.chars().count() < 2 {
        "Cursor handoff".to_string()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT_ID: &str = "00000000-0000-0000-0000-000000000000";

    #[test]
    fn loads_checkpoint_and_canonical_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let chat_dir = root.path().join("workspace-hash").join(CHAT_ID);
        std::fs::create_dir_all(&chat_dir).unwrap();
        std::fs::write(chat_dir.join("store.db"), b"sqlite").unwrap();
        std::fs::write(
            chat_dir.join("meta.json"),
            serde_json::json!({"cwd": workspace.path()}).to_string(),
        )
        .unwrap();

        let checkpoint = load_cursor_chat_from(root.path(), CHAT_ID).unwrap();

        assert_eq!(checkpoint.session_id, CHAT_ID);
        assert_eq!(
            checkpoint.working_dir,
            workspace.path().canonicalize().unwrap()
        );
        assert!(require_workspace(&checkpoint, workspace.path()).is_ok());
    }

    #[test]
    fn rejects_invalid_id_and_workspace_mismatch() {
        assert!(validate_cursor_chat_id("../../store.db").is_err());
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let checkpoint = CursorChatCheckpoint {
            session_id: CHAT_ID.to_string(),
            working_dir: first.path().canonicalize().unwrap(),
        };

        assert!(require_workspace(&checkpoint, second.path()).is_err());
    }

    #[cfg(feature = "discord")]
    #[test]
    fn publish_title_has_safe_fallback_and_limit() {
        assert_eq!(sanitize_publish_title(" "), "Cursor handoff");
        assert_eq!(sanitize_publish_title("x"), "Cursor handoff");
        assert_eq!(sanitize_publish_title(&"a".repeat(120)).len(), 100);
    }
}
