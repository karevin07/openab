//! Discord UI for the dispatcher's pending queue.
//!
//! Split out of `discord.rs`: the Queue Manager card, its edit/replace modals,
//! the permission predicates that decide who may touch someone else's queued
//! request, and the interaction handlers behind them.
//!
//! The queue payloads themselves stay private to [`crate::dispatch`] — this
//! module only ever sees the `PendingMessage` / `ActiveMessage` projections the
//! dispatcher chooses to expose.
//!
//! Three small helpers deliberately stayed behind in `discord.rs`:
//! `queue_manager_button`, `should_post_queue_notice` and
//! `queue_enqueued_notice` are rendered by the Task Status Card and the adapter's
//! lifecycle hook, not by the Queue Manager, and nothing here uses them.

use crate::adapter::{ChannelRef, MessageRef, TaskLifecycleEvent};
use crate::discord::{
    build_sender_context, modal_input_value, suppress_mentions, task_status_edit,
    truncate_for_discord, Handler, InteractionCard, SELECT_MENU_PAGE_SIZE, SELECT_OPTION_TEXT_MAX,
};
use crate::dispatch::{ActiveMessage, PendingMessage};
use crate::task_registry::{TaskRecord, TaskState};
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInputText,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateModal,
    CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption,
};
use serenity::model::application::{ButtonStyle, ComponentInteractionDataKind, InputTextStyle};
use serenity::model::id::{ChannelId, MessageId};
use serenity::model::permissions::Permissions;
use serenity::prelude::*;

