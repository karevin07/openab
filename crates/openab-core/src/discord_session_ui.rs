//! Discord UI for the Session Manager.
//!
//! Split out of `discord.rs`: the per-project session list, its selection and
//! control components, and the handler that drives them. A "managed session" is
//! the join of a task record with the pool's [`SessionSnapshot`], which is why
//! this module reads both registries rather than owning state of its own.
//!
//! `reconciled_handoff_task_state` lives here because it is the rule that keeps
//! the Task Status Card honest about who currently owns a session — Discord or
//! an external Cursor terminal — and the Session Manager is where that
//! reconciliation is defined and tested.

use crate::acp::{SessionSnapshot, SessionState};
use crate::discord::{
    archive_discord_thread, inline_code, is_denied_user, suppress_mentions, task_status_edit,
    truncate_for_discord, Handler, InteractionCard, SELECT_MENU_PAGE_SIZE, SELECT_OPTION_TEXT_MAX,
};
use crate::project_registry::ProjectBinding;
use crate::task_registry::{TaskRecord, TaskState};
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, EditInteractionResponse,
};
use serenity::model::application::{ButtonStyle, ComponentInteractionDataKind};
use serenity::model::id::{ChannelId, MessageId};
use serenity::prelude::*;

#[derive(Debug, Clone)]
pub(crate) struct ManagedSessionEntry {
    pub(crate) task: TaskRecord,
    pub(crate) snapshot: SessionSnapshot,
}

fn managed_session_presentation(entry: &ManagedSessionEntry) -> (&'static str, &'static str, u32) {
    if entry.snapshot.externally_detached {
        return ("🖥️", "Cursor 接手中", 0x9B59B6);
    }
    match entry.snapshot.state {
        SessionState::Active => ("🟢", "執行中", 0x2ECC71),
        SessionState::Suspended | SessionState::Persisted => ("🟦", "可接續", 0x3498DB),
        SessionState::None if entry.task.state == TaskState::Closed => ("⚫", "已關閉", 0x95A5A6),
        SessionState::None => ("⚪", "尚未建立", 0x95A5A6),
    }
}

pub(crate) fn reconciled_handoff_task_state(
    task_state: TaskState,
    snapshot: &SessionSnapshot,
) -> Option<TaskState> {
    if task_state != TaskState::Cursor || snapshot.externally_detached {
        return None;
    }
    Some(if snapshot.state == SessionState::None {
        TaskState::Closed
    } else {
        TaskState::Ready
    })
}