fn queue_wait_display(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub(crate) fn queue_manager_card(
    task: &TaskRecord,
    active_items: &[ActiveMessage],
    items: &[PendingMessage],
    selected_id: Option<u64>,
    note: Option<String>,
) -> InteractionCard {
    let selected = selected_id.and_then(|id| items.iter().find(|item| item.id == id));
    let active_item = active_items.first();
    let active = active_item.is_some() || task.state == TaskState::Running;
    let mut embed = CreateEmbed::new()
        .title(format!("📋 {} · Queue Manager", suppress_mentions(&task.title)))
        .description(
            "管理尚未送進 Cursor 的需求。移除或編輯 pending request 不會中斷目前執行中的工作。",
        )
        .colour(if active { 0x2ECC71 } else { 0x5865F2 })
        .field(
            "Cursor",
            if active {
                "🟢 目前有一輪正在執行"
            } else {
                "⚪ 目前沒有執行中的輪次"
            },
            true,
        )
        .field("Waiting", format!("**{}** request(s)", items.len()), true);

    if let Some(active_item) = active_item {
        let recovery_note = if active_item.recovered_from_active {
            "\n♻️ **Replayed after OpenAB restart**"
        } else {
            ""
        };
        embed = embed.field(
            "Active request",
            format!(
                "**#{} · {}**\n{}{}",
                active_item.id,
                suppress_mentions(&active_item.sender_name),
                suppress_mentions(&truncate_for_discord(&active_item.prompt, 900)),
                recovery_note,
            ),
            false,
        );
    }

    if let Some(item) = selected {
        let attachments = if item.attachment_count == 0 {
            "none".to_string()
        } else {
            format!("{} block(s)", item.attachment_count)
        };
        embed = embed
            .field(
                "Selected request",
                format!(
                    "**#{} · {}** · waiting {} · attachments: {attachments}{}",
                    item.id,
                    suppress_mentions(&item.sender_name),
                    queue_wait_display(item.waiting_seconds),
                    if item.recovered_from_active {
                        " · ♻️ recovered after restart"
                    } else {
                        ""
                    },
                ),
                false,
            )
            .field(
                "Prompt preview",
                suppress_mentions(&truncate_for_discord(&item.prompt, 1000)),
                false,
            );
    } else if items.is_empty() {
        embed = embed.field(
            "Pending requests",
            "_Queue is empty. New Discord messages will appear here while Cursor is busy._",
            false,
        );
    } else {
        let list = items
            .iter()
            .take(SELECT_MENU_PAGE_SIZE)
            .map(|item| {
                let preview = item.prompt.lines().next().unwrap_or("").trim();
                format!(
                    "`#{}` · {} · {}{}",
                    item.id,
                    suppress_mentions(&truncate_for_discord(preview, 70)),
                    queue_wait_display(item.waiting_seconds),
                    if item.recovered_from_active {
                        " · ♻️ recovered"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        embed = embed.field("Pending requests", list, false);
    }
    embed = embed.footer(CreateEmbedFooter::new(
        "Queue order and payloads survive OpenAB restarts · recovered active work may run again",
    ));

    let mut rows = Vec::new();
    if !items.is_empty() {
        let options = items
            .iter()
            .take(SELECT_MENU_PAGE_SIZE)
            .map(|item| {
                let preview = item.prompt.lines().next().unwrap_or("").trim();
                let mut option = CreateSelectMenuOption::new(
                    truncate_for_discord(
                        &format!("#{} · {}", item.id, preview),
                        SELECT_OPTION_TEXT_MAX,
                    ),
                    item.id.to_string(),
                )
                .description(truncate_for_discord(
                    &format!(
                        "{} · waiting {} · {} attachment(s){}",
                        item.sender_name,
                        queue_wait_display(item.waiting_seconds),
                        item.attachment_count,
                        if item.recovered_from_active {
                            " · recovered"
                        } else {
                            ""
                        }
                    ),
                    SELECT_OPTION_TEXT_MAX,
                ));
                if selected_id == Some(item.id) {
                    option = option.default_selection(true);
                }
                option
            })
            .collect();
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new("oab_queue:select", CreateSelectMenuKind::String { options })
                .placeholder("Choose a pending request"),
        ));
    }

    let mut primary = Vec::new();
    if active {
        primary.push(
            CreateButton::new("oab_queue:stop")
                .label("■ Stop current")
                .style(ButtonStyle::Danger),
        );
    }
    if let Some(active_item) = active_item {
        primary.push(
            CreateButton::new(format!("oab_queue:replace:{}", active_item.id))
                .label("✏️ Stop & replace")
                .style(ButtonStyle::Primary),
        );
    }
    if let Some(item) = selected {
        primary.push(
            CreateButton::new(format!("oab_queue:edit:{}", item.id))
                .label("✏️ Edit")
                .style(ButtonStyle::Primary),
        );
        primary.push(
            CreateButton::new(format!("oab_queue:remove:{}", item.id))
                .label("Remove")
                .style(ButtonStyle::Secondary),
        );
        if items.first().is_some_and(|first| first.id != item.id) {
            primary.push(
                CreateButton::new(format!("oab_queue:next:{}", item.id))
                    .label("Move next")
                    .style(ButtonStyle::Secondary),
            );
        }
    }
    if !primary.is_empty() {
        rows.push(CreateActionRow::Buttons(primary));
    }
    let mut maintenance = vec![
        CreateButton::new("oab_queue:refresh")
            .label("↻ Refresh")
            .style(ButtonStyle::Secondary),
    ];
    if !items.is_empty() {
        maintenance.push(
            CreateButton::new("oab_queue:clear_prompt")
                .label("Clear pending…")
                .style(ButtonStyle::Danger),
        );
    }
    rows.push(CreateActionRow::Buttons(maintenance));

    InteractionCard {
        content: note
            .map(|value| truncate_for_discord(&value, 1900))
            .unwrap_or_default(),
        embed,
        components: rows,
    }
}

pub(crate) fn queue_clear_confirmation_card(task: &TaskRecord, items: &[PendingMessage]) -> InteractionCard {
    let pending_count = items.len();
    let max_id = items.iter().map(|item| item.id).max().unwrap_or(0);
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("⚠️ Clear every pending request?")
            .description(format!(
                "This removes **{pending_count}** request(s) waiting in **{}**. The active Cursor turn will continue and removed requests cannot be recovered.",
                suppress_mentions(&task.title)
            ))
            .colour(0xE74C3C),
        components: vec![CreateActionRow::Buttons(vec![
            CreateButton::new(format!("oab_queue:confirm_clear:{max_id}"))
                .label("Clear pending")
                .style(ButtonStyle::Danger),
            CreateButton::new("oab_queue:refresh")
                .label("Keep queue")
                .style(ButtonStyle::Secondary),
        ])],
    }
}

pub(crate) fn queue_edit_modal(item: &PendingMessage) -> CreateModal {
    CreateModal::new(
        format!("oab_queue_edit:{}", item.id),
        "Edit pending request",
    )
    .components(vec![CreateActionRow::InputText(
        CreateInputText::new(
            InputTextStyle::Paragraph,
            "Update before Cursor receives it",
            "prompt",
        )
        .value(truncate_for_discord(&item.prompt, 4000))
        .min_length(1)
        .max_length(4000),
    )])
}

pub(crate) fn queue_replace_modal(item: &ActiveMessage) -> CreateModal {
    CreateModal::new(
        format!("oab_queue_replace:{}", item.id),
        "Stop and replace active request",
    )
    .components(vec![CreateActionRow::InputText(
        CreateInputText::new(
            InputTextStyle::Paragraph,
            "Revised request (completed changes are not rolled back)",
            "prompt",
        )
        .value(truncate_for_discord(&item.prompt, 4000))
        .min_length(1)
        .max_length(4000),
    )])
}

pub(crate) fn queue_manage_all_allowed(
    task: &TaskRecord,
    user_id: u64,
    permissions: Option<Permissions>,
) -> bool {
    task.created_by == user_id
        || permissions.is_some_and(|permissions| {
            permissions.contains(Permissions::ADMINISTRATOR)
                || permissions.contains(Permissions::MANAGE_THREADS)
        })
}

pub(crate) fn queue_item_allowed(
    task: &TaskRecord,
    sender_id: &str,
    user_id: u64,
    permissions: Option<Permissions>,
) -> bool {
    queue_manage_all_allowed(task, user_id, permissions)
        || sender_id.parse::<u64>().ok() == Some(user_id)
}

impl Handler {
    async fn refresh_queue_task_status(&self, ctx: &Context, task: &TaskRecord) {
        let Some(message_id) = task.status_message_id else {
            return;
        };
        if let Err(error) = ChannelId::new(task.thread_id)
            .edit_message(
                &ctx.http,
                MessageId::new(message_id),
                task_status_edit(task),
            )
            .await
        {
            tracing::warn!(%error, thread_id = task.thread_id, "failed to refresh Task Status after queue change");
        }
    }

    pub(crate) async fn handle_queue_control(
        &self,
        ctx: &Context,
        comp: &serenity::model::application::ComponentInteraction,
    ) {
        if let Err(message) = self
            .resolve_session_scope(ctx, comp.user.id.get(), comp.user.bot, comp.channel_id)
            .await
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let thread_id = comp.channel_id.get();
        let Some(mut task) = self.task_registry.task_for_thread(thread_id) else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Task metadata is unavailable. Refresh Project Home.")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };
        let action = comp.data.custom_id.strip_prefix("oab_queue:").unwrap_or("");
        let thread_id_string = thread_id.to_string();
        let permissions = comp.member.as_ref().and_then(|member| member.permissions);
        let manage_all = queue_manage_all_allowed(&task, comp.user.id.get(), permissions);

        if action == "open" {
            let items = self
                .dispatcher
                .pending_messages("discord", &thread_id_string);
            let active_items = self
                .dispatcher
                .active_messages("discord", &thread_id_string);
            let response = CreateInteractionResponse::Message(
                queue_manager_card(&task, &active_items, &items, None, None)
                    .into_message()
                    .ephemeral(true),
            );
            if let Err(error) = comp.create_response(&ctx.http, response).await {
                tracing::error!(%error, "failed to open queue manager");
            }
            return;
        }

        if let Some(id) = action
            .strip_prefix("edit:")
            .and_then(|value| value.parse::<u64>().ok())
        {
            let items = self
                .dispatcher
                .pending_messages("discord", &thread_id_string);
            let active_items = self
                .dispatcher
                .active_messages("discord", &thread_id_string);
            let Some(item) = items.iter().find(|item| item.id == id) else {
                let response = CreateInteractionResponse::UpdateMessage(
                    queue_manager_card(
                        &task,
                        &active_items,
                        &items,
                        None,
                        Some("⚠️ This request has already started or was removed.".to_string()),
                    )
                    .into_message(),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if !queue_item_allowed(&task, &item.sender_id, comp.user.id.get(), permissions) {
                let response = CreateInteractionResponse::UpdateMessage(
                    queue_manager_card(
                        &task,
                        &active_items,
                        &items,
                        Some(id),
                        Some("🚫 Only the request sender, task owner, or thread manager can edit it.".to_string()),
                    )
                    .into_message(),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            }
            if let Err(error) = comp
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Modal(queue_edit_modal(item)),
                )
                .await
            {
                tracing::error!(%error, queue_id = id, "failed to open queue edit modal");
            }
            return;
        }

        if let Some(id) = action
            .strip_prefix("replace:")
            .and_then(|value| value.parse::<u64>().ok())
        {
            let active_items = self
                .dispatcher
                .active_messages("discord", &thread_id_string);
            let Some(item) = active_items.iter().find(|item| item.id == id) else {
                let items = self
                    .dispatcher
                    .pending_messages("discord", &thread_id_string);
                let response = CreateInteractionResponse::UpdateMessage(
                    queue_manager_card(
                        &task,
                        &active_items,
                        &items,
                        None,
                        Some("⚠️ The active request changed. Refresh and try again.".to_string()),
                    )
                    .into_message(),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if !queue_item_allowed(&task, &item.sender_id, comp.user.id.get(), permissions) {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("🚫 Only the request sender, task owner, or thread manager can replace it.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            }
            let _ = comp
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Modal(queue_replace_modal(item)),
                )
                .await;
            return;
        }

        if action == "stop" {
            if !manage_all {
                let items = self
                    .dispatcher
                    .pending_messages("discord", &thread_id_string);
                let active_items = self
                    .dispatcher
                    .active_messages("discord", &thread_id_string);
                let response = CreateInteractionResponse::UpdateMessage(
                    queue_manager_card(
                        &task,
                        &active_items,
                        &items,
                        None,
                        Some("🚫 Only the task owner or a thread manager can stop the current turn.".to_string()),
                    )
                    .into_message(),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            }
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer queue stop control");
                return;
            }
            let session_key = format!("discord:{thread_id}");
            let note = match self.router.pool().cancel_session(&session_key).await {
                Ok(()) => "🛑 Stop signal sent. Pending requests remain in the queue.".to_string(),
                Err(error) => format!("⚠️ Could not stop the current task: {error}"),
            };
            task = self.task_registry.task_for_thread(thread_id).unwrap_or(task);
            let items = self
                .dispatcher
                .pending_messages("discord", &thread_id_string);
            let active_items = self
                .dispatcher
                .active_messages("discord", &thread_id_string);
            let _ = comp
                .edit_response(
                    &ctx.http,
                    queue_manager_card(&task, &active_items, &items, None, Some(note)).into_edit(),
                )
                .await;
            return;
        }

        if action == "clear_prompt" {
            let items = self
                .dispatcher
                .pending_messages("discord", &thread_id_string);
            let active_items = self
                .dispatcher
                .active_messages("discord", &thread_id_string);
            let response = if !manage_all {
                CreateInteractionResponse::UpdateMessage(
                    queue_manager_card(
                        &task,
                        &active_items,
                        &items,
                        None,
                        Some("🚫 Only the task owner or a thread manager can clear the queue.".to_string()),
                    )
                    .into_message(),
                )
            } else if items.is_empty() {
                CreateInteractionResponse::UpdateMessage(
                    queue_manager_card(
                        &task,
                        &active_items,
                        &items,
                        None,
                        Some("Queue is already empty.".to_string()),
                    )
                    .into_message(),
                )
            } else {
                CreateInteractionResponse::UpdateMessage(
                    queue_clear_confirmation_card(&task, &items).into_message(),
                )
            };
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }

        let selected_id = if action == "select" {
            match &comp.data.kind {
                ComponentInteractionDataKind::StringSelect { values } => {
                    values.first().and_then(|value| value.parse::<u64>().ok())
                }
                _ => None,
            }
        } else {
            action
                .strip_prefix("remove:")
                .or_else(|| action.strip_prefix("next:"))
                .and_then(|value| value.parse::<u64>().ok())
        };

        let mut note = None;
        let mut selected_after = selected_id;
        if let Some(max_id) = action
            .strip_prefix("confirm_clear:")
            .and_then(|value| value.parse::<u64>().ok())
        {
            if !manage_all {
                note = Some(
                    "🚫 Only the task owner or a thread manager can clear the queue.".to_string(),
                );
            } else {
                let removed = self.dispatcher.clear_pending_messages_through(
                    "discord",
                    &thread_id_string,
                    max_id,
                );
                if removed > 0 {
                    match self.task_registry.discard_queued(thread_id, removed) {
                        Ok(updated) => task = updated,
                        Err(error) => {
                            tracing::warn!(%error, thread_id, removed, "failed to reconcile cleared queue count");
                        }
                    }
                    note = Some(format!("🧹 Removed {removed} pending request(s)."));
                    selected_after = None;
                    self.refresh_queue_task_status(ctx, &task).await;
                } else {
                    note = Some("Queue is already empty.".to_string());
                }
            }
        } else if action.starts_with("remove:") {
            let allowed = selected_id
                .and_then(|id| {
                    self.dispatcher
                        .pending_messages("discord", &thread_id_string)
                        .into_iter()
                        .find(|item| item.id == id)
                })
                .is_some_and(|item| {
                    queue_item_allowed(&task, &item.sender_id, comp.user.id.get(), permissions)
                });
            let removed = allowed && selected_id.is_some_and(|id| {
                self.dispatcher
                    .remove_pending_message("discord", &thread_id_string, id)
            });
            if removed {
                match self.task_registry.discard_queued(thread_id, 1) {
                    Ok(updated) => task = updated,
                    Err(error) => {
                        tracing::warn!(%error, thread_id, "failed to reconcile removed queue item");
                    }
                }
                note = Some("🗑️ Pending request removed. The active turn was not interrupted.".to_string());
                selected_after = None;
                self.refresh_queue_task_status(ctx, &task).await;
            } else if !allowed {
                note = Some(
                    "🚫 Only the request sender, task owner, or thread manager can remove it."
                        .to_string(),
                );
            } else {
                note = Some("⚠️ This request has already started or was removed.".to_string());
                selected_after = None;
            }
        } else if action.starts_with("next:") {
            let item = selected_id.and_then(|id| {
                self.dispatcher
                    .pending_messages("discord", &thread_id_string)
                    .into_iter()
                    .find(|item| item.id == id)
            });
            let allowed = item.as_ref().is_some_and(|item| {
                queue_item_allowed(&task, &item.sender_id, comp.user.id.get(), permissions)
            });
            if allowed
                && selected_id.is_some_and(|id| {
                    self.dispatcher.move_pending_message_to_front(
                        "discord",
                        &thread_id_string,
                        id,
                    )
                })
            {
                note = Some("⬆️ Request moved to the next queue position.".to_string());
            } else if !allowed {
                note = Some(
                    "🚫 Only the request sender, task owner, or thread manager can reorder it."
                        .to_string(),
                );
            } else {
                note = Some("⚠️ This request has already started or was removed.".to_string());
                selected_after = None;
            }
        } else if !matches!(action, "select" | "refresh") {
            note = Some("⚠️ This queue control is no longer available.".to_string());
            selected_after = None;
        }

        let items = self
            .dispatcher
            .pending_messages("discord", &thread_id_string);
        let active_items = self
            .dispatcher
            .active_messages("discord", &thread_id_string);
        if selected_after.is_some_and(|id| !items.iter().any(|item| item.id == id)) {
            selected_after = None;
        }
        let response = CreateInteractionResponse::UpdateMessage(
            queue_manager_card(&task, &active_items, &items, selected_after, note).into_message(),
        );
        if let Err(error) = comp.create_response(&ctx.http, response).await {
            tracing::error!(%error, action, "failed to update queue manager");
        }
    }

    pub(crate) async fn handle_queue_edit_modal(
        &self,
        ctx: &Context,
        modal: &serenity::model::application::ModalInteraction,
    ) {
        if let Err(message) = self
            .resolve_session_scope(ctx, modal.user.id.get(), modal.user.bot, modal.channel_id)
            .await
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let thread_id = modal.channel_id.get();
        let Some(task) = self.task_registry.task_for_thread(thread_id) else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Task metadata is unavailable.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let id = modal
            .data
            .custom_id
            .strip_prefix("oab_queue_edit:")
            .and_then(|value| value.parse::<u64>().ok());
        let prompt = modal_input_value(modal, "prompt")
            .map(str::trim)
            .unwrap_or("");
        let thread_id_string = thread_id.to_string();
        let permissions = modal.member.as_ref().and_then(|member| member.permissions);
        let pending_before = self
            .dispatcher
            .pending_messages("discord", &thread_id_string);
        let pending_item = id.and_then(|id| pending_before.iter().find(|item| item.id == id));
        let allowed = pending_item.is_some_and(|item| {
                queue_item_allowed(&task, &item.sender_id, modal.user.id.get(), permissions)
            });
        let note = if prompt.is_empty() {
            "⚠️ Request cannot be empty.".to_string()
        } else if pending_item.is_none() {
            "⚠️ This request has already started or was removed.".to_string()
        } else if !allowed {
            "🚫 Only the request sender, task owner, or thread manager can edit it.".to_string()
        } else if id.is_some_and(|id| {
            self.dispatcher
                .edit_pending_message("discord", &thread_id_string, id, prompt)
        }) {
            "✅ Pending request updated before Cursor received it.".to_string()
        } else {
            "⚠️ This request has already started or was removed.".to_string()
        };
        let items = self
            .dispatcher
            .pending_messages("discord", &thread_id_string);
        let active_items = self
            .dispatcher
            .active_messages("discord", &thread_id_string);
        let selected = id.filter(|id| items.iter().any(|item| item.id == *id));
        let response = CreateInteractionResponse::UpdateMessage(
            queue_manager_card(
                &task,
                &active_items,
                &items,
                selected,
                Some(note),
            )
            .into_message(),
        );
        if let Err(error) = modal.create_response(&ctx.http, response).await {
            tracing::error!(%error, queue_id = ?id, "failed to update edited queue request");
        }
    }

    async fn enqueue_claimed_replacement_prompt(
        &self,
        ctx: &Context,
        task: &TaskRecord,
        user: &serenity::model::user::User,
        active_message_id: u64,
        prompt: String,
    ) -> Result<(), String> {
        self.task_registry
            .record_prompt(task.thread_id, &prompt)
            .map_err(|error| format!("Could not save replacement request: {error}"))?;
        let preview = suppress_mentions(&truncate_for_discord(&prompt, 1800));
        let user_id = user.id.get();
        let trigger = ChannelId::new(task.thread_id)
            .send_message(
                &ctx.http,
                CreateMessage::new().content(format!(
                    "🔁 **Replacement request from <@{user_id}>**\n{preview}"
                )),
            )
            .await
            .map_err(|error| format!("Could not post replacement to the task thread: {error}"))?;
        let thread_channel = ChannelRef {
            platform: "discord".into(),
            channel_id: task.thread_id.to_string(),
            thread_id: None,
            parent_id: Some(task.project_channel_id.to_string()),
            origin_event_id: None,
        };
        let trigger_ref = MessageRef {
            channel: thread_channel.clone(),
            message_id: trigger.id.to_string(),
        };
        let sender = build_sender_context(
            &user_id.to_string(),
            &user.name,
            user.global_name.as_deref().unwrap_or(&user.name),
            &task.thread_id.to_string(),
            Some(&task.project_channel_id.to_string()),
            false,
            &chrono::Utc::now().to_rfc3339(),
            &trigger.id.to_string(),
            &ctx.cache.current_user().id.to_string(),
        );
        let adapter = self.discord_adapter(ctx);
        adapter
            .update_task_lifecycle(&thread_channel, TaskLifecycleEvent::Enqueued)
            .await
            .map_err(|error| format!("Could not update queue lifecycle: {error}"))?;
        let message = crate::dispatch::BufferedMessage {
            sender_json: serde_json::to_string(&sender).unwrap_or_default(),
            sender_name: sender.sender_name,
            estimated_tokens: crate::dispatch::estimate_tokens(&prompt, &[]),
            prompt,
            extra_blocks: Vec::new(),
            trigger_msg: trigger_ref,
            arrived_at: std::time::Instant::now(),
            other_bot_present: false,
            recipient: None,
        };
        if let Err(error) = self
            .dispatcher
            .enqueue_claimed_replacement(active_message_id, message)
        {
            let _ = adapter
                .update_task_lifecycle(
                    &thread_channel,
                    TaskLifecycleEvent::Failed {
                        message: error.to_string(),
                    },
                )
                .await;
            return Err(format!("Could not enqueue replacement: {error}"));
        }
        Ok(())
    }

    pub(crate) async fn handle_queue_replace_modal(
        &self,
        ctx: &Context,
        modal: &serenity::model::application::ModalInteraction,
    ) {
        if let Err(message) = self
            .resolve_session_scope(ctx, modal.user.id.get(), modal.user.bot, modal.channel_id)
            .await
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(message)
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let thread_id = modal.channel_id.get();
        let thread_id_string = thread_id.to_string();
        let Some(task) = self.task_registry.task_for_thread(thread_id) else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Task metadata is unavailable.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let active_id = modal
            .data
            .custom_id
            .strip_prefix("oab_queue_replace:")
            .and_then(|value| value.parse::<u64>().ok());
        let prompt = modal_input_value(modal, "prompt")
            .map(str::trim)
            .unwrap_or("");
        let active_items = self
            .dispatcher
            .active_messages("discord", &thread_id_string);
        let active_item = active_id.and_then(|id| active_items.iter().find(|item| item.id == id));
        let permissions = modal.member.as_ref().and_then(|member| member.permissions);
        let allowed = active_item.is_some_and(|item| {
            queue_item_allowed(&task, &item.sender_id, modal.user.id.get(), permissions)
        });

        let immediate_error = if prompt.is_empty() {
            Some("⚠️ Replacement request cannot be empty.")
        } else if active_item.is_none() {
            Some("⚠️ The active request changed. Refresh and try again.")
        } else if !allowed {
            Some("🚫 Only the request sender, task owner, or thread manager can replace it.")
        } else {
            None
        };
        if let Some(message) = immediate_error {
            let items = self
                .dispatcher
                .pending_messages("discord", &thread_id_string);
            let response = CreateInteractionResponse::UpdateMessage(
                queue_manager_card(
                    &task,
                    &active_items,
                    &items,
                    None,
                    Some(message.to_string()),
                )
                .into_message(),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let active_id = active_id.expect("validated active queue ID");
        if !self
            .dispatcher
            .claim_active_for_replace("discord", &thread_id_string, active_id)
        {
            let items = self
                .dispatcher
                .pending_messages("discord", &thread_id_string);
            let active_items = self
                .dispatcher
                .active_messages("discord", &thread_id_string);
            let response = CreateInteractionResponse::UpdateMessage(
                queue_manager_card(
                    &task,
                    &active_items,
                    &items,
                    None,
                    Some("⚠️ The active request changed before it could be claimed.".to_string()),
                )
                .into_message(),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        if let Err(error) = modal.defer(&ctx.http).await {
            self.dispatcher.release_active_replace(active_id);
            tracing::error!(%error, active_id, "failed to defer Stop & Replace modal");
            return;
        }

        let result = match self
            .router
            .pool()
            .cancel_session(&format!("discord:{thread_id}"))
            .await
        {
            Ok(()) => {
                self.enqueue_claimed_replacement_prompt(
                    ctx,
                    &task,
                    &modal.user,
                    active_id,
                    prompt.to_string(),
                )
                .await
            }
            Err(error) => Err(format!("Could not stop the active request: {error}")),
        };
        self.dispatcher.release_active_replace(active_id);

        let task = self.task_registry.task_for_thread(thread_id).unwrap_or(task);
        let items = self
            .dispatcher
            .pending_messages("discord", &thread_id_string);
        let active_items = self
            .dispatcher
            .active_messages("discord", &thread_id_string);
        let note = result.map_or_else(
            |error| format!("⚠️ {error}"),
            |()| "🔁 Stop signal sent; the revised request is next in queue.".to_string(),
        );
        let _ = modal
            .edit_response(
                &ctx.http,
                queue_manager_card(&task, &active_items, &items, None, Some(note)).into_edit(),
            )
            .await;
        self.refresh_queue_task_status(ctx, &task).await;
    }
}