fn session_manager_card(
    binding: &ProjectBinding,
    entries: &[ManagedSessionEntry],
    selected_thread_id: Option<u64>,
    note: Option<String>,
) -> InteractionCard {
    let selected = selected_thread_id.and_then(|thread_id| {
        entries
            .iter()
            .find(|entry| entry.task.thread_id == thread_id)
    });
    let resumable = entries
        .iter()
        .filter(|entry| entry.snapshot.state != SessionState::None)
        .count();
    let on_computer = entries
        .iter()
        .filter(|entry| entry.snapshot.externally_detached)
        .count();
    let closed = entries
        .iter()
        .filter(|entry| {
            entry.task.state == TaskState::Closed && entry.snapshot.state == SessionState::None
        })
        .count();

    let mut embed = CreateEmbed::new()
        .title(format!("🧠 @{} · Cursor Sessions", binding.workspace_alias))
        .description(
            "選擇一個 task 查看 Cursor session 狀態。Close 可同時封存 Discord thread；repository 與 Cursor checkpoint 都不會刪除。",
        )
        .colour(0x5865F2)
        .field(
            "總覽",
            format!(
                "**{}** tasks · **{resumable}** resumable · **{on_computer}** on computer · **{closed}** closed",
                entries.len()
            ),
            false,
        );

    if let Some(entry) = selected {
        let (icon, state, colour) = managed_session_presentation(entry);
        embed = embed
            .title(format!("{icon} {}", entry.task.title))
            .colour(colour)
            .field("Session", format!("**{state}**"), true)
            .field(
                "Workspace",
                inline_code(&format!("@{}", binding.workspace_alias)),
                true,
            )
            .field("Task thread", format!("<#{}>", entry.task.thread_id), false)
            .field(
                "Last updated",
                format!("<t:{}:R>", entry.task.updated_at.timestamp()),
                true,
            );
        if entry.snapshot.externally_detached {
            embed = embed.field(
                "注意",
                "這個 session 正由電腦上的 Cursor 使用。請先正常離開 Cursor terminal，才能從 Discord 關閉。",
                false,
            );
        } else if entry.task.state == TaskState::Closed
            && entry.snapshot.state == SessionState::None
        {
            embed = embed.field(
                "清理紀錄",
                "Session 已關閉。用 **Archive thread** 移出 Discord 的 active threads；用 **Remove session record** 隱藏 OpenAB task metadata。兩者都不會刪除 Cursor checkpoint。",
                false,
            );
        }
    } else if entries.is_empty() {
        embed = embed.field(
            "Sessions",
            "_這個 project 尚無 task。回到 Project Home 點 **New task** 開始。_",
            false,
        );
    } else {
        let list = entries
            .iter()
            .take(10)
            .map(|entry| {
                let (icon, state, _) = managed_session_presentation(entry);
                format!(
                    "{icon} <#{}> · {state} · <t:{}:R>",
                    entry.task.thread_id,
                    entry.task.updated_at.timestamp()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        embed = embed.field("Recent sessions", list, false).field(
            "安全清除",
            "先選擇 session，再按 **Close…**，可只關閉 session 或一併封存 thread。關閉後才能 **Remove session record**；不提供直接刪除 Cursor checkpoint。",
            false,
        );
    }
    embed = embed.footer(CreateEmbedFooter::new(
        "最多顯示最近 25 筆 · 一個 task thread 對應一個 Cursor session",
    ));

    let mut rows = Vec::new();
    if !entries.is_empty() {
        let options = entries
            .iter()
            .take(SELECT_MENU_PAGE_SIZE)
            .map(|entry| {
                let (icon, state, _) = managed_session_presentation(entry);
                let mut option = CreateSelectMenuOption::new(
                    truncate_for_discord(&entry.task.title, SELECT_OPTION_TEXT_MAX),
                    entry.task.thread_id.to_string(),
                )
                .description(truncate_for_discord(
                    &format!(
                        "{icon} {state} · {} UTC",
                        entry.task.updated_at.format("%m-%d %H:%M")
                    ),
                    SELECT_OPTION_TEXT_MAX,
                ));
                if selected_thread_id == Some(entry.task.thread_id) {
                    option = option.default_selection(true);
                }
                option
            })
            .collect();
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                format!("oab_sessions:select:{}", binding.channel_id),
                CreateSelectMenuKind::String { options },
            )
            .placeholder("選擇 Cursor session"),
        ));
    }

    let mut buttons = Vec::new();
    if let Some(entry) = selected {
        buttons.push(
            CreateButton::new_link(format!(
                "https://discord.com/channels/{}/{}",
                entry.task.guild_id, entry.task.thread_id
            ))
            .label("Open thread"),
        );
        buttons.push(
            CreateButton::new(format!(
                "oab_sessions:view:{}:{}",
                binding.channel_id, entry.task.thread_id
            ))
            .label("↻ Refresh")
            .style(ButtonStyle::Secondary),
        );
        let can_close = !entry.snapshot.externally_detached
            && (entry.snapshot.state != SessionState::None
                || entry.task.state != TaskState::Closed);
        let can_remove = entry.task.state == TaskState::Closed
            && entry.snapshot.state == SessionState::None
            && !entry.snapshot.externally_detached;
        if can_close {
            buttons.push(
                CreateButton::new(format!(
                    "oab_sessions:close:{}:{}",
                    binding.channel_id, entry.task.thread_id
                ))
                .label("✕ Close…")
                .style(ButtonStyle::Danger),
            );
        } else if can_remove {
            buttons.push(
                CreateButton::new(format!(
                    "oab_sessions:archive:{}:{}",
                    binding.channel_id, entry.task.thread_id
                ))
                .label("Archive thread")
                .style(ButtonStyle::Secondary),
            );
        } else {
            buttons.push(
                CreateButton::new(format!(
                    "oab_sessions:close:{}:{}",
                    binding.channel_id, entry.task.thread_id
                ))
                .label("✕ Close…")
                .style(ButtonStyle::Danger)
                .disabled(true),
            );
        }
        buttons.push(
            CreateButton::new(format!(
                "oab_sessions:remove:{}:{}",
                binding.channel_id, entry.task.thread_id
            ))
            .label("Remove session record")
            .style(ButtonStyle::Secondary)
            .disabled(!can_remove),
        );
    } else {
        buttons.push(
            CreateButton::new(format!("oab_sessions:open:{}", binding.channel_id))
                .label("↻ Refresh")
                .style(ButtonStyle::Secondary),
        );
    }
    buttons.push(
        CreateButton::new("oab_help:back")
            .label("← Help")
            .style(ButtonStyle::Secondary),
    );
    rows.push(CreateActionRow::Buttons(buttons));

    InteractionCard {
        content: note
            .map(|value| truncate_for_discord(&value, 1900))
            .unwrap_or_default(),
        embed,
        components: rows,
    }
}

#[cfg(test)]
pub(crate) fn session_manager_message(
    binding: &ProjectBinding,
    entries: &[ManagedSessionEntry],
    selected_thread_id: Option<u64>,
    note: Option<String>,
) -> CreateInteractionResponseMessage {
    session_manager_card(binding, entries, selected_thread_id, note).into_message()
}

pub(crate) fn session_manager_edit(
    binding: &ProjectBinding,
    entries: &[ManagedSessionEntry],
    selected_thread_id: Option<u64>,
    note: Option<String>,
) -> EditInteractionResponse {
    session_manager_card(binding, entries, selected_thread_id, note).into_edit()
}

impl Handler {
    pub(crate) async fn managed_sessions_for_project(
        &self,
        project_channel_id: u64,
    ) -> Vec<ManagedSessionEntry> {
        let tasks = self
            .task_registry
            .recent_for_project(project_channel_id, SELECT_MENU_PAGE_SIZE);
        let mut entries = Vec::with_capacity(tasks.len());
        for mut task in tasks {
            let snapshot = self
                .router
                .pool()
                .session_snapshot(&format!("discord:{}", task.thread_id))
                .await;
            if let Some(state) = reconciled_handoff_task_state(task.state, &snapshot) {
                if let Ok(updated) = self.task_registry.set_state(task.thread_id, state) {
                    task = updated;
                }
            }
            entries.push(ManagedSessionEntry { task, snapshot });
        }
        entries
    }

    pub(crate) async fn handle_session_manager_component(
        &self,
        ctx: &Context,
        comp: &serenity::model::application::ComponentInteraction,
    ) {
        if comp.user.bot
            || is_denied_user(
                false,
                self.allow_all_users,
                &self.allowed_users,
                comp.user.id.get(),
            )
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 你沒有管理 Cursor sessions 的權限。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }

        let mut parts = comp
            .data
            .custom_id
            .strip_prefix("oab_sessions:")
            .unwrap_or("")
            .split(':');
        let action = parts.next().unwrap_or("");
        let Some(project_channel_id) = parts.next().and_then(|value| value.parse::<u64>().ok())
        else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ This Session Manager card is no longer valid. Open `/help` again.")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };

        if let Err(error) = comp.defer(&ctx.http).await {
            tracing::error!(%error, action, "failed to defer Session Manager interaction");
            return;
        }

        let binding = match self.project_binding_for_channel(ctx, comp.channel_id).await {
            Ok((binding, _)) if binding.channel_id == project_channel_id => binding,
            _ => {
                let _ = comp
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .content(
                                "🚫 This Session Manager does not belong to the current project.",
                            )
                            .embeds(Vec::new())
                            .components(Vec::new()),
                    )
                    .await;
                return;
            }
        };

        let selected_thread_id = if action == "select" {
            match &comp.data.kind {
                ComponentInteractionDataKind::StringSelect { values } => {
                    values.first().and_then(|value| value.parse::<u64>().ok())
                }
                _ => None,
            }
        } else {
            parts.next().and_then(|value| value.parse::<u64>().ok())
        };

        if action == "open" {
            let entries = self.managed_sessions_for_project(project_channel_id).await;
            let _ = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(&binding, &entries, None, None),
                )
                .await;
            return;
        }

        let Some(thread_id) = selected_thread_id else {
            let entries = self.managed_sessions_for_project(project_channel_id).await;
            let _ = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(
                        &binding,
                        &entries,
                        None,
                        Some("⚠️ 請重新選擇一個 session。".to_string()),
                    ),
                )
                .await;
            return;
        };
        let Some(task) = self
            .task_registry
            .task_for_thread(thread_id)
            .filter(|task| task.project_channel_id == project_channel_id)
        else {
            let entries = self.managed_sessions_for_project(project_channel_id).await;
            let _ = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(
                        &binding,
                        &entries,
                        None,
                        Some("⚠️ 這筆 task 已不存在，清單已更新。".to_string()),
                    ),
                )
                .await;
            return;
        };

        if matches!(action, "select" | "view") {
            let entries = self.managed_sessions_for_project(project_channel_id).await;
            let _ = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(&binding, &entries, Some(thread_id), None),
                )
                .await;
            return;
        }

        if action == "close" {
            let snapshot = self
                .router
                .pool()
                .session_snapshot(&format!("discord:{thread_id}"))
                .await;
            if snapshot.externally_detached {
                let entries = self.managed_sessions_for_project(project_channel_id).await;
                let _ = comp
                    .edit_response(
                        &ctx.http,
                        session_manager_edit(
                            &binding,
                            &entries,
                            Some(thread_id),
                            Some(
                                "⚠️ Cursor 正在電腦上使用這個 session。請先正常離開 terminal 再關閉。"
                                    .to_string(),
                            ),
                        ),
                    )
                    .await;
                return;
            }
            let confirmation = EditInteractionResponse::new()
                .content("")
                .embed(
                    CreateEmbed::new()
                        .title("⚠️ Close Cursor session?")
                        .description(format!(
                            "即將關閉 **{}**。建議同時封存 Discord thread，讓它移出 active threads。repository 與 Cursor checkpoint 都不會刪除。",
                            suppress_mentions(&task.title)
                        ))
                        .colour(0xE74C3C),
                )
                .components(vec![CreateActionRow::Buttons(vec![
                    CreateButton::new(format!(
                        "oab_sessions:confirm_close_archive:{project_channel_id}:{thread_id}"
                    ))
                    .label("Close & archive")
                    .style(ButtonStyle::Danger),
                    CreateButton::new(format!(
                        "oab_sessions:confirm_close_only:{project_channel_id}:{thread_id}"
                    ))
                    .label("Close session only")
                    .style(ButtonStyle::Secondary),
                    CreateButton::new(format!(
                        "oab_sessions:view:{project_channel_id}:{thread_id}"
                    ))
                    .label("Keep session")
                    .style(ButtonStyle::Secondary),
                ])]);
            let _ = comp.edit_response(&ctx.http, confirmation).await;
            return;
        }

        if matches!(action, "confirm_close_archive" | "confirm_close_only") {
            let session_key = format!("discord:{thread_id}");
            let snapshot = self.router.pool().session_snapshot(&session_key).await;
            let mut session_closed = false;
            let mut note = if snapshot.externally_detached {
                "⚠️ Cursor 已在電腦上接手。請先正常離開 terminal，session 未被關閉。".to_string()
            } else {
                let dropped = self
                    .dispatcher
                    .cancel_buffered_thread("discord", &thread_id.to_string());
                let reset_result = if snapshot.state == SessionState::None {
                    Ok(())
                } else {
                    self.router.pool().reset_session(&session_key).await
                };
                match reset_result {
                    Ok(()) => {
                        session_closed = true;
                        match self.task_registry.set_state(thread_id, TaskState::Closed) {
                            Ok(_) if dropped > 0 => format!(
                                "✅ Session closed and {dropped} buffered message(s) dropped. Cursor checkpoint was kept."
                            ),
                            Ok(_) => {
                                "✅ Session closed. Cursor checkpoint was kept.".to_string()
                            }
                            Err(error) => format!(
                                "⚠️ Session closed, but task metadata could not be updated: {error}"
                            ),
                        }
                    }
                    Err(error) => format!("⚠️ Could not close session: {error}"),
                }
            };

            if let Some(updated) = self.task_registry.task_for_thread(thread_id) {
                if let Some(message_id) = updated.status_message_id {
                    if let Err(error) = ChannelId::new(thread_id)
                        .edit_message(
                            &ctx.http,
                            MessageId::new(message_id),
                            task_status_edit(&updated),
                        )
                        .await
                    {
                        tracing::warn!(%error, thread_id, "failed to refresh Task Status after manager close");
                    }
                }
            }
            if let Err(error) = self.upsert_project_home(ctx, &binding).await {
                tracing::warn!(%error, "failed to refresh Project Home after manager close");
            }

            if session_closed && action == "confirm_close_archive" {
                match archive_discord_thread(&ctx.http, thread_id).await {
                    Ok(()) => note.push_str(" Discord thread archived."),
                    Err(error) => {
                        tracing::warn!(%error, thread_id, "session closed but thread archive failed");
                        note.push_str("\n⚠️ Session was closed, but the Discord thread could not be archived. Check the bot's Manage Threads permission.");
                    }
                }
            }
            let entries = self.managed_sessions_for_project(project_channel_id).await;
            let _ = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(&binding, &entries, Some(thread_id), Some(note)),
                )
                .await;
            return;
        }

        if action == "archive" {
            let snapshot = self
                .router
                .pool()
                .session_snapshot(&format!("discord:{thread_id}"))
                .await;
            let note = if task.state != TaskState::Closed
                || snapshot.state != SessionState::None
                || snapshot.externally_detached
            {
                "⚠️ 請先關閉 session，再封存 Discord thread。".to_string()
            } else {
                match archive_discord_thread(&ctx.http, thread_id).await {
                    Ok(()) => "✅ Discord thread archived. Session record and Cursor checkpoint were kept."
                        .to_string(),
                    Err(error) => format!(
                        "⚠️ {error}. Check the bot's Manage Threads permission; the session record was kept."
                    ),
                }
            };
            let entries = self.managed_sessions_for_project(project_channel_id).await;
            let _ = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(&binding, &entries, Some(thread_id), Some(note)),
                )
                .await;
            return;
        }

        if action == "remove" {
            let snapshot = self
                .router
                .pool()
                .session_snapshot(&format!("discord:{thread_id}"))
                .await;
            let note = if task.state != TaskState::Closed
                || snapshot.state != SessionState::None
                || snapshot.externally_detached
            {
                "⚠️ 請先關閉 session，才能從清單移除。".to_string()
            } else {
                match self.task_registry.remove_task(thread_id) {
                    Ok(Some(_)) => {
                        "✅ 已從 Session Manager 移除。Discord thread 與 Cursor checkpoint 都有保留。"
                            .to_string()
                    }
                    Ok(None) => "ℹ️ 這筆 task 已經不在清單中。".to_string(),
                    Err(error) => format!("⚠️ 無法移除 task metadata：{error}"),
                }
            };
            let entries = self.managed_sessions_for_project(project_channel_id).await;
            let _ = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(&binding, &entries, None, Some(note)),
                )
                .await;
            if let Err(error) = self.upsert_project_home(ctx, &binding).await {
                tracing::warn!(%error, "failed to refresh Project Home after task removal");
            }
            return;
        }

        let entries = self.managed_sessions_for_project(project_channel_id).await;
        let _ = comp
            .edit_response(
                &ctx.http,
                session_manager_edit(
                    &binding,
                    &entries,
                    Some(thread_id),
                    Some("⚠️ This Session Manager action is no longer available.".to_string()),
                ),
            )
            .await;
    }
}
