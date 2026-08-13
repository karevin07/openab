use crate::acp::protocol::{ConfigOption, UsageReport};
use crate::acp::{ContentBlock, SessionSnapshot, SessionState};
use crate::adapter::{
    AdapterRouter, ChannelRef, ChatAdapter, MessageRef, SenderContext, TaskLifecycleEvent,
};
use crate::bot_turns::{BotTurnTracker, TurnAction, TurnSeverity, BOT_TURN_LIMIT_WARNING_PREFIX};
use crate::config::{AllowBots, AllowUsers, DiscordProjectActionConfig, SttConfig};
use crate::discord_admin::{
    AdminInventory, AdminStatus, CategoryPlan, ChannelPlan, CleanupCandidates, CreatedCategory,
    CreatedTextChannel, DeletedResource, DeletionPlan, DiscordAdminClient,
};
use crate::dispatch::DispatchTarget;
use crate::format;
use crate::media;
use crate::project_registry::{ProjectAccessTarget, ProjectBinding, ProjectRegistry};
use crate::remind::{self, ReminderStore};
use crate::task_registry::{TaskRecord, TaskRegistry, TaskState};
use crate::trust::l3_gate_applies;
use crate::workspace_attachment::prepare_workspace_pngs;
use async_trait::async_trait;
use serenity::builder::{
    CreateActionRow, CreateAttachment, CreateAutocompleteResponse, CreateButton, CreateChannel,
    CreateCommand, CreateCommandOption, CreateEmbed, CreateEmbedFooter, CreateInputText,
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    CreateMessage, CreateModal, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption,
    CreateThread, EditChannel, EditInteractionResponse, EditMessage, GetMessages,
};
use serenity::http::Http;
use serenity::model::application::{
    ActionRowComponent, ButtonStyle, Command, CommandOptionType, ComponentInteractionDataKind,
    InputTextStyle, Interaction,
};
use serenity::model::channel::{
    AutoArchiveDuration, ChannelType, Message, MessageType, PermissionOverwrite,
    PermissionOverwriteType, Reaction, ReactionType,
};
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId, UserId};
use serenity::model::permissions::Permissions;
use serenity::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::sync::{Arc, OnceLock};
use tracing::{debug, error, info, warn};

/// Hard cap on consecutive bot messages in a channel or thread.
/// Prevents runaway loops between multiple bots in "all" mode.
const MAX_CONSECUTIVE_BOT_TURNS: u32 = 1000;

/// Maximum entries in the participation cache before eviction.
const PARTICIPATION_CACHE_MAX: usize = 1000;

/// Discord StringSelectMenu hard limit on options.
const SELECT_MENU_PAGE_SIZE: usize = 25;

/// Discord caps select menu option labels and descriptions at 100
/// characters; anything longer makes the entire interaction response fail
/// with "Invalid Form Body", which surfaces to users as "The application
/// did not respond". (Hit in the wild when a backend model description
/// exceeded the cap.)
const SELECT_OPTION_TEXT_MAX: usize = 100;

/// Keep workspace catalogs comfortably below Discord's 2000-character limit.
const WORKSPACE_LIST_LIMIT: usize = 25;

/// Truncate to at most `max` characters (not bytes — Discord counts
/// characters, and slicing on a byte boundary would panic on multi-byte
/// UTF-8). Appends '…' when truncated.
fn truncate_for_discord(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn inline_code(value: &str) -> String {
    format!("`{}`", value.replace('`', "'"))
}

fn suppress_mentions(value: &str) -> String {
    value.replace('@', "@\u{200b}")
}

fn workspace_display(value: &str, aliases: &std::collections::HashMap<String, String>) -> String {
    if let Some(alias) = value.strip_prefix('@') {
        return aliases.get(alias).map_or_else(
            || inline_code(value),
            |path| format!("{} ({})", inline_code(value), inline_code(path)),
        );
    }

    aliases
        .iter()
        .find(|(_, path)| path.as_str() == value)
        .map_or_else(
            || inline_code(value),
            |(alias, _)| {
                format!(
                    "{} ({})",
                    inline_code(&format!("@{alias}")),
                    inline_code(value)
                )
            },
        )
}

fn format_workspace_status(
    snapshot: &SessionSnapshot,
    channel_default: Option<&str>,
    aliases: &std::collections::HashMap<String, String>,
) -> String {
    let current = snapshot
        .working_dir
        .as_deref()
        .map(|path| workspace_display(path, aliases))
        .unwrap_or_else(|| "_No session workspace yet_".to_string());
    let default = channel_default
        .map(|spec| workspace_display(spec, aliases))
        .unwrap_or_else(|| "_Not configured_".to_string());
    let mut names: Vec<_> = aliases.keys().map(|name| format!("@{name}")).collect();
    names.sort();
    let available = if names.is_empty() {
        "_None_".to_string()
    } else {
        names
            .iter()
            .take(10)
            .map(|name| inline_code(name))
            .collect::<Vec<_>>()
            .join(", ")
    };

    truncate_for_discord(
        &format!(
            "📁 **Workspace status**\nCurrent session: {current}\nChannel default: {default}\nAvailable aliases: {available}"
        ),
        1900,
    )
}

fn format_workspace_list(
    aliases: &std::collections::HashMap<String, String>,
    channel_default: Option<&str>,
) -> String {
    let mut entries: Vec<_> = aliases.iter().collect();
    entries.sort_by_key(|(alias, _)| *alias);

    let mut lines = vec!["📚 **Available workspaces**".to_string()];
    if entries.is_empty() {
        lines.push("_No workspace aliases configured._".to_string());
    } else {
        lines.extend(
            entries
                .iter()
                .take(WORKSPACE_LIST_LIMIT)
                .map(|(alias, path)| {
                    format!(
                        "• {} — {}",
                        inline_code(&format!("@{alias}")),
                        inline_code(path)
                    )
                }),
        );
        if entries.len() > WORKSPACE_LIST_LIMIT {
            lines.push(format!(
                "_…and {} more workspace(s)._",
                entries.len() - WORKSPACE_LIST_LIMIT
            ));
        }
    }
    let default = channel_default
        .map(|spec| workspace_display(spec, aliases))
        .unwrap_or_else(|| "_Not configured_".to_string());
    lines.push(format!("Channel default: {default}"));
    truncate_for_discord(&lines.join("\n"), 1900)
}

fn session_state_presentation(
    state: SessionState,
    externally_detached: bool,
) -> (&'static str, u32, &'static str) {
    if externally_detached {
        return (
            "Cursor 接手中",
            0x9B59B6,
            "主機上的 Cursor terminal 正在使用這個 session；正常離開後再回 Discord 繼續。",
        );
    }
    match state {
        SessionState::Active => (
            "執行中",
            0x2ECC71,
            "Agent 正在處理任務；可停止目前工作，但不要同時從 Cursor 操作。",
        ),
        SessionState::Suspended => (
            "可在 Discord 接續",
            0xF1C40F,
            "Session context 已保存；在這個 thread 傳送下一則訊息即可接續。",
        ),
        SessionState::Persisted => (
            "可在 Discord 接續",
            0x3498DB,
            "Session context 已保存；在這個 thread 傳送下一則訊息即可載入。",
        ),
        SessionState::None => (
            "尚未開始",
            0x95A5A6,
            "傳送第一個開發需求以建立 Cursor session，或 attach 一個本機 chat。",
        ),
    }
}

const SESSION_CLOSE_CONFIRMATION: &str = "⚠️ **Close this session?** This stops current work, drops buffered messages, and removes the OpenAB session mapping. The repository, Discord thread, and Cursor checkpoint are kept; the next message starts a new session context.";

fn session_closed_note(dropped: usize) -> String {
    if dropped > 0 {
        format!(
            "✅ Session closed and {dropped} buffered message(s) dropped. Cursor checkpoint was kept; send a new message to start a fresh session context."
        )
    } else {
        "✅ Session closed. Cursor checkpoint was kept; send a new message to start a fresh session context."
            .to_string()
    }
}

struct InteractionCard {
    content: String,
    embed: CreateEmbed,
    components: Vec<CreateActionRow>,
}

impl InteractionCard {
    fn into_message(self) -> CreateInteractionResponseMessage {
        CreateInteractionResponseMessage::new()
            .content(self.content)
            .embed(self.embed)
            .components(self.components)
    }

    fn into_edit(self) -> EditInteractionResponse {
        EditInteractionResponse::new()
            .content(self.content)
            .embed(self.embed)
            .components(self.components)
    }
}

fn session_control_card(
    snapshot: &SessionSnapshot,
    aliases: &HashMap<String, String>,
    channel_id: u64,
    note: Option<String>,
) -> InteractionCard {
    let (state, colour, guidance) =
        session_state_presentation(snapshot.state, snapshot.externally_detached);
    let workspace = snapshot
        .working_dir
        .as_deref()
        .map(|path| workspace_display(path, aliases))
        .unwrap_or_else(|| "_Not assigned_".to_string());
    let embed = CreateEmbed::new()
        .title("🧵 Session 狀態與控制")
        .description(guidance)
        .colour(colour)
        .field("狀態", format!("**{state}**"), true)
        .field("Workspace", workspace, true)
        .field(
            "Discord thread",
            inline_code(&channel_id.to_string()),
            false,
        )
        .footer(CreateEmbedFooter::new(
            "一個 Discord thread 對應一個 Cursor session",
        ));
    let has_session = snapshot.state != SessionState::None;
    let buttons = CreateActionRow::Buttons(vec![
        CreateButton::new("oab_session:refresh")
            .label("↻ Refresh")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_session:cancel")
            .label("■ Stop")
            .style(ButtonStyle::Secondary)
            .disabled(snapshot.state != SessionState::Active || snapshot.externally_detached),
        CreateButton::new("oab_session:detach")
            .label("↗ Prepare for Cursor")
            .style(ButtonStyle::Primary)
            .disabled(!has_session || snapshot.externally_detached),
        CreateButton::new("oab_session:close")
            .label("✕ Close")
            .style(ButtonStyle::Danger)
            .disabled(!has_session),
        CreateButton::new("oab_help:open")
            .label("? Help")
            .style(ButtonStyle::Secondary),
    ]);
    InteractionCard {
        content: note
            .map(|value| truncate_for_discord(&value, 1900))
            .unwrap_or_default(),
        embed,
        components: vec![buttons],
    }
}

fn session_control_message(
    snapshot: &SessionSnapshot,
    aliases: &HashMap<String, String>,
    channel_id: u64,
    note: Option<String>,
) -> CreateInteractionResponseMessage {
    session_control_card(snapshot, aliases, channel_id, note).into_message()
}

fn session_control_edit(
    snapshot: &SessionSnapshot,
    aliases: &HashMap<String, String>,
    channel_id: u64,
    note: Option<String>,
) -> EditInteractionResponse {
    session_control_card(snapshot, aliases, channel_id, note).into_edit()
}

fn project_access_display(binding: &ProjectBinding) -> String {
    let additional = binding
        .access_user_ids
        .iter()
        .map(|id| format!("<@{id}>"))
        .chain(binding.access_role_ids.iter().map(|id| format!("<@&{id}>")))
        .collect::<Vec<_>>();
    let display = if additional.is_empty() {
        format!("<@{}> (creator)", binding.created_by)
    } else {
        format!(
            "<@{}> (creator), {}",
            binding.created_by,
            additional.join(", ")
        )
    };
    truncate_for_discord(&display, 1000)
}

fn task_state_presentation(state: TaskState) -> (&'static str, &'static str, u32) {
    match state {
        TaskState::Queued => ("⏳", "Queued", 0xF1C40F),
        TaskState::Running => ("🟢", "Running", 0x2ECC71),
        TaskState::Ready => ("🟦", "Waiting for you", 0x3498DB),
        TaskState::Cursor => ("🖥️", "Cursor 接手中", 0x9B59B6),
        TaskState::Failed => ("🔴", "Failed", 0xE74C3C),
        TaskState::Closed => ("⚫", "Closed", 0x95A5A6),
    }
}

fn task_status_embed(task: &TaskRecord) -> CreateEmbed {
    let (icon, state, colour) = task_state_presentation(task.state);
    let mut embed = CreateEmbed::new()
        .title(format!("{icon} {}", task.title))
        .description(match task.state {
            TaskState::Queued => "需求已排入 queue，OpenAB 會依序處理。",
            TaskState::Running => "Cursor agent 正在處理目前的需求。",
            TaskState::Ready => "本輪已完成；直接在 thread 回覆即可接續同一個 session。",
            TaskState::Cursor => "Session 已交給主機上的 Cursor terminal。",
            TaskState::Failed => "本輪執行失敗；請查看下方訊息後重試或調整需求。",
            TaskState::Closed => "Session 已關閉；新訊息將建立新的 session context。",
        })
        .colour(colour)
        .field("狀態", format!("**{state}**"), true)
        .field(
            "Workspace",
            inline_code(&format!("@{}", task.workspace_alias)),
            true,
        )
        .field("Started by", format!("<@{}>", task.created_by), true)
        .field("Task thread", format!("<#{}>", task.thread_id), false);
    if task.queued_messages > 0 {
        embed = embed.field(
            "Queue",
            format!("{} message(s) waiting", task.queued_messages),
            true,
        );
    }
    if let Some(error) = task.last_error.as_deref() {
        embed = embed.field("Last error", truncate_for_discord(error, 900), false);
    }
    if task.state == TaskState::Cursor {
        embed = embed.field(
            "在電腦執行",
            format!(
                "```bash\nmake session-resume THREAD_ID={}\n```\n正常離開 Cursor 後，回到這個 thread 傳送下一則訊息。",
                task.thread_id
            ),
            false,
        );
    }
    embed.footer(CreateEmbedFooter::new(format!(
        "Updated {} UTC",
        task.updated_at.format("%Y-%m-%d %H:%M")
    )))
}

fn task_control_rows(task: &TaskRecord) -> Vec<CreateActionRow> {
    let help = || {
        CreateButton::new("oab_help:open")
            .label("? Help")
            .style(ButtonStyle::Secondary)
    };
    let project = || {
        CreateButton::new_link(format!(
            "https://discord.com/channels/{}/{}",
            task.guild_id, task.project_channel_id
        ))
        .label("← Project")
    };
    let buttons = match task.state {
        TaskState::Queued => vec![
            CreateButton::new("oab_session:refresh")
                .label("↻ Check status")
                .style(ButtonStyle::Secondary),
            help(),
        ],
        TaskState::Running => vec![
            CreateButton::new("oab_session:cancel")
                .label("■ Stop")
                .style(ButtonStyle::Danger),
            CreateButton::new("oab_session:refresh")
                .label("↻ Refresh")
                .style(ButtonStyle::Secondary),
            help(),
        ],
        TaskState::Ready => vec![
            CreateButton::new("oab_task:continue")
                .label("💬 Continue")
                .style(ButtonStyle::Primary),
            CreateButton::new("oab_session:detach")
                .label("🖥️ Continue on computer")
                .style(ButtonStyle::Secondary),
            CreateButton::new("oab_session:close")
                .label("✕ Close")
                .style(ButtonStyle::Danger),
            help(),
        ],
        TaskState::Cursor => vec![
            CreateButton::new("oab_session:refresh")
                .label("↻ Check status")
                .style(ButtonStyle::Secondary),
            project(),
            help(),
        ],
        TaskState::Failed => {
            let mut buttons = Vec::new();
            if task.last_prompt.is_some() {
                buttons.push(
                    CreateButton::new("oab_task:retry")
                        .label("↻ Retry")
                        .style(ButtonStyle::Primary),
                );
                buttons.push(
                    CreateButton::new("oab_task:edit")
                        .label("✏️ Edit and retry")
                        .style(ButtonStyle::Secondary),
                );
            }
            buttons.extend([
                CreateButton::new("oab_task:error")
                    .label("🔍 Error details")
                    .style(ButtonStyle::Secondary),
                CreateButton::new("oab_session:detach")
                    .label("🖥️ Use Cursor")
                    .style(ButtonStyle::Secondary),
                CreateButton::new("oab_session:close")
                    .label("✕ Close")
                    .style(ButtonStyle::Danger),
            ]);
            buttons
        }
        TaskState::Closed => vec![project(), help()],
    };
    let mut rows = vec![CreateActionRow::Buttons(buttons)];
    if task.state == TaskState::Failed && task.last_prompt.is_some() {
        rows.push(CreateActionRow::Buttons(vec![project(), help()]));
    }
    rows
}

fn task_status_message(task: &TaskRecord) -> CreateMessage {
    CreateMessage::new()
        .embed(task_status_embed(task))
        .components(task_control_rows(task))
}

fn task_status_edit(task: &TaskRecord) -> EditMessage {
    EditMessage::new()
        .content("")
        .embed(task_status_embed(task))
        .components(task_control_rows(task))
}

fn task_status_interaction_edit(
    task: &TaskRecord,
    note: Option<String>,
) -> EditInteractionResponse {
    EditInteractionResponse::new()
        .content(
            note.map(|value| truncate_for_discord(&value, 1900))
                .unwrap_or_default(),
        )
        .embed(task_status_embed(task))
        .components(task_control_rows(task))
}

fn project_recent_tasks(tasks: &[TaskRecord]) -> String {
    if tasks.is_empty() {
        return "_尚無 task。點 **New task** 開始。_".to_string();
    }
    tasks
        .iter()
        .take(5)
        .map(|task| {
            let (icon, state, _) = task_state_presentation(task.state);
            format!(
                "{icon} <#{}> · {state} · <t:{}:R>",
                task.thread_id,
                task.updated_at.timestamp()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn project_info_embed(binding: &ProjectBinding) -> CreateEmbed {
    CreateEmbed::new()
        .title(format!("📁 @{}", binding.workspace_alias))
        .description(
            "This channel is a project home. Each top-level development request creates an isolated Discord thread and Cursor session.",
        )
        .colour(0x5865F2)
        .field(
            "Workspace",
            inline_code(&format!("@{}", binding.workspace_alias)),
            true,
        )
        .field("Access", project_access_display(binding), false)
        .footer(CreateEmbedFooter::new("Managed by OpenAB"))
}

fn project_welcome_components(tasks: &[TaskRecord]) -> Vec<CreateActionRow> {
    let mut rows = vec![CreateActionRow::Buttons(vec![
        CreateButton::new("oab_project:new")
            .label("▶ New task")
            .style(ButtonStyle::Primary),
        CreateButton::new("oab_project:attach")
            .label("📤 Attach local chat")
            .style(ButtonStyle::Success),
        CreateButton::new("oab_project:actions")
            .label("⚡ Quick actions")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_project:sessions")
            .label("🧠 Sessions")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_help:open")
            .label("? Help")
            .style(ButtonStyle::Secondary),
    ])];
    if !tasks.is_empty() {
        let options = tasks
            .iter()
            .take(10)
            .map(|task| {
                let (_, state, _) = task_state_presentation(task.state);
                CreateSelectMenuOption::new(
                    truncate_for_discord(&task.title, SELECT_OPTION_TEXT_MAX),
                    task.thread_id.to_string(),
                )
                .description(truncate_for_discord(
                    &format!("{state} · {} UTC", task.updated_at.format("%m-%d %H:%M")),
                    SELECT_OPTION_TEXT_MAX,
                ))
            })
            .collect();
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new("oab_recent_task", CreateSelectMenuKind::String { options })
                .placeholder("Recent tasks"),
        ));
    }
    rows
}

fn project_actions_message(
    binding: &ProjectBinding,
    actions: &[&DiscordProjectActionConfig],
) -> CreateInteractionResponseMessage {
    let mut embed = CreateEmbed::new()
        .title(format!("⚡ @{} · Quick actions", binding.workspace_alias))
        .description(
            "選擇常用工作後，OpenAB 會開啟可編輯的 New Task 視窗。確認內容後才會建立 Agent session；這裡不會直接執行任意 shell。",
        )
        .colour(0xF1C40F)
        .field(
            "Repository",
            inline_code(&format!("@{}", binding.workspace_alias)),
            true,
        );
    if actions.is_empty() {
        return CreateInteractionResponseMessage::new()
            .embed(embed.field(
                "尚未設定",
                "請由管理者在 `[[discord.project_actions]]` 加入這個 workspace 的常用工作。",
                false,
            ))
            .ephemeral(true);
    }

    let options = actions
        .iter()
        .take(SELECT_MENU_PAGE_SIZE)
        .map(|action| {
            let mut option = CreateSelectMenuOption::new(action.label.trim(), &action.id);
            if !action.description.trim().is_empty() {
                option = option.description(action.description.trim());
            }
            option
        })
        .collect();
    let placeholder = if actions.len() > SELECT_MENU_PAGE_SIZE {
        embed = embed.footer(CreateEmbedFooter::new(format!(
            "顯示前 {SELECT_MENU_PAGE_SIZE} 個，共 {} 個 actions",
            actions.len()
        )));
        format!("選擇常用工作（前 {SELECT_MENU_PAGE_SIZE} 個）")
    } else {
        "選擇常用工作".to_string()
    };
    let select = CreateSelectMenu::new(
        "oab_project_actions",
        CreateSelectMenuKind::String { options },
    )
    .placeholder(placeholder);
    CreateInteractionResponseMessage::new()
        .embed(embed)
        .components(vec![CreateActionRow::SelectMenu(select)])
        .ephemeral(true)
}

fn project_task_modal(title: Option<&str>, prompt: Option<&str>) -> CreateModal {
    let mut title_input =
        CreateInputText::new(InputTextStyle::Short, "Task title", "title")
            .placeholder("Fix login redirect")
            .max_length(100)
            .required(false);
    if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
        title_input = title_input.value(truncate_for_discord(title.trim(), 100));
    }
    let mut prompt_input = CreateInputText::new(
        InputTextStyle::Paragraph,
        "What should Cursor do?",
        "prompt",
    )
    .placeholder("Describe the outcome, constraints, and how to verify it")
    .min_length(1)
    .max_length(4000);
    if let Some(prompt) = prompt.filter(|value| !value.trim().is_empty()) {
        prompt_input = prompt_input.value(truncate_for_discord(prompt.trim(), 4000));
    }
    CreateModal::new("oab_project_new", "Start a new development task").components(vec![
        CreateActionRow::InputText(title_input),
        CreateActionRow::InputText(prompt_input),
    ])
}

fn project_welcome_message(binding: &ProjectBinding, tasks: &[TaskRecord]) -> CreateMessage {
    let embed = project_info_embed(binding)
        .field(
            "1 · Start a task",
            "Tap **New task** for a custom request, or **Quick actions** for a repository shortcut.",
            false,
        )
        .field(
            "2 · Attach a local chat",
            "Use the button below to select a recent exited Cursor chat from this repository.",
            false,
        )
        .field(
            "3 · Continue and control",
            "Reply in the task thread to preserve context. Run `/session status` for lifecycle controls.",
            false,
        )
        .field("Recent tasks", project_recent_tasks(tasks), false);
    CreateMessage::new()
        .embed(embed)
        .components(project_welcome_components(tasks))
}

fn project_welcome_edit(binding: &ProjectBinding, tasks: &[TaskRecord]) -> EditMessage {
    let embed = project_info_embed(binding)
        .field(
            "1 · Start a task",
            "Tap **New task** for a custom request, or **Quick actions** for a repository shortcut.",
            false,
        )
        .field(
            "2 · Attach a local chat",
            "Use the button below to select a recent exited Cursor chat from this repository.",
            false,
        )
        .field(
            "3 · Continue and control",
            "Reply in the task thread to preserve context. Run `/session status` for lifecycle controls.",
            false,
        )
        .field("Recent tasks", project_recent_tasks(tasks), false);
    EditMessage::new()
        .embed(embed)
        .components(project_welcome_components(tasks))
}

fn project_is_visible_to(
    binding: &ProjectBinding,
    user_id: u64,
    role_ids: &HashSet<u64>,
    permissions: Option<Permissions>,
) -> bool {
    let elevated = permissions.is_some_and(|permissions| {
        permissions.contains(Permissions::ADMINISTRATOR)
            || permissions.contains(Permissions::MANAGE_CHANNELS)
    });
    elevated
        || binding.created_by == user_id
        || binding.access_user_ids.contains(&user_id)
        || binding
            .access_role_ids
            .iter()
            .any(|role_id| role_ids.contains(role_id))
}

fn project_selector_row(projects: &[ProjectBinding]) -> Option<CreateActionRow> {
    if projects.is_empty() {
        return None;
    }
    let options = projects
        .iter()
        .take(SELECT_MENU_PAGE_SIZE)
        .map(|binding| {
            CreateSelectMenuOption::new(
                truncate_for_discord(
                    &format!("📁 @{}", binding.workspace_alias),
                    SELECT_OPTION_TEXT_MAX,
                ),
                binding.channel_id.to_string(),
            )
            .description("Open this repository's Project Home")
        })
        .collect();
    let placeholder = if projects.len() > SELECT_MENU_PAGE_SIZE {
        format!("Choose a repository (first {SELECT_MENU_PAGE_SIZE})")
    } else {
        "Choose a repository project".to_string()
    };
    Some(CreateActionRow::SelectMenu(
        CreateSelectMenu::new("oab_help_project", CreateSelectMenuKind::String { options })
            .placeholder(placeholder),
    ))
}

fn help_action_center(
    image_url: Option<&str>,
    projects: &[ProjectBinding],
    admin_control_enabled: bool,
) -> CreateInteractionResponseMessage {
    let mut embed = CreateEmbed::new()
        .title("🧭 OpenAB · What do you want to do?")
        .description("不需要記 Slash Commands。選擇現在的情境，OpenAB 只會顯示下一步。")
        .colour(0x5865F2)
        .field(
            "工作方式",
            "Project channel = repository\nTask thread = Cursor session",
            false,
        )
        .footer(CreateEmbedFooter::new(
            "Discord 與 Cursor terminal 不要同時操作同一個 session",
        ));
    if let Some(url) = image_url {
        embed = embed.thumbnail(url);
    }
    if projects.len() > SELECT_MENU_PAGE_SIZE {
        embed = embed.field(
            "Repository projects",
            format!(
                "顯示前 {SELECT_MENU_PAGE_SIZE} 個可存取 projects；其餘仍可從 Discord channel list 開啟。"
            ),
            false,
        );
    }
    let mut components = vec![CreateActionRow::Buttons(vec![
        CreateButton::new("oab_help:discord")
            .label("📱 Start on Discord")
            .style(ButtonStyle::Primary),
        CreateButton::new("oab_help:cursor")
            .label("🖥️ Continue on computer")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_help:attach")
            .label("📤 Publish local chat")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_help:sessions")
            .label("🧠 Manage sessions")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_help:troubleshoot")
            .label("🛠️ Troubleshoot")
            .style(ButtonStyle::Secondary),
    ])];
    if admin_control_enabled {
        components.push(CreateActionRow::Buttons(vec![CreateButton::new(
            "oab_admin:open",
        )
        .label("🛡️ Server management")
        .style(ButtonStyle::Secondary)]));
    }
    if let Some(row) = project_selector_row(projects) {
        components.push(row);
    }
    CreateInteractionResponseMessage::new()
        .embed(embed)
        .components(components)
}

fn admin_navigation_buttons() -> Vec<CreateActionRow> {
    vec![
        CreateActionRow::Buttons(vec![
            CreateButton::new("oab_admin:refresh")
                .label("↻ Refresh")
                .style(ButtonStyle::Secondary),
            CreateButton::new("oab_admin:inventory")
                .label("🗂️ Server structure")
                .style(ButtonStyle::Secondary),
            CreateButton::new("oab_admin:create")
                .label("＋ Create category")
                .style(ButtonStyle::Secondary),
            CreateButton::new("oab_admin:channel_setup")
                .label("# Channel setup")
                .style(ButtonStyle::Primary),
            CreateButton::new("oab_admin:cleanup")
                .label("🧹 Safe cleanup")
                .style(ButtonStyle::Secondary),
        ]),
        CreateActionRow::Buttons(vec![
            CreateButton::new("oab_help:back")
                .label("← Help")
                .style(ButtonStyle::Secondary),
        ]),
    ]
}

fn admin_status_card(status: &AdminStatus) -> InteractionCard {
    let permissions = [
        ("Manage Server", status.permissions.manage_guild),
        ("Manage Channels", status.permissions.manage_channels),
        ("Manage Roles", status.permissions.manage_roles),
        ("View Channels", status.permissions.view_channel),
    ]
    .into_iter()
    .map(|(name, enabled)| format!("{} {name}", if enabled { "✅" } else { "❌" }))
    .collect::<Vec<_>>()
    .join("\n");
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("🛡️ Server management")
            .description("由獨立 Discord Admin Bot 執行。每次操作都會重新驗證你的 allowlist 身分。")
            .colour(0x5865F2)
            .field(
                "Server",
                format!(
                    "{}\n{}",
                    suppress_mentions(&status.server.name),
                    inline_code(&status.server.id)
                ),
                true,
            )
            .field(
                "Admin Bot",
                format!(
                    "{}\n{}",
                    suppress_mentions(&status.bot.name),
                    inline_code(&status.bot.id)
                ),
                true,
            )
            .field("Permissions", permissions, false)
            .field(
                "Resources",
                format!(
                    "{} categories · {} text · {} forums · {} voice",
                    status.counts.categories,
                    status.counts.text_channels,
                    status.counts.forums,
                    status.counts.voice_channels
                ),
                false,
            )
            .footer(CreateEmbedFooter::new(
                "Phase A-C: inspect, provision channels, and safely clean empty resources",
            )),
        components: admin_navigation_buttons(),
    }
}

fn admin_inventory_card(inventory: &AdminInventory) -> InteractionCard {
    let mut lines = Vec::new();
    for category in &inventory.categories {
        lines.push(format!(
            "**{}** · {} channel(s)",
            suppress_mentions(&category.name),
            category.channels.len()
        ));
        for channel in category.channels.iter().take(8) {
            lines.push(format!(
                "  • {} · `{}`",
                suppress_mentions(&channel.name),
                channel.kind
            ));
        }
        if category.channels.len() > 8 {
            lines.push(format!(
                "  • … {} more",
                category.channels.len().saturating_sub(8)
            ));
        }
    }
    if !inventory.uncategorized.is_empty() {
        lines.push(format!(
            "**Uncategorized** · {} channel(s)",
            inventory.uncategorized.len()
        ));
        for channel in inventory.uncategorized.iter().take(8) {
            lines.push(format!(
                "  • {} · `{}`",
                suppress_mentions(&channel.name),
                channel.kind
            ));
        }
    }
    if lines.is_empty() {
        lines.push("_No channels found._".to_string());
    }
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("🗂️ Server structure")
            .description(truncate_for_discord(&lines.join("\n"), 3900))
            .colour(0x3498DB)
            .footer(CreateEmbedFooter::new(format!(
                "{} categories · read-only inventory",
                inventory.categories.len()
            ))),
        components: admin_navigation_buttons(),
    }
}

fn admin_category_modal() -> CreateModal {
    CreateModal::new("oab_admin_category_create", "Preview category creation").components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Category name", "name")
                .placeholder("Projects")
                .min_length(1)
                .max_length(100),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(
                InputTextStyle::Short,
                "Sidebar position (optional)",
                "position",
            )
            .placeholder("0")
            .max_length(5)
            .required(false),
        ),
    ])
}

fn admin_category_preview_card(plan: &CategoryPlan) -> InteractionCard {
    let position = plan
        .position
        .map_or_else(|| "Discord default".to_string(), |value| value.to_string());
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("⚠️ Confirm category creation")
            .description("Review the exact change. Nothing has been created yet.")
            .colour(0xF1C40F)
            .field("Operation", inline_code(&plan.operation), false)
            .field("Name", suppress_mentions(&plan.name), true)
            .field("Position", position, true)
            .footer(CreateEmbedFooter::new(format!(
                "This preview expires in {} seconds",
                plan.expires_in_seconds
            ))),
        components: vec![CreateActionRow::Buttons(vec![
            CreateButton::new(format!("oab_admin:confirm:{}", plan.id))
                .label("Create category")
                .style(ButtonStyle::Success),
            CreateButton::new("oab_admin:cancel")
                .label("Cancel")
                .style(ButtonStyle::Secondary),
        ])],
    }
}

fn admin_category_created_card(category: &CreatedCategory) -> InteractionCard {
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("✅ Category created")
            .description(format!(
                "Created **{}** at position {}.",
                suppress_mentions(&category.name),
                category.position
            ))
            .colour(0x2ECC71)
            .field("Category ID", inline_code(&category.id), false),
        components: admin_navigation_buttons(),
    }
}

fn admin_channel_category_card(inventory: &AdminInventory) -> InteractionCard {
    let mut embed = CreateEmbed::new()
        .title("# Channel setup")
        .description(
            "Choose the parent category, then enter a channel name and optional topic. The channel inherits that category's permissions.",
        )
        .colour(0x5865F2)
        .footer(CreateEmbedFooter::new(
            "Selecting a category opens a form; no change happens before confirmation",
        ));
    let mut components = Vec::new();
    if inventory.categories.is_empty() {
        embed = embed.field(
            "No categories found",
            "Create a category first, then return to Channel setup.",
            false,
        );
    } else {
        let options = inventory
            .categories
            .iter()
            .take(SELECT_MENU_PAGE_SIZE)
            .map(|category| {
                CreateSelectMenuOption::new(
                    truncate_for_discord(
                        &format!("🗂️ {}", suppress_mentions(&category.name)),
                        SELECT_OPTION_TEXT_MAX,
                    ),
                    category.id.clone(),
                )
                .description(format!("{} channel(s)", category.channels.len()))
            })
            .collect();
        let placeholder = if inventory.categories.len() > SELECT_MENU_PAGE_SIZE {
            format!("Choose a category (first {SELECT_MENU_PAGE_SIZE})")
        } else {
            "Choose a parent category".to_string()
        };
        components.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                "oab_admin_channel_category",
                CreateSelectMenuKind::String { options },
            )
            .placeholder(placeholder),
        ));
        embed = embed.field(
            "Available categories",
            format!("{} category(s)", inventory.categories.len()),
            false,
        );
    }
    components.push(CreateActionRow::Buttons(vec![
        CreateButton::new("oab_admin:channel_setup")
            .label("↻ Refresh categories")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_admin:create")
            .label("＋ Create category")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_admin:refresh")
            .label("← Status")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_help:back")
            .label("← Help")
            .style(ButtonStyle::Secondary),
    ]));
    InteractionCard {
        content: String::new(),
        embed,
        components,
    }
}

fn admin_channel_modal(category_id: u64) -> CreateModal {
    CreateModal::new(
        format!("oab_admin_channel_create:{category_id}"),
        "Preview text channel creation",
    )
    .components(vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Channel name", "name")
                .placeholder("backend-api")
                .min_length(1)
                .max_length(100),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Paragraph, "Topic (optional)", "topic")
                .placeholder("What this channel is used for")
                .max_length(1024)
                .required(false),
        ),
    ])
}

fn admin_channel_preview_card(plan: &ChannelPlan) -> InteractionCard {
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("⚠️ Confirm text channel creation")
            .description("Review the exact change. Nothing has been created yet.")
            .colour(0xF1C40F)
            .field("Operation", inline_code(&plan.operation), false)
            .field("Channel", format!("#{}", suppress_mentions(&plan.name)), true)
            .field("Category", suppress_mentions(&plan.category.name), true)
            .field("Category ID", inline_code(&plan.category.id), false)
            .field(
                "Topic",
                plan.topic
                    .as_deref()
                    .map_or_else(|| "_None_".to_string(), suppress_mentions),
                false,
            )
            .field("Permission behavior", "Inherit from parent category", false)
            .footer(CreateEmbedFooter::new(format!(
                "Single-use preview · expires in {} seconds",
                plan.expires_in_seconds
            ))),
        components: vec![CreateActionRow::Buttons(vec![
            CreateButton::new(format!("oab_admin:confirm_channel:{}", plan.id))
                .label("Create text channel")
                .style(ButtonStyle::Success),
            CreateButton::new("oab_admin:cancel")
                .label("Cancel")
                .style(ButtonStyle::Secondary),
        ])],
    }
}

fn admin_channel_created_card(channel: &CreatedTextChannel) -> InteractionCard {
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("✅ Text channel created")
            .description(format!(
                "Created **#{}** inside **{}**.",
                suppress_mentions(&channel.name),
                suppress_mentions(&channel.category.name)
            ))
            .colour(0x2ECC71)
            .field("Channel ID", inline_code(&channel.id), false)
            .field(
                "Topic",
                channel
                    .topic
                    .as_deref()
                    .map_or_else(|| "_None_".to_string(), suppress_mentions),
                false,
            )
            .field("Permission behavior", "Inherited from parent category", false),
        components: admin_navigation_buttons(),
    }
}

fn admin_cleanup_card(cleanup: &CleanupCandidates) -> InteractionCard {
    let mut embed = CreateEmbed::new()
        .title("🧹 Safe cleanup")
        .description(format!(
            "Only resources at least **{} hours** old are listed. Categories must have no children; text channels must have no messages or active threads.",
            cleanup.minimum_age_hours
        ))
        .colour(0xE67E22)
        .footer(CreateEmbedFooter::new(
            "Selecting an item only opens a preview; deletion still requires confirmation",
        ));
    let mut rows = Vec::new();
    if cleanup.candidates.is_empty() {
        embed = embed.field(
            "Nothing to clean",
            "No category or text channel currently meets the strict cleanup rules.",
            false,
        );
    } else {
        let options = cleanup
            .candidates
            .iter()
            .take(SELECT_MENU_PAGE_SIZE)
            .map(|candidate| {
                let icon = if candidate.target_type == "category" {
                    "🗂️"
                } else {
                    "#"
                };
                let location = candidate
                    .category_name
                    .as_deref()
                    .map_or_else(|| "uncategorized".to_string(), suppress_mentions);
                CreateSelectMenuOption::new(
                    truncate_for_discord(
                        &format!("{icon} {}", suppress_mentions(&candidate.name)),
                        SELECT_OPTION_TEXT_MAX,
                    ),
                    format!("{}:{}", candidate.target_type, candidate.id),
                )
                .description(truncate_for_discord(
                    &format!("{}h old · {location}", candidate.age_hours),
                    SELECT_OPTION_TEXT_MAX,
                ))
            })
            .collect();
        let placeholder = if cleanup.candidates.len() > SELECT_MENU_PAGE_SIZE {
            format!("Choose an empty resource (first {SELECT_MENU_PAGE_SIZE})")
        } else {
            "Choose an empty resource to preview".to_string()
        };
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                "oab_admin_cleanup",
                CreateSelectMenuKind::String { options },
            )
            .placeholder(placeholder),
        ));
        embed = embed.field(
            "Candidates",
            format!(
                "{} resource(s) meet the current rules. No deletion has happened yet.",
                cleanup.candidates.len()
            ),
            false,
        );
    }
    rows.push(CreateActionRow::Buttons(vec![
        CreateButton::new("oab_admin:cleanup")
            .label("↻ Refresh candidates")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_admin:refresh")
            .label("← Status")
            .style(ButtonStyle::Secondary),
        CreateButton::new("oab_help:back")
            .label("← Help")
            .style(ButtonStyle::Secondary),
    ]));
    InteractionCard {
        content: String::new(),
        embed,
        components: rows,
    }
}

fn admin_deletion_preview_card(plan: &DeletionPlan) -> InteractionCard {
    let kind = if plan.target_type == "category" {
        "Category"
    } else {
        "Text channel"
    };
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("🗑️ Confirm permanent deletion")
            .description(
                "This permanently deletes the selected Discord resource. Eligibility was checked again immediately before this preview.",
            )
            .colour(0xE74C3C)
            .field("Operation", inline_code(&plan.operation), false)
            .field("Type", kind, true)
            .field("Name", suppress_mentions(&plan.name), true)
            .field("Resource ID", inline_code(&plan.target_id), false)
            .field("Why it qualifies", suppress_mentions(&plan.reason), false)
            .field("Age", format!("{} hours", plan.age_hours), true)
            .footer(CreateEmbedFooter::new(format!(
                "Single-use preview · expires in {} seconds",
                plan.expires_in_seconds
            ))),
        components: vec![CreateActionRow::Buttons(vec![
            CreateButton::new(format!("oab_admin:confirm_delete:{}", plan.id))
                .label("Delete permanently")
                .style(ButtonStyle::Danger),
            CreateButton::new("oab_admin:cancel")
                .label("Keep it")
                .style(ButtonStyle::Secondary),
        ])],
    }
}

fn admin_deleted_card(deleted: &DeletedResource) -> InteractionCard {
    let kind = if deleted.target_type == "category" {
        "category"
    } else {
        "text channel"
    };
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("✅ Empty resource deleted")
            .description(format!(
                "Deleted {kind} **{}**.",
                suppress_mentions(&deleted.name)
            ))
            .colour(0x2ECC71)
            .field("Resource ID", inline_code(&deleted.id), false),
        components: admin_navigation_buttons(),
    }
}

fn admin_error_card(error: &anyhow::Error) -> InteractionCard {
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("⚠️ Server management unavailable")
            .description(truncate_for_discord(&error.to_string(), 1500))
            .colour(0xE74C3C)
            .footer(CreateEmbedFooter::new(
                "No server change was made. Check both bot logs if this persists.",
            )),
        components: vec![CreateActionRow::Buttons(vec![
            CreateButton::new("oab_admin:refresh")
                .label("Try again")
                .style(ButtonStyle::Primary),
            CreateButton::new("oab_help:back")
                .label("← Help")
                .style(ButtonStyle::Secondary),
        ])],
    }
}

fn project_link_button(binding: &ProjectBinding) -> CreateButton {
    CreateButton::new_link(format!(
        "https://discord.com/channels/{}/{}",
        binding.guild_id, binding.channel_id
    ))
    .label("← Open Project")
}

fn help_topic_message(
    topic: &str,
    current_channel_id: u64,
    task: Option<&TaskRecord>,
    binding: Option<&ProjectBinding>,
    projects: &[ProjectBinding],
) -> CreateInteractionResponseMessage {
    let (title, description) = match topic {
        "discord" => (
            "📱 在 Discord 開始開發",
            "1. 開啟 repository 的 Project Home。\n2. 點 **New task** 輸入需求，或用 **Quick actions** 選常用工作。\n3. 送出後在 task thread 回覆，即可保留同一個 context。\n4. 要查看 repository 內的 PNG，直接請 Agent 將相對路徑圖片傳回 Discord。",
        ),
        "cursor" => (
            "🖥️ 回到電腦接續",
            "1. 等目前的 Discord 回覆完成。\n2. 點 **Continue on computer**。\n3. 複製卡片顯示的 `make session-resume ...` 到主機執行。\n4. 正常離開 Cursor 後，再回同一個 thread。",
        ),
        "attach" => (
            "📤 將本機 Cursor chat 發佈到 Discord",
            "1. 先正常離開 Cursor terminal。\n2. 回到相同 repository 的 Project Home。\n3. 點 **Attach local chat**，選擇 chat 並輸入 thread title。",
        ),
        "sessions" => (
            "🧠 管理 Cursor sessions",
            "請在 managed Project channel 或其 task thread 開啟 `/help`，再點 **Manage sessions**。你可以查看該 repository 的 sessions、回到 thread、關閉 context，或移除已關閉的清單紀錄。",
        ),
        _ => (
            "🛠️ 快速排除問題",
            "• 卡片狀態未更新：點 **Refresh**。\n• Cursor handoff 顯示 busy：等待回覆完成或先 **Stop**。\n• Attach 找不到 chat：先正常離開 Cursor UI。\n• Workspace 不符：回到正確的 Project Home。",
        ),
    };
    let mut buttons = Vec::new();
    match topic {
        "discord" => {
            if let Some(binding) = binding {
                if binding.channel_id == current_channel_id {
                    buttons.push(
                        CreateButton::new("oab_project:new")
                            .label("▶ New task")
                            .style(ButtonStyle::Primary),
                    );
                    buttons.push(
                        CreateButton::new("oab_project:actions")
                            .label("⚡ Quick actions")
                            .style(ButtonStyle::Secondary),
                    );
                } else {
                    buttons.push(project_link_button(binding));
                }
            }
        }
        "cursor" => {
            if task.is_some_and(|task| matches!(task.state, TaskState::Ready | TaskState::Failed)) {
                buttons.push(
                    CreateButton::new("oab_session:detach")
                        .label("🖥️ Continue on computer")
                        .style(ButtonStyle::Primary),
                );
            }
        }
        "attach" => {
            if let Some(binding) = binding {
                if binding.channel_id == current_channel_id {
                    buttons.push(
                        CreateButton::new("oab_project:attach")
                            .label("📤 Attach local chat")
                            .style(ButtonStyle::Primary),
                    );
                } else {
                    buttons.push(project_link_button(binding));
                }
            }
        }
        _ => {
            if task.is_some() {
                buttons.push(
                    CreateButton::new("oab_session:refresh")
                        .label("↻ Refresh status")
                        .style(ButtonStyle::Primary),
                );
            }
        }
    }
    buttons.push(
        CreateButton::new("oab_help:back")
            .label("← Back")
            .style(ButtonStyle::Secondary),
    );
    let mut rows = vec![CreateActionRow::Buttons(buttons)];
    if topic == "discord" {
        if let Some(row) = project_selector_row(projects) {
            rows.push(row);
        }
    }
    CreateInteractionResponseMessage::new()
        .embed(
            CreateEmbed::new()
                .title(title)
                .description(description)
                .colour(0x5865F2),
        )
        .components(rows)
}

fn help_project_message(
    binding: &ProjectBinding,
    current_channel_id: u64,
) -> CreateInteractionResponseMessage {
    let mut buttons = Vec::new();
    if binding.channel_id == current_channel_id {
        buttons.push(
            CreateButton::new("oab_project:new")
                .label("▶ New task")
                .style(ButtonStyle::Primary),
        );
        buttons.push(
            CreateButton::new("oab_project:attach")
                .label("📤 Attach local chat")
                .style(ButtonStyle::Secondary),
        );
        buttons.push(
            CreateButton::new("oab_project:actions")
                .label("⚡ Quick actions")
                .style(ButtonStyle::Secondary),
        );
    } else {
        buttons.push(project_link_button(binding));
    }
    buttons.push(
        CreateButton::new("oab_help:back")
            .label("← Back")
            .style(ButtonStyle::Secondary),
    );
    CreateInteractionResponseMessage::new()
        .embed(
            CreateEmbed::new()
                .title(format!("📁 @{}", binding.workspace_alias))
                .description("已選擇這個 repository project。使用 **New task** 輸入需求，或從 **Quick actions** 選擇常用工作。")
                .colour(0x5865F2)
                .field("Project channel", format!("<#{}>", binding.channel_id), true)
                .field(
                    "Workspace",
                    inline_code(&format!("@{}", binding.workspace_alias)),
                    true,
                ),
        )
        .components(vec![CreateActionRow::Buttons(buttons)])
}

#[derive(Debug, Clone)]
struct ManagedSessionEntry {
    task: TaskRecord,
    snapshot: SessionSnapshot,
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

fn reconciled_handoff_task_state(
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
            "選擇一個 task 查看 Cursor session 狀態。Close 只清除 OpenAB 的 session 關聯，不會刪除 repository 或 Cursor checkpoint。",
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
                "Session 已關閉；可用 **Remove from list** 隱藏這筆 task metadata。Discord thread 與 Cursor checkpoint 都會保留。",
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
            "先選擇 session，再按 **Close session**。關閉後才能 **Remove from list**；不提供直接刪除 Cursor checkpoint，避免不可逆資料遺失。",
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
        buttons.push(
            CreateButton::new(format!(
                "oab_sessions:close:{}:{}",
                binding.channel_id, entry.task.thread_id
            ))
            .label("✕ Close session")
            .style(ButtonStyle::Danger)
            .disabled(!can_close),
        );
        let can_remove = entry.task.state == TaskState::Closed
            && entry.snapshot.state == SessionState::None
            && !entry.snapshot.externally_detached;
        buttons.push(
            CreateButton::new(format!(
                "oab_sessions:remove:{}:{}",
                binding.channel_id, entry.task.thread_id
            ))
            .label("Remove from list")
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
fn session_manager_message(
    binding: &ProjectBinding,
    entries: &[ManagedSessionEntry],
    selected_thread_id: Option<u64>,
    note: Option<String>,
) -> CreateInteractionResponseMessage {
    session_manager_card(binding, entries, selected_thread_id, note).into_message()
}

fn session_manager_edit(
    binding: &ProjectBinding,
    entries: &[ManagedSessionEntry],
    selected_thread_id: Option<u64>,
    note: Option<String>,
) -> EditInteractionResponse {
    session_manager_card(binding, entries, selected_thread_id, note).into_edit()
}

fn task_prompt_modal(action: &str, initial_prompt: Option<&str>) -> CreateModal {
    let (title, label, placeholder) = if action == "edit" {
        (
            "Edit and retry",
            "Update the request before retrying",
            "Adjust the outcome, constraints, or verification steps",
        )
    } else {
        (
            "Continue this task",
            "What should Cursor do next?",
            "Describe the next outcome and how to verify it",
        )
    };
    let mut input = CreateInputText::new(InputTextStyle::Paragraph, label, "prompt")
        .placeholder(placeholder)
        .min_length(1)
        .max_length(4000);
    if let Some(prompt) = initial_prompt {
        input = input.value(truncate_for_discord(prompt, 4000));
    }
    CreateModal::new(format!("oab_task_prompt:{action}"), title)
        .components(vec![CreateActionRow::InputText(input)])
}

fn modal_input_value<'a>(
    modal: &'a serenity::model::application::ModalInteraction,
    custom_id: &str,
) -> Option<&'a str> {
    modal
        .data
        .components
        .iter()
        .flat_map(|row| row.components.iter())
        .find_map(|component| match component {
            ActionRowComponent::InputText(input) if input.custom_id == custom_id => {
                input.value.as_deref()
            }
            _ => None,
        })
}

fn cursor_chat_choice_label(chat: &crate::cursor_session::CursorChatSummary) -> String {
    let short_id = chat.session_id.get(..8).unwrap_or(&chat.session_id);
    let updated = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(chat.updated_at_ms as i64)
        .map(|value| value.format("%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "Unknown time".to_string());
    format!("{updated} · {short_id}")
}

fn friendly_attach_error(error: &str) -> String {
    if error.contains("invalid Cursor chat ID") {
        "Chat ID 格式不正確，請從最近 chats 清單重新選擇。".into()
    } else if error.contains("checkpoint was not found") {
        "找不到這個 Cursor chat。請確認它建立於 openab-cursor，並執行 `make session-publish-list` 檢查。".into()
    } else if error.contains("Cursor still owns this chat") {
        "Cursor 仍在使用這個 chat。請先在 terminal UI 輸入 `/exit` 或按 Ctrl-D，再重新操作。".into()
    } else if error.contains("belongs to") {
        "這個 chat 屬於其他 repository，請回到正確的 Project Home。".into()
    } else if error.contains("already attached") || error.contains("already has a session") {
        "這個 chat 或 Discord thread 已經綁定 session，請選擇其他項目。".into()
    } else if error.contains("workspace is unavailable") {
        "Project workspace 目前無法存取，請檢查 repository mount 後重啟 OpenAB。".into()
    } else {
        error.to_string()
    }
}

fn sanitize_project_channel_name(input: &str) -> String {
    let mut name = String::new();
    let mut previous_dash = false;
    for ch in input.trim().to_lowercase().chars() {
        if ch.is_alphanumeric() || ch == '_' {
            name.push(ch);
            previous_dash = false;
        } else if !previous_dash && !name.is_empty() {
            name.push('-');
            previous_dash = true;
        }
    }
    let mut name = name.trim_matches('-').chars().take(100).collect::<String>();
    if name.is_empty() {
        name = "project".into();
    } else if name.chars().count() == 1 {
        name.push_str("-project");
    }
    name
}

fn project_channel_access_permissions() -> Permissions {
    Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY
        | Permissions::ATTACH_FILES
        | Permissions::EMBED_LINKS
        | Permissions::ADD_REACTIONS
        | Permissions::USE_APPLICATION_COMMANDS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::SEND_MESSAGES_IN_THREADS
}

fn project_workspace_choices(
    aliases: &HashMap<String, String>,
    used_aliases: &HashSet<String>,
    query: &str,
) -> Vec<(String, String)> {
    let query = query.trim().trim_start_matches('@').to_lowercase();
    let mut names: Vec<_> = aliases
        .keys()
        .filter(|alias| !used_aliases.contains(*alias))
        .filter(|alias| query.is_empty() || alias.to_lowercase().contains(&query))
        .filter(|alias| alias.chars().count() <= 100)
        .cloned()
        .collect();
    names.sort();
    names.truncate(SELECT_MENU_PAGE_SIZE);
    names
        .into_iter()
        .map(|alias| (format!("@{alias}"), alias))
        .collect()
}

fn is_unknown_discord_channel_error(error: &serenity::Error) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("unknown channel") || message.contains("code: 10003")
}

fn is_unknown_discord_message_error(error: &serenity::Error) -> bool {
    let message = error.to_string().to_lowercase();
    message.contains("unknown message") || message.contains("code: 10008")
}

fn session_command_channel_allowed(
    channel_id: u64,
    thread_parent_id: Option<u64>,
    allowed_channels: &HashSet<u64>,
    allow_all_channels: bool,
) -> bool {
    allow_all_channels
        || allowed_channels.contains(&channel_id)
        || thread_parent_id.is_some_and(|id| allowed_channels.contains(&id))
}

/// Avoid unbounded Discord history exports from very large threads.
const THREAD_EXPORT_MESSAGE_LIMIT: usize = 5000;

// --- DiscordAdapter: implements ChatAdapter for Discord via serenity ---

pub struct DiscordAdapter {
    http: Arc<Http>,
    task_registry: Option<TaskRegistry>,
    project_registry: Option<ProjectRegistry>,
}

impl DiscordAdapter {
    pub fn new(http: Arc<Http>) -> Self {
        Self {
            http,
            task_registry: None,
            project_registry: None,
        }
    }

    pub fn with_task_ui(
        http: Arc<Http>,
        task_registry: TaskRegistry,
        project_registry: ProjectRegistry,
    ) -> Self {
        Self {
            http,
            task_registry: Some(task_registry),
            project_registry: Some(project_registry),
        }
    }

    /// Resolve the effective Discord channel ID from a ChannelRef.
    /// Discord threads are channels, so prefer thread_id when set.
    fn resolve_channel(channel: &ChannelRef) -> &str {
        channel.thread_id.as_deref().unwrap_or(&channel.channel_id)
    }

    async fn refresh_task_ui(&self, task: &TaskRecord) -> anyhow::Result<()> {
        if let Some(message_id) = task.status_message_id {
            ChannelId::new(task.thread_id)
                .edit_message(
                    &self.http,
                    MessageId::new(message_id),
                    task_status_edit(task),
                )
                .await?;
        }
        let (Some(task_registry), Some(project_registry)) =
            (&self.task_registry, &self.project_registry)
        else {
            return Ok(());
        };
        let Some(binding) = project_registry.binding_for_channel(task.project_channel_id) else {
            return Ok(());
        };
        let Some(home_message_id) = binding.home_message_id else {
            return Ok(());
        };
        let recent = task_registry.recent_for_project(task.project_channel_id, 10);
        ChannelId::new(task.project_channel_id)
            .edit_message(
                &self.http,
                MessageId::new(home_message_id),
                project_welcome_edit(&binding, &recent),
            )
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ChatAdapter for DiscordAdapter {
    fn platform(&self) -> &'static str {
        "discord"
    }

    fn message_limit(&self) -> usize {
        2000
    }

    async fn send_message(
        &self,
        channel: &ChannelRef,
        content: &str,
    ) -> anyhow::Result<MessageRef> {
        let ch_id: u64 = Self::resolve_channel(channel).parse()?;
        let msg = ChannelId::new(ch_id).say(&self.http, content).await?;
        Ok(MessageRef {
            channel: channel.clone(),
            message_id: msg.id.to_string(),
        })
    }

    async fn send_workspace_attachments(
        &self,
        channel: &ChannelRef,
        workspace: &str,
        paths: &[String],
    ) -> anyhow::Result<()> {
        let prepared = prepare_workspace_pngs(workspace, paths)?;
        let files = prepared
            .into_iter()
            .map(|image| CreateAttachment::bytes(image.bytes, image.filename))
            .collect::<Vec<_>>();
        let ch_id: u64 = Self::resolve_channel(channel).parse()?;
        ChannelId::new(ch_id)
            .send_files(
                &self.http,
                files,
                CreateMessage::new().content("🖼️ Workspace image attachment"),
            )
            .await?;
        Ok(())
    }

    async fn send_message_with_reply(
        &self,
        channel: &ChannelRef,
        content: &str,
        reply_to_message_id: &str,
    ) -> anyhow::Result<MessageRef> {
        let ch_id: u64 = Self::resolve_channel(channel).parse()?;
        let msg_id: u64 = reply_to_message_id.parse().unwrap_or(0);
        if msg_id == 0 {
            // Invalid message ID, fall back to plain send
            return self.send_message(channel, content).await;
        }
        let builder = serenity::builder::CreateMessage::new()
            .content(content)
            .reference_message((ChannelId::new(ch_id), MessageId::new(msg_id)));
        match ChannelId::new(ch_id)
            .send_message(&self.http, builder)
            .await
        {
            Ok(msg) => Ok(MessageRef {
                channel: channel.clone(),
                message_id: msg.id.to_string(),
            }),
            Err(e) => {
                // Fallback to plain send if reply fails (e.g. unknown message, cross-channel)
                tracing::warn!(error = ?e, reply_to = reply_to_message_id, "reply_to failed, falling back to plain send");
                self.send_message(channel, content).await
            }
        }
    }

    async fn delete_message(&self, msg: &MessageRef) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(&msg.channel).parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .delete_message(ChannelId::new(ch_id), MessageId::new(msg_id), None)
            .await?;
        Ok(())
    }

    async fn delete_channel(&self, channel: &ChannelRef) -> anyhow::Result<()> {
        let channel_id: u64 = Self::resolve_channel(channel).parse()?;
        ChannelId::new(channel_id).delete(&self.http).await?;
        Ok(())
    }

    async fn update_task_lifecycle(
        &self,
        channel: &ChannelRef,
        event: TaskLifecycleEvent,
    ) -> anyhow::Result<()> {
        let Some(registry) = &self.task_registry else {
            return Ok(());
        };
        let thread_id: u64 = Self::resolve_channel(channel).parse()?;
        if registry.task_for_thread(thread_id).is_none() {
            return Ok(());
        }
        let task = match event {
            TaskLifecycleEvent::Enqueued => registry.enqueue(thread_id)?,
            TaskLifecycleEvent::Started { batch_size } => {
                registry.start_turn(thread_id, batch_size)?
            }
            TaskLifecycleEvent::Finished => registry.finish_turn(thread_id, None)?,
            TaskLifecycleEvent::Failed { message } => {
                registry.finish_turn(thread_id, Some(truncate_for_discord(&message, 900)))?
            }
        };
        self.refresh_task_ui(&task).await
    }

    async fn edit_message(&self, msg: &MessageRef, content: &str) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(&msg.channel).parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        ChannelId::new(ch_id)
            .edit_message(
                &self.http,
                MessageId::new(msg_id),
                EditMessage::new().content(content),
            )
            .await?;
        Ok(())
    }

    fn use_streaming(&self, other_bot_present: bool) -> bool {
        !other_bot_present
    }

    async fn create_thread(
        &self,
        channel: &ChannelRef,
        trigger_msg: &MessageRef,
        title: &str,
    ) -> anyhow::Result<ChannelRef> {
        let ch_id: u64 = channel.channel_id.parse()?;
        let msg_id: u64 = trigger_msg.message_id.parse()?;
        let thread = ChannelId::new(ch_id)
            .create_thread_from_message(
                &self.http,
                MessageId::new(msg_id),
                CreateThread::new(title).auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await?;
        Ok(ChannelRef {
            platform: "discord".into(),
            channel_id: thread.id.to_string(),
            thread_id: None,
            parent_id: Some(channel.channel_id.clone()),
            origin_event_id: None,
        })
    }

    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(&msg.channel).parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .create_reaction(
                ChannelId::new(ch_id),
                MessageId::new(msg_id),
                &ReactionType::Unicode(emoji.to_string()),
            )
            .await?;
        Ok(())
    }

    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(&msg.channel).parse()?;
        let msg_id: u64 = msg.message_id.parse()?;
        self.http
            .delete_reaction_me(
                ChannelId::new(ch_id),
                MessageId::new(msg_id),
                &ReactionType::Unicode(emoji.to_string()),
            )
            .await?;
        Ok(())
    }

    async fn rename_thread(&self, channel: &ChannelRef, title: &str) -> anyhow::Result<()> {
        let ch_id: u64 = Self::resolve_channel(channel).parse()?;
        // Truncate at char boundary to avoid panic on multi-byte chars (中文/Emoji).
        let truncated: &str = if title.chars().count() > 100 {
            let end = title
                .char_indices()
                .nth(100)
                .map(|(i, _)| i)
                .unwrap_or(title.len());
            &title[..end]
        } else {
            title
        };
        ChannelId::new(ch_id)
            .edit(&self.http, EditChannel::new().name(truncated))
            .await?;
        Ok(())
    }
}

// --- Handler: serenity EventHandler that delegates to AdapterRouter ---

pub struct Handler {
    pub router: Arc<AdapterRouter>,
    pub allow_all_channels: bool,
    pub allow_all_users: bool,
    pub allowed_channels: HashSet<u64>,
    pub allowed_users: HashSet<u64>,
    pub stt_config: SttConfig,
    pub adapter: OnceLock<Arc<dyn ChatAdapter>>,
    /// Optional filestore for uploading file attachments.
    #[cfg(feature = "filestore")]
    pub filestore: Option<Arc<crate::filestore::Filestore>>,
    pub allow_bot_messages: AllowBots,
    pub trusted_bot_ids: HashSet<u64>,
    pub allow_user_messages: AllowUsers,
    /// Role IDs that trigger the bot (same as direct @mention).
    pub allowed_role_ids: HashSet<u64>,
    /// Positive-only cache: thread channel_id → cached_at for threads where bot has participated.
    pub participated_threads: tokio::sync::Mutex<HashMap<String, tokio::time::Instant>>,
    /// Positive-only cache: thread channel_id → cached_at for threads where other bots have posted.
    /// Like participation, a thread becoming multi-bot is irreversible (bot messages don't disappear).
    pub multibot_threads: tokio::sync::Mutex<HashMap<String, tokio::time::Instant>>,
    /// Persistent disk cache for multibot thread detection (survives restarts).
    pub multibot_cache: crate::multibot_cache::MultibotCache,
    /// TTL for participation cache entries (from pool.session_ttl_hours).
    pub session_ttl: std::time::Duration,
    /// Configurable soft limit on bot turns per thread (reset by human message).
    pub max_bot_turns: u32,
    /// Per-thread bot turn tracker. Both counters reset on human msg.
    pub bot_turns: tokio::sync::Mutex<BotTurnTracker>,
    /// Allow the bot to respond to Discord DMs.
    pub allow_dm: bool,
    /// Per-thread dispatcher (Message mode uses cap=1 for FIFO; Thread/Lane use configured cap).
    pub dispatcher: Arc<crate::dispatch::Dispatcher>,
    /// Ambient mode dispatcher for passive channel listening.
    pub ambient: Option<Arc<crate::ambient::AmbientDispatcher>>,
    /// Reminder store for /remind slash command.
    pub reminder_store: ReminderStore,
    /// Track scheduled reminder IDs to prevent duplicate scheduling on reconnect.
    pub scheduled_ids: tokio::sync::Mutex<std::collections::HashSet<String>>,
    /// Whether administrators can create project channels through `/project`.
    pub project_channels_enabled: bool,
    /// Category under which new private project channels are created.
    pub project_category_id: Option<u64>,
    /// Runtime Discord channel-to-workspace bindings persisted across restarts.
    pub project_registry: ProjectRegistry,
    /// Persistent task metadata used by Project Home and task status cards.
    pub task_registry: TaskRegistry,
    /// Trusted, per-workspace Agent prompt shortcuts rendered in Project Home.
    pub project_actions: Vec<DiscordProjectActionConfig>,
    /// Optional client for the isolated Discord Admin Bot control plane.
    pub admin_control: Option<DiscordAdminClient>,
}

struct DiscordCommandScope {
    session_key: String,
    channel_ref: ChannelRef,
}

impl Handler {
    fn discord_adapter(&self, ctx: &Context) -> Arc<dyn ChatAdapter> {
        self.adapter
            .get_or_init(|| {
                Arc::new(DiscordAdapter::with_task_ui(
                    ctx.http.clone(),
                    self.task_registry.clone(),
                    self.project_registry.clone(),
                ))
            })
            .clone()
    }

    fn effective_allowed_channels(&self) -> HashSet<u64> {
        let mut channels = self.allowed_channels.clone();
        let aliases = self.router.workspace_aliases();
        channels.extend(
            self.project_registry
                .all()
                .into_iter()
                .filter(|binding| aliases.contains_key(&binding.workspace_alias))
                .map(|binding| binding.channel_id),
        );
        channels
    }

    /// Check if the bot has participated in a Discord thread, and whether
    /// other bots have also posted in it.
    /// Returns `(involved, other_bot_present)`.
    /// Fail-closed: returns `(false, false)` on API error.
    /// Caches positive results only (both participation and multi-bot status are irreversible).
    async fn bot_participated_in_thread(
        &self,
        http: &Http,
        channel_id: ChannelId,
        bot_id: UserId,
    ) -> (bool, bool) {
        let key = channel_id.to_string();

        // Check positive caches
        let cached_involved = {
            let cache = self.participated_threads.lock().await;
            cache
                .get(&key)
                .is_some_and(|ts| ts.elapsed() < self.session_ttl)
        };
        let cached_multibot = {
            let cache = self.multibot_threads.lock().await;
            cache
                .get(&key)
                .is_some_and(|ts| ts.elapsed() < self.session_ttl)
        } || self.multibot_cache.is_multibot(&key);

        // Both cached → skip fetch entirely
        // With early detection from msg.author, multibot_threads is populated
        // eagerly — no need to fetch just to check for other bots.
        if cached_involved {
            return (true, cached_multibot);
        }

        // Fetch recent messages
        let messages = match channel_id
            .messages(http, serenity::builder::GetMessages::new().limit(200))
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::warn!(
                    channel_id = %channel_id,
                    error = %e,
                    "failed to fetch thread messages for participation check, rejecting (fail-closed)"
                );
                return (false, false);
            }
        };

        let involved = cached_involved || messages.iter().any(|m| m.author.id == bot_id);
        // other_bot_present relies solely on early detection + disk cache;
        // no longer scanned from fetched messages (200-msg window was unreliable).
        let other_bot_present = cached_multibot;

        if involved && !cached_involved {
            let mut cache = self.participated_threads.lock().await;
            cache.insert(key.clone(), tokio::time::Instant::now());

            // Evict if over capacity
            if cache.len() > PARTICIPATION_CACHE_MAX {
                cache.retain(|_, ts| ts.elapsed() < self.session_ttl);
                if cache.len() > PARTICIPATION_CACHE_MAX {
                    let mut entries: Vec<_> = cache.iter().map(|(k, v)| (k.clone(), *v)).collect();
                    entries.sort_by_key(|(_, ts)| *ts);
                    let evict_count = entries.len() / 2;
                    for (k, _) in entries.into_iter().take(evict_count) {
                        cache.remove(&k);
                    }
                }
            }
        }

        (involved, other_bot_present)
    }
}

#[serenity::async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        let bot_id = ctx.cache.current_user().id;
        let effective_allowed_channels = self.effective_allowed_channels();

        // Early multibot detection: cache that another bot is present.
        // Runs before self-check and bot gating so we always detect other bots. (#481)
        if msg.author.bot && msg.author.id != bot_id {
            let key = msg.channel_id.to_string();
            {
                let mut cache = self.multibot_threads.lock().await;
                cache
                    .entry(key.clone())
                    .or_insert_with(tokio::time::Instant::now);
            }
            // Persist to disk — multibot is irreversible
            self.multibot_cache.mark_multibot(&key).await;
        }

        // Bot turn counting: runs before self-check so ALL bot messages
        // (including own) count toward the per-thread limit. This means
        // soft_limit=20 = 20 total bot messages in the thread (~10 per bot
        // in a two-bot ping-pong). (#483)
        {
            let thread_key = msg.channel_id.to_string();
            let mut tracker = self.bot_turns.lock().await;
            if msg.author.bot {
                match tracker.classify_bot_message(&thread_key) {
                    TurnAction::Continue => {}
                    TurnAction::SilentStop => return,
                    TurnAction::WarnAndStop {
                        severity,
                        turns,
                        user_message,
                    } => {
                        match severity {
                            TurnSeverity::Hard => tracing::warn!(
                                channel_id = %msg.channel_id,
                                turns,
                                "hard bot turn limit reached",
                            ),
                            TurnSeverity::Soft => tracing::info!(
                                channel_id = %msg.channel_id,
                                turns,
                                max = self.max_bot_turns,
                                "soft bot turn limit reached",
                            ),
                        }
                        // Only post the warning if this bot is allowed in the channel/thread.
                        // Bot turn counting intentionally runs before channel gating so ALL
                        // bot messages are counted, but the *warning message* must respect
                        // channel permissions — otherwise bots that never participated in a
                        // thread will spam it with warnings.
                        //
                        // Must match the full thread allowlist semantics: a thread is allowed
                        // if its own channel_id OR its parent_id is in allowed_channels.
                        let ch = msg.channel_id.get();
                        let in_allowed_channel = effective_allowed_channels.contains(&ch);
                        let mut allowed_here = self.allow_all_channels || in_allowed_channel;
                        if !allowed_here {
                            // Reuse detect_thread() for thread allowlist semantics.
                            // Only called on the WarnAndStop path (once per soft/hard
                            // limit hit), not on every bot message.
                            if let Ok(serenity::model::channel::Channel::Guild(gc)) =
                                msg.channel_id.to_channel(&ctx.http).await
                            {
                                let (in_thread, _) = detect_thread(
                                    gc.thread_metadata.is_some(),
                                    gc.parent_id.map(|id| id.get()),
                                    gc.owner_id.map(|id| id.get()),
                                    bot_id.get(),
                                    &effective_allowed_channels,
                                    self.allow_all_channels,
                                    in_allowed_channel,
                                );
                                if in_thread {
                                    allowed_here = true;
                                }
                            }
                        }
                        if msg.author.id != bot_id && allowed_here {
                            // Only warn if this bot actually participated in the
                            // thread — prevents uninvolved bots from spamming
                            // warnings in shared channels. (#727)
                            // Second value is `is_multibot`; not needed here.
                            let (participated, _) = self
                                .bot_participated_in_thread(&ctx.http, msg.channel_id, bot_id)
                                .await;
                            if participated {
                                // Dedup: skip if another bot already posted the same
                                // warning in this thread. Prevents N duplicate warnings
                                // when N bot processes each hit the soft limit. (#530)
                                let recent = msg
                                    .channel_id
                                    .messages(
                                        &ctx.http,
                                        serenity::builder::GetMessages::new().limit(10),
                                    )
                                    .await
                                    .unwrap_or_default();
                                let pairs: Vec<(bool, &str)> = recent
                                    .iter()
                                    .map(|m| (m.author.bot, m.content.as_str()))
                                    .collect();
                                let already_warned = turn_limit_warning_present(&pairs);
                                if !already_warned {
                                    let _ = msg.channel_id.say(&ctx.http, &user_message).await;
                                }
                            }
                        }
                        return;
                    }
                }
            } else if matches!(msg.kind, MessageType::Regular | MessageType::InlineReply)
                && !msg.content.is_empty()
            {
                tracker.on_human_message(&thread_key);
            }
        }

        // Ignore own messages (after counting toward bot turns above)
        if msg.author.id == bot_id {
            return;
        }

        let adapter = self.discord_adapter(&ctx);

        let channel_id = msg.channel_id.get();
        let in_allowed_channel =
            self.allow_all_channels || effective_allowed_channels.contains(&channel_id);

        let is_mentioned = msg.mentions_user_id(bot_id)
            || msg.content.contains(&format!("<@{}>", bot_id))
            || (!self.allowed_role_ids.is_empty()
                && msg
                    .mention_roles
                    .iter()
                    .any(|r| self.allowed_role_ids.contains(&r.get())));

        // Early-gating optimization for bot messages to avoid unnecessary
        // async/HTTP thread detection calls when ambient mode is inactive and
        // the bot would gate it out anyway. (#1197 regression safety)
        if msg.author.bot && !is_mentioned && self.ambient.is_none() {
            match self.allow_bot_messages {
                AllowBots::Off | AllowBots::Mentions => return,
                AllowBots::All => {} // fall through — still needs thread detection for normal dispatch
            }
        }

        // Thread detection: single to_channel() call for both allowed and
        // non-allowed channels. Moved before bot gating so ambient context
        // can be resolved early — bot messages in ambient contexts must bypass
        // discord-level bot gating (#1197).
        let (
            in_thread,
            bot_owns_thread,
            thread_parent_id,
            is_dm,
            is_structural_thread,
            structural_parent_id,
        ) = match msg.channel_id.to_channel(&ctx.http).await {
            Ok(serenity::model::channel::Channel::Guild(gc)) => {
                let parent = gc.parent_id.map(|id| id.get().to_string());
                let has_thread_metadata = gc.thread_metadata.is_some();
                let parent_u64 = gc.parent_id.map(|id| id.get());
                let result = detect_thread(
                    has_thread_metadata,
                    parent_u64,
                    gc.owner_id.map(|id| id.get()),
                    bot_id.get(),
                    &effective_allowed_channels,
                    self.allow_all_channels,
                    in_allowed_channel,
                );
                tracing::debug!(
                    channel_id = %msg.channel_id,
                    parent_id = ?gc.parent_id,
                    owner_id = ?gc.owner_id,
                    has_thread_metadata,
                    in_thread = result.0,
                    bot_owns = ?result.1,
                    "thread check"
                );
                (
                    result.0,
                    result.1.unwrap_or(false),
                    if has_thread_metadata { parent } else { None },
                    false,
                    has_thread_metadata,
                    if has_thread_metadata {
                        parent_u64
                    } else {
                        None
                    },
                )
            }
            Ok(serenity::model::channel::Channel::Private(_)) => {
                tracing::debug!(channel_id = %msg.channel_id, "DM channel");
                (false, false, None, true, false, None)
            }
            Ok(other) => {
                tracing::debug!(channel_id = %msg.channel_id, kind = ?other, "not a guild thread");
                (false, false, None, false, false, None)
            }
            Err(e) => {
                tracing::debug!(channel_id = %msg.channel_id, error = %e, "to_channel failed");
                (false, false, None, false, false, None)
            }
        };

        // Check if message is in an ambient context (resolved early so bot
        // messages destined for ambient can bypass discord-level bot gating).
        let in_ambient_context = self.ambient.as_ref().is_some_and(|ambient| {
            ambient.should_buffer(
                channel_id,
                is_structural_thread,
                bot_owns_thread,
                structural_parent_id,
            )
        });
        // Managed project channels are dedicated bot entrypoints: a human's
        // top-level message starts a task without requiring an @mention. Once
        // the task moves into a thread, the normal involved-thread rules apply.
        let implicit_project_prompt = !msg.author.bot
            && !is_structural_thread
            && self.project_registry.contains_channel(channel_id);

        // --- Ambient early-route for bot messages ---
        // Bot messages in an ambient context that do NOT @mention this bot are
        // routed directly to the ambient buffer, bypassing discord-level bot
        // gating entirely. Ambient mode is passive observation — the bot gating
        // logic (allow_bot_messages mode, trusted_bot_ids) only applies to
        // messages that would trigger an active response. (#1197)
        //
        // @mention from a bot in ambient context → discard buffer + fall through
        // to normal bot gating + dispatch (same as before).
        if msg.author.bot && in_ambient_context && !is_mentioned {
            if let Some(ambient) = self.ambient.as_ref() {
                if !ambient.allow_bot_messages() {
                    debug!(channel_id = %msg.channel_id, bot_id = %msg.author.id, "ambient early-route: bot msg rejected (allow_bot_messages=false)");
                } else {
                    let prompt = resolve_mentions(&msg.content, bot_id, &self.allowed_role_ids);
                    if prompt.is_empty() && msg.attachments.is_empty() {
                        return;
                    }

                    let display_name = msg
                        .member
                        .as_ref()
                        .and_then(|m| m.nick.as_ref())
                        .or(msg.author.global_name.as_ref())
                        .unwrap_or(&msg.author.name);

                    let channel_ref = ChannelRef {
                        platform: "discord".into(),
                        channel_id: channel_id.to_string(),
                        thread_id: None,
                        parent_id: None,
                        origin_event_id: None,
                    };

                    let ambient_msg = crate::ambient::AmbientMessage {
                        sender_name: display_name.to_owned(),
                        sender_id: msg.author.id.to_string(),
                        prompt,
                        extra_blocks: Vec::new(),
                        arrived_at: std::time::Instant::now(),
                    };

                    let target = Arc::clone(&self.router) as Arc<dyn DispatchTarget>;
                    debug!(channel_id = %msg.channel_id, bot_id = %msg.author.id, "ambient early-route: bot msg buffered");
                    ambient
                        .submit(
                            &channel_id.to_string(),
                            channel_ref,
                            adapter.clone(),
                            target,
                            ambient_msg,
                        )
                        .await;
                }
            }
            return;
        }

        // Bot message gating (from upstream #321)
        // NOTE: Bot messages in ambient contexts are handled above and never
        // reach here (unless they @mention this bot).
        if msg.author.bot {
            // Trusted bot admission override: when a bot listed in `trusted_bot_ids`
            // explicitly @mentions this bot, bypass the entire `allow_bot_messages`
            // mode check. This treats the trusted bot's @mention identically to a
            // human @mention — the bot becomes involved in the thread and the message
            // is dispatched regardless of the `allow_bot_messages` setting.
            //
            // Rationale: `trusted_bot_ids` expresses admin-level trust. A trusted bot
            // that @mentions this bot is performing a deliberate handoff/coordination
            // action, equivalent to a human pulling the bot into a conversation.
            //
            // Safety: requires both (1) explicit @mention AND (2) sender in
            // trusted_bot_ids. Messages from trusted bots without @mention still
            // follow normal gating. Empty trusted_bot_ids (default) disables this
            // entirely — no behavioral change for existing deployments.
            let trusted_mention = is_mentioned
                && !self.trusted_bot_ids.is_empty()
                && self.trusted_bot_ids.contains(&msg.author.id.get());

            if !trusted_mention {
                match self.allow_bot_messages {
                    AllowBots::Off => return,
                    AllowBots::Mentions => {
                        if !is_mentioned {
                            return;
                        }
                    }
                    AllowBots::All => {
                        let cap = MAX_CONSECUTIVE_BOT_TURNS as usize;
                        let limit = std::cmp::min(MAX_CONSECUTIVE_BOT_TURNS, 100) as u8;
                        let history = ctx
                            .cache
                            .channel_messages(msg.channel_id)
                            .map(|msgs| {
                                let mut recent: Vec<_> = msgs
                                    .iter()
                                    .filter(|(mid, _)| **mid < msg.id)
                                    .map(|(_, m)| m.clone())
                                    .collect();
                                recent.sort_unstable_by_key(|m| std::cmp::Reverse(m.id));
                                recent.truncate(cap);
                                recent
                            })
                            .filter(|msgs| !msgs.is_empty());

                        let recent = if let Some(cached) = history {
                            cached
                        } else {
                            match msg
                                .channel_id
                                .messages(
                                    &ctx.http,
                                    serenity::builder::GetMessages::new()
                                        .before(msg.id)
                                        .limit(limit),
                                )
                                .await
                            {
                                Ok(msgs) => msgs,
                                Err(e) => {
                                    tracing::warn!(channel_id = %msg.channel_id, error = %e, "failed to fetch history for bot turn cap, rejecting (fail-closed)");
                                    return;
                                }
                            }
                        };

                        let consecutive_bot = recent
                            .iter()
                            .take_while(|m| m.author.bot && m.author.id != bot_id)
                            .count();
                        if consecutive_bot >= cap {
                            tracing::warn!(channel_id = %msg.channel_id, cap, "bot turn cap reached, ignoring");
                            return;
                        }
                    }
                }

                if !self.trusted_bot_ids.is_empty()
                    && !self.trusted_bot_ids.contains(&msg.author.id.get())
                {
                    tracing::debug!(bot_id = %msg.author.id, "bot not in trusted_bot_ids, ignoring");
                    return;
                }
            }
        }

        // DM gating: allow_dm must be true, otherwise reject
        if is_dm && !self.allow_dm {
            tracing::debug!(channel_id = %msg.channel_id, "DM rejected (allow_dm=false)");
            return;
        }

        if !is_dm && !in_allowed_channel && !in_thread && !in_ambient_context {
            return;
        }

        // --- Ambient Mode routing ---
        // Route to ambient when the message belongs to an ambient context:
        //  - a top-level message directly in an ambient channel, or
        //  - a message in a thread under an ambient channel (including
        //    bot-owned threads — the bot passively observes all threads).
        // @mention in an ambient context → discard buffer + normal dispatch.
        // NOTE: Bot messages without @mention are already handled by the
        // early-route above; this block handles human messages and bot @mentions.
        if in_ambient_context {
            let ambient = self.ambient.as_ref().unwrap();
            if !is_dm {
                if is_mentioned {
                    // Discard ambient buffer — mention takes priority.
                    ambient.discard_buffer(&channel_id.to_string()).await;
                    // Fall through to normal dispatch below.
                } else {
                    // Route to ambient buffer (not normal dispatch).
                    // Bot messages only if allow_bot_messages is true for ambient.
                    if msg.author.bot && !ambient.allow_bot_messages() {
                        return;
                    }

                    let prompt = resolve_mentions(&msg.content, bot_id, &self.allowed_role_ids);
                    if prompt.is_empty() && msg.attachments.is_empty() {
                        return;
                    }

                    let display_name = msg
                        .member
                        .as_ref()
                        .and_then(|m| m.nick.as_ref())
                        .or(msg.author.global_name.as_ref())
                        .unwrap_or(&msg.author.name);

                    let channel_ref = ChannelRef {
                        platform: "discord".into(),
                        channel_id: channel_id.to_string(),
                        thread_id: None,
                        parent_id: None,
                        origin_event_id: None,
                    };

                    let ambient_msg = crate::ambient::AmbientMessage {
                        sender_name: display_name.to_owned(),
                        sender_id: msg.author.id.to_string(),
                        prompt,
                        extra_blocks: Vec::new(), // Skip attachments for ambient v1
                        arrived_at: std::time::Instant::now(),
                    };

                    let target = Arc::clone(&self.router) as Arc<dyn DispatchTarget>;
                    ambient
                        .submit(
                            &channel_id.to_string(),
                            channel_ref,
                            adapter.clone(),
                            target,
                            ambient_msg,
                        )
                        .await;
                    return;
                }
            }
        }

        // User message gating (mirrors Slack's AllowUsers logic).
        // Mentions: always require @mention, even in bot's own threads.
        // Involved (default): skip @mention if the bot owns the thread
        //   (Option A) OR has previously posted in it (Option B).
        // MultibotMentions: same as Involved, but if other bots are also
        //   in the thread, require @mention to avoid all bots responding.
        // DMs are treated as implicit @mention (mirrors Slack behavior).
        if !is_mentioned && !is_dm && !implicit_project_prompt {
            match self.allow_user_messages {
                AllowUsers::Mentions => return,
                AllowUsers::Involved => {
                    if !in_thread {
                        return;
                    }
                    let (involved, _) = if bot_owns_thread {
                        (true, false) // other_bot_present not needed for Involved mode
                    } else {
                        self.bot_participated_in_thread(&ctx.http, msg.channel_id, bot_id)
                            .await
                    };
                    if !involved {
                        tracing::debug!(channel_id = %msg.channel_id, "bot not involved in thread, ignoring");
                        return;
                    }
                }
                AllowUsers::MultibotMentions => {
                    if !in_thread {
                        return;
                    }
                    let (involved, other_bot) = if bot_owns_thread {
                        // Still need to check for other bots
                        let (_, other) = self
                            .bot_participated_in_thread(&ctx.http, msg.channel_id, bot_id)
                            .await;
                        (true, other)
                    } else {
                        self.bot_participated_in_thread(&ctx.http, msg.channel_id, bot_id)
                            .await
                    };
                    if !involved {
                        tracing::debug!(channel_id = %msg.channel_id, "bot not involved in thread, ignoring");
                        return;
                    }
                    if other_bot {
                        tracing::debug!(channel_id = %msg.channel_id, "multi-bot thread, requiring @mention");
                        return;
                    }
                }
            }
        }

        if is_denied_user(
            msg.author.bot,
            self.allow_all_users,
            &self.allowed_users,
            msg.author.id.get(),
        ) {
            tracing::info!(user_id = %msg.author.id, "denied user, ignoring");
            let msg_ref = discord_msg_ref(&msg);
            let _ = adapter.add_reaction(&msg_ref, "🚫").await;
            return;
        }

        let prompt = resolve_mentions(&msg.content, bot_id, &self.allowed_role_ids);

        // No text and no attachments → skip
        if prompt.is_empty() && msg.attachments.is_empty() {
            return;
        }

        let display_name = msg
            .member
            .as_ref()
            .and_then(|m| m.nick.as_ref())
            .or(msg.author.global_name.as_ref())
            .unwrap_or(&msg.author.name);
        let sender = build_sender_context(
            &msg.author.id.to_string(),
            &msg.author.name,
            display_name,
            &msg.channel_id.to_string(),
            thread_parent_id.as_deref(),
            msg.author.bot,
            &msg.timestamp.to_rfc3339().unwrap_or_default(),
            &msg.id.to_string(),
            &bot_id.to_string(),
        );

        // Build extra content blocks from attachments (audio -> STT, text -> inline,
        // image -> encode, video -> URL for agent-side inspection).
        let mut extra_blocks = Vec::new();
        let mut echo_entries: Vec<crate::stt::EchoEntry> = Vec::new();
        let mut failed_image_files: Vec<String> = Vec::new();
        let mut text_file_bytes: u64 = 0;
        let mut text_file_count: u32 = 0;
        const TEXT_TOTAL_CAP: u64 = 1024 * 1024; // 1 MB total for all text file attachments
        const TEXT_FILE_COUNT_CAP: u32 = 5;

        for attachment in &msg.attachments {
            let mime = attachment.content_type.as_deref().unwrap_or("");
            if media::is_audio_mime(mime) {
                if self.stt_config.enabled {
                    let mime_clean = mime.split(';').next().unwrap_or(mime).trim();
                    match media::download_and_transcribe(
                        &attachment.url,
                        &attachment.filename,
                        mime_clean,
                        u64::from(attachment.size),
                        &self.stt_config,
                        None,
                    )
                    .await
                    {
                        Some(transcript) => {
                            debug!(filename = %attachment.filename, chars = transcript.len(), "voice transcript injected");
                            extra_blocks.insert(
                                0,
                                ContentBlock::Text {
                                    text: format!("[Voice message transcript]: {transcript}"),
                                },
                            );
                            echo_entries.push(crate::stt::EchoEntry::Success(transcript));
                        }
                        None => {
                            warn!(filename = %attachment.filename, "STT failed for voice attachment");
                            echo_entries.push(crate::stt::EchoEntry::Failed);
                        }
                    }
                } else {
                    tracing::warn!(filename = %attachment.filename, "skipping audio attachment (STT disabled)");
                    let msg_ref = discord_msg_ref(&msg);
                    let _ = adapter.add_reaction(&msg_ref, "🎤").await;
                }
            } else if media::is_text_file(&attachment.filename, attachment.content_type.as_deref())
            {
                if text_file_count >= TEXT_FILE_COUNT_CAP {
                    tracing::warn!(filename = %attachment.filename, count = text_file_count, "text file count cap reached, skipping");
                    continue;
                }
                // Pre-check with Discord-reported size (fast path, avoids unnecessary download).
                // Running total uses actual downloaded bytes for accurate accounting.
                // When filestore is configured, skip the cap for files > 512KB (they'll
                // be uploaded to S3, not inlined).
                let attachment_size = u64::from(attachment.size);
                #[cfg(feature = "filestore")]
                let skip_cap =
                    self.filestore.is_some() && attachment_size > crate::media::TEXT_INLINE_LIMIT;
                #[cfg(not(feature = "filestore"))]
                let skip_cap = false;
                if !skip_cap && text_file_bytes + attachment_size > TEXT_TOTAL_CAP {
                    tracing::warn!(filename = %attachment.filename, total = text_file_bytes, "text attachments total exceeds 1MB cap, skipping remaining");
                    continue;
                }
                #[cfg(feature = "filestore")]
                let text_file_result = media::download_and_read_text_file(
                    &attachment.url,
                    &attachment.filename,
                    attachment_size,
                    None,
                    self.filestore.as_deref(),
                )
                .await;
                #[cfg(not(feature = "filestore"))]
                let text_file_result = media::download_and_read_text_file(
                    &attachment.url,
                    &attachment.filename,
                    attachment_size,
                    None,
                )
                .await;
                if let Some((block, actual_bytes)) = text_file_result {
                    text_file_bytes += actual_bytes;
                    text_file_count += 1;
                    debug!(filename = %attachment.filename, "adding text file attachment");
                    extra_blocks.push(block);
                }
            } else {
                match media::download_and_encode_image(
                    &attachment.url,
                    attachment.content_type.as_deref(),
                    &attachment.filename,
                    u64::from(attachment.size),
                    None,
                )
                .await
                {
                    Ok(block) => {
                        debug!(url = %attachment.url, filename = %attachment.filename, "adding image attachment");
                        extra_blocks.push(block);
                        extra_blocks.push(ContentBlock::Text {
                            text: format!(
                                "[Image attachment]\nfilename: {}\ncontent_type: {}\nsize_bytes: {}\nurl: {} (expires ~24h)",
                                attachment.filename,
                                attachment.content_type.as_deref().unwrap_or("unknown"),
                                attachment.size,
                                attachment.url,
                            ),
                        });
                    }
                    Err(media::MediaFetchError::NotAnImage) => {
                        if media::is_video_file(
                            &attachment.filename,
                            attachment.content_type.as_deref(),
                        ) {
                            debug!(url = %attachment.url, filename = %attachment.filename, "adding video attachment link");
                            extra_blocks.push(video_attachment_block(
                                &attachment.filename,
                                attachment.content_type.as_deref(),
                                u64::from(attachment.size),
                                &attachment.url,
                            ));
                        }
                        // For all other unsupported formats (PDF, ZIP, binary, etc.):
                        // upload to filestore if available so the agent gets a presigned URL.
                        #[cfg(feature = "filestore")]
                        if !media::is_video_file(
                            &attachment.filename,
                            attachment.content_type.as_deref(),
                        ) {
                            if let Some(ref fs) = self.filestore {
                                if let Some((block, _)) = media::download_and_upload_any_file(
                                    &attachment.url,
                                    &attachment.filename,
                                    u64::from(attachment.size),
                                    attachment.content_type.as_deref(),
                                    None,
                                    fs,
                                )
                                .await
                                {
                                    extra_blocks.push(block);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            url = %attachment.url,
                            filename = %attachment.filename,
                            error = %e,
                            "image attachment failed"
                        );
                        failed_image_files.push(attachment.filename.clone());
                    }
                }
            }
        }

        tracing::debug!(
            num_extra_blocks = extra_blocks.len(),
            num_attachments = msg.attachments.len(),
            in_thread,
            "processing"
        );

        let thread_channel = if in_thread || is_dm {
            // DMs use the DM channel directly (no threads in DMs).
            ChannelRef {
                platform: "discord".into(),
                channel_id: msg.channel_id.get().to_string(),
                thread_id: None,
                parent_id: thread_parent_id.clone(),
                origin_event_id: None,
            }
        } else {
            match get_or_create_thread(&ctx, &adapter, &msg, &prompt).await {
                Ok(ch) => ch,
                Err(e) => {
                    error!("failed to create thread: {e}");
                    return;
                }
            }
        };

        if let Some(project_channel_id) = thread_channel
            .parent_id
            .as_deref()
            .and_then(|value| value.parse::<u64>().ok())
        {
            if let Some(binding) = self
                .project_registry
                .binding_for_channel(project_channel_id)
            {
                let task_title = format::shorten_thread_name(&prompt);
                if let Err(error) = self
                    .ensure_task(
                        &ctx,
                        &binding,
                        thread_channel
                            .channel_id
                            .parse::<u64>()
                            .unwrap_or(msg.channel_id.get()),
                        &task_title,
                        msg.author.id.get(),
                    )
                    .await
                {
                    tracing::warn!(%error, channel_id = %thread_channel.channel_id, "failed to register Discord task");
                }
            }
        }
        if !prompt.trim().is_empty() {
            if let Ok(thread_id) = thread_channel.channel_id.parse::<u64>() {
                if self.task_registry.task_for_thread(thread_id).is_some() {
                    if let Err(error) = self.task_registry.record_prompt(thread_id, &prompt) {
                        tracing::warn!(%error, thread_id, "failed to save retryable Discord prompt");
                    }
                }
            }
        }

        // Notify user if any images couldn't be processed.
        if !failed_image_files.is_empty() {
            let file_list = failed_image_files
                .iter()
                .map(|n| format!("`{}`", n.replace('`', "'")))
                .collect::<Vec<_>>()
                .join(", ");
            let warn_msg = format!(
                ":warning: I couldn't process the image(s) you shared ({}). \
                 The files may be inaccessible or in an unsupported format (PNG/JPEG/GIF/WebP only).",
                file_list
            );
            if let Err(e) = adapter.send_message(&thread_channel, &warn_msg).await {
                tracing::warn!(error = %e, "failed to send image warning to user");
            }
        }

        let trigger_msg = discord_msg_ref(&msg);

        // Per-thread streaming: check if another bot is present in this thread
        let other_bot_present_flag =
            {
                let cache = self.multibot_threads.lock().await;
                cache.contains_key(&msg.channel_id.to_string())
            } || self.multibot_cache.is_multibot(&msg.channel_id.to_string());

        // Backfill thread_id: when OAB just created a new thread, the sender
        // was built before the thread existed. Patch it so the agent sees
        // thread_id on the very first turn.
        let mut sender = sender;
        if sender.thread_id.is_none() && thread_channel.parent_id.is_some() {
            sender.thread_id = Some(thread_channel.channel_id.clone());
        }

        let dispatcher = self.dispatcher.clone();
        let stt_cfg = self.stt_config.clone();
        let gate_router = self.router.clone();

        tokio::spawn(async move {
            // Best-effort echo before the agent reply so the user can verify STT.
            crate::stt::post_echo(
                &adapter,
                &thread_channel,
                &trigger_msg,
                &echo_entries,
                &stt_cfg,
            )
            .await;

            let sender_id = sender.sender_id.clone();
            let sender_name = sender.sender_name.clone();

            // Shared ingress trust gate (L3 identity). Redundant-but-matching with
            // Discord's own user check that already ran pre-dispatch, so it cannot
            // deny anything already admitted (non-regressive). L2 (channel/thread/DM)
            // stays in the adapter for Discord — its registry entry is L2-open.
            //
            // Bots are skipped here: Discord's `is_denied_user` has a `!is_bot`
            // bypass (bot admission is handled separately by allow_bot_messages +
            // trusted_bot_ids), and the shared L3 gate is human-identity only.
            // Running it on bots would wrongly drop trusted bot-to-bot messages
            // when allow_all_users=false (multi-agent). See PR #1270 review F1.
            // Phase 1c makes this authoritative and removes the scattered check.
            if l3_gate_applies(sender.is_bot) {
                let decision = gate_router.gate_incoming(
                    "discord",
                    &thread_channel.channel_id,
                    is_dm,
                    &sender_id,
                );
                if !decision.is_allowed() {
                    tracing::info!(
                        sender = %sender_id,
                        channel = %thread_channel.channel_id,
                        ?decision,
                        "discord message denied by trust gate"
                    );
                    return;
                }
            }
            let sender_json = serde_json::to_string(&sender).unwrap();
            let thread_key = dispatcher.key("discord", &thread_channel.channel_id, &sender_id);
            let estimated_tokens = crate::dispatch::estimate_tokens(&prompt, &extra_blocks);
            let buf_msg = crate::dispatch::BufferedMessage {
                sender_json,
                sender_name,
                prompt,
                extra_blocks,
                trigger_msg,
                arrived_at: std::time::Instant::now(),
                estimated_tokens,
                other_bot_present: other_bot_present_flag,
                recipient: None, // Slack-only (assistant mode); N/A for Discord
            };
            let _ = adapter
                .update_task_lifecycle(&thread_channel, TaskLifecycleEvent::Enqueued)
                .await;
            if let Err(e) = dispatcher
                .submit(thread_key, thread_channel.clone(), adapter.clone(), buf_msg)
                .await
            {
                let _ = adapter
                    .update_task_lifecycle(
                        &thread_channel,
                        TaskLifecycleEvent::Failed {
                            message: e.to_string(),
                        },
                    )
                    .await;
                error!("dispatcher submit error: {e}");
            }
        });
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        let bot_id = ctx.cache.current_user().id;

        // Ignore bot's own reactions to prevent feedback loops.
        if reaction.user_id == Some(bot_id) {
            return;
        }

        // Extract unicode emoji string from the reaction.
        let emoji_str = match &reaction.emoji {
            ReactionType::Unicode(s) => s.clone(),
            _ => {
                tracing::debug!(emoji = ?reaction.emoji, "ignoring non-unicode reaction");
                return;
            }
        };

        // Look up mapping (early exit before any API calls).
        let mapping = &self.router.reactions_config().mapping;
        let prompt = match mapping.get(&emoji_str) {
            Some(text) => text.clone(),
            None => return, // emoji not mapped
        };

        let user_id = match reaction.user_id {
            Some(id) => id,
            None => return,
        };

        // Determine if reactor is a bot (from member hint or user fetch).
        let is_reactor_bot = reaction
            .member
            .as_ref()
            .map(|m| m.user.bot)
            .unwrap_or(false);

        // Bot gating: apply same allow_bot_messages policy as message().
        if is_reactor_bot {
            match self.allow_bot_messages {
                AllowBots::Off => return,
                // For reactions there is no @mention concept — treat as "not mentioned".
                AllowBots::Mentions => return,
                AllowBots::All => {
                    // When trusted_bot_ids is configured, only those bots are allowed.
                    if !self.trusted_bot_ids.is_empty()
                        && !self.trusted_bot_ids.contains(&user_id.get())
                    {
                        return;
                    }
                }
            }
        }

        let adapter = self.discord_adapter(&ctx);

        let channel_id = reaction.channel_id;
        let effective_allowed_channels = self.effective_allowed_channels();

        // AllowUsers::Mentions means reactions cannot trigger (no @mention possible).
        if self.allow_user_messages == AllowUsers::Mentions {
            return;
        }

        // --- Pre-spawn: channel/thread detection + allowlist + participation ---
        // Doing this before spawn so we have &self for bot_participated_in_thread
        // and can reject unallowed channels without any expensive API calls.

        let in_allowed_channel =
            self.allow_all_channels || effective_allowed_channels.contains(&channel_id.get());

        // F3 fix: Use detect_thread helper.
        let (thread_channel, is_thread) = match channel_id.to_channel(&ctx.http).await {
            Ok(serenity::model::channel::Channel::Guild(gc)) => {
                let has_thread_metadata = gc.thread_metadata.is_some();
                let parent = gc.parent_id.map(|p| p.get());
                let (in_allowed_thread, _bot_owns) = detect_thread(
                    has_thread_metadata,
                    parent,
                    gc.owner_id.map(|o| o.get()),
                    bot_id.get(),
                    &effective_allowed_channels,
                    self.allow_all_channels,
                    in_allowed_channel,
                );
                if has_thread_metadata {
                    if !in_allowed_thread {
                        return;
                    }
                    (
                        ChannelRef {
                            platform: "discord".into(),
                            channel_id: channel_id.get().to_string(),
                            thread_id: None,
                            parent_id: parent.map(|p| p.to_string()),
                            origin_event_id: None,
                        },
                        true,
                    )
                } else {
                    if !in_allowed_channel {
                        return;
                    }
                    (
                        ChannelRef {
                            platform: "discord".into(),
                            channel_id: channel_id.get().to_string(),
                            thread_id: None,
                            parent_id: None,
                            origin_event_id: None,
                        },
                        false,
                    )
                }
            }
            _ => return,
        };

        // F1 fix: Only call bot_participated_in_thread when the channel IS a
        // thread AND gating mode requires it. This completely avoids the
        // 200-message API fetch for non-thread channels and unallowed threads.
        let (bot_involved, other_bot_present) = if is_thread
            && matches!(
                self.allow_user_messages,
                AllowUsers::Involved | AllowUsers::MultibotMentions
            ) {
            self.bot_participated_in_thread(&ctx.http, channel_id, bot_id)
                .await
        } else {
            // For non-thread: still check multibot cache for dispatch info.
            let mb = {
                let cache = self.multibot_threads.lock().await;
                cache.contains_key(&channel_id.to_string())
            } || self.multibot_cache.is_multibot(&channel_id.to_string());
            (false, mb)
        };

        // Gating decision based on allow_user_messages mode.
        let message_author_id = reaction.message_author_id;
        let targets_this_bot = message_author_id.is_some_and(|a| a == bot_id);
        if !should_process_reaction(
            self.allow_user_messages,
            is_thread,
            bot_involved,
            other_bot_present,
            targets_this_bot,
        ) {
            return;
        }

        // --- Spawn: user resolution + is_denied_user + dispatch ---
        let message_id = reaction.message_id;
        let allow_all_users = self.allow_all_users;
        let allowed_users = self.allowed_users.clone();
        let allow_bot_messages = self.allow_bot_messages;
        let trusted_bot_ids = self.trusted_bot_ids.clone();
        let dispatcher = self.dispatcher.clone();
        let http = ctx.http.clone();

        tokio::spawn(async move {
            // F2 fix: Fetch user info first, then apply user gating with confirmed bot status.
            let (sender_name, display_name, is_bot_confirmed) = match user_id.to_user(&http).await {
                Ok(user) => {
                    let display = user.global_name.as_ref().unwrap_or(&user.name).clone();
                    (user.name.clone(), display, user.bot)
                }
                Err(_) => {
                    let fallback = user_id.to_string();
                    (fallback.clone(), fallback, is_reactor_bot)
                }
            };

            // Defense-in-depth: if to_user() reveals this is a bot but member was
            // None (rare edge case), re-apply bot gating retroactively.
            if is_bot_confirmed && !is_reactor_bot {
                match allow_bot_messages {
                    AllowBots::Off | AllowBots::Mentions => return,
                    AllowBots::All => {
                        if !trusted_bot_ids.is_empty() && !trusted_bot_ids.contains(&user_id.get())
                        {
                            return;
                        }
                    }
                }
            }

            // F2 fix: User allowlist check AFTER to_user() confirms bot status.
            if is_denied_user(
                is_bot_confirmed,
                allow_all_users,
                &allowed_users,
                user_id.get(),
            ) {
                return;
            }

            let trigger_msg = MessageRef {
                channel: ChannelRef {
                    platform: "discord".into(),
                    channel_id: channel_id.get().to_string(),
                    thread_id: None,
                    parent_id: None,
                    origin_event_id: None,
                },
                message_id: message_id.to_string(),
            };

            // F3 fix: Use build_sender_context helper.
            let sender = build_sender_context(
                &user_id.to_string(),
                &sender_name,
                &display_name,
                &channel_id.get().to_string(),
                thread_channel.parent_id.as_deref(),
                is_bot_confirmed,
                &chrono::Utc::now().to_rfc3339(),
                &message_id.to_string(),
                &bot_id.to_string(),
            );

            let sender_id = sender.sender_id.clone();
            let sender_name_clone = sender.sender_name.clone();
            let sender_json = serde_json::to_string(&sender).unwrap();
            let thread_key = dispatcher.key("discord", &thread_channel.channel_id, &sender_id);
            let estimated_tokens = crate::dispatch::estimate_tokens(&prompt, &[]);
            let buf_msg = crate::dispatch::BufferedMessage {
                sender_json,
                sender_name: sender_name_clone,
                prompt,
                extra_blocks: Vec::new(),
                trigger_msg,
                arrived_at: std::time::Instant::now(),
                estimated_tokens,
                other_bot_present,
                recipient: None,
            };

            let _ = adapter
                .update_task_lifecycle(&thread_channel, TaskLifecycleEvent::Enqueued)
                .await;
            if let Err(e) = dispatcher
                .submit(thread_key, thread_channel.clone(), adapter.clone(), buf_msg)
                .await
            {
                let _ = adapter
                    .update_task_lifecycle(
                        &thread_channel,
                        TaskLifecycleEvent::Failed {
                            message: e.to_string(),
                        },
                    )
                    .await;
                error!("reaction mapping dispatcher submit error: {e}");
            }
        });
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "discord bot connected");

        // Build the shared command list once.
        let commands = vec![
            CreateCommand::new("models").description("Select the AI model for this session"),
            CreateCommand::new("agents").description("Select the agent mode for this session"),
            CreateCommand::new("cancel").description("Cancel the current operation"),
            CreateCommand::new("cancel-all")
                .description("Cancel current operation and drop all buffered messages"),
            CreateCommand::new("reset").description("Reset the conversation session"),
            CreateCommand::new("help")
                .description("Show the Discord and Cursor handoff quick guide"),
            CreateCommand::new("workspace")
                .description("Inspect workspace routing for this channel or session")
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "status",
                    "Show the current session workspace and channel default",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "list",
                    "List configured workspace aliases",
                )),
            CreateCommand::new("session")
                .description("Attach, inspect, detach, or close a Cursor session")
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "status",
                    "Show session lifecycle state and workspace",
                ))
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::SubCommand,
                        "attach",
                        "Attach an exited local Cursor chat to this project or thread",
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "chat_id",
                            "Select a recent exited Cursor chat from this project",
                        )
                        .required(true)
                        .set_autocomplete(true),
                    )
                    .add_sub_option(CreateCommandOption::new(
                        CommandOptionType::String,
                        "title",
                        "New thread title when run in a project channel",
                    )),
                )
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "detach",
                    "Pause Discord ownership so a local ACP client can resume it",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "close",
                    "Close this session and clear buffered messages",
                )),
            CreateCommand::new("project")
                .description("Create and manage private workspace channels")
                .default_member_permissions(Permissions::MANAGE_CHANNELS)
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::SubCommand,
                        "create",
                        "Create a private channel for a workspace",
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "workspace",
                            "Workspace alias from /workspace list (for example: openab)",
                        )
                        .required(true)
                        .set_autocomplete(true),
                    )
                    .add_sub_option(CreateCommandOption::new(
                        CommandOptionType::String,
                        "name",
                        "Optional Discord channel name",
                    ))
                    .add_sub_option(CreateCommandOption::new(
                        CommandOptionType::Role,
                        "role",
                        "Optional role that can access the private channel",
                    )),
                )
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "list",
                    "List project channels in this server",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "status",
                    "Show the project bound to this channel",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "home",
                    "Create or update the interactive Project Home card",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::SubCommand,
                    "remove",
                    "Unlink this channel without deleting it or the repository",
                ))
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::SubCommandGroup,
                        "access",
                        "Manage users and roles that can access this project channel",
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::SubCommand,
                            "add",
                            "Grant a user or role access to this project channel",
                        )
                        .add_sub_option(CreateCommandOption::new(
                            CommandOptionType::User,
                            "user",
                            "User to grant access",
                        ))
                        .add_sub_option(CreateCommandOption::new(
                            CommandOptionType::Role,
                            "role",
                            "Role to grant access",
                        )),
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::SubCommand,
                            "remove",
                            "Revoke a user or role from this project channel",
                        )
                        .add_sub_option(CreateCommandOption::new(
                            CommandOptionType::User,
                            "user",
                            "User to revoke",
                        ))
                        .add_sub_option(CreateCommandOption::new(
                            CommandOptionType::Role,
                            "role",
                            "Role to revoke",
                        )),
                    ),
                ),
            CreateCommand::new("remind")
                .description("Set a one-shot reminder to mention users/roles after a delay")
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "targets",
                        "Users/roles to mention (e.g. @user1 @role1)",
                    )
                    .required(true),
                )
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "message",
                        "Reminder message",
                    )
                    .required(true),
                )
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "delay",
                        "Delay before firing (e.g. 30m, 2h, 1d)",
                    )
                    .required(true),
                ),
            CreateCommand::new("auth").description("Authenticate the backend agent (device flow)"),
            CreateCommand::new("usage")
                .description("Show backend account usage and billing information"),
            CreateCommand::new("export-thread")
                .description("Download this thread as a text file")
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "limit",
                    "Export only the most recent N messages (1–5000)",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::String,
                    "since",
                    "Export messages after this message ID",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Integer,
                    "days",
                    "Export messages from the last N days (1–365)",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Boolean,
                    "all",
                    "Export all messages (up to 5000). Default is last 100.",
                )),
        ];

        // Register global commands only. Registering the same commands per-guild
        // makes Discord show duplicate slash commands in guild command pickers.
        if let Err(e) = Command::set_global_commands(&ctx.http, commands.clone()).await {
            tracing::warn!(error = %e, "failed to register global slash commands");
        } else {
            info!("registered global slash commands");
        }

        // One-time migration cleanup: older versions registered the same
        // slash commands per-guild, and Discord persists those server-side.
        // Keep guild command sets empty so only global commands are shown.
        for guild in &ready.guilds {
            let guild_id = guild.id;
            if let Err(e) = guild_id.set_commands(&ctx.http, Vec::new()).await {
                tracing::warn!(
                    %guild_id,
                    error = %e,
                    "failed to clear stale guild slash commands"
                );
            }
        }

        self.reconcile_project_channels(&ctx).await;

        // Re-schedule any pending reminders that survived a restart.
        let pending = self.reminder_store.pending().await;
        if !pending.is_empty() {
            let mut scheduled = self.scheduled_ids.lock().await;
            let mut count = 0;
            for r in pending {
                if scheduled.insert(r.id.clone()) {
                    remind::schedule_reminder(ctx.http.clone(), self.reminder_store.clone(), r);
                    count += 1;
                }
            }
            if count > 0 {
                info!(count, "re-scheduled pending reminders");
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Autocomplete(cmd) if cmd.data.name == "project" => {
                self.handle_project_autocomplete(&ctx, &cmd).await;
            }
            Interaction::Autocomplete(cmd) if cmd.data.name == "session" => {
                self.handle_session_autocomplete(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "models" => {
                self.handle_config_command(&ctx, &cmd, "model", "model")
                    .await;
            }
            Interaction::Command(cmd) if cmd.data.name == "agents" => {
                self.handle_config_command(&ctx, &cmd, "agent", "agent")
                    .await;
            }
            Interaction::Command(cmd) if cmd.data.name == "cancel" => {
                self.handle_cancel_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "cancel-all" => {
                self.handle_cancel_all_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "reset" => {
                self.handle_reset_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "help" => {
                self.handle_help_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "workspace" => {
                self.handle_workspace_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "session" => {
                self.handle_session_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "project" => {
                self.handle_project_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "remind" => {
                self.handle_remind_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "export-thread" => {
                self.handle_export_thread_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "auth" => {
                self.handle_auth_command(&ctx, &cmd).await;
            }
            Interaction::Command(cmd) if cmd.data.name == "usage" => {
                self.handle_usage_command(&ctx, &cmd).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("oab_session:") => {
                self.handle_session_control(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("oab_task:") => {
                self.handle_task_control(&ctx, &comp).await;
            }
            Interaction::Component(comp)
                if comp.data.custom_id.starts_with("oab_admin:")
                    || comp.data.custom_id == "oab_admin_cleanup"
                    || comp.data.custom_id == "oab_admin_channel_category" =>
            {
                self.handle_admin_component(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("oab_help:") => {
                self.handle_help_component(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id == "oab_help_project" => {
                self.handle_help_project_select(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("oab_sessions:") => {
                self.handle_session_manager_component(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id == "oab_project_actions" => {
                self.handle_project_action_select(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("oab_project:") => {
                self.handle_project_component(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id == "oab_attach_chat" => {
                self.handle_attach_chat_select(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id == "oab_recent_task" => {
                self.handle_recent_task_select(&ctx, &comp).await;
            }
            Interaction::Modal(modal) if modal.data.custom_id == "oab_project_new" => {
                self.handle_project_new_task_modal(&ctx, &modal).await;
            }
            Interaction::Modal(modal) if modal.data.custom_id == "oab_admin_category_create" => {
                self.handle_admin_category_modal(&ctx, &modal).await;
            }
            Interaction::Modal(modal)
                if modal
                    .data
                    .custom_id
                    .starts_with("oab_admin_channel_create:") =>
            {
                self.handle_admin_channel_modal(&ctx, &modal).await;
            }
            Interaction::Modal(modal)
                if modal.data.custom_id.starts_with("oab_project_attach:") =>
            {
                self.handle_project_attach_modal(&ctx, &modal).await;
            }
            Interaction::Modal(modal) if modal.data.custom_id.starts_with("oab_task_prompt:") => {
                self.handle_task_prompt_modal(&ctx, &modal).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("acp_config_") => {
                self.handle_config_select(&ctx, &comp).await;
            }
            Interaction::Component(comp) if comp.data.custom_id.starts_with("acp_pg:") => {
                self.handle_pagination(&ctx, &comp).await;
            }
            _ => {}
        }
    }
}

// --- Slash command & interaction handlers ---

impl Handler {
    fn project_actions_for(&self, workspace_alias: &str) -> Vec<&DiscordProjectActionConfig> {
        self.project_actions
            .iter()
            .filter(|action| action.workspace_alias == workspace_alias)
            .collect()
    }

    fn visible_projects(
        &self,
        guild_id: Option<u64>,
        user_id: u64,
        role_ids: &HashSet<u64>,
        permissions: Option<Permissions>,
    ) -> Vec<ProjectBinding> {
        let Some(guild_id) = guild_id else {
            return Vec::new();
        };
        self.project_registry
            .list_guild(guild_id)
            .into_iter()
            .filter(|binding| project_is_visible_to(binding, user_id, role_ids, permissions))
            .collect()
    }

    async fn managed_sessions_for_project(
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

    async fn handle_help_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        let _scope = match self.resolve_command_scope(ctx, cmd).await {
            Ok(scope) => scope,
            Err(message) => {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(message)
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
        };
        let avatar_url = ctx.cache.current_user().avatar_url();
        let role_ids = cmd
            .member
            .as_ref()
            .map(|member| member.roles.iter().map(|role_id| role_id.get()).collect())
            .unwrap_or_default();
        let permissions = cmd.member.as_ref().and_then(|member| member.permissions);
        let projects = self.visible_projects(
            cmd.guild_id.map(|guild_id| guild_id.get()),
            cmd.user.id.get(),
            &role_ids,
            permissions,
        );
        let response = CreateInteractionResponse::Message(
            help_action_center(
                avatar_url.as_deref(),
                &projects,
                self.admin_control.is_some(),
            )
            .ephemeral(true),
        );
        if let Err(error) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(%error, "failed to respond to /help command");
        }
    }

    async fn handle_help_component(
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
                    .content("🚫 你沒有使用這個 Bot 的權限。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let topic = comp
            .data
            .custom_id
            .strip_prefix("oab_help:")
            .unwrap_or("open");
        let role_ids = comp
            .member
            .as_ref()
            .map(|member| member.roles.iter().map(|role_id| role_id.get()).collect())
            .unwrap_or_default();
        let permissions = comp.member.as_ref().and_then(|member| member.permissions);
        let projects = self.visible_projects(
            comp.guild_id.map(|guild_id| guild_id.get()),
            comp.user.id.get(),
            &role_ids,
            permissions,
        );
        if matches!(topic, "open" | "back") {
            let avatar_url = ctx.cache.current_user().avatar_url();
            let response = if topic == "back" {
                CreateInteractionResponse::UpdateMessage(help_action_center(
                    avatar_url.as_deref(),
                    &projects,
                    self.admin_control.is_some(),
                ))
            } else {
                CreateInteractionResponse::Message(
                    help_action_center(
                        avatar_url.as_deref(),
                        &projects,
                        self.admin_control.is_some(),
                    )
                    .ephemeral(true),
                )
            };
            if let Err(error) = comp.create_response(&ctx.http, response).await {
                tracing::error!(%error, "failed to open help action center");
            }
            return;
        }

        let task = self.task_registry.task_for_thread(comp.channel_id.get());
        let binding = task
            .as_ref()
            .and_then(|task| {
                self.project_registry
                    .binding_for_channel(task.project_channel_id)
            })
            .or_else(|| {
                self.project_registry
                    .binding_for_channel(comp.channel_id.get())
            });
        let binding = match binding {
            Some(binding) => Some(binding),
            None => self
                .project_binding_for_channel(ctx, comp.channel_id)
                .await
                .ok()
                .map(|(binding, _)| binding),
        };
        if topic == "sessions" {
            let Some(binding) = binding else {
                let message = help_topic_message(
                    topic,
                    comp.channel_id.get(),
                    task.as_ref(),
                    None,
                    &projects,
                );
                let _ = comp
                    .create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(message))
                    .await;
                return;
            };
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer session manager from help");
                return;
            }
            let entries = self.managed_sessions_for_project(binding.channel_id).await;
            if let Err(error) = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(&binding, &entries, None, None),
                )
                .await
            {
                tracing::error!(%error, "failed to open session manager from help");
            }
            return;
        }
        let message = help_topic_message(
            topic,
            comp.channel_id.get(),
            task.as_ref(),
            binding.as_ref(),
            &projects,
        );
        if let Err(error) = comp
            .create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(message))
            .await
        {
            tracing::error!(%error, topic, "failed to update help action center");
        }
    }

    async fn handle_help_project_select(
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
                    .content("🚫 你沒有使用這個 Bot 的權限。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }

        let role_ids = comp
            .member
            .as_ref()
            .map(|member| member.roles.iter().map(|role_id| role_id.get()).collect())
            .unwrap_or_default();
        let permissions = comp.member.as_ref().and_then(|member| member.permissions);
        let projects = self.visible_projects(
            comp.guild_id.map(|guild_id| guild_id.get()),
            comp.user.id.get(),
            &role_ids,
            permissions,
        );
        let selected_channel_id = match &comp.data.kind {
            ComponentInteractionDataKind::StringSelect { values } => {
                values.first().and_then(|value| value.parse::<u64>().ok())
            }
            _ => None,
        };
        let selected = selected_channel_id.and_then(|channel_id| {
            projects
                .iter()
                .find(|binding| binding.channel_id == channel_id)
        });
        let message = selected.map_or_else(
            || {
                let avatar_url = ctx.cache.current_user().avatar_url();
                help_action_center(
                    avatar_url.as_deref(),
                    &projects,
                    self.admin_control.is_some(),
                )
                .content("⚠️ 這個 project 已移除或你已沒有存取權限，清單已重新整理。")
            },
            |binding| help_project_message(binding, comp.channel_id.get()),
        );
        if let Err(error) = comp
            .create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(message))
            .await
        {
            tracing::error!(%error, "failed to open selected project from help");
        }
    }

    async fn handle_admin_component(
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
                    .content("🚫 你沒有使用這個 Bot 的權限。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let Some(client) = &self.admin_control else {
            let response = CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Server management is not configured.")
                    .embeds(Vec::new())
                    .components(vec![CreateActionRow::Buttons(vec![CreateButton::new(
                        "oab_help:back",
                    )
                    .label("← Help")
                    .style(ButtonStyle::Secondary)])]),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };
        let Some(guild_id) = comp.guild_id.map(|value| value.get()) else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Server management is only available inside the configured server.")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };
        if comp.data.custom_id == "oab_admin_channel_category" {
            let category_id = match &comp.data.kind {
                ComponentInteractionDataKind::StringSelect { values } => {
                    values.first().and_then(|value| value.parse::<u64>().ok())
                }
                _ => None,
            };
            let Some(category_id) = category_id else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This category selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if let Err(error) = comp
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Modal(admin_channel_modal(category_id)),
                )
                .await
            {
                tracing::error!(%error, "failed to open admin channel modal");
            }
            return;
        }
        if comp.data.custom_id == "oab_admin_cleanup" {
            let selected = match &comp.data.kind {
                ComponentInteractionDataKind::StringSelect { values } => values.first(),
                _ => None,
            };
            let Some((target_type, target_id)) = selected
                .and_then(|value| value.split_once(':'))
                .and_then(|(target_type, target_id)| {
                    target_id
                        .parse::<u64>()
                        .ok()
                        .map(|target_id| (target_type, target_id))
                })
            else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This cleanup selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer cleanup preview selection");
                return;
            }
            let card = match client
                .preview_deletion(comp.user.id.get(), guild_id, target_type, target_id)
                .await
            {
                Ok(preview) => admin_deletion_preview_card(&preview.plan),
                Err(error) => {
                    tracing::warn!(%error, "Discord Admin cleanup preview failed");
                    admin_error_card(&error)
                }
            };
            if let Err(error) = comp.edit_response(&ctx.http, card.into_edit()).await {
                tracing::error!(%error, "failed to show cleanup preview");
            }
            return;
        }
        let action = comp.data.custom_id.strip_prefix("oab_admin:").unwrap_or("");
        if action == "create" {
            if let Err(error) = comp
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Modal(admin_category_modal()),
                )
                .await
            {
                tracing::error!(%error, "failed to open admin category modal");
            }
            return;
        }
        if action == "cancel" {
            let response = CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content("Operation cancelled. No server change was made.")
                    .embeds(Vec::new())
                    .components(vec![CreateActionRow::Buttons(vec![
                        CreateButton::new("oab_admin:refresh")
                            .label("← Server management")
                            .style(ButtonStyle::Secondary),
                        CreateButton::new("oab_help:back")
                            .label("← Help")
                            .style(ButtonStyle::Secondary),
                    ])]),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        if let Err(error) = comp.defer(&ctx.http).await {
            tracing::error!(%error, action, "failed to defer admin control interaction");
            return;
        }
        let result = if action == "inventory" {
            client
                .inventory(comp.user.id.get(), guild_id)
                .await
                .map(|inventory| admin_inventory_card(&inventory))
        } else if action == "channel_setup" {
            client
                .inventory(comp.user.id.get(), guild_id)
                .await
                .map(|inventory| admin_channel_category_card(&inventory))
        } else if action == "cleanup" {
            client
                .cleanup(comp.user.id.get(), guild_id)
                .await
                .map(|cleanup| admin_cleanup_card(&cleanup))
        } else if let Some(plan_id) = action.strip_prefix("confirm_delete:") {
            client
                .apply_deletion(comp.user.id.get(), guild_id, plan_id)
                .await
                .map(|result| admin_deleted_card(&result.deleted))
        } else if let Some(plan_id) = action.strip_prefix("confirm_channel:") {
            client
                .apply_channel(comp.user.id.get(), guild_id, plan_id)
                .await
                .map(|result| admin_channel_created_card(&result.channel))
        } else if let Some(plan_id) = action.strip_prefix("confirm:") {
            client
                .apply_category(comp.user.id.get(), guild_id, plan_id)
                .await
                .map(|result| admin_category_created_card(&result.category))
        } else {
            client
                .status(comp.user.id.get(), guild_id)
                .await
                .map(|status| admin_status_card(&status))
        };
        let card = result.unwrap_or_else(|error| {
            tracing::warn!(%error, action, "Discord Admin control request failed");
            admin_error_card(&error)
        });
        if let Err(error) = comp.edit_response(&ctx.http, card.into_edit()).await {
            tracing::error!(%error, action, "failed to update admin control card");
        }
    }

    async fn handle_admin_category_modal(
        &self,
        ctx: &Context,
        modal: &serenity::model::application::ModalInteraction,
    ) {
        if modal.user.bot
            || is_denied_user(
                false,
                self.allow_all_users,
                &self.allowed_users,
                modal.user.id.get(),
            )
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 你沒有使用這個 Bot 的權限。")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let Some(client) = &self.admin_control else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Server management is not configured.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let Some(guild_id) = modal.guild_id.map(|value| value.get()) else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Server management is only available inside the configured server.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let name = modal_input_value(modal, "name")
            .map(str::trim)
            .unwrap_or_default();
        let position = modal_input_value(modal, "position")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::parse::<i64>)
            .transpose();
        let position = match position {
            Ok(Some(value)) if value < 0 => {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ Position must be zero or greater.")
                        .ephemeral(true),
                );
                let _ = modal.create_response(&ctx.http, response).await;
                return;
            }
            Ok(value) => value,
            Err(_) => {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ Position must be a whole number.")
                        .ephemeral(true),
                );
                let _ = modal.create_response(&ctx.http, response).await;
                return;
            }
        };
        if let Err(error) = modal.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, "failed to defer admin category preview");
            return;
        }
        let card = match client
            .preview_category(modal.user.id.get(), guild_id, name, position)
            .await
        {
            Ok(preview) => admin_category_preview_card(&preview.plan),
            Err(error) => {
                tracing::warn!(%error, "Discord Admin category preview failed");
                admin_error_card(&error)
            }
        };
        if let Err(error) = modal.edit_response(&ctx.http, card.into_edit()).await {
            tracing::error!(%error, "failed to show admin category preview");
        }
    }

    async fn handle_admin_channel_modal(
        &self,
        ctx: &Context,
        modal: &serenity::model::application::ModalInteraction,
    ) {
        if modal.user.bot
            || is_denied_user(
                false,
                self.allow_all_users,
                &self.allowed_users,
                modal.user.id.get(),
            )
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 你沒有使用這個 Bot 的權限。")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let Some(client) = &self.admin_control else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Server management is not configured.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let Some(guild_id) = modal.guild_id.map(|value| value.get()) else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Server management is only available inside the configured server.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let Some(category_id) = modal
            .data
            .custom_id
            .strip_prefix("oab_admin_channel_create:")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ This category selection is invalid. Start Channel setup again.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let name = modal_input_value(modal, "name")
            .map(str::trim)
            .unwrap_or_default();
        let topic = modal_input_value(modal, "topic")
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if let Err(error) = modal.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, "failed to defer admin channel preview");
            return;
        }
        let card = match client
            .preview_channel(
                modal.user.id.get(),
                guild_id,
                category_id,
                name,
                topic,
            )
            .await
        {
            Ok(preview) => admin_channel_preview_card(&preview.plan),
            Err(error) => {
                tracing::warn!(%error, "Discord Admin channel preview failed");
                admin_error_card(&error)
            }
        };
        if let Err(error) = modal.edit_response(&ctx.http, card.into_edit()).await {
            tracing::error!(%error, "failed to show admin channel preview");
        }
    }

    async fn handle_session_manager_component(
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
                            "即將關閉 **{}**。這會停止目前工作並清除 OpenAB 的 session 關聯；repository、Discord thread 與 Cursor checkpoint 都不會刪除。",
                            suppress_mentions(&task.title)
                        ))
                        .colour(0xE74C3C),
                )
                .components(vec![CreateActionRow::Buttons(vec![
                    CreateButton::new(format!(
                        "oab_sessions:confirm_close:{project_channel_id}:{thread_id}"
                    ))
                    .label("Close session")
                    .style(ButtonStyle::Danger),
                    CreateButton::new(format!(
                        "oab_sessions:view:{project_channel_id}:{thread_id}"
                    ))
                    .label("Keep session")
                    .style(ButtonStyle::Secondary),
                ])]);
            let _ = comp.edit_response(&ctx.http, confirmation).await;
            return;
        }

        if action == "confirm_close" {
            let session_key = format!("discord:{thread_id}");
            let snapshot = self.router.pool().session_snapshot(&session_key).await;
            let note = if snapshot.externally_detached {
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
                    Ok(()) => match self.task_registry.set_state(thread_id, TaskState::Closed) {
                        Ok(_) if dropped > 0 => format!(
                            "✅ Session closed and {dropped} buffered message(s) dropped. Cursor checkpoint was kept."
                        ),
                        Ok(_) => {
                            "✅ Session closed. Cursor checkpoint was kept; you may now remove this item from the list."
                                .to_string()
                        }
                        Err(error) => format!("⚠️ Session closed, but task metadata could not be updated: {error}"),
                    },
                    Err(error) => format!("⚠️ Could not close session: {error}"),
                }
            };
            let entries = self.managed_sessions_for_project(project_channel_id).await;
            let _ = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(&binding, &entries, Some(thread_id), Some(note)),
                )
                .await;

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

    async fn handle_project_autocomplete(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        let Some(focused) = cmd.data.autocomplete() else {
            return;
        };
        if focused.name != "workspace" {
            return;
        }
        let used_aliases: HashSet<_> = cmd
            .guild_id
            .map(|guild_id| {
                self.project_registry
                    .list_guild(guild_id.get())
                    .into_iter()
                    .map(|binding| binding.workspace_alias)
                    .collect()
            })
            .unwrap_or_default();
        let choices = project_workspace_choices(
            &self.router.workspace_aliases(),
            &used_aliases,
            focused.value,
        );
        let response = choices.into_iter().fold(
            CreateAutocompleteResponse::new(),
            |response, (label, value)| response.add_string_choice(label, value),
        );
        if let Err(error) = cmd
            .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(response))
            .await
        {
            tracing::warn!(%error, "failed to respond to project workspace autocomplete");
        }
    }

    async fn project_binding_for_channel(
        &self,
        ctx: &Context,
        channel_id: ChannelId,
    ) -> Result<(ProjectBinding, bool), String> {
        let channel = channel_id
            .to_channel(&ctx.http)
            .await
            .map_err(|error| format!("Could not inspect this Discord channel: {error}"))?;
        let serenity::model::channel::Channel::Guild(channel) = channel else {
            return Err("請在 managed project channel 或其 task thread 使用這個功能。".into());
        };
        let is_thread = channel.thread_metadata.is_some();
        let project_channel_id = if is_thread {
            channel
                .parent_id
                .map(|id| id.get())
                .ok_or_else(|| "這個 thread 沒有 parent project channel。".to_string())?
        } else {
            channel_id.get()
        };
        let binding = self
            .project_registry
            .binding_for_channel(project_channel_id)
            .ok_or_else(|| {
                "請在 managed project channel 或其 task thread 使用這個功能。".to_string()
            })?;
        Ok((binding, is_thread))
    }

    async fn available_cursor_chats(
        &self,
        binding: &ProjectBinding,
        query: &str,
    ) -> Result<Vec<crate::cursor_session::CursorChatSummary>, String> {
        let workspace = self
            .router
            .workspace_aliases()
            .get(&binding.workspace_alias)
            .cloned()
            .ok_or_else(|| "Project workspace 目前未設定。".to_string())?;
        let query = query.trim().to_lowercase();
        let chats = crate::cursor_session::list_cursor_chats_for_workspace(std::path::Path::new(
            &workspace,
        ))
        .map_err(|error| friendly_attach_error(&error.to_string()))?;
        let mut available = Vec::new();
        for chat in chats {
            let label = cursor_chat_choice_label(&chat).to_lowercase();
            if !query.is_empty()
                && !chat.session_id.to_lowercase().contains(&query)
                && !label.contains(&query)
            {
                continue;
            }
            let checkpoint = crate::cursor_session::CursorChatCheckpoint {
                session_id: chat.session_id.clone(),
                working_dir: chat.working_dir.clone(),
            };
            if crate::cursor_session::cursor_chat_is_running(&checkpoint) {
                continue;
            }
            if self
                .router
                .pool()
                .external_session_is_available(&chat.session_id)
                .await
            {
                available.push(chat);
                if available.len() == SELECT_MENU_PAGE_SIZE {
                    break;
                }
            }
        }
        Ok(available)
    }

    async fn ensure_task_status_card(
        &self,
        ctx: &Context,
        task: &TaskRecord,
    ) -> Result<TaskRecord, String> {
        if let Some(message_id) = task.status_message_id {
            match ChannelId::new(task.thread_id)
                .edit_message(
                    &ctx.http,
                    MessageId::new(message_id),
                    task_status_edit(task),
                )
                .await
            {
                Ok(_) => return Ok(task.clone()),
                Err(error) if is_unknown_discord_message_error(&error) => {}
                Err(error) => return Err(format!("Could not update Task Status: {error}")),
            }
        }
        let message = ChannelId::new(task.thread_id)
            .send_message(&ctx.http, task_status_message(task))
            .await
            .map_err(|error| format!("Could not post Task Status: {error}"))?;
        match self
            .task_registry
            .set_status_message(task.thread_id, message.id.get())
        {
            Ok(updated) => Ok(updated),
            Err(error) => {
                let _ = message.delete(&ctx.http).await;
                Err(format!("Could not save Task Status: {error}"))
            }
        }
    }

    async fn ensure_task(
        &self,
        ctx: &Context,
        binding: &ProjectBinding,
        thread_id: u64,
        title: &str,
        created_by: u64,
    ) -> Result<TaskRecord, String> {
        let now = chrono::Utc::now();
        let title = truncate_for_discord(
            if title.trim().is_empty() {
                "Untitled task"
            } else {
                title.trim()
            },
            100,
        );
        let (task, created) = self
            .task_registry
            .ensure(TaskRecord {
                guild_id: binding.guild_id,
                project_channel_id: binding.channel_id,
                workspace_alias: binding.workspace_alias.clone(),
                thread_id,
                title,
                created_by,
                status_message_id: None,
                state: TaskState::Ready,
                queued_messages: 0,
                last_error: None,
                last_prompt: None,
                created_at: now,
                updated_at: now,
            })
            .map_err(|error| format!("Could not save task metadata: {error}"))?;
        let task = if created || task.status_message_id.is_none() {
            self.ensure_task_status_card(ctx, &task).await?
        } else {
            task
        };
        if created {
            if let Err(error) = self.upsert_project_home(ctx, binding).await {
                tracing::warn!(%error, thread_id, "failed to refresh Project Home after task registration");
            }
        }
        Ok(task)
    }

    async fn handle_session_autocomplete(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        let query = cmd
            .data
            .autocomplete()
            .filter(|focused| focused.name == "chat_id")
            .map(|focused| focused.value)
            .unwrap_or("");
        let chats = match self.project_binding_for_channel(ctx, cmd.channel_id).await {
            Ok((binding, _)) => self
                .available_cursor_chats(&binding, query)
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let response =
            chats
                .into_iter()
                .fold(CreateAutocompleteResponse::new(), |response, chat| {
                    response.add_string_choice(cursor_chat_choice_label(&chat), chat.session_id)
                });
        if let Err(error) = cmd
            .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(response))
            .await
        {
            tracing::warn!(%error, "failed to respond to session attach autocomplete");
        }
    }

    async fn upsert_project_home(
        &self,
        ctx: &Context,
        binding: &ProjectBinding,
    ) -> Result<bool, String> {
        let channel_id = ChannelId::new(binding.channel_id);
        let recent_tasks = self
            .task_registry
            .recent_for_project(binding.channel_id, 10);
        let mut home_message_id = binding.home_message_id;
        if home_message_id.is_none() {
            let expected_title = format!("📁 @{}", binding.workspace_alias);
            let messages = channel_id
                .messages(&ctx.http, GetMessages::new().limit(100))
                .await
                .map_err(|error| format!("Could not inspect existing Project Home: {error}"))?;
            home_message_id = messages
                .iter()
                .find(|message| {
                    message.author.id == ctx.cache.current_user().id
                        && message
                            .embeds
                            .iter()
                            .any(|embed| embed.title.as_deref() == Some(expected_title.as_str()))
                })
                .map(|message| message.id.get());
        }
        if let Some(message_id) = home_message_id {
            match channel_id
                .edit_message(
                    &ctx.http,
                    MessageId::new(message_id),
                    project_welcome_edit(binding, &recent_tasks),
                )
                .await
            {
                Ok(_) => {
                    if binding.home_message_id != Some(message_id) {
                        self.project_registry
                            .set_home_message_id(binding.guild_id, binding.channel_id, message_id)
                            .map_err(|error| {
                                format!("Could not save Project Home message: {error}")
                            })?;
                    }
                    return Ok(false);
                }
                Err(error) if is_unknown_discord_message_error(&error) => {}
                Err(error) => return Err(format!("Could not update Project Home: {error}")),
            }
        }

        let message = channel_id
            .send_message(&ctx.http, project_welcome_message(binding, &recent_tasks))
            .await
            .map_err(|error| format!("Could not post Project Home: {error}"))?;
        if let Err(error) = self.project_registry.set_home_message_id(
            binding.guild_id,
            binding.channel_id,
            message.id.get(),
        ) {
            let _ = message.delete(&ctx.http).await;
            return Err(format!("Could not save Project Home message: {error}"));
        }
        Ok(true)
    }

    async fn reconcile_project_channels(&self, ctx: &Context) {
        let aliases = self.router.workspace_aliases();
        let mut active = 0usize;
        let mut stale = 0usize;
        let mut unavailable = 0usize;
        let mut moved = 0usize;

        for binding in self.project_registry.all() {
            if !aliases.contains_key(&binding.workspace_alias) {
                self.router
                    .unbind_workspace_channel("discord", &binding.channel_id.to_string());
                unavailable += 1;
                tracing::warn!(
                    channel_id = binding.channel_id,
                    alias = %binding.workspace_alias,
                    "project reconciliation: workspace alias unavailable; binding retained"
                );
                continue;
            }

            match ChannelId::new(binding.channel_id)
                .to_channel(&ctx.http)
                .await
            {
                Ok(serenity::model::channel::Channel::Guild(channel))
                    if channel.guild_id.get() == binding.guild_id
                        && channel.kind == ChannelType::Text
                        && channel.thread_metadata.is_none() =>
                {
                    self.router.bind_workspace_channel(
                        "discord",
                        &binding.channel_id.to_string(),
                        &format!("@{}", binding.workspace_alias),
                    );
                    if self.project_category_id.is_some()
                        && channel.parent_id.map(|id| id.get()) != self.project_category_id
                    {
                        moved += 1;
                        tracing::warn!(
                            channel_id = binding.channel_id,
                            actual_category_id = ?channel.parent_id.map(|id| id.get()),
                            expected_category_id = ?self.project_category_id,
                            "project reconciliation: channel moved outside configured category"
                        );
                    }
                    if let Err(error) = self.upsert_project_home(ctx, &binding).await {
                        tracing::warn!(
                            %error,
                            channel_id = binding.channel_id,
                            "project reconciliation: Project Home update failed"
                        );
                    }
                    active += 1;
                }
                Ok(_) => {
                    self.router
                        .unbind_workspace_channel("discord", &binding.channel_id.to_string());
                    match self
                        .project_registry
                        .remove(binding.guild_id, binding.channel_id)
                    {
                        Ok(Some(_)) => stale += 1,
                        Ok(None) => {}
                        Err(error) => tracing::error!(
                            %error,
                            channel_id = binding.channel_id,
                            "project reconciliation: failed to prune invalid binding"
                        ),
                    }
                }
                Err(error) if is_unknown_discord_channel_error(&error) => {
                    self.router
                        .unbind_workspace_channel("discord", &binding.channel_id.to_string());
                    match self
                        .project_registry
                        .remove(binding.guild_id, binding.channel_id)
                    {
                        Ok(Some(_)) => stale += 1,
                        Ok(None) => {}
                        Err(remove_error) => tracing::error!(
                            error = %remove_error,
                            channel_id = binding.channel_id,
                            "project reconciliation: failed to prune deleted channel"
                        ),
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        channel_id = binding.channel_id,
                        "project reconciliation: channel check failed; binding retained"
                    );
                }
            }
        }

        info!(
            active,
            stale, unavailable, moved, "project channel reconciliation completed"
        );
    }

    async fn handle_project_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        if let Err(error) = cmd.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, "failed to defer /project response");
            return;
        }

        let content = self
            .run_project_command(ctx, cmd)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, user_id = %cmd.user.id, "project command rejected");
                format!("⚠️ {error}")
            });
        if let Err(error) = cmd
            .edit_response(&ctx.http, EditInteractionResponse::new().content(content))
            .await
        {
            tracing::error!(%error, "failed to edit /project response");
        }
    }

    async fn run_project_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) -> Result<String, String> {
        if !self.project_channels_enabled {
            return Err("Project channel creation is disabled in OpenAB configuration.".into());
        }
        if cmd.user.bot {
            return Err("Bots cannot manage project channels.".into());
        }
        if is_denied_user(
            false,
            self.allow_all_users,
            &self.allowed_users,
            cmd.user.id.get(),
        ) {
            return Err("You are not allowed to use this bot.".into());
        }
        let permissions = cmd
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .ok_or_else(|| "Run this command inside a Discord server.".to_string())?;
        if !permissions.contains(Permissions::ADMINISTRATOR)
            && !permissions.contains(Permissions::MANAGE_CHANNELS)
        {
            return Err("You need the Manage Channels permission to use this command.".into());
        }
        let guild_id = cmd
            .guild_id
            .ok_or_else(|| "Run this command inside a Discord server.".to_string())?;
        let top = cmd
            .data
            .options
            .first()
            .ok_or_else(|| "Choose a project subcommand.".to_string())?;
        let (action, options) = match &top.value {
            serenity::model::application::CommandDataOptionValue::SubCommand(options) => {
                (top.name.as_str(), options.as_slice())
            }
            serenity::model::application::CommandDataOptionValue::SubCommandGroup(group)
                if top.name == "access" =>
            {
                let command = group
                    .first()
                    .ok_or_else(|| "Choose access add or remove.".to_string())?;
                let options = match &command.value {
                    serenity::model::application::CommandDataOptionValue::SubCommand(options) => {
                        options.as_slice()
                    }
                    _ => return Err("Invalid project access subcommand.".into()),
                };
                let action = match command.name.as_str() {
                    "add" => "access-add",
                    "remove" => "access-remove",
                    _ => return Err("Unknown project access subcommand.".into()),
                };
                (action, options)
            }
            _ => return Err("Invalid project subcommand.".into()),
        };

        match action {
            "create" => {
                let raw_alias = options
                    .iter()
                    .find(|option| option.name == "workspace")
                    .and_then(|option| option.value.as_str())
                    .ok_or_else(|| "A workspace alias is required.".to_string())?;
                let alias = raw_alias.trim().trim_start_matches('@');
                if alias.is_empty() {
                    return Err("Workspace alias cannot be empty.".into());
                }
                let aliases = self.router.workspace_aliases();
                if !aliases.contains_key(alias) {
                    return Err(format!(
                        "Unknown workspace {}. Use `/workspace list` to see available aliases.",
                        inline_code(&format!("@{alias}"))
                    ));
                }
                if let Some(existing) = self
                    .project_registry
                    .binding_for_alias(guild_id.get(), alias)
                {
                    return Err(format!(
                        "Workspace {} already uses <#{}>.",
                        inline_code(&format!("@{alias}")),
                        existing.channel_id
                    ));
                }

                let requested_name = options
                    .iter()
                    .find(|option| option.name == "name")
                    .and_then(|option| option.value.as_str())
                    .unwrap_or(alias);
                let channel_name = sanitize_project_channel_name(requested_name);
                let access_role_id = options
                    .iter()
                    .find(|option| option.name == "role")
                    .and_then(|option| option.value.as_role_id());
                let category_id = self.project_category_id.ok_or_else(|| {
                    "Project category is missing from OpenAB configuration.".to_string()
                })?;

                let access = project_channel_access_permissions();
                let mut overwrites = vec![
                    PermissionOverwrite {
                        allow: Permissions::empty(),
                        deny: Permissions::VIEW_CHANNEL,
                        kind: PermissionOverwriteType::Role(guild_id.everyone_role()),
                    },
                    PermissionOverwrite {
                        allow: access,
                        deny: Permissions::empty(),
                        kind: PermissionOverwriteType::Member(cmd.user.id),
                    },
                    PermissionOverwrite {
                        allow: access,
                        deny: Permissions::empty(),
                        kind: PermissionOverwriteType::Member(ctx.cache.current_user().id),
                    },
                ];
                if let Some(role_id) = access_role_id {
                    overwrites.push(PermissionOverwrite {
                        allow: access,
                        deny: Permissions::empty(),
                        kind: PermissionOverwriteType::Role(role_id),
                    });
                }

                let audit_reason = format!("OpenAB project channel for @{alias}");
                let builder = CreateChannel::new(&channel_name)
                    .category(ChannelId::new(category_id))
                    .topic(format!("OpenAB project: @{alias}"))
                    .permissions(overwrites)
                    .audit_log_reason(&audit_reason);
                let channel = guild_id
                    .create_channel(&ctx.http, builder)
                    .await
                    .map_err(|error| {
                        format!(
                            "Could not create the Discord channel. Check the category ID and the bot's Manage Channels/Manage Roles permissions: {error}"
                        )
                    })?;

                let binding = ProjectBinding {
                    guild_id: guild_id.get(),
                    channel_id: channel.id.get(),
                    workspace_alias: alias.to_string(),
                    created_by: cmd.user.id.get(),
                    access_role_id: None,
                    access_user_ids: Vec::new(),
                    access_role_ids: access_role_id.map(|id| vec![id.get()]).unwrap_or_default(),
                    home_message_id: None,
                    created_at: chrono::Utc::now(),
                };
                if let Err(error) = self.project_registry.add(binding.clone()) {
                    if let Err(delete_error) = channel.id.delete(&ctx.http).await {
                        tracing::error!(%delete_error, channel_id = %channel.id, "failed to roll back unregistered project channel");
                    }
                    return Err(format!(
                        "Could not save the project mapping; the new channel was rolled back: {error}"
                    ));
                }
                self.router.bind_workspace_channel(
                    "discord",
                    &channel.id.to_string(),
                    &format!("@{alias}"),
                );

                if let Err(error) = self.upsert_project_home(ctx, &binding).await {
                    tracing::warn!(%error, channel_id = %channel.id, "failed to upsert Project Home");
                }

                Ok(format!(
                    "✅ Created private project channel <#{}> for {}. Open it and send the first task to start a dedicated thread.",
                    channel.id,
                    inline_code(&format!("@{alias}"))
                ))
            }
            "list" => {
                let entries = self.project_registry.list_guild(guild_id.get());
                if entries.is_empty() {
                    return Ok("📁 No managed project channels in this server.".into());
                }
                let mut lines = vec!["📁 **Managed project channels**".to_string()];
                lines.extend(entries.iter().map(|binding| {
                    format!(
                        "• <#{}> — {}",
                        binding.channel_id,
                        inline_code(&format!("@{}", binding.workspace_alias))
                    )
                }));
                Ok(truncate_for_discord(&lines.join("\n"), 1900))
            }
            "status" | "home" | "remove" | "access-add" | "access-remove" => {
                let channel = cmd
                    .channel_id
                    .to_channel(&ctx.http)
                    .await
                    .map_err(|error| format!("Could not inspect this channel: {error}"))?;
                let project_channel_id = match channel {
                    serenity::model::channel::Channel::Guild(channel)
                        if channel.thread_metadata.is_some() =>
                    {
                        channel.parent_id.map(|id| id.get()).ok_or_else(|| {
                            "This thread does not have a parent project channel.".to_string()
                        })?
                    }
                    serenity::model::channel::Channel::Guild(_) => cmd.channel_id.get(),
                    _ => return Err("Run this command in a server channel or thread.".into()),
                };

                if action == "home" {
                    let binding = self
                        .project_registry
                        .binding_for_channel(project_channel_id)
                        .filter(|binding| binding.guild_id == guild_id.get())
                        .ok_or_else(|| {
                            "This channel is not managed by the project registry.".to_string()
                        })?;
                    let created = self.upsert_project_home(ctx, &binding).await?;
                    return Ok(format!(
                        "✅ {} Project Home in <#{project_channel_id}>.",
                        if created { "Posted" } else { "Updated" }
                    ));
                }

                if action == "status" {
                    let binding = self
                        .project_registry
                        .binding_for_channel(project_channel_id)
                        .ok_or_else(|| {
                            "This channel is not managed by the project registry.".to_string()
                        })?;
                    let users = binding
                        .access_user_ids
                        .iter()
                        .map(|id| format!("<@{id}>"))
                        .collect::<Vec<_>>();
                    let roles = binding
                        .access_role_ids
                        .iter()
                        .map(|id| format!("<@&{id}>"))
                        .collect::<Vec<_>>();
                    let access = users.into_iter().chain(roles).collect::<Vec<_>>();
                    let access = if access.is_empty() {
                        "_Creator only_".to_string()
                    } else {
                        access.join(", ")
                    };
                    return Ok(truncate_for_discord(
                        &format!(
                            "📁 **Project status**\nChannel: <#{}>\nWorkspace: {}\nCreated by: <@{}>\nAdditional access: {access}",
                            binding.channel_id,
                            inline_code(&format!("@{}", binding.workspace_alias)),
                            binding.created_by
                        ),
                        1900,
                    ));
                }

                if action == "access-add" || action == "access-remove" {
                    let binding = self
                        .project_registry
                        .binding_for_channel(project_channel_id)
                        .filter(|binding| binding.guild_id == guild_id.get())
                        .ok_or_else(|| {
                            "This channel is not managed by the project registry.".to_string()
                        })?;
                    let user_id = options
                        .iter()
                        .find(|option| option.name == "user")
                        .and_then(|option| option.value.as_user_id());
                    let role_id = options
                        .iter()
                        .find(|option| option.name == "role")
                        .and_then(|option| option.value.as_role_id());
                    if user_id.is_some() == role_id.is_some() {
                        return Err("Choose exactly one user or one role.".into());
                    }

                    let (target, overwrite_type, mention) = if let Some(user_id) = user_id {
                        if user_id == ctx.cache.current_user().id {
                            return Err("The OpenAB bot access entry cannot be changed.".into());
                        }
                        if action == "access-remove" && user_id.get() == binding.created_by {
                            return Err("The project creator cannot be removed.".into());
                        }
                        (
                            ProjectAccessTarget::User(user_id.get()),
                            PermissionOverwriteType::Member(user_id),
                            format!("<@{}>", user_id.get()),
                        )
                    } else {
                        let role_id = role_id.unwrap();
                        if role_id == guild_id.everyone_role() {
                            return Err("The @everyone role cannot be changed.".into());
                        }
                        (
                            ProjectAccessTarget::Role(role_id.get()),
                            PermissionOverwriteType::Role(role_id),
                            format!("<@&{}>", role_id.get()),
                        )
                    };
                    let registered = match target {
                        ProjectAccessTarget::User(id) => binding.access_user_ids.contains(&id),
                        ProjectAccessTarget::Role(id) => binding.access_role_ids.contains(&id),
                    };
                    if action == "access-add" && registered {
                        return Err(format!("{mention} already has registered project access."));
                    }
                    if action == "access-remove" && !registered {
                        return Err(format!("{mention} is not in the project access list."));
                    }

                    if action == "access-add" {
                        ChannelId::new(project_channel_id)
                            .create_permission(
                                &ctx.http,
                                PermissionOverwrite {
                                    allow: project_channel_access_permissions(),
                                    deny: Permissions::empty(),
                                    kind: overwrite_type,
                                },
                            )
                            .await
                            .map_err(|error| {
                                format!("Could not grant Discord channel access: {error}")
                            })?;
                        let updated = match self.project_registry.add_access(
                            guild_id.get(),
                            project_channel_id,
                            target,
                        ) {
                            Ok(updated) => updated,
                            Err(error) => {
                                let _ = ChannelId::new(project_channel_id)
                                    .delete_permission(&ctx.http, overwrite_type)
                                    .await;
                                return Err(format!("Could not save project access: {error}"));
                            }
                        };
                        if let Err(error) = self.upsert_project_home(ctx, &updated).await {
                            tracing::warn!(%error, project_channel_id, "failed to refresh Project Home access");
                        }
                        return Ok(format!(
                            "✅ Granted {mention} access to <#{project_channel_id}>."
                        ));
                    }

                    ChannelId::new(project_channel_id)
                        .delete_permission(&ctx.http, overwrite_type)
                        .await
                        .map_err(|error| {
                            format!("Could not revoke Discord channel access: {error}")
                        })?;
                    let updated = match self.project_registry.remove_access(
                        guild_id.get(),
                        project_channel_id,
                        target,
                    ) {
                        Ok(updated) => updated,
                        Err(error) => {
                            let _ = ChannelId::new(project_channel_id)
                                .create_permission(
                                    &ctx.http,
                                    PermissionOverwrite {
                                        allow: project_channel_access_permissions(),
                                        deny: Permissions::empty(),
                                        kind: overwrite_type,
                                    },
                                )
                                .await;
                            return Err(format!("Could not save project access: {error}"));
                        }
                    };
                    if let Err(error) = self.upsert_project_home(ctx, &updated).await {
                        tracing::warn!(%error, project_channel_id, "failed to refresh Project Home access");
                    }
                    return Ok(format!(
                        "✅ Revoked {mention} access from <#{project_channel_id}>."
                    ));
                }

                let removed = self
                    .project_registry
                    .remove(guild_id.get(), project_channel_id)
                    .map_err(|error| format!("Could not save the registry update: {error}"))?
                    .ok_or_else(|| {
                        "This channel is not managed by the project registry.".to_string()
                    })?;
                self.router
                    .unbind_workspace_channel("discord", &project_channel_id.to_string());
                if let Err(error) = self.task_registry.remove_project(project_channel_id) {
                    tracing::warn!(%error, project_channel_id, "failed to remove project task metadata");
                }
                Ok(format!(
                    "✅ Unlinked <#{}> from {}. The Discord channel and repository were not deleted.",
                    project_channel_id,
                    inline_code(&format!("@{}", removed.workspace_alias))
                ))
            }
            _ => Err("Unknown project subcommand.".into()),
        }
    }

    async fn resolve_command_scope(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) -> Result<DiscordCommandScope, String> {
        self.resolve_session_scope(ctx, cmd.user.id.get(), cmd.user.bot, cmd.channel_id)
            .await
    }

    async fn resolve_session_scope(
        &self,
        ctx: &Context,
        user_id: u64,
        user_is_bot: bool,
        channel_id: ChannelId,
    ) -> Result<DiscordCommandScope, String> {
        if user_is_bot {
            return Err("🤖 Bots cannot use session management commands.".to_string());
        }
        if is_denied_user(false, self.allow_all_users, &self.allowed_users, user_id) {
            return Err("🚫 You are not allowed to use this bot.".to_string());
        }

        let effective_allowed_channels = self.effective_allowed_channels();
        let parent_id = match channel_id.to_channel(&ctx.http).await {
            Ok(serenity::model::channel::Channel::Guild(channel)) => {
                let parent_id = if channel.thread_metadata.is_some() {
                    channel.parent_id.map(|id| id.get())
                } else {
                    None
                };
                let allowed = session_command_channel_allowed(
                    channel_id.get(),
                    parent_id,
                    &effective_allowed_channels,
                    self.allow_all_channels,
                );
                if !allowed {
                    return Err(
                        "⚠️ Run this command inside an allowed Discord channel or thread."
                            .to_string(),
                    );
                }
                parent_id
            }
            Ok(serenity::model::channel::Channel::Private(_)) if self.allow_dm => None,
            Ok(serenity::model::channel::Channel::Private(_)) => {
                return Err("⚠️ Discord DMs are disabled for this bot.".to_string());
            }
            Ok(_) => {
                return Err(
                    "⚠️ Run this command inside an allowed Discord channel or thread.".to_string(),
                );
            }
            Err(error) => {
                warn!(%channel_id, %error, "failed to resolve slash command channel");
                return Err("⚠️ Could not inspect this Discord channel. Try again.".to_string());
            }
        };

        Ok(DiscordCommandScope {
            session_key: format!("discord:{}", channel_id.get()),
            channel_ref: ChannelRef {
                platform: "discord".into(),
                channel_id: channel_id.to_string(),
                thread_id: None,
                parent_id: parent_id.map(|id| id.to_string()),
                origin_event_id: None,
            },
        })
    }

    async fn handle_workspace_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        let scope = match self.resolve_command_scope(ctx, cmd).await {
            Ok(scope) => scope,
            Err(message) => {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(message)
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
        };

        let aliases = self.router.workspace_aliases();
        let channel_default = self.router.channel_workspace_spec(&scope.channel_ref);
        let subcommand = cmd
            .data
            .options
            .first()
            .map(|option| option.name.as_str())
            .unwrap_or("status");
        let content = match subcommand {
            "list" => format_workspace_list(&aliases, channel_default.as_deref()),
            _ => {
                let snapshot = self
                    .router
                    .pool()
                    .session_snapshot(&scope.session_key)
                    .await;
                format_workspace_status(&snapshot, channel_default.as_deref(), &aliases)
            }
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(content)
                .ephemeral(true),
        );
        if let Err(error) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(%error, "failed to respond to /workspace command");
        }
    }

    async fn attach_cursor_chat_request(
        &self,
        ctx: &Context,
        channel_id: ChannelId,
        chat_id: &str,
        title: &str,
        created_by: u64,
    ) -> Result<String, String> {
        let (binding, is_thread) = self.project_binding_for_channel(ctx, channel_id).await?;
        let project_channel_id = binding.channel_id;
        let workspace = self
            .router
            .workspace_aliases()
            .get(&binding.workspace_alias)
            .cloned()
            .ok_or_else(|| "Project workspace 目前未設定。".to_string())?;

        if is_thread {
            let checkpoint = crate::cursor_session::attach_cursor_chat(
                self.router.pool().as_ref(),
                chat_id,
                std::path::Path::new(&workspace),
                &channel_id.get().to_string(),
            )
            .await
            .map_err(|error| friendly_attach_error(&error.to_string()))?;
            let adapter = self.discord_adapter(ctx);
            let thread = ChannelRef {
                platform: "discord".into(),
                channel_id: channel_id.get().to_string(),
                thread_id: None,
                parent_id: Some(project_channel_id.to_string()),
                origin_event_id: None,
            };
            if let Err(error) = adapter
                .send_message(
                    &thread,
                    &format!(
                        "✅ Cursor session `{}` is attached to **@{}**. Send the next message here to continue the same chat.",
                        checkpoint.session_id, binding.workspace_alias
                    ),
                )
                .await
            {
                tracing::warn!(%error, %channel_id, "session attached but confirmation failed");
            }
            self.ensure_task(ctx, &binding, channel_id.get(), title, created_by)
                .await?;
            return Ok(format!(
                "✅ Cursor chat `{}` 已綁定到 <#{}>，請在該 thread 傳送下一則訊息繼續。",
                checkpoint
                    .session_id
                    .get(..8)
                    .unwrap_or(&checkpoint.session_id),
                channel_id.get()
            ));
        }

        let adapter = self.discord_adapter(ctx);
        let thread = crate::cursor_session::publish_cursor_chat(
            adapter.as_ref(),
            self.router.pool().as_ref(),
            &binding,
            std::path::Path::new(&workspace),
            chat_id,
            title,
        )
        .await
        .map_err(|error| friendly_attach_error(&error.to_string()))?;
        self.ensure_task(
            ctx,
            &binding,
            thread
                .channel_id
                .parse()
                .map_err(|_| "Invalid Discord thread ID")?,
            title,
            created_by,
        )
        .await?;
        Ok(format!(
            "✅ Cursor chat 已發佈到 <#{}>，請到該 thread 繼續。",
            thread.channel_id
        ))
    }

    async fn handle_session_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        let scope = match self.resolve_command_scope(ctx, cmd).await {
            Ok(scope) => scope,
            Err(message) => {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(message)
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
        };

        let top = cmd.data.options.first();
        let subcommand = top.map(|option| option.name.as_str()).unwrap_or("status");
        let options = top
            .and_then(|option| match &option.value {
                serenity::model::application::CommandDataOptionValue::SubCommand(options) => {
                    Some(options.as_slice())
                }
                _ => None,
            })
            .unwrap_or(&[]);
        if subcommand == "attach" {
            let chat_id = options
                .iter()
                .find(|option| option.name == "chat_id")
                .and_then(|option| option.value.as_str())
                .unwrap_or("");
            let title = options
                .iter()
                .find(|option| option.name == "title")
                .and_then(|option| option.value.as_str())
                .unwrap_or("Cursor handoff");
            if let Err(error) = cmd.defer_ephemeral(&ctx.http).await {
                tracing::error!(%error, "failed to defer /session attach response");
                return;
            }
            let content = self
                .attach_cursor_chat_request(ctx, cmd.channel_id, chat_id, title, cmd.user.id.get())
                .await
                .unwrap_or_else(|error| format!("⚠️ 無法 attach Cursor chat：{error}"));
            if let Err(error) = cmd
                .edit_response(&ctx.http, EditInteractionResponse::new().content(content))
                .await
            {
                tracing::error!(%error, "failed to edit /session attach response");
            }
            return;
        }
        if subcommand == "status" {
            let snapshot = self
                .router
                .pool()
                .session_snapshot(&scope.session_key)
                .await;
            let response = CreateInteractionResponse::Message(
                session_control_message(
                    &snapshot,
                    &self.router.workspace_aliases(),
                    cmd.channel_id.get(),
                    None,
                )
                .ephemeral(true),
            );
            if let Err(error) = cmd.create_response(&ctx.http, response).await {
                tracing::error!(%error, "failed to respond to /session status");
            }
            return;
        }

        if let Err(error) = cmd.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, subcommand, "failed to defer /session command");
            return;
        }

        let mut task_state_update = None;
        let content = if subcommand == "close" {
            let dropped = self
                .dispatcher
                .cancel_buffered_thread("discord", &cmd.channel_id.get().to_string());
            match self.router.pool().reset_session(&scope.session_key).await {
                Ok(()) if dropped > 0 => {
                    task_state_update = Some(TaskState::Closed);
                    session_closed_note(dropped)
                }
                Ok(()) => {
                    task_state_update = Some(TaskState::Closed);
                    session_closed_note(0)
                }
                Err(_) if dropped > 0 => {
                    format!("🧹 Dropped {dropped} buffered message(s). No session state was open.")
                }
                Err(_) => "⚠️ No session state to close in this channel or thread.".to_string(),
            }
        } else if subcommand == "detach" {
            match self.router.pool().detach_session(&scope.session_key).await {
                Ok(()) => {
                    task_state_update = Some(TaskState::Cursor);
                    concat!(
                        "✅ Session detached and ready for local resume. ",
                        "Do not send another Discord message until the local ACP client exits; ",
                        "the next Discord message will then restore the updated session."
                    )
                    .to_string()
                }
                Err(error) => format!("⚠️ Could not detach session: {error}"),
            }
        } else {
            "⚠️ Unknown session command.".to_string()
        };

        if let Some(state) = task_state_update {
            if let Ok(task) = self.task_registry.set_state(cmd.channel_id.get(), state) {
                if let Some(binding) = self
                    .project_registry
                    .binding_for_channel(task.project_channel_id)
                {
                    let _ = self.upsert_project_home(ctx, &binding).await;
                }
            }
        }

        if let Err(error) = cmd
            .edit_response(&ctx.http, EditInteractionResponse::new().content(content))
            .await
        {
            tracing::error!(%error, "failed to respond to /session command");
        }
    }

    async fn handle_session_control(
        &self,
        ctx: &Context,
        comp: &serenity::model::application::ComponentInteraction,
    ) {
        let scope = match self
            .resolve_session_scope(ctx, comp.user.id.get(), comp.user.bot, comp.channel_id)
            .await
        {
            Ok(scope) => scope,
            Err(message) => {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(message)
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            }
        };
        let action = comp
            .data
            .custom_id
            .strip_prefix("oab_session:")
            .unwrap_or("");

        if action == "close" {
            let confirmation = CreateInteractionResponseMessage::new()
                .content(SESSION_CLOSE_CONFIRMATION)
                .components(vec![CreateActionRow::Buttons(vec![
                    CreateButton::new("oab_session:confirm_close")
                        .label("Close session")
                        .style(ButtonStyle::Danger),
                    CreateButton::new("oab_session:refresh")
                        .label("Keep session")
                        .style(ButtonStyle::Secondary),
                ])])
                .ephemeral(true);
            if let Err(error) = comp
                .create_response(&ctx.http, CreateInteractionResponse::Message(confirmation))
                .await
            {
                tracing::error!(%error, "failed to show session close confirmation");
            }
            return;
        }

        if let Err(error) = comp.defer(&ctx.http).await {
            tracing::error!(%error, action, "failed to defer session control");
            return;
        }

        let mut task_state_update = None;
        let note = match action {
            "refresh" => None,
            "cancel" => Some(
                match self.router.pool().cancel_session(&scope.session_key).await {
                    Ok(()) => "🛑 Stop signal sent. The session remains available.".to_string(),
                    Err(error) => format!("⚠️ Could not stop the current task: {error}"),
                },
            ),
            "detach" => Some(
                match self.router.pool().detach_session(&scope.session_key).await {
                    Ok(()) => {
                        task_state_update = Some(TaskState::Cursor);
                        format!(
                        "✅ Ready for Cursor on the host. Run `make session-resume THREAD_ID={}`. Do not send Discord messages until the terminal UI exits.",
                        comp.channel_id.get()
                    )
                    }
                    Err(error) => format!("⚠️ Could not detach session: {error}"),
                },
            ),
            "confirm_close" => {
                let dropped = self
                    .dispatcher
                    .cancel_buffered_thread("discord", &comp.channel_id.get().to_string());
                Some(
                    match self.router.pool().reset_session(&scope.session_key).await {
                        Ok(()) if dropped > 0 => {
                            task_state_update = Some(TaskState::Closed);
                            session_closed_note(dropped)
                        }
                        Ok(()) => {
                            task_state_update = Some(TaskState::Closed);
                            session_closed_note(0)
                        }
                        Err(_) if dropped > 0 => format!(
                            "🧹 Dropped {dropped} buffered message(s). No session state was open."
                        ),
                        Err(_) => "⚠️ No session state was open.".to_string(),
                    },
                )
            }
            _ => Some(
                "⚠️ This session control is no longer available. Run `/session status` again."
                    .to_string(),
            ),
        };

        let snapshot = self
            .router
            .pool()
            .session_snapshot(&scope.session_key)
            .await;
        let current_task = self.task_registry.task_for_thread(comp.channel_id.get());
        if task_state_update.is_none() {
            task_state_update = current_task
                .as_ref()
                .and_then(|task| reconciled_handoff_task_state(task.state, &snapshot));
        }
        let task = match task_state_update {
            Some(state) => self
                .task_registry
                .set_state(comp.channel_id.get(), state)
                .ok()
                .or(current_task),
            None => current_task,
        };
        if task_state_update.is_some() {
            if let Some(task) = task.as_ref() {
                if task
                    .status_message_id
                    .is_some_and(|message_id| message_id != comp.message.id.get())
                {
                    if let Some(message_id) = task.status_message_id {
                        let _ = ChannelId::new(task.thread_id)
                            .edit_message(
                                &ctx.http,
                                MessageId::new(message_id),
                                task_status_edit(task),
                            )
                            .await;
                    }
                }
            }
            if let Some(binding) = task.as_ref().and_then(|task| {
                self.project_registry
                    .binding_for_channel(task.project_channel_id)
            }) {
                if let Err(error) = self.upsert_project_home(ctx, &binding).await {
                    tracing::warn!(%error, "failed to refresh Project Home after session control");
                }
            }
        }
        let message = match task {
            Some(task) => task_status_interaction_edit(&task, note),
            None => session_control_edit(
                &snapshot,
                &self.router.workspace_aliases(),
                comp.channel_id.get(),
                note,
            ),
        };
        if let Err(error) = comp
            .edit_response(&ctx.http, message)
            .await
        {
            tracing::error!(%error, action, "failed to update session control");
        }
    }

    async fn submit_task_prompt(
        &self,
        ctx: &Context,
        task: &TaskRecord,
        expected_state: TaskState,
        user: &serenity::model::user::User,
        prompt: String,
    ) -> Result<(), String> {
        let current = self
            .task_registry
            .task_for_thread(task.thread_id)
            .ok_or_else(|| "Task metadata is no longer available.".to_string())?;
        if current.state != expected_state {
            return Err("Task state changed. Refresh the card and try again.".to_string());
        }
        self.task_registry
            .record_prompt(task.thread_id, &prompt)
            .map_err(|error| format!("Could not save retry request: {error}"))?;
        let preview = suppress_mentions(&truncate_for_discord(&prompt, 1800));
        let user_id = user.id.get();
        let trigger = ChannelId::new(task.thread_id)
            .send_message(
                &ctx.http,
                CreateMessage::new()
                    .content(format!("👤 **Request from <@{user_id}>**\n{preview}")),
            )
            .await
            .map_err(|error| format!("Could not post request to the task thread: {error}"))?;
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
        let dispatcher = self.dispatcher.clone();
        let adapter = self.discord_adapter(ctx);
        tokio::spawn(async move {
            let _ = adapter
                .update_task_lifecycle(&thread_channel, TaskLifecycleEvent::Enqueued)
                .await;
            let thread_key =
                dispatcher.key("discord", &thread_channel.channel_id, &sender.sender_id);
            let buf_msg = crate::dispatch::BufferedMessage {
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
            if let Err(error) = dispatcher
                .submit(thread_key, thread_channel.clone(), adapter.clone(), buf_msg)
                .await
            {
                let _ = adapter
                    .update_task_lifecycle(
                        &thread_channel,
                        TaskLifecycleEvent::Failed {
                            message: error.to_string(),
                        },
                    )
                    .await;
            }
        });
        Ok(())
    }

    async fn handle_task_control(
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
                    .content("🚫 你沒有操作這個 task 的權限。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let Some(task) = self.task_registry.task_for_thread(comp.channel_id.get()) else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Task metadata is unavailable. Refresh Project Home.")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };
        let action = comp.data.custom_id.strip_prefix("oab_task:").unwrap_or("");
        if action == "continue" || action == "edit" {
            let expected = if action == "edit" {
                TaskState::Failed
            } else {
                TaskState::Ready
            };
            if task.state != expected {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ Task state changed. Refresh the card and try again.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            }
            let modal = task_prompt_modal(
                action,
                (action == "edit")
                    .then_some(task.last_prompt.as_deref())
                    .flatten(),
            );
            if let Err(error) = comp
                .create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
                .await
            {
                tracing::error!(%error, action, "failed to open task prompt modal");
            }
            return;
        }
        if action == "error" {
            let error = task
                .last_error
                .as_deref()
                .unwrap_or("No error details were recorded for this task.");
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .embed(
                        CreateEmbed::new()
                            .title("🔍 Error details")
                            .description(truncate_for_discord(error, 3900))
                            .colour(0xE74C3C)
                            .footer(CreateEmbedFooter::new(
                                "Retry, edit the request, or continue in Cursor",
                            )),
                    )
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        if action != "retry" || task.state != TaskState::Failed {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ This task control is no longer available.")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let Some(prompt) = task.last_prompt.clone() else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ No retryable text request was recorded. Use Edit and retry.")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };
        if let Err(error) = comp.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, "failed to defer task retry");
            return;
        }
        let result = self
            .submit_task_prompt(ctx, &task, TaskState::Failed, &comp.user, prompt)
            .await;
        let content = result.map_or_else(
            |error| format!("⚠️ Could not retry: {error}"),
            |()| format!("✅ Retry queued in <#{}>.", task.thread_id),
        );
        let _ = comp
            .edit_response(&ctx.http, EditInteractionResponse::new().content(content))
            .await;
    }

    async fn handle_task_prompt_modal(
        &self,
        ctx: &Context,
        modal: &serenity::model::application::ModalInteraction,
    ) {
        if modal.user.bot
            || is_denied_user(
                false,
                self.allow_all_users,
                &self.allowed_users,
                modal.user.id.get(),
            )
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 你沒有操作這個 task 的權限。")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let action = modal
            .data
            .custom_id
            .strip_prefix("oab_task_prompt:")
            .unwrap_or("");
        let expected_state = if action == "edit" {
            TaskState::Failed
        } else {
            TaskState::Ready
        };
        let Some(task) = self.task_registry.task_for_thread(modal.channel_id.get()) else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Task metadata is unavailable.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let prompt = modal_input_value(modal, "prompt")
            .map(str::trim)
            .unwrap_or("");
        if prompt.is_empty() {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Request cannot be empty.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        if let Err(error) = modal.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, "failed to defer task prompt modal");
            return;
        }
        let result = self
            .submit_task_prompt(ctx, &task, expected_state, &modal.user, prompt.to_string())
            .await;
        let content = result.map_or_else(
            |error| format!("⚠️ Could not submit request: {error}"),
            |()| format!("✅ Request queued in <#{}>.", task.thread_id),
        );
        let _ = modal
            .edit_response(&ctx.http, EditInteractionResponse::new().content(content))
            .await;
    }

    async fn handle_project_component(
        &self,
        ctx: &Context,
        comp: &serenity::model::application::ComponentInteraction,
    ) {
        let content = if comp.user.bot {
            Some("🤖 Bots cannot use project controls.".to_string())
        } else if is_denied_user(
            false,
            self.allow_all_users,
            &self.allowed_users,
            comp.user.id.get(),
        ) {
            Some("🚫 You are not allowed to use this bot.".to_string())
        } else {
            None
        };
        if let Some(content) = content {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(content)
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }

        let Some(binding) = self
            .project_registry
            .binding_for_channel(comp.channel_id.get())
        else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ This channel is no longer linked to an OpenAB project.")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };
        let action = comp
            .data
            .custom_id
            .strip_prefix("oab_project:")
            .unwrap_or("");
        if action == "sessions" {
            if let Err(error) = comp.defer_ephemeral(&ctx.http).await {
                tracing::error!(%error, "failed to defer project Session Manager");
                return;
            }
            let entries = self.managed_sessions_for_project(binding.channel_id).await;
            if let Err(error) = comp
                .edit_response(
                    &ctx.http,
                    session_manager_edit(&binding, &entries, None, None),
                )
                .await
            {
                tracing::error!(%error, "failed to open project Session Manager");
            }
            return;
        }
        if action == "new" {
            let modal = project_task_modal(None, None);
            if let Err(error) = comp
                .create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
                .await
            {
                tracing::error!(%error, "failed to open new task modal");
            }
            return;
        }
        if action == "actions" {
            let actions = self.project_actions_for(&binding.workspace_alias);
            let response = CreateInteractionResponse::Message(project_actions_message(
                &binding, &actions,
            ));
            if let Err(error) = comp.create_response(&ctx.http, response).await {
                tracing::error!(%error, "failed to open repository quick actions");
            }
            return;
        }
        if action == "attach" {
            let chats = match self.available_cursor_chats(&binding, "").await {
                Ok(chats) => chats,
                Err(error) => {
                    let response = CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(format!("⚠️ {error}"))
                            .ephemeral(true),
                    );
                    let _ = comp.create_response(&ctx.http, response).await;
                    return;
                }
            };
            if chats.is_empty() {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(
                            "📤 這個 repository 沒有可接續的本機 chat。請先正常離開 Cursor UI；可在主機執行 `make session-publish-list` 檢查。",
                        )
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            }
            let options = chats
                .into_iter()
                .map(|chat| {
                    CreateSelectMenuOption::new(
                        cursor_chat_choice_label(&chat),
                        chat.session_id.clone(),
                    )
                    .description(format!("Cursor chat {}", chat.session_id))
                })
                .collect();
            let select =
                CreateSelectMenu::new("oab_attach_chat", CreateSelectMenuKind::String { options })
                    .placeholder("選擇最近的 Cursor chat");
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!(
                        "📤 **Attach local chat to @{}**\n只顯示這個 repository 尚未綁定 Discord 的 chats。",
                        binding.workspace_alias
                    ))
                    .components(vec![CreateActionRow::SelectMenu(select)])
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let message = match action {
            "status" => CreateInteractionResponseMessage::new()
                .embed(project_info_embed(&binding))
                .ephemeral(true),
            _ => CreateInteractionResponseMessage::new()
                .content("⚠️ This project control is no longer available.")
                .ephemeral(true),
        };
        if let Err(error) = comp
            .create_response(&ctx.http, CreateInteractionResponse::Message(message))
            .await
        {
            tracing::error!(%error, action, "failed to respond to project control");
        }
    }

    async fn handle_project_action_select(
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
                    .content("🚫 你沒有使用這個 Bot 的權限。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let Some(binding) = self
            .project_registry
            .binding_for_channel(comp.channel_id.get())
        else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ This channel is no longer linked to an OpenAB project.")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };
        let selected_id = match &comp.data.kind {
            ComponentInteractionDataKind::StringSelect { values } => {
                values.first().map(String::as_str)
            }
            _ => None,
        };
        let selected = selected_id.and_then(|id| {
            self.project_actions.iter().find(|action| {
                action.workspace_alias == binding.workspace_alias && action.id == id
            })
        });
        let Some(action) = selected else {
            let actions = self.project_actions_for(&binding.workspace_alias);
            let response = CreateInteractionResponse::Message(
                project_actions_message(&binding, &actions)
                    .content("⚠️ 這個 action 已移除，清單已重新整理。"),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        };
        let title = if action.title.trim().is_empty() {
            action.label.trim()
        } else {
            action.title.trim()
        };
        if let Err(error) = comp
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Modal(project_task_modal(
                    Some(title),
                    Some(&action.prompt),
                )),
            )
            .await
        {
            tracing::error!(%error, action_id = %action.id, "failed to open repository action task modal");
        }
    }

    async fn handle_project_attach_modal(
        &self,
        ctx: &Context,
        modal: &serenity::model::application::ModalInteraction,
    ) {
        if modal.user.bot
            || is_denied_user(
                false,
                self.allow_all_users,
                &self.allowed_users,
                modal.user.id.get(),
            )
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 You are not allowed to attach Cursor chats.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let chat_id = modal
            .data
            .custom_id
            .strip_prefix("oab_project_attach:")
            .unwrap_or("");
        let title = modal_input_value(modal, "title")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("Cursor handoff");
        if let Err(error) = modal.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, "failed to defer Cursor attach modal response");
            return;
        }
        let content = self
            .attach_cursor_chat_request(ctx, modal.channel_id, chat_id, title, modal.user.id.get())
            .await
            .unwrap_or_else(|error| format!("⚠️ 無法 attach Cursor chat：{error}"));
        if let Err(error) = modal
            .edit_response(&ctx.http, EditInteractionResponse::new().content(content))
            .await
        {
            tracing::error!(%error, "failed to edit Cursor attach modal response");
        }
    }

    async fn handle_project_new_task_modal(
        &self,
        ctx: &Context,
        modal: &serenity::model::application::ModalInteraction,
    ) {
        if modal.user.bot
            || is_denied_user(
                false,
                self.allow_all_users,
                &self.allowed_users,
                modal.user.id.get(),
            )
        {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 你沒有建立 task 的權限。")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let Some(binding) = self
            .project_registry
            .binding_for_channel(modal.channel_id.get())
        else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ 請從 managed Project Home 建立 task。")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let prompt = modal_input_value(modal, "prompt")
            .map(str::trim)
            .unwrap_or("");
        if prompt.is_empty() {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Task request 不可為空。")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        }
        let title = modal_input_value(modal, "title")
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_for_discord(value, 100))
            .unwrap_or_else(|| format::shorten_thread_name(prompt));
        if let Err(error) = modal.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, "failed to defer new task modal");
            return;
        }

        let parent_id = ChannelId::new(binding.channel_id);
        let trigger = match parent_id
            .send_message(
                &ctx.http,
                CreateMessage::new().content(format!(
                    "🧵 **{}**\nStarted by <@{}> via Project Home",
                    suppress_mentions(&title.replace(['*', '`'], "")),
                    modal.user.id
                )),
            )
            .await
        {
            Ok(message) => message,
            Err(error) => {
                let _ = modal
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .content(format!("⚠️ Could not create task: {error}")),
                    )
                    .await;
                return;
            }
        };
        let thread = match parent_id
            .create_thread_from_message(
                &ctx.http,
                trigger.id,
                CreateThread::new(&title).auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await
        {
            Ok(thread) => thread,
            Err(error) => {
                let _ = trigger.delete(&ctx.http).await;
                let _ = modal
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new()
                            .content(format!("⚠️ Could not create task thread: {error}")),
                    )
                    .await;
                return;
            }
        };
        let task = match self
            .ensure_task(ctx, &binding, thread.id.get(), &title, modal.user.id.get())
            .await
        {
            Ok(task) => task,
            Err(error) => {
                let _ = thread.id.delete(&ctx.http).await;
                let _ = trigger.delete(&ctx.http).await;
                let _ = modal
                    .edit_response(
                        &ctx.http,
                        EditInteractionResponse::new().content(format!("⚠️ {error}")),
                    )
                    .await;
                return;
            }
        };
        if let Err(error) = self.task_registry.record_prompt(task.thread_id, prompt) {
            tracing::warn!(%error, thread_id = task.thread_id, "failed to save retryable task prompt");
        }
        let request_preview = suppress_mentions(&truncate_for_discord(prompt, 1800));
        if let Err(error) = thread
            .id
            .send_message(
                &ctx.http,
                CreateMessage::new().content(format!(
                    "👤 **Request from <@{}>**\n{}",
                    modal.user.id, request_preview
                )),
            )
            .await
        {
            tracing::warn!(%error, thread_id = %thread.id, "failed to post task request preview");
        }

        let thread_channel = ChannelRef {
            platform: "discord".into(),
            channel_id: thread.id.to_string(),
            thread_id: None,
            parent_id: Some(binding.channel_id.to_string()),
            origin_event_id: None,
        };
        let trigger_ref = MessageRef {
            channel: ChannelRef {
                platform: "discord".into(),
                channel_id: binding.channel_id.to_string(),
                thread_id: None,
                parent_id: None,
                origin_event_id: None,
            },
            message_id: trigger.id.to_string(),
        };
        let display_name = modal
            .user
            .global_name
            .as_deref()
            .unwrap_or(&modal.user.name);
        let sender = build_sender_context(
            &modal.user.id.to_string(),
            &modal.user.name,
            display_name,
            &thread.id.to_string(),
            Some(&binding.channel_id.to_string()),
            false,
            &chrono::Utc::now().to_rfc3339(),
            &trigger.id.to_string(),
            &ctx.cache.current_user().id.to_string(),
        );
        let dispatcher = self.dispatcher.clone();
        let adapter = self.discord_adapter(ctx);
        let prompt = prompt.to_string();
        tokio::spawn(async move {
            let _ = adapter
                .update_task_lifecycle(&thread_channel, TaskLifecycleEvent::Enqueued)
                .await;
            let thread_key =
                dispatcher.key("discord", &thread_channel.channel_id, &sender.sender_id);
            let buf_msg = crate::dispatch::BufferedMessage {
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
            if let Err(error) = dispatcher
                .submit(thread_key, thread_channel.clone(), adapter.clone(), buf_msg)
                .await
            {
                let _ = adapter
                    .update_task_lifecycle(
                        &thread_channel,
                        TaskLifecycleEvent::Failed {
                            message: error.to_string(),
                        },
                    )
                    .await;
            }
        });
        let _ = modal
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content(format!(
                    "✅ Task **{}** started in <#{}>.",
                    task.title, task.thread_id
                )),
            )
            .await;
    }

    async fn handle_recent_task_select(
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
                    .content("🚫 你沒有使用這個 Bot 的權限。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let selected = match &comp.data.kind {
            ComponentInteractionDataKind::StringSelect { values } => {
                values.first().and_then(|value| value.parse::<u64>().ok())
            }
            _ => None,
        };
        let project_channel_id = comp.channel_id.get();
        let task = selected
            .and_then(|thread_id| self.task_registry.task_for_thread(thread_id))
            .filter(|task| task.project_channel_id == project_channel_id);
        let content = task.map_or_else(
            || "⚠️ 這個 task 已不可用，請執行 `/project home` 更新清單。".to_string(),
            |task| {
                format!(
                    "🧵 **{}**\nOpen <#{}> to continue.",
                    task.title, task.thread_id
                )
            },
        );
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(content)
                .ephemeral(true),
        );
        let _ = comp.create_response(&ctx.http, response).await;
    }

    async fn handle_attach_chat_select(
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
                    .content("🚫 你沒有使用這個 Bot 的權限。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        let chat_id = match &comp.data.kind {
            ComponentInteractionDataKind::StringSelect { values } => {
                values.first().map(String::as_str).unwrap_or("")
            }
            _ => "",
        };
        let valid = match self.project_binding_for_channel(ctx, comp.channel_id).await {
            Ok((binding, _)) => self
                .available_cursor_chats(&binding, chat_id)
                .await
                .unwrap_or_default()
                .iter()
                .any(|chat| chat.session_id == chat_id),
            Err(_) => false,
        };
        if !valid {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ 這個 chat 已不可用，請回到 Project Home 重新選擇。")
                    .ephemeral(true),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }

        let modal = CreateModal::new(
            format!("oab_project_attach:{chat_id}"),
            "Attach local Cursor chat",
        )
        .components(vec![CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Discord thread title", "title")
                .placeholder("Cursor handoff")
                .max_length(100)
                .required(false),
        )]);
        if let Err(error) = comp
            .create_response(&ctx.http, CreateInteractionResponse::Modal(modal))
            .await
        {
            tracing::error!(%error, "failed to open Cursor attach title modal");
        }
    }

    /// Build a Discord select menu from ACP configOptions with the given category.
    /// Paginates options in pages of 25 (Discord limit). The current selection is
    /// always placed first so it appears on page 0.
    fn build_config_select(
        options: &[ConfigOption],
        category: &str,
        page: usize,
    ) -> Option<CreateSelectMenu> {
        let opt = options
            .iter()
            .find(|o| o.category.as_deref() == Some(category))?;

        // Put current selection first so it always lands on page 0,
        // then fill remaining slots in original order.
        let sorted: Vec<_> = opt
            .options
            .iter()
            .filter(|o| o.value == opt.current_value)
            .chain(opt.options.iter().filter(|o| o.value != opt.current_value))
            .collect();

        let menu_options: Vec<CreateSelectMenuOption> = sorted
            .iter()
            .skip(page * SELECT_MENU_PAGE_SIZE)
            .take(SELECT_MENU_PAGE_SIZE)
            .map(|o| {
                let mut item = CreateSelectMenuOption::new(
                    truncate_for_discord(&o.name, SELECT_OPTION_TEXT_MAX),
                    &o.value,
                );
                if let Some(desc) = &o.description {
                    item = item.description(truncate_for_discord(desc, SELECT_OPTION_TEXT_MAX));
                }
                if o.value == opt.current_value {
                    item = item.default_selection(true);
                }
                item
            })
            .collect();

        if menu_options.is_empty() {
            return None;
        }

        let current_name = opt
            .options
            .iter()
            .find(|o| o.value == opt.current_value)
            .map(|o| o.name.as_str())
            .unwrap_or(&opt.current_value);
        let total_pages = sorted.len().div_ceil(SELECT_MENU_PAGE_SIZE);
        let placeholder = if total_pages > 1 {
            format!(
                "Current: {} (page {}/{})",
                current_name,
                page + 1,
                total_pages
            )
        } else {
            format!("Current: {}", current_name)
        };

        Some(
            CreateSelectMenu::new(
                format!("acp_config_{}", opt.id),
                CreateSelectMenuKind::String {
                    options: menu_options,
                },
            )
            .placeholder(placeholder),
        )
    }

    /// Build ◀/▶ pagination buttons. Returns None when only one page exists.
    fn build_pagination_buttons(
        category: &str,
        page: usize,
        total_pages: usize,
    ) -> Option<CreateActionRow> {
        if total_pages <= 1 {
            return None;
        }
        let prev = CreateButton::new(format!("acp_pg:{}:{}", category, page.saturating_sub(1)))
            .label("◀")
            .style(ButtonStyle::Secondary)
            .disabled(page == 0);
        let next = CreateButton::new(format!("acp_pg:{}:{}", category, page + 1))
            .label("▶")
            .style(ButtonStyle::Secondary)
            .disabled(page + 1 >= total_pages);
        let indicator = CreateButton::new("acp_pg_noop")
            .label(format!("{}/{}", page + 1, total_pages))
            .style(ButtonStyle::Secondary)
            .disabled(true);
        Some(CreateActionRow::Buttons(vec![prev, indicator, next]))
    }

    /// Build the full component rows (select menu + optional pagination) for a config category.
    /// When `page` is `None`, auto-selects the page containing the current value.
    fn build_config_components(
        options: &[ConfigOption],
        category: &str,
        page: Option<usize>,
    ) -> Option<Vec<CreateActionRow>> {
        let opt = options
            .iter()
            .find(|o| o.category.as_deref() == Some(category))?;
        let total_pages = opt.options.len().div_ceil(SELECT_MENU_PAGE_SIZE);
        let page = match page {
            Some(p) => p.min(total_pages.saturating_sub(1)),
            None => opt
                .options
                .iter()
                .position(|o| o.value == opt.current_value)
                .map(|i| i / SELECT_MENU_PAGE_SIZE)
                .unwrap_or(0),
        };

        let select = Self::build_config_select(options, category, page)?;
        let mut rows = vec![CreateActionRow::SelectMenu(select)];
        if let Some(buttons) = Self::build_pagination_buttons(category, page, total_pages) {
            rows.push(buttons);
        }
        Some(rows)
    }

    async fn handle_config_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
        category: &str,
        label: &str,
    ) {
        let thread_key = format!("discord:{}", cmd.channel_id.get());
        let config_options = self.router.pool().get_config_options(&thread_key).await;

        let response = match Self::build_config_components(&config_options, category, None) {
            Some(rows) => CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!("🔧 Select a {label}:"))
                    .components(rows)
                    .ephemeral(true),
            ),
            None => CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!("⚠️ No {label} options available. Start a conversation first by @mentioning the bot."))
                    .ephemeral(true),
            ),
        };

        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, category, "failed to respond to slash command");
        }
    }

    async fn handle_usage_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        let thread_key = format!("discord:{}", cmd.channel_id.get());

        if !self.router.pool().has_active_session(&thread_key).await {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(
                        "⚠️ No active session. Start a conversation first by @mentioning the bot.",
                    )
                    .ephemeral(true),
            );
            if let Err(e) = cmd.create_response(&ctx.http, response).await {
                tracing::error!(error = %e, "failed to respond to /usage command");
            }
            return;
        }

        // The ACP round-trip can exceed Discord's 3-second interaction
        // deadline — acknowledge with a deferred ephemeral response first.
        let defer = CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, defer).await {
            tracing::error!(error = %e, "failed to defer /usage response");
            return;
        }

        let followup = match self.router.pool().get_usage(&thread_key).await {
            Ok(report) => {
                let (content, embed) = build_usage_reply(&report);
                CreateInteractionResponseFollowup::new()
                    .content(content)
                    .embed(embed)
                    .ephemeral(true)
            }
            Err(e) => CreateInteractionResponseFollowup::new()
                .content(format!("⚠️ {e}"))
                .ephemeral(true),
        };
        if let Err(e) = cmd.create_followup(&ctx.http, followup).await {
            tracing::error!(error = %e, "failed to send /usage followup");
        }
    }

    async fn handle_cancel_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        let thread_key = format!("discord:{}", cmd.channel_id.get());
        let result = self.router.pool().cancel_session(&thread_key).await;

        let msg = match result {
            Ok(()) => "🛑 Cancel signal sent.".to_string(),
            Err(e) => format!("⚠️ {e}"),
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(msg)
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to /cancel command");
        }
    }

    async fn handle_cancel_all_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        // /cancel-all is the nuclear escape hatch: stop the in-flight turn AND clear
        // every lane's buffer in this thread, so a human can intervene from a clean slate.
        let session_key = format!("discord:{}", cmd.channel_id.get());
        let dropped = self
            .dispatcher
            .cancel_buffered_thread("discord", &cmd.channel_id.get().to_string());

        let cancel_result = self.router.pool().cancel_session(&session_key).await;

        // Buffer count is approximate (sweep races with new arrivals) so we surface
        // a binary "cleared / nothing" signal rather than a misleading exact number.
        let msg = match (cancel_result, dropped) {
            (Ok(()), 0) => "🛑 Cancel signal sent.".to_string(),
            (Ok(()), _) => "🛑 Cancel signal sent. Buffered messages cleared.".to_string(),
            (Err(_), 0) => {
                "⚠️ Nothing to cancel — no active session and no buffered messages.".to_string()
            }
            (Err(_), _) => "🛑 Buffered messages cleared. No active session to cancel.".to_string(),
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(msg)
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to /cancel-all command");
        }
    }

    async fn handle_reset_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        // /reset clears every lane's buffer in this thread and tears down the shared
        // ACP session — the next message in the thread starts a fresh conversation.
        let session_key = format!("discord:{}", cmd.channel_id.get());
        let dropped = self
            .dispatcher
            .cancel_buffered_thread("discord", &cmd.channel_id.get().to_string());

        let result = self.router.pool().reset_session(&session_key).await;

        let msg = match result {
            Ok(()) if dropped > 0 => {
                format!("🔄 Session reset. Dropped {dropped} buffered message(s). Start a new conversation!")
            }
            Ok(()) => "🔄 Session reset. Start a new conversation!".to_string(),
            Err(_) if dropped > 0 => {
                format!("🔄 Dropped {dropped} buffered message(s). No active session to reset.")
            }
            Err(_) => {
                "⚠️ No active session to reset. Start a conversation first by @mentioning the bot."
                    .to_string()
            }
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(msg)
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to /reset command");
        }
    }

    async fn handle_remind_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        // Only humans can use /remind
        if cmd.user.bot {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Only humans can set reminders.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // Extract options
        let opts = &cmd.data.options;
        let targets_raw = opts
            .iter()
            .find(|o| o.name == "targets")
            .and_then(|o| o.value.as_str())
            .unwrap_or("");
        let message = opts
            .iter()
            .find(|o| o.name == "message")
            .and_then(|o| o.value.as_str())
            .unwrap_or("");
        let delay_raw = opts
            .iter()
            .find(|o| o.name == "delay")
            .and_then(|o| o.value.as_str())
            .unwrap_or("");

        if targets_raw.is_empty() || message.is_empty() || delay_raw.is_empty() {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ All fields (targets, message, delay) are required.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // Parse delay
        let delay_secs = match remind::parse_delay(delay_raw) {
            Ok(s) => s,
            Err(e) => {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(format!("⚠️ Invalid delay: {e}"))
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
        };

        if let Err(e) = remind::validate_message(message) {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!("⚠️ {e}"))
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // Strip @everyone / @here to prevent unintended mass pings.
        let message = remind::sanitize_message(message);

        // Extract mention strings from targets (keep raw — Discord renders them)
        let targets: Vec<String> = targets_raw
            .split_whitespace()
            .filter(|t| t.starts_with("<@") && t.ends_with('>'))
            .map(|t| t.to_string())
            .collect();

        if targets.is_empty() {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ No valid mentions found in targets. Use @user or @role.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        if targets.len() > remind::MAX_TARGETS {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(format!(
                        "⚠️ Too many targets (max {}). Use a @role instead.",
                        remind::MAX_TARGETS
                    ))
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // F4: Per-user rate limit (max 5 active reminders)
        let user_id = cmd.user.id.get();
        let pending = self.reminder_store.pending().await;
        let user_count = pending.iter().filter(|r| r.sender_id == user_id).count();
        if user_count >= 5 {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ You already have 5 active reminders. Wait for some to fire before adding more.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        let fire_at = chrono::Utc::now() + chrono::Duration::seconds(delay_secs as i64);
        let reminder = remind::Reminder {
            id: uuid::Uuid::new_v4().to_string(),
            channel_id: cmd.channel_id.get(),
            sender_id: cmd.user.id.get(),
            targets: targets.clone(),
            message: message.clone(),
            fire_at,
            created_at: chrono::Utc::now(),
        };

        // Persist and schedule
        self.reminder_store.add(reminder.clone()).await;
        self.scheduled_ids.lock().await.insert(reminder.id.clone());
        remind::schedule_reminder(ctx.http.clone(), self.reminder_store.clone(), reminder);

        let delay_str = remind::format_delay(delay_secs);
        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content(format!(
                    "⏰ Reminder set! Will fire in **{delay_str}** and mention {}",
                    targets.join(" ")
                ))
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to /remind command");
        }
    }

    async fn handle_auth_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        // Reject bot users — consistent with other slash-command handlers (e.g. /remind).
        if cmd.user.bot {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🤖 Bots cannot use `/auth`.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // Access control — only allowed users can trigger auth.
        if is_denied_user(
            false,
            self.allow_all_users,
            &self.allowed_users,
            cmd.user.id.get(),
        ) {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 You are not allowed to use this bot.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // DM-only — auth codes are sensitive; reject if not in a DM channel.
        if cmd.guild_id.is_some() {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🔒 `/auth` is only available in DMs for security. Please DM me and run `/auth` there.")
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        // Single-flight guard — prevent concurrent /auth invocations.
        static AUTH_IN_PROGRESS: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if AUTH_IN_PROGRESS.swap(true, std::sync::atomic::Ordering::Acquire) {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(
                        "⚠️ Authentication already in progress. Please wait for it to complete.",
                    )
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        let auth_cmd = ["OPENAB_AGENT_LOGIN_COMMAND", "OPENAB_AGENT_AUTH_COMMAND"]
            .into_iter()
            .find_map(|key| {
                std::env::var(key)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            });
        let auth_cmd = match auth_cmd {
            Some(command) => command,
            None => {
                AUTH_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(
                            "⚠️ No login command configured (`OPENAB_AGENT_LOGIN_COMMAND` or legacy `OPENAB_AGENT_AUTH_COMMAND` not set).",
                        )
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
        };

        // Acknowledge with a deferred ephemeral response so we have time to run the command.
        let defer = CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, defer).await {
            AUTH_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
            tracing::error!(error = %e, "failed to defer /auth response");
            return;
        }

        let http = ctx.http.clone();
        let token = cmd.token.clone();
        let user_id = cmd.user.id.get();

        tokio::spawn(async move {
            use std::sync::Arc;
            use tokio::io::AsyncBufReadExt;
            use tokio::process::Command as TokioCommand;

            // Drop guard ensures AUTH_IN_PROGRESS is cleared even on panic.
            struct AuthGuard;
            impl Drop for AuthGuard {
                fn drop(&mut self) {
                    AUTH_IN_PROGRESS.store(false, std::sync::atomic::Ordering::Release);
                }
            }
            let _guard = AuthGuard;

            info!(user_id, "/auth: starting auth command");

            let child = TokioCommand::new("sh")
                .arg("-c")
                .arg(&auth_cmd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "/auth: failed to spawn auth command");
                    let _ = http
                        .create_followup_message(
                            &token,
                            &CreateInteractionResponseFollowup::new()
                                .content(format!("❌ Failed to start auth command: {e}"))
                                .ephemeral(true),
                            Vec::new(),
                        )
                        .await;
                    return;
                }
            };

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let url_found = Arc::new(tokio::sync::Notify::new());

            // Spawn background drain tasks — they run to EOF, keeping pipes open.
            let lines_out = lines.clone();
            let url_found_out = url_found.clone();
            let stdout_task = tokio::spawn(async move {
                if let Some(stdout) = stdout {
                    let mut reader = tokio::io::BufReader::new(stdout).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        let has_url = line.contains("http://") || line.contains("https://");
                        lines_out
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(line);
                        if has_url {
                            url_found_out.notify_one();
                        }
                    }
                }
            });

            let lines_err = lines.clone();
            let url_found_err = url_found.clone();
            let stderr_task = tokio::spawn(async move {
                if let Some(stderr) = stderr {
                    let mut reader = tokio::io::BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = reader.next_line().await {
                        let has_url = line.contains("http://") || line.contains("https://");
                        lines_err
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(line);
                        if has_url {
                            url_found_err.notify_one();
                        }
                    }
                }
            });

            // Wait for a URL to appear, the command to exit early, or a 30s timeout.
            let mut early_exit: Option<std::io::Result<std::process::ExitStatus>> = None;
            tokio::select! {
                _ = url_found.notified() => {
                    info!("/auth: URL detected in output");
                    // Brief sleep to let trailing lines (code/instructions) be captured.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                res = child.wait() => {
                    // The auth command exited before printing a URL — fail fast
                    // instead of waiting out the full collection window.
                    warn!("/auth: auth command exited before a URL was detected");
                    early_exit = Some(res);
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                    warn!("/auth: 30s URL-collection window expired without detecting URL");
                }
            }

            // Handle an early exit (the command terminated during the URL window).
            if let Some(res) = early_exit {
                let _ = tokio::join!(stdout_task, stderr_task);
                let collected =
                    strip_ansi_codes(&lines.lock().unwrap_or_else(|e| e.into_inner()).join("\n"));
                let detail = if collected.trim().is_empty() {
                    String::new()
                } else {
                    let snippet: String = collected.chars().take(500).collect();
                    format!("\n```\n{snippet}\n```")
                };
                let content = match res {
                    Ok(status) if status.success() => {
                        format!(
                            "⚠️ Auth command exited (status 0) before a login URL was detected. Run `/auth` again to retry.{detail}"
                        )
                    }
                    Ok(status) => {
                        format!(
                            "❌ Auth command exited early ({status}) before producing a login URL.{detail}"
                        )
                    }
                    Err(e) => format!("❌ Error waiting for auth command: {e}"),
                };
                let _ = http
                    .create_followup_message(
                        &token,
                        &CreateInteractionResponseFollowup::new()
                            .content(content)
                            .ephemeral(true),
                        Vec::new(),
                    )
                    .await;
                return;
            }

            let collected_lines = lines.lock().unwrap_or_else(|e| e.into_inner()).clone();

            if collected_lines.is_empty() {
                warn!("/auth: no output captured, killing child process");
                let _ = child.kill().await;
                let _ = tokio::join!(stdout_task, stderr_task);
                let _ = http.create_followup_message(
                    &token,
                    &CreateInteractionResponseFollowup::new()
                        .content("⚠️ Login command produced no output within 30 seconds. Verify `OPENAB_AGENT_LOGIN_COMMAND` (or legacy `OPENAB_AGENT_AUTH_COMMAND`) is set and prints a login URL to stdout/stderr.")
                        .ephemeral(true),
                    Vec::new(),
                ).await;
                return;
            }

            // Send the captured output as plain text (no code block) so URLs are
            // clickable in Discord.
            let output = strip_ansi_codes(&collected_lines.join("\n"));
            let output = ensure_url_separation(&output);
            let prefix = "🔐 **Agent Authentication**\n\n";
            let suffix = "\n\nFollow the instructions above. Waiting for authorization...";
            // Discord enforces the 2000-char limit in UTF-16 code units; budget and
            // truncate by UTF-16 units rather than Unicode scalar values. See
            // `truncate_to_utf16_budget` for the testable implementation.
            let truncated = truncate_to_utf16_budget(&output, prefix, suffix, 2000);
            let msg = format!("{prefix}{truncated}{suffix}");
            let _ = http
                .create_followup_message(
                    &token,
                    &CreateInteractionResponseFollowup::new()
                        .content(msg)
                        .ephemeral(true),
                    Vec::new(),
                )
                .await;

            // Wait for the process to complete (user authorizes in browser).
            // Use 14min (not 15) to leave headroom for the Discord interaction token TTL.
            let timeout = std::time::Duration::from_secs(14 * 60);
            match tokio::time::timeout(timeout, child.wait()).await {
                Ok(Ok(status)) if status.success() => {
                    info!("/auth: authentication successful");
                    let _ = http
                        .create_followup_message(
                            &token,
                            &CreateInteractionResponseFollowup::new()
                                .content("✅ Authentication successful!")
                                .ephemeral(true),
                            Vec::new(),
                        )
                        .await;
                }
                Ok(Ok(status)) => {
                    warn!(%status, "/auth: authentication failed");
                    let _ = http
                        .create_followup_message(
                            &token,
                            &CreateInteractionResponseFollowup::new()
                                .content(format!(
                                    "❌ Authentication failed (exit code: {}).",
                                    status
                                ))
                                .ephemeral(true),
                            Vec::new(),
                        )
                        .await;
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "/auth: error waiting for auth process");
                    let _ = http
                        .create_followup_message(
                            &token,
                            &CreateInteractionResponseFollowup::new()
                                .content(format!("❌ Auth process error: {e}"))
                                .ephemeral(true),
                            Vec::new(),
                        )
                        .await;
                }
                Err(_) => {
                    warn!("/auth: timed out waiting for authorization");
                    let _ = child.kill().await;
                    let _ = http
                        .create_followup_message(
                            &token,
                            &CreateInteractionResponseFollowup::new()
                                .content("⏰ Authentication timed out. Run `/auth` again to retry.")
                                .ephemeral(true),
                            Vec::new(),
                        )
                        .await;
                }
            }

            // Let background drain tasks complete.
            let _ = tokio::join!(stdout_task, stderr_task);
        });
    }

    async fn handle_export_thread_command(
        &self,
        ctx: &Context,
        cmd: &serenity::model::application::CommandInteraction,
    ) {
        if is_denied_user(
            false,
            self.allow_all_users,
            &self.allowed_users,
            cmd.user.id.get(),
        ) {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("🚫 You are not allowed to use this bot.")
                    .ephemeral(true),
            );
            if let Err(e) = cmd.create_response(&ctx.http, response).await {
                tracing::error!(error = %e, "failed to deny /export-thread command");
            }
            return;
        }

        let channel_id = cmd.channel_id;
        let effective_allowed_channels = self.effective_allowed_channels();
        let (export_allowed, export_name) = match channel_id.to_channel(&ctx.http).await {
            Ok(serenity::model::channel::Channel::Guild(gc)) => {
                let in_allowed_channel = self.allow_all_channels
                    || effective_allowed_channels.contains(&channel_id.get());
                let (in_thread, _) = detect_thread(
                    gc.thread_metadata.is_some(),
                    gc.parent_id.map(|id| id.get()),
                    gc.owner_id.map(|id| id.get()),
                    ctx.cache.current_user().id.get(),
                    &effective_allowed_channels,
                    self.allow_all_channels,
                    in_allowed_channel,
                );
                (in_thread, gc.name.clone())
            }
            Ok(serenity::model::channel::Channel::Private(_)) => (self.allow_dm, "dm".to_string()),
            Ok(_) => (false, "channel".to_string()),
            Err(e) => {
                tracing::warn!(channel_id = %channel_id, error = %e, "failed to inspect channel for export");
                (false, "channel".to_string())
            }
        };

        if !export_allowed {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ Run this command inside an allowed Discord thread or DM.")
                    .ephemeral(true),
            );
            if let Err(e) = cmd.create_response(&ctx.http, response).await {
                tracing::error!(error = %e, "failed to respond to /export-thread rejection");
            }
            return;
        }

        // --- Parse and validate filter params (mutual exclusion) ---
        let opts = &cmd.data.options;
        let limit_opt = opts
            .iter()
            .find(|o| o.name == "limit")
            .and_then(|o| o.value.as_i64());
        let since_opt = opts
            .iter()
            .find(|o| o.name == "since")
            .and_then(|o| o.value.as_str());
        let days_opt = opts
            .iter()
            .find(|o| o.name == "days")
            .and_then(|o| o.value.as_i64());
        let all_opt = opts
            .iter()
            .find(|o| o.name == "all")
            .and_then(|o| o.value.as_bool())
            .unwrap_or(false);

        let filter_count = limit_opt.is_some() as u8
            + since_opt.is_some() as u8
            + days_opt.is_some() as u8
            + all_opt as u8;
        if filter_count > 1 {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(
                        "⚠️ Please specify only one filter: `limit`, `since`, `days`, or `all`.",
                    )
                    .ephemeral(true),
            );
            let _ = cmd.create_response(&ctx.http, response).await;
            return;
        }

        let filter = if all_opt {
            ExportFilter::All
        } else if let Some(n) = limit_opt {
            if !(1..=5000).contains(&n) {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ `limit` must be between 1 and 5000.")
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
            ExportFilter::Limit(n as usize)
        } else if let Some(id_str) = since_opt {
            match id_str.parse::<u64>() {
                Ok(id) if id > 0 => ExportFilter::After(MessageId::new(id)),
                _ => {
                    let response = CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("⚠️ `since` must be a valid message ID (right-click a message → Copy Message ID).")
                            .ephemeral(true),
                    );
                    let _ = cmd.create_response(&ctx.http, response).await;
                    return;
                }
            }
        } else if let Some(d) = days_opt {
            if !(1..=365).contains(&d) {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ `days` must be between 1 and 365.")
                        .ephemeral(true),
                );
                let _ = cmd.create_response(&ctx.http, response).await;
                return;
            }
            let since_ts = chrono::Utc::now() - chrono::Duration::days(d);
            let ts_ms = since_ts.timestamp_millis() as u64;
            ExportFilter::After(timestamp_ms_to_snowflake(ts_ms))
        } else {
            // Default: export last 100 messages (use limit:N or all:true for more)
            ExportFilter::Limit(100)
        };

        let response = CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::new()
                .content("Preparing thread export...")
                .ephemeral(true),
        );
        if let Err(e) = cmd.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to acknowledge /export-thread command");
            return;
        }

        match export_channel_messages(
            &ctx.http,
            channel_id,
            &export_name,
            cmd.attachment_size_limit,
            filter,
        )
        .await
        {
            Ok(result) => {
                let mut content = format!("Exported {} messages.", result.written);
                if result.hit_cap {
                    content.push_str(&format!(
                        " Only the most recent {} messages were fetched — older messages were not included.",
                        result.fetched
                    ));
                }
                if result.byte_truncated {
                    content.push_str(&format!(
                        " Transcript truncated to fit Discord's attachment size limit ({} of {} fetched messages included).",
                        result.written, result.fetched
                    ));
                }
                let attachment =
                    CreateAttachment::bytes(result.transcript.into_bytes(), result.filename);
                let followup = CreateInteractionResponseFollowup::new()
                    .content(content)
                    .add_file(attachment)
                    .ephemeral(true);
                if let Err(e) = cmd.create_followup(&ctx.http, followup).await {
                    tracing::error!(error = %e, "failed to send /export-thread attachment");
                }
            }
            Err(e) => {
                tracing::warn!(channel_id = %channel_id, error = %e, "failed to export thread");
                let followup = CreateInteractionResponseFollowup::new()
                    .content(format!("⚠️ Failed to export thread: {e}"))
                    .ephemeral(true);
                if let Err(e) = cmd.create_followup(&ctx.http, followup).await {
                    tracing::error!(error = %e, "failed to send /export-thread error");
                }
            }
        }
    }

    async fn handle_config_select(
        &self,
        ctx: &Context,
        comp: &serenity::model::application::ComponentInteraction,
    ) {
        let config_id = comp
            .data
            .custom_id
            .strip_prefix("acp_config_")
            .unwrap_or("")
            .to_string();

        if config_id.is_empty() {
            return;
        }

        let selected_value = match &comp.data.kind {
            ComponentInteractionDataKind::StringSelect { values } => match values.first() {
                Some(v) => v.clone(),
                None => return,
            },
            _ => return,
        };

        let thread_key = format!("discord:{}", comp.channel_id.get());

        let result = self
            .router
            .pool()
            .set_config_option(&thread_key, &config_id, &selected_value)
            .await;

        let response_msg = match result {
            Ok(updated_options) => {
                let display_name = updated_options
                    .iter()
                    .find(|o| o.id == config_id)
                    .and_then(|o| o.options.iter().find(|v| v.value == selected_value))
                    .map(|v| v.name.as_str())
                    .unwrap_or(&selected_value);
                format!("✅ Switched to **{}**", display_name)
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to set config option");
                format!("❌ Failed to switch: {}", e)
            }
        };

        let response = CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(response_msg)
                .components(vec![]),
        );

        if let Err(e) = comp.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, "failed to respond to config select");
        }
    }

    async fn handle_pagination(
        &self,
        ctx: &Context,
        comp: &serenity::model::application::ComponentInteraction,
    ) {
        // Parse custom_id format: acp_pg:{category}:{page}
        let parts: Vec<&str> = comp.data.custom_id.splitn(3, ':').collect();
        let (category, page) = match parts.as_slice() {
            [_, cat, pg] => match pg.parse::<usize>() {
                Ok(p) => (*cat, p),
                Err(_) => return,
            },
            _ => return,
        };

        // Only allow known config categories.
        if !matches!(category, "model" | "agent") {
            return;
        }

        let thread_key = format!("discord:{}", comp.channel_id.get());
        let config_options = self.router.pool().get_config_options(&thread_key).await;

        let response = match Self::build_config_components(&config_options, category, Some(page)) {
            Some(rows) => CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(format!("🔧 Select a {category}:"))
                    .components(rows),
            ),
            None => CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(format!("⚠️ No {category} options available."))
                    .components(vec![]),
            ),
        };

        if let Err(e) = comp.create_response(&ctx.http, response).await {
            tracing::error!(error = %e, category, "failed to respond to pagination");
        }
    }
}

// --- Discord-specific helpers ---

/// Render the body lines of a usage report (everything except the plan title
/// and billing-cycle footer). Returns the text and whether any breakdown is
/// over its plan limit.
fn format_usage_body(report: &UsageReport) -> (String, bool) {
    let mut lines: Vec<String> = Vec::new();
    let mut over_limit = false;

    for b in &report.breakdowns {
        match b.limit {
            Some(limit) => {
                let pct = b.percentage.unwrap_or_else(|| {
                    if limit > 0.0 {
                        (b.used / limit * 100.0).round() as u64
                    } else {
                        0
                    }
                });
                if pct > 100 {
                    over_limit = true;
                }
                // 10-slot progress bar, clamped at 100%.
                let filled = (pct.min(100) as usize) / 10;
                let bar: String = "█".repeat(filled) + &"░".repeat(10 - filled);
                lines.push(format!(
                    "{}: {:.2} / {:.0} `{}` {}%{}",
                    b.display_name,
                    b.used,
                    limit,
                    bar,
                    pct,
                    if pct > 100 { " ⚠️" } else { "" }
                ));
            }
            // No per-user cap (e.g. pooled enterprise credits).
            None => lines.push(format!("{}: {:.2} used", b.display_name, b.used)),
        }
        if let Some(charges) = b.overage_charges {
            if charges > 0.0 {
                lines.push(format!(
                    "Overage charges: {:.2} {}",
                    charges,
                    b.currency.as_deref().unwrap_or("USD")
                ));
            }
        }
    }

    (lines.join("\n"), over_limit)
}

/// Build the /usage reply as full-size message content plus a minimal
/// color-strip embed. Discord renders embed descriptions at a smaller font
/// than message content, so the report body lives in `content` (normal font)
/// while the embed carries only the at-a-glance color signal (green within
/// the plan limit, red when any breakdown is over) and the billing-cycle
/// footer.
fn build_usage_reply(report: &UsageReport) -> (String, CreateEmbed) {
    let (body, over_limit) = format_usage_body(report);
    let content = format!("📊 **Usage — {}**\n{}", report.plan_name, body);
    let mut embed = CreateEmbed::new().colour(if over_limit { 0xE74C3C } else { 0x2ECC71 });
    if let Some(reset) = &report.billing_cycle_reset {
        embed = embed.footer(CreateEmbedFooter::new(format!(
            "Billing cycle resets {reset}"
        )));
    }
    (content, embed)
}

fn discord_msg_ref(msg: &Message) -> MessageRef {
    MessageRef {
        channel: ChannelRef {
            platform: "discord".into(),
            channel_id: msg.channel_id.get().to_string(),
            thread_id: None,
            parent_id: None,
            origin_event_id: None,
        },
        message_id: msg.id.to_string(),
    }
}

struct ExportResult {
    filename: String,
    transcript: String,
    /// Messages successfully pulled from Discord.
    fetched: usize,
    /// Messages that fit in the transcript (≤ `fetched`; differs when the
    /// attachment-size limit truncates).
    written: usize,
    /// We stopped fetching because we hit the message cap and the thread still
    /// has more messages we did not include.
    hit_cap: bool,
    /// Transcript was cut to keep the attachment under Discord's size limit.
    byte_truncated: bool,
}

/// Filter mode for export_channel_messages.
enum ExportFilter {
    /// Fetch all messages (newest-first via `before`), capped at THREAD_EXPORT_MESSAGE_LIMIT.
    All,
    /// Fetch the most recent N messages (newest-first via `before`).
    Limit(usize),
    /// Fetch messages after a synthetic snowflake (newest-first via `before`, with boundary filtering).
    After(MessageId),
}

/// Discord epoch: 2015-01-01T00:00:00Z in milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Convert a UTC timestamp (in milliseconds since Unix epoch) to a synthetic
/// Discord snowflake suitable for use as an `after` cursor.
fn timestamp_ms_to_snowflake(timestamp_ms: u64) -> MessageId {
    let discord_ms = timestamp_ms.saturating_sub(DISCORD_EPOCH_MS);
    // Snowflake IDs use NonZeroU64 in serenity; ensure at least 1.
    MessageId::new((discord_ms << 22).max(1))
}

async fn export_channel_messages(
    http: &Http,
    channel_id: ChannelId,
    channel_name: &str,
    attachment_size_limit: u32,
    filter: ExportFilter,
) -> anyhow::Result<ExportResult> {
    let cap = match &filter {
        ExportFilter::Limit(n) => *n,
        _ => THREAD_EXPORT_MESSAGE_LIMIT,
    };

    let mut messages = Vec::new();
    let mut hit_cap = false;

    match &filter {
        ExportFilter::All | ExportFilter::Limit(_) => {
            // Fetch newest-first using `before` pagination, then reverse.
            let mut before = None;
            loop {
                if messages.len() >= cap {
                    hit_cap = true;
                    break;
                }
                let remaining = cap - messages.len();
                let limit = remaining.min(100) as u8;
                let mut request = GetMessages::new().limit(limit);
                if let Some(before_id) = before {
                    request = request.before(before_id);
                }
                let batch = channel_id.messages(http, request).await?;
                if batch.is_empty() {
                    break;
                }
                before = batch.last().map(|m| m.id);
                let batch_len = batch.len();
                messages.extend(batch);
                if batch_len < limit as usize {
                    break;
                }
            }
            // Probe to confirm we actually left messages behind.
            if hit_cap {
                let probe = GetMessages::new().limit(1);
                let probe = if let Some(before_id) = before {
                    probe.before(before_id)
                } else {
                    probe
                };
                if matches!(channel_id.messages(http, probe).await, Ok(b) if b.is_empty()) {
                    hit_cap = false;
                }
            }
            messages.reverse();
        }
        ExportFilter::After(after_id) => {
            // Fetch newest-first using `before` pagination, stop when we hit
            // messages at or before the filter boundary. This ensures that when
            // the cap is reached, we keep the *newest* messages in the window.
            let mut before = None;
            loop {
                if messages.len() >= cap {
                    hit_cap = true;
                    break;
                }
                let remaining = cap - messages.len();
                let limit = remaining.min(100) as u8;
                let mut request = GetMessages::new().limit(limit);
                if let Some(before_id) = before {
                    request = request.before(before_id);
                }
                let batch = channel_id.messages(http, request).await?;
                if batch.is_empty() {
                    break;
                }
                before = batch.last().map(|m| m.id);
                let batch_len = batch.len();
                // Filter out messages at or before the boundary.
                let filtered: Vec<_> = batch.into_iter().filter(|m| m.id > *after_id).collect();
                let hit_boundary = filtered.len() < batch_len;
                messages.extend(filtered);
                if hit_boundary {
                    // We've reached the time boundary; no need to fetch older.
                    break;
                }
                if batch_len < limit as usize {
                    break;
                }
            }
            // Probe only if we stopped due to cap (not boundary).
            if hit_cap {
                let probe = GetMessages::new().limit(1);
                let probe = if let Some(before_id) = before {
                    probe.before(before_id)
                } else {
                    probe
                };
                if let Ok(batch) = channel_id.messages(http, probe).await {
                    // If the next message is beyond our filter boundary,
                    // we didn't actually leave relevant messages behind.
                    let has_more_in_window = batch.iter().any(|m| m.id > *after_id);
                    if !has_more_in_window {
                        hit_cap = false;
                    }
                }
            }
            messages.reverse();
        }
    }

    let filename = export_filename(channel_id, channel_name);
    if attachment_size_limit < 2048 {
        tracing::warn!(
            attachment_size_limit,
            "attachment_size_limit is very small; export will likely be truncated"
        );
    }
    let max_bytes = usize::try_from(attachment_size_limit)
        .unwrap_or(8 * 1024 * 1024)
        .saturating_sub(1024)
        .max(1024);
    let (transcript, written, byte_truncated) =
        format_thread_export(channel_id, channel_name, &messages, max_bytes);
    let fetched = messages.len();

    Ok(ExportResult {
        filename,
        transcript,
        fetched,
        written,
        hit_cap,
        byte_truncated,
    })
}

fn format_thread_export(
    channel_id: ChannelId,
    channel_name: &str,
    messages: &[Message],
    max_bytes: usize,
) -> (String, usize, bool) {
    let header = format!(
        "Discord thread export\nChannel: {channel_name} ({channel_id})\nMessages: {}\n\n",
        messages.len()
    );
    let entries: Vec<String> = messages.iter().map(format_export_message).collect();
    assemble_export(&header, &entries, max_bytes)
}

/// Build the transcript body from a pre-rendered header and a list of
/// already-formatted message entries, honouring `max_bytes`.
///
/// Returns `(transcript, written, truncated)` where `written` is the number of
/// entries actually included. Split out from `format_thread_export` so the
/// truncation boundary logic can be unit-tested without constructing real
/// `serenity::model::channel::Message` values.
fn assemble_export(header: &str, entries: &[String], max_bytes: usize) -> (String, usize, bool) {
    let mut out = String::from(header);
    let mut written = 0;
    let mut truncated = false;

    for entry in entries {
        if out.len() + entry.len() > max_bytes {
            truncated = true;
            break;
        }
        out.push_str(entry);
        written += 1;
    }

    if truncated {
        let note = "\n[Export truncated to fit Discord attachment size limit]\n";
        let room = max_bytes.saturating_sub(out.len());
        if room >= note.len() {
            out.push_str(note);
        }
    }

    (out, written, truncated)
}

fn format_export_message(msg: &Message) -> String {
    let bot_marker = if msg.author.bot { " [bot]" } else { "" };
    let mut out = format!(
        "[{}] {}{} ({})\n",
        msg.timestamp, msg.author.name, bot_marker, msg.author.id
    );

    if msg.content.is_empty() {
        out.push_str("(no text)\n");
    } else {
        out.push_str(&msg.content);
        out.push('\n');
    }

    for attachment in &msg.attachments {
        let mime = attachment.content_type.as_deref().unwrap_or("unknown");
        out.push_str(&format!(
            "[attachment] {} ({} bytes, {}): {}\n",
            attachment.filename, attachment.size, mime, attachment.url
        ));
    }

    out.push('\n');
    out
}

fn export_filename(channel_id: ChannelId, channel_name: &str) -> String {
    let safe_name = sanitize_filename_component(channel_name);
    format!("discord-thread-{safe_name}-{channel_id}.txt")
}

/// Reduce a free-form Discord channel/thread name to a safe ASCII filename
/// fragment.
///
/// Non-ASCII characters are dropped silently — a purely-Chinese thread name
/// like "扈三娘的房間" yields a date-based fallback (e.g. `"20260512"`).
/// The caller appends the channel ID, which already guarantees uniqueness,
/// and an ASCII fragment plays nicer with downstream tools (mail attachments,
/// S3 keys, browser save-as dialogs). The 64-byte cap leaves room for the
/// `discord-thread-` prefix and the channel-ID suffix within typical
/// filesystem limits.
fn sanitize_filename_component(input: &str) -> String {
    let mut safe = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            safe.push(ch);
        } else if ch.is_whitespace() || matches!(ch, '.' | '/') {
            safe.push('-');
        }
    }
    let safe = safe.trim_matches('-');
    if safe.is_empty() {
        // Use current date as a human-friendly fallback when the thread name
        // is entirely non-ASCII.
        chrono::Utc::now().format("%Y%m%d").to_string()
    } else {
        safe.chars().take(64).collect()
    }
}

async fn get_or_create_thread(
    ctx: &Context,
    adapter: &Arc<dyn ChatAdapter>,
    msg: &Message,
    prompt: &str,
) -> anyhow::Result<ChannelRef> {
    let channel = msg.channel_id.to_channel(&ctx.http).await?;
    if let serenity::model::channel::Channel::Guild(ref gc) = channel {
        // Already in a thread — reuse it. Uses thread_metadata (see detect_thread()).
        if gc.thread_metadata.is_some() {
            return Ok(ChannelRef {
                platform: "discord".into(),
                channel_id: msg.channel_id.get().to_string(),
                thread_id: None,
                parent_id: None,
                origin_event_id: None,
            });
        }
    }

    let thread_name = format::shorten_thread_name(prompt);
    let parent = ChannelRef {
        platform: "discord".into(),
        channel_id: msg.channel_id.get().to_string(),
        thread_id: None,
        parent_id: None,
        origin_event_id: None,
    };
    let trigger_ref = discord_msg_ref(msg);
    match adapter
        .create_thread(&parent, &trigger_ref, &thread_name)
        .await
    {
        Ok(ch) => Ok(ch),
        Err(e) if is_thread_already_exists_error(&e) => {
            // Another bot won the race from the same trigger message. Discord
            // only allows one thread per message, so refetch the message and
            // join the thread our sibling just created.
            let refreshed = msg
                .channel_id
                .message(&ctx.http, msg.id)
                .await
                .map_err(|fe| {
                    anyhow::anyhow!("thread_already_exists (race), but refetch failed: {fe}")
                })?;
            let existing = refreshed.thread.ok_or_else(|| {
                anyhow::anyhow!(
                    "thread_already_exists (race), but message has no thread after refetch"
                )
            })?;
            tracing::info!(
                channel_id = %msg.channel_id,
                thread_id = %existing.id,
                "joining thread created by sibling bot from same trigger message"
            );
            Ok(ChannelRef {
                platform: "discord".into(),
                channel_id: existing.id.to_string(),
                thread_id: None,
                parent_id: Some(msg.channel_id.get().to_string()),
                origin_event_id: None,
            })
        }
        Err(e) => Err(e),
    }
}

/// Detect Discord's "A thread has already been created for this message" error
/// (JSON error code 160004). Triggered when two bots responding to the same
/// @-mention race to create a thread from the same trigger message.
///
/// Uses string matching because serenity surfaces Discord API errors as
/// formatted strings — there is no structured error code we can match on.
/// Unit tests pin the expected patterns so serenity formatting changes are caught.
fn is_thread_already_exists_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("160004") || msg.contains("already been created")
}

static ROLE_MENTION_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"<@&\d+>").unwrap());

fn resolve_mentions(content: &str, bot_id: UserId, allowed_role_ids: &HashSet<u64>) -> String {
    // 1. Strip the bot's own trigger mention
    let out = content
        .replace(&format!("<@{}>", bot_id), "")
        .replace(&format!("<@!{}>", bot_id), "");
    // 2. Strip allowed role mentions (they triggered the bot, not useful in prompt)
    let out = if allowed_role_ids.is_empty() {
        out
    } else {
        allowed_role_ids
            .iter()
            .fold(out, |s, id| s.replace(&format!("<@&{}>", id), ""))
    };
    // 3. Other user mentions: keep <@UID> as-is so the LLM can mention back
    // 4. Fallback: replace remaining role mentions only (user mentions are preserved)
    let out = ROLE_MENTION_RE.replace_all(&out, "@(role)").to_string();
    out.trim().to_string()
}

fn video_attachment_block(
    filename: &str,
    content_type: Option<&str>,
    size: u64,
    url: &str,
) -> ContentBlock {
    ContentBlock::Text {
        text: format!(
            "[Video attachment]\nfilename: {}\ncontent_type: {}\nsize_bytes: {}\nurl: {}",
            filename,
            content_type.unwrap_or("unknown"),
            size,
            url
        ),
    }
}

/// Build a `SenderContext` for Discord messages.
///
/// Pure function extracted from `EventHandler::message` for testability.
/// When `thread_parent_id` is `Some`, the message is inside a thread:
/// - `channel_id` → parent channel (where the thread lives)
/// - `thread_id`  → thread's own channel ID
///
/// This mirrors Slack's model where `channel_id` is always the parent
/// channel and `thread_id` (thread_ts) identifies the thread.
///
/// Note: `ChannelRef.channel_id` uses the *opposite* convention — it holds
/// the thread's channel ID for routing (Discord API sends to thread by its
/// channel ID). See `ChannelRef` doc comments for details.
#[allow(clippy::too_many_arguments)]
fn build_sender_context(
    sender_id: &str,
    sender_name: &str,
    display_name: &str,
    msg_channel_id: &str,
    thread_parent_id: Option<&str>,
    is_bot: bool,
    timestamp: &str,
    message_id: &str,
    receiver_id: &str,
) -> SenderContext {
    SenderContext {
        schema: "openab.sender.v1".into(),
        sender_id: sender_id.to_string(),
        sender_name: sender_name.to_string(),
        display_name: display_name.to_string(),
        channel: "discord".into(),
        channel_id: thread_parent_id.unwrap_or(msg_channel_id).to_string(),
        thread_id: thread_parent_id.map(|_| msg_channel_id.to_string()),
        is_bot,
        timestamp: Some(timestamp.to_string()),
        message_id: Some(message_id.to_string()),
        receiver_id: Some(receiver_id.to_string()),
        output_instructions: Some(vec![
            "When the user asks you to display or send an existing PNG from this session workspace, begin the final answer with one [[attach:relative/path.png]] line per image (maximum 4). Use workspace-relative paths only; never use absolute paths."
                .to_string(),
        ]),
    }
}

/// Pure thread detection: determines whether a channel is a Discord thread
/// in an allowed parent, and whether the bot owns it.
///
/// Returns `(in_allowed_thread, bot_owns)`:
/// - `in_allowed_thread`: true only if the channel IS a thread AND its parent
///   is permitted (via allowlist, `allow_all_channels`, or `in_allowed_channel`).
/// - `bot_owns`: `None` if the channel is not a thread (ownership is meaningless);
///   `Some(true/false)` if it IS a thread, indicating whether the bot owns it.
///
/// Uses `thread_metadata.is_some()` — the canonical way to identify threads.
/// `parent_id` is NOT reliable for thread detection: category children also
/// have `parent_id` set. `parent_id` is only used here for the allowlist check.
///
/// Discord API refs:
/// - Channel Object (parent_id / thread_metadata fields):
///   https://docs.discord.com/developers/resources/channel#channel-object
/// - Thread Metadata ("thread-specific fields not needed by other channels"):
///   https://docs.discord.com/developers/resources/channel#thread-metadata-object
fn detect_thread(
    has_thread_metadata: bool,
    parent_id: Option<u64>,
    owner_id: Option<u64>,
    bot_id: u64,
    allowed_channels: &HashSet<u64>,
    allow_all_channels: bool,
    in_allowed_channel: bool,
) -> (bool, Option<bool>) {
    if !has_thread_metadata {
        return (false, None);
    }
    let in_allowed_thread = in_allowed_channel
        || allow_all_channels
        || parent_id.is_some_and(|pid| allowed_channels.contains(&pid));
    let bot_owns = owner_id.is_some_and(|oid| oid == bot_id);
    (in_allowed_thread, Some(bot_owns))
}

/// Returns `true` if the author should be denied by the user allowlist.
/// Bot authors skip this check — they are gated by `allow_bot_messages` + `trusted_bot_ids`.
fn is_denied_user(
    is_bot: bool,
    allow_all_users: bool,
    allowed_users: &HashSet<u64>,
    user_id: u64,
) -> bool {
    !is_bot && !allow_all_users && !allowed_users.contains(&user_id)
}

/// Returns `true` if a bot message should bypass the `allow_bot_messages` mode check.
/// A trusted bot that @mentions this bot is treated the same as a human @mention —
/// it can pull the bot into a thread regardless of the `allow_bot_messages` setting.
#[cfg(test)]
fn is_trusted_bot_mention(
    is_mentioned: bool,
    trusted_bot_ids: &HashSet<u64>,
    author_id: u64,
) -> bool {
    is_mentioned && !trusted_bot_ids.is_empty() && trusted_bot_ids.contains(&author_id)
}

/// Pure decision function: should a DM be processed?
/// Returns `true` if the DM should be processed (bot responds).
/// Mirrors the DM gating logic in EventHandler::message:
/// - `allow_dm` must be true
/// - `allowed_users` still applies (checked separately via `is_denied_user`)
/// - DMs bypass `allowed_channels` and `@mention` requirements
#[cfg(test)]
fn should_process_dm(allow_dm: bool) -> bool {
    allow_dm
}

/// Pure decision function: should thread creation be skipped?
/// Returns `true` when the message should reuse the current channel
/// directly (existing thread or DM), `false` when a new thread should
/// be created. Pins the invariant that DMs never call
/// `get_or_create_thread()` — Discord DM channels cannot create threads.
#[cfg(test)]
fn should_skip_thread_creation(in_thread: bool, is_dm: bool) -> bool {
    in_thread || is_dm
}

/// Pure decision function: should this message be processed or ignored?
/// Returns `true` if the message should be processed (bot responds).
/// Extracted from the EventHandler::message gating logic for testability.
#[cfg(test)]
fn should_process_user_message(
    mode: AllowUsers,
    is_mentioned: bool,
    in_thread: bool,
    involved: bool,
    other_bot_present: bool,
) -> bool {
    if is_mentioned {
        return true;
    }
    match mode {
        AllowUsers::Mentions => false,
        AllowUsers::Involved => in_thread && involved,
        AllowUsers::MultibotMentions => {
            if !in_thread || !involved {
                return false;
            }
            !other_bot_present
        }
    }
}

/// Pure decision function: should a reaction event be processed?
/// Returns `true` if the reaction should trigger the mapped prompt.
///
/// Unlike message gating, reactions have no @mention concept. In
/// MultibotMentions mode, targeting is determined by whether the reaction
/// was placed on this bot's message (`targets_this_bot`).
///
/// This function is called AFTER:
/// - channel/thread allowlist has passed
/// - `is_thread` is known from `detect_thread`
/// - `bot_involved` is from `bot_participated_in_thread` (only if is_thread)
fn should_process_reaction(
    mode: AllowUsers,
    is_thread: bool,
    bot_involved: bool,
    other_bot_present: bool,
    targets_this_bot: bool,
) -> bool {
    match mode {
        AllowUsers::Mentions => false,
        AllowUsers::Involved => is_thread && bot_involved,
        AllowUsers::MultibotMentions => {
            if !is_thread || !bot_involved {
                return false;
            }
            if other_bot_present {
                return targets_this_bot;
            }
            true
        }
    }
}

/// Returns true if any bot message in `messages` contains a turn limit warning.
/// Used to dedup `WarnAndStop` across multiple bot processes sharing a thread. (#530)
/// Note: this is best-effort — a narrow race window exists where two bots fetch
/// simultaneously and both see no warning, resulting in a duplicate. For most
/// deployments this is acceptable; strict once-only semantics would require
/// shared state (e.g. gateway-owned emission or distributed lock).
///
/// Accepts `(is_bot, content)` pairs so the logic can be unit-tested without
/// constructing `serenity::model::channel::Message` values (see existing test
/// boundary comment at `format_thread_export`).
fn turn_limit_warning_present(messages: &[(bool, &str)]) -> bool {
    messages
        .iter()
        .any(|(is_bot, content)| *is_bot && content.contains(BOT_TURN_LIMIT_WARNING_PREFIX))
}

/// Strip ANSI escape sequences (color codes, cursor movement, etc.) from text.
/// Auth CLIs like `codex` emit these for terminal styling, but they render as
/// garbage in Discord messages.
fn strip_ansi_codes(s: &str) -> String {
    static ANSI_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\([A-Z]").unwrap());
    ANSI_RE.replace_all(s, "").into_owned()
}

/// Ensure URLs are not glued to preceding text after ANSI stripping.
/// Discord's markdown parser collapses list-continuation whitespace when a Link
/// node is adjacent to a Text node, causing `accounthttps://...` rendering.
/// This inserts a newline before any URL that immediately follows a non-whitespace char.
fn ensure_url_separation(s: &str) -> String {
    static URL_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?P<prev>\S)(?P<url>https?://)").unwrap());
    URL_RE.replace_all(s, "${prev}\n${url}").into_owned()
}

/// Truncate `body` so that, prefixed by `prefix` and suffixed by `suffix`, the
/// whole message fits within `limit` measured in **UTF-16 code units** — which
/// is how Discord enforces its 2000-character message cap. Truncation only ever
/// happens on a `char` boundary, so a multi-byte scalar (e.g. an emoji that
/// encodes as a surrogate pair) is never split. Returns the truncated `body`
/// (without prefix/suffix).
///
/// Extracted from `handle_auth_command` so the boundary arithmetic — which is
/// easy to get wrong by conflating Unicode scalar count with UTF-16 code units —
/// can be unit-tested in isolation.
fn truncate_to_utf16_budget(body: &str, prefix: &str, suffix: &str, limit: usize) -> String {
    let budget = limit
        .saturating_sub(prefix.encode_utf16().count())
        .saturating_sub(suffix.encode_utf16().count());
    let mut truncated = String::new();
    let mut used = 0usize;
    for ch in body.chars() {
        let w = ch.len_utf16();
        if used + w > budget {
            break;
        }
        used += w;
        truncated.push(ch);
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_turns::{TurnResult, BOT_TURN_LIMIT_WARNING_PREFIX, HARD_BOT_TURN_LIMIT};

    // --- truncate_for_discord (select menu option 100-char cap) ---

    #[test]
    fn truncate_for_discord_short_string_unchanged() {
        assert_eq!(truncate_for_discord("auto", 100), "auto");
    }

    #[test]
    fn truncate_for_discord_exactly_at_limit_unchanged() {
        let s = "x".repeat(100);
        assert_eq!(truncate_for_discord(&s, 100), s);
    }

    #[test]
    fn truncate_for_discord_over_limit_truncated_with_ellipsis() {
        // Real-world case: the claude-fable-5 model description (~140 chars)
        // broke the /models slash command with Invalid Form Body.
        let desc = "[Internal] DEVELOPMENT USE CASES ONLY, NOT FOR CUSTOMER DATA, ITAR OR PII. \
                    Experimental preview of Claude Fable 5 model with 1M context window";
        let out = truncate_for_discord(desc, 100);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_for_discord_counts_chars_not_bytes() {
        // 120 CJK chars = 360 bytes; must not panic on byte boundaries and
        // must come back at exactly 100 chars.
        let s = "測".repeat(120);
        let out = truncate_for_discord(&s, 100);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with('…'));
    }

    // --- Phase 2 workspace/session command formatting and scope ---

    #[test]
    fn session_commands_allow_configured_channel_or_thread_parent() {
        let allowed = HashSet::from([42]);
        assert!(session_command_channel_allowed(42, None, &allowed, false));
        assert!(session_command_channel_allowed(
            100,
            Some(42),
            &allowed,
            false
        ));
        assert!(!session_command_channel_allowed(
            100,
            Some(99),
            &allowed,
            false
        ));
        assert!(session_command_channel_allowed(100, None, &allowed, true));
    }

    #[test]
    fn project_channel_names_are_safe_and_bounded() {
        assert_eq!(sanitize_project_channel_name(" OpenAB API "), "openab-api");
        assert_eq!(sanitize_project_channel_name("///"), "project");
        assert_eq!(sanitize_project_channel_name("x"), "x-project");
        assert!(
            sanitize_project_channel_name(&"a".repeat(150))
                .chars()
                .count()
                <= 100
        );
    }

    #[test]
    fn project_channel_access_supports_thread_workflow() {
        let permissions = project_channel_access_permissions();
        assert!(permissions.contains(Permissions::VIEW_CHANNEL));
        assert!(permissions.contains(Permissions::CREATE_PUBLIC_THREADS));
        assert!(permissions.contains(Permissions::SEND_MESSAGES_IN_THREADS));
    }

    #[test]
    fn project_workspace_autocomplete_filters_used_aliases_and_query() {
        let aliases = HashMap::from([
            ("api".to_string(), "/work/api".to_string()),
            ("frontend".to_string(), "/work/frontend".to_string()),
            ("example-library".to_string(), "/work/example-library".to_string()),
        ]);
        let used = HashSet::from(["api".to_string()]);

        assert_eq!(
            project_workspace_choices(&aliases, &used, "vault"),
            vec![("@example-library".to_string(), "example-library".to_string())]
        );
        assert_eq!(
            project_workspace_choices(&aliases, &used, ""),
            vec![
                ("@frontend".to_string(), "frontend".to_string()),
                ("@example-library".to_string(), "example-library".to_string()),
            ]
        );
    }

    #[test]
    fn workspace_status_shows_session_channel_default_and_aliases() {
        let aliases = HashMap::from([
            ("web".to_string(), "/projects/web".to_string()),
            ("api".to_string(), "/projects/api".to_string()),
        ]);
        let snapshot = SessionSnapshot {
            state: SessionState::Active,
            working_dir: Some("/projects/api".to_string()),
            externally_detached: false,
        };

        let output = format_workspace_status(&snapshot, Some("@web"), &aliases);
        assert!(output.contains("`@api` (`/projects/api`)"));
        assert!(output.contains("`@web` (`/projects/web`)"));
        assert!(output.contains("`@api`, `@web`"));
    }

    #[test]
    fn workspace_list_is_sorted_and_marks_channel_default() {
        let aliases = HashMap::from([
            ("web".to_string(), "/projects/web".to_string()),
            ("api".to_string(), "/projects/api".to_string()),
        ]);

        let output = format_workspace_list(&aliases, Some("@web"));
        assert!(output.find("`@api`").unwrap() < output.find("`@web`").unwrap());
        assert!(output.contains("Channel default: `@web` (`/projects/web`)"));
    }

    #[test]
    fn session_states_have_expected_control_panel_copy() {
        assert_eq!(
            session_state_presentation(SessionState::Active, false).0,
            "執行中"
        );
        assert_eq!(
            session_state_presentation(SessionState::Suspended, false).0,
            "可在 Discord 接續"
        );
        assert_eq!(
            session_state_presentation(SessionState::Persisted, false).0,
            "可在 Discord 接續"
        );
        assert_eq!(
            session_state_presentation(SessionState::None, false).0,
            "尚未開始"
        );
        assert_eq!(
            session_state_presentation(SessionState::Persisted, true).0,
            "Cursor 接手中"
        );
    }

    #[test]
    fn cursor_handoff_state_reconciles_after_terminal_exit() {
        let resumable = SessionSnapshot {
            state: SessionState::Persisted,
            working_dir: Some("/work/api".into()),
            externally_detached: false,
        };
        assert_eq!(
            reconciled_handoff_task_state(TaskState::Cursor, &resumable),
            Some(TaskState::Ready)
        );

        let missing = SessionSnapshot {
            state: SessionState::None,
            working_dir: None,
            externally_detached: false,
        };
        assert_eq!(
            reconciled_handoff_task_state(TaskState::Cursor, &missing),
            Some(TaskState::Closed)
        );

        let detached = SessionSnapshot {
            externally_detached: true,
            ..resumable
        };
        assert_eq!(
            reconciled_handoff_task_state(TaskState::Cursor, &detached),
            None
        );
        assert_eq!(
            reconciled_handoff_task_state(TaskState::Ready, &detached),
            None
        );
    }

    #[test]
    fn close_copy_states_what_is_removed_and_retained() {
        assert!(SESSION_CLOSE_CONFIRMATION.contains("OpenAB session mapping"));
        assert!(SESSION_CLOSE_CONFIRMATION.contains("Cursor checkpoint are kept"));
        assert!(session_closed_note(0).contains("Cursor checkpoint was kept"));
        assert!(session_closed_note(2).contains("2 buffered message(s) dropped"));
    }

    fn ui_task(state: TaskState, last_prompt: Option<&str>) -> TaskRecord {
        let now = chrono::Utc::now();
        TaskRecord {
            guild_id: 1,
            project_channel_id: 2,
            workspace_alias: "api".into(),
            thread_id: 3,
            title: "Fix API".into(),
            created_by: 4,
            status_message_id: Some(5),
            state,
            queued_messages: 0,
            last_error: (state == TaskState::Failed).then(|| "agent exited".into()),
            last_prompt: last_prompt.map(str::to_string),
            created_at: now,
            updated_at: now,
        }
    }

    fn ui_binding() -> ProjectBinding {
        ProjectBinding {
            guild_id: 1,
            channel_id: 2,
            workspace_alias: "api".into(),
            created_by: 4,
            access_role_id: None,
            access_user_ids: Vec::new(),
            access_role_ids: Vec::new(),
            home_message_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn ui_project_action(id: &str) -> DiscordProjectActionConfig {
        DiscordProjectActionConfig {
            workspace_alias: "api".into(),
            id: id.into(),
            label: format!("Action {id}"),
            description: format!("Run {id}"),
            title: format!("Task {id}"),
            prompt: format!("Run the {id} workflow without changing files."),
        }
    }

    fn managed_entry(
        thread_id: u64,
        task_state: TaskState,
        state: SessionState,
    ) -> ManagedSessionEntry {
        let mut task = ui_task(task_state, None);
        task.thread_id = thread_id;
        task.title = format!("Task {thread_id}");
        ManagedSessionEntry {
            task,
            snapshot: SessionSnapshot {
                state,
                working_dir: Some("/work/api".into()),
                externally_detached: false,
            },
        }
    }

    fn component_with_custom_id<'a>(
        value: &'a serde_json::Value,
        custom_id: &str,
    ) -> Option<&'a serde_json::Value> {
        if value.get("custom_id").and_then(serde_json::Value::as_str) == Some(custom_id) {
            return Some(value);
        }
        match value {
            serde_json::Value::Array(values) => values
                .iter()
                .find_map(|value| component_with_custom_id(value, custom_id)),
            serde_json::Value::Object(values) => values
                .values()
                .find_map(|value| component_with_custom_id(value, custom_id)),
            _ => None,
        }
    }

    #[test]
    fn task_controls_are_contextual_instead_of_disabled() {
        let ready =
            serde_json::to_string(&task_control_rows(&ui_task(TaskState::Ready, None))).unwrap();
        assert!(ready.contains("oab_task:continue"));
        assert!(ready.contains("oab_session:detach"));
        assert!(!ready.contains("oab_task:retry"));

        let running =
            serde_json::to_string(&task_control_rows(&ui_task(TaskState::Running, None))).unwrap();
        assert!(running.contains("oab_session:cancel"));
        assert!(!running.contains("oab_session:detach"));

        let failed = serde_json::to_string(&task_control_rows(&ui_task(
            TaskState::Failed,
            Some("retry me"),
        )))
        .unwrap();
        assert!(failed.contains("oab_task:retry"));
        assert!(failed.contains("oab_task:edit"));
        assert!(failed.contains("oab_task:error"));
    }

    #[test]
    fn cursor_task_card_contains_copyable_resume_command() {
        let embed =
            serde_json::to_string(&task_status_embed(&ui_task(TaskState::Cursor, None))).unwrap();
        assert!(embed.contains("make session-resume THREAD_ID=3"));
    }

    #[test]
    fn help_and_project_home_expose_session_manager() {
        let help = serde_json::to_string(&help_action_center(None, &[], true)).unwrap();
        assert!(help.contains("oab_help:sessions"));
        assert!(help.contains("oab_admin:open"));

        let project = serde_json::to_string(&project_welcome_components(&[])).unwrap();
        assert!(project.contains("oab_project:sessions"));
        assert!(project.contains("oab_project:actions"));

        let admin = serde_json::to_string(&admin_navigation_buttons()).unwrap();
        assert!(admin.contains("oab_admin:cleanup"));
        assert!(admin.contains("oab_admin:channel_setup"));
    }

    #[test]
    fn admin_channel_setup_caps_categories_and_opens_scoped_modal() {
        let categories = (1..=30)
            .map(|index| crate::discord_admin::AdminCategory {
                id: index.to_string(),
                name: format!("Category {index}"),
                position: index,
                channels: Vec::new(),
            })
            .collect();
        let inventory = AdminInventory {
            categories,
            uncategorized: Vec::new(),
        };
        let value =
            serde_json::to_value(admin_channel_category_card(&inventory).into_message()).unwrap();
        let select = component_with_custom_id(&value, "oab_admin_channel_category").unwrap();

        assert_eq!(
            select["options"].as_array().unwrap().len(),
            SELECT_MENU_PAGE_SIZE
        );
        assert!(value.to_string().contains("first 25"));

        let modal = serde_json::to_string(&admin_channel_modal(123)).unwrap();
        assert!(modal.contains("oab_admin_channel_create:123"));
        assert!(modal.contains("Channel name"));
        assert!(modal.contains("Topic (optional)"));
    }

    #[test]
    fn admin_cleanup_card_caps_candidates_and_requires_preview_selection() {
        let candidates = (1..=30)
            .map(|index| crate::discord_admin::CleanupCandidate {
                id: index.to_string(),
                name: format!("unused-{index}"),
                target_type: "text_channel".into(),
                category_name: Some("Archive".into()),
                age_hours: 72,
                created_at: "2026-01-01T00:00:00+00:00".into(),
                reason: "Empty text channel".into(),
            })
            .collect();
        let cleanup = CleanupCandidates {
            minimum_age_hours: 24,
            candidates,
        };
        let value = serde_json::to_value(admin_cleanup_card(&cleanup).into_message()).unwrap();
        let select = component_with_custom_id(&value, "oab_admin_cleanup").unwrap();

        assert_eq!(
            select["options"].as_array().unwrap().len(),
            SELECT_MENU_PAGE_SIZE
        );
        assert!(value.to_string().contains("first 25"));
        assert!(!value.to_string().contains("confirm_delete"));
    }

    #[test]
    fn project_actions_card_caps_options_and_modal_prefills_prompt() {
        let actions = (1..=30)
            .map(|index| ui_project_action(&format!("action-{index}")))
            .collect::<Vec<_>>();
        let action_refs = actions.iter().collect::<Vec<_>>();
        let value = serde_json::to_value(project_actions_message(&ui_binding(), &action_refs))
            .expect("actions card should serialize");
        let select = component_with_custom_id(&value, "oab_project_actions").unwrap();
        assert_eq!(
            select["options"].as_array().unwrap().len(),
            SELECT_MENU_PAGE_SIZE
        );
        assert!(value.to_string().contains("顯示前 25 個，共 30 個 actions"));

        let action = ui_project_action("test");
        let modal = serde_json::to_string(&project_task_modal(
            Some(&action.title),
            Some(&action.prompt),
        ))
        .unwrap();
        assert!(modal.contains("Task test"));
        assert!(modal.contains("Run the test workflow without changing files."));
    }

    #[test]
    fn help_project_selector_caps_options_at_discord_limit() {
        let projects = (1..=30)
            .map(|channel_id| {
                let mut binding = ui_binding();
                binding.channel_id = channel_id;
                binding.workspace_alias = format!("repo-{channel_id}");
                binding
            })
            .collect::<Vec<_>>();
        let value = serde_json::to_value(help_action_center(None, &projects, false)).unwrap();
        let select = component_with_custom_id(&value, "oab_help_project").unwrap();

        assert_eq!(
            select["options"].as_array().unwrap().len(),
            SELECT_MENU_PAGE_SIZE
        );
        assert!(value.to_string().contains("Discord channel list"));
    }

    #[test]
    fn project_selector_visibility_honors_registered_access() {
        let mut binding = ui_binding();
        binding.created_by = 10;
        binding.access_user_ids = vec![20];
        binding.access_role_ids = vec![30];

        assert!(project_is_visible_to(&binding, 10, &HashSet::new(), None));
        assert!(project_is_visible_to(&binding, 20, &HashSet::new(), None));
        assert!(project_is_visible_to(
            &binding,
            40,
            &HashSet::from([30]),
            None
        ));
        assert!(project_is_visible_to(
            &binding,
            40,
            &HashSet::new(),
            Some(Permissions::MANAGE_CHANNELS)
        ));
        assert!(!project_is_visible_to(
            &binding,
            40,
            &HashSet::new(),
            Some(Permissions::empty())
        ));
    }

    #[test]
    fn selected_project_card_uses_contextual_action() {
        let binding = ui_binding();
        let local = serde_json::to_string(&help_project_message(&binding, 2)).unwrap();
        assert!(local.contains("oab_project:new"));
        assert!(local.contains("oab_project:attach"));

        let remote = serde_json::to_string(&help_project_message(&binding, 99)).unwrap();
        assert!(remote.contains("https://discord.com/channels/1/2"));
        assert!(!remote.contains("oab_project:new"));
    }

    #[test]
    fn session_manager_caps_select_options_and_targets_the_project() {
        let binding = ui_binding();
        let entries = (1..=30)
            .map(|thread_id| managed_entry(thread_id, TaskState::Ready, SessionState::Persisted))
            .collect::<Vec<_>>();
        let value =
            serde_json::to_value(session_manager_message(&binding, &entries, None, None)).unwrap();
        let select = component_with_custom_id(&value, "oab_sessions:select:2").unwrap();
        assert_eq!(
            select["options"].as_array().unwrap().len(),
            SELECT_MENU_PAGE_SIZE
        );
    }

    #[test]
    fn session_manager_only_removes_closed_sessions_and_protects_cursor_handoff() {
        let binding = ui_binding();
        let closed = managed_entry(3, TaskState::Closed, SessionState::None);
        let value = serde_json::to_value(session_manager_message(
            &binding,
            std::slice::from_ref(&closed),
            Some(3),
            None,
        ))
        .unwrap();
        let remove = component_with_custom_id(&value, "oab_sessions:remove:2:3").unwrap();
        assert_eq!(remove["disabled"].as_bool(), Some(false));

        let mut detached = managed_entry(4, TaskState::Cursor, SessionState::Persisted);
        detached.snapshot.externally_detached = true;
        let value = serde_json::to_value(session_manager_message(
            &binding,
            std::slice::from_ref(&detached),
            Some(4),
            None,
        ))
        .unwrap();
        let close = component_with_custom_id(&value, "oab_sessions:close:2:4").unwrap();
        let remove = component_with_custom_id(&value, "oab_sessions:remove:2:4").unwrap();
        assert_eq!(close["disabled"].as_bool(), Some(true));
        assert_eq!(remove["disabled"].as_bool(), Some(true));
    }

    #[test]
    fn project_access_display_includes_creator_users_and_roles() {
        let binding = ProjectBinding {
            guild_id: 1,
            channel_id: 2,
            workspace_alias: "api".to_string(),
            created_by: 3,
            access_role_id: None,
            access_user_ids: vec![4],
            access_role_ids: vec![5],
            home_message_id: None,
            created_at: chrono::Utc::now(),
        };

        assert_eq!(
            project_access_display(&binding),
            "<@3> (creator), <@4>, <@&5>"
        );
    }

    // --- format_usage_report tests (/usage slash command) ---

    fn usage_breakdown() -> crate::acp::protocol::UsageBreakdown {
        crate::acp::protocol::UsageBreakdown {
            display_name: "Credits".into(),
            used: 12781.64,
            limit: Some(10000.0),
            percentage: Some(127),
            overage_charges: Some(111.27),
            currency: Some("USD".into()),
        }
    }

    #[test]
    fn format_usage_over_limit() {
        let report = UsageReport {
            plan_name: "KIRO POWER".into(),
            billing_cycle_reset: Some("2026-08-01".into()),
            breakdowns: vec![usage_breakdown()],
        };
        let (body, over_limit) = format_usage_body(&report);
        assert!(over_limit);
        assert!(body.contains("12781.64 / 10000"));
        assert!(body.contains("127%"));
        assert!(body.contains("⚠️"));
        assert!(body.contains("Overage charges: 111.27 USD"));
    }

    /// The report body must ride in the message content (normal font size),
    /// not the embed description (rendered smaller by Discord clients). The
    /// embed only carries the color strip + billing-cycle footer.
    #[test]
    fn usage_reply_body_in_content_not_embed() {
        let report = UsageReport {
            plan_name: "KIRO POWER".into(),
            billing_cycle_reset: Some("2026-08-01".into()),
            breakdowns: vec![usage_breakdown()],
        };
        let (content, embed) = build_usage_reply(&report);
        assert!(content.starts_with("📊 **Usage — KIRO POWER**"));
        assert!(content.contains("12781.64 / 10000"));
        assert!(content.contains("Overage charges: 111.27 USD"));
        let json = serde_json::to_value(&embed).expect("embed serializes");
        assert!(
            json.get("description").is_none(),
            "body must not be in embed"
        );
        assert!(json.get("title").is_none(), "title must not be in embed");
        assert_eq!(json["color"], 0xE74C3C, "over limit → red strip");
        assert_eq!(json["footer"]["text"], "Billing cycle resets 2026-08-01");
    }

    #[test]
    fn format_usage_no_limit_shows_consumption_only() {
        let report = UsageReport {
            plan_name: "ENTERPRISE".into(),
            billing_cycle_reset: None,
            breakdowns: vec![crate::acp::protocol::UsageBreakdown {
                display_name: "Credits".into(),
                used: 320.0,
                limit: None,
                percentage: None,
                overage_charges: None,
                currency: None,
            }],
        };
        let (body, over_limit) = format_usage_body(&report);
        assert!(!over_limit);
        assert!(body.contains("Credits: 320.00 used"));
        assert!(!body.contains('/'));
        assert!(!body.contains("Overage"));
    }

    #[test]
    fn format_usage_under_limit_no_warning() {
        let report = UsageReport {
            plan_name: "FREE".into(),
            billing_cycle_reset: None,
            breakdowns: vec![crate::acp::protocol::UsageBreakdown {
                display_name: "Credits".into(),
                used: 50.0,
                limit: Some(100.0),
                percentage: Some(50),
                overage_charges: Some(0.0),
                currency: Some("USD".into()),
            }],
        };
        let (body, over_limit) = format_usage_body(&report);
        assert!(!over_limit);
        assert!(body.contains("50%"));
        assert!(!body.contains("⚠️"));
        assert!(!body.contains("Overage"));
        // 50% → 5 of 10 bar slots filled.
        assert!(body.contains("█████░░░░░"));
    }

    // --- truncate_to_utf16_budget tests (#1185 /auth output relay) ---

    /// Body shorter than the budget is returned unchanged.
    #[test]
    fn truncate_utf16_short_body_unchanged() {
        assert_eq!(truncate_to_utf16_budget("hello", "", "", 2000), "hello");
    }

    /// prefix + suffix consume the budget; the body gets the remainder.
    #[test]
    fn truncate_utf16_respects_prefix_suffix_budget() {
        // limit 10, prefix "pre" (3) + suffix "su" (2) = 5 → 5 ASCII units left.
        assert_eq!(
            truncate_to_utf16_budget("abcdefghij", "pre", "su", 10),
            "abcde"
        );
    }

    /// A supplementary-plane scalar counts as TWO UTF-16 code units, not one.
    #[test]
    fn truncate_utf16_counts_surrogate_pairs_as_two_units() {
        // '🔐' (U+1F510) is one scalar but two UTF-16 units.
        // Budget 5 → two emoji (4 units) fit; a third (→6) does not.
        let out = truncate_to_utf16_budget("🔐🔐🔐", "", "", 5);
        assert_eq!(out, "🔐🔐");
        assert_eq!(out.encode_utf16().count(), 4);
    }

    /// A scalar is never split: a 2-unit emoji cannot fit a 1-unit budget.
    #[test]
    fn truncate_utf16_never_splits_a_scalar() {
        assert_eq!(truncate_to_utf16_budget("🔐rest", "", "", 1), "");
    }

    /// When affixes alone exceed the limit, the budget saturates to zero.
    #[test]
    fn truncate_utf16_zero_budget_when_affixes_exceed_limit() {
        assert_eq!(
            truncate_to_utf16_budget("anything", "longprefix", "longsuffix", 4),
            ""
        );
    }

    /// The assembled message (prefix + body + suffix) never exceeds the limit,
    /// even for output dense with multi-unit scalars — this is the regression
    /// guard for the original `chars().count()` (scalar) miscount.
    #[test]
    fn truncate_utf16_assembled_total_within_limit() {
        let prefix = "🔐 **Agent Authentication**\n```\n";
        let suffix = "\n```\nFollow the instructions above. Waiting for authorization...";
        let body = "https://example.com/device AB🔐CD\n".repeat(200);
        let out = truncate_to_utf16_budget(&body, prefix, suffix, 2000);
        let total = prefix.encode_utf16().count()
            + out.encode_utf16().count()
            + suffix.encode_utf16().count();
        assert!(total <= 2000, "assembled total {total} exceeds 2000");
    }

    // --- strip_ansi_codes tests ---

    #[test]
    fn strip_ansi_removes_color_codes() {
        let input = "\x1b[90mOpenAI\x1b[0m \x1b[94mhttps://auth.openai.com\x1b[0m";
        assert_eq!(strip_ansi_codes(input), "OpenAI https://auth.openai.com");
    }

    #[test]
    fn strip_ansi_passthrough_clean_text() {
        assert_eq!(strip_ansi_codes("no codes here"), "no codes here");
    }

    #[test]
    fn strip_ansi_removes_non_sgr_sequences() {
        let input = "\x1b[?25lhello\x1b[?25h \x1b(Bworld";
        assert_eq!(strip_ansi_codes(input), "hello world");
    }

    // --- ensure_url_separation tests ---

    #[test]
    fn url_separation_inserts_newline_when_glued() {
        assert_eq!(
            ensure_url_separation("accounthttps://auth.openai.com/codex/device"),
            "account\nhttps://auth.openai.com/codex/device"
        );
    }

    #[test]
    fn url_separation_preserves_existing_space() {
        assert_eq!(
            ensure_url_separation("account https://auth.openai.com"),
            "account https://auth.openai.com"
        );
    }

    #[test]
    fn url_separation_preserves_existing_newline() {
        assert_eq!(
            ensure_url_separation("account\nhttps://auth.openai.com"),
            "account\nhttps://auth.openai.com"
        );
    }

    #[test]
    fn url_separation_handles_http() {
        assert_eq!(
            ensure_url_separation("clickhttp://example.com"),
            "click\nhttp://example.com"
        );
    }

    // --- resolve_mentions tests ---

    /// Bot's own <@UID> mention is stripped from the prompt.
    #[test]
    fn resolve_mentions_strips_bot_mention() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("hello <@111> world", bot_id, &HashSet::new());
        assert_eq!(result, "hello  world");
    }

    /// Bot's own legacy <@!UID> mention is also stripped.
    #[test]
    fn resolve_mentions_strips_bot_mention_legacy() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("hello <@!111> world", bot_id, &HashSet::new());
        assert_eq!(result, "hello  world");
    }

    /// Other users' <@UID> mentions are preserved so the LLM can mention them back.
    #[test]
    fn resolve_mentions_preserves_other_user_mentions() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("<@111> say hi to <@222>", bot_id, &HashSet::new());
        assert_eq!(result, "say hi to <@222>");
    }

    /// Role mentions <@&UID> are replaced with @(role) placeholder.
    #[test]
    fn resolve_mentions_replaces_role_mentions() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("hello <@&999>", bot_id, &HashSet::new());
        assert_eq!(result, "hello @(role)");
    }

    /// Message containing only the bot mention results in empty string.
    #[test]
    fn resolve_mentions_empty_after_strip() {
        let bot_id = UserId::new(111);
        let result = resolve_mentions("<@111>", bot_id, &HashSet::new());
        assert_eq!(result, "");
    }

    /// Allowed role mentions are stripped from prompt (not replaced with @(role)).
    #[test]
    fn resolve_mentions_strips_allowed_role() {
        let bot_id = UserId::new(111);
        let roles: HashSet<u64> = [999].into_iter().collect();
        let result = resolve_mentions("hello <@&999> world", bot_id, &roles);
        assert_eq!(result, "hello  world");
    }

    /// Non-allowed role mentions are still replaced with @(role).
    #[test]
    fn resolve_mentions_keeps_other_roles_as_placeholder() {
        let bot_id = UserId::new(111);
        let roles: HashSet<u64> = [999].into_iter().collect();
        let result = resolve_mentions("<@&999> check <@&888>", bot_id, &roles);
        assert_eq!(result, "check @(role)");
    }

    #[test]
    fn video_attachment_block_includes_actionable_metadata() {
        let block = video_attachment_block(
            "demo.mp4",
            Some("video/mp4"),
            12345,
            "https://cdn.discordapp.com/attachments/demo.mp4",
        );

        let ContentBlock::Text { text } = block else {
            panic!("video attachments must be forwarded as text metadata");
        };

        assert!(text.contains("[Video attachment]"));
        assert!(text.contains("filename: demo.mp4"));
        assert!(text.contains("content_type: video/mp4"));
        assert!(text.contains("size_bytes: 12345"));
        assert!(text.contains("url: https://cdn.discordapp.com/attachments/demo.mp4"));
    }

    #[test]
    fn image_attachment_block_includes_url_and_metadata() {
        // Simulates the format string used in the image attachment handler.
        let filename = "screenshot.png";
        let content_type = Some("image/png");
        let size: u32 = 142048;
        let url = "https://cdn.discordapp.com/attachments/123/456/screenshot.png";

        let text = format!(
            "[Image attachment]\nfilename: {}\ncontent_type: {}\nsize_bytes: {}\nurl: {} (expires ~24h)",
            filename,
            content_type.unwrap_or("unknown"),
            size,
            url,
        );

        assert!(text.contains("[Image attachment]"));
        assert!(text.contains("filename: screenshot.png"));
        assert!(text.contains("content_type: image/png"));
        assert!(text.contains("size_bytes: 142048"));
        assert!(text.contains("url: https://cdn.discordapp.com/attachments/123/456/screenshot.png"));
        assert!(text.contains("(expires ~24h)"));
    }

    #[test]
    fn image_attachment_block_missing_content_type_falls_back() {
        let content_type: Option<&str> = None;
        let text = format!(
            "[Image attachment]\nfilename: {}\ncontent_type: {}\nsize_bytes: {}\nurl: {} (expires ~24h)",
            "photo.jpg",
            content_type.unwrap_or("unknown"),
            99999,
            "https://cdn.discordapp.com/attachments/1/2/photo.jpg",
        );

        assert!(text.contains("content_type: unknown"));
    }

    // --- thread-race error detection ---

    /// Detects the Discord error code for "thread already exists" (160004).
    #[test]
    fn is_thread_already_exists_matches_code() {
        let err = anyhow::Error::msg(
            r#"HTTP error: {"code": 160004, "message": "A thread has already been created for this message."}"#,
        );
        assert!(is_thread_already_exists_error(&err));
    }

    /// Detects the human-readable form of the error in case serenity renders
    /// it without the numeric code.
    #[test]
    fn is_thread_already_exists_matches_message() {
        let err = anyhow::anyhow!("A thread has already been created for this message.");
        assert!(is_thread_already_exists_error(&err));
    }

    /// Unrelated errors do not match — we don't want the fallback path
    /// swallowing real failures like permission denied.
    #[test]
    fn is_thread_already_exists_ignores_other_errors() {
        let err = anyhow::anyhow!("Missing Permissions");
        assert!(!is_thread_already_exists_error(&err));
        let err = anyhow::anyhow!("rate limit exceeded");
        assert!(!is_thread_already_exists_error(&err));
    }

    // --- thread export helpers ---

    #[test]
    fn sanitize_filename_component_keeps_safe_ascii() {
        assert_eq!(
            sanitize_filename_component("release notes_v2"),
            "release-notes_v2"
        );
    }

    #[test]
    fn sanitize_filename_component_falls_back_for_empty_result() {
        let result = sanitize_filename_component("///...");
        // Fallback is a YYYYMMDD date string
        assert_eq!(result.len(), 8);
        assert!(result.chars().all(|c| c.is_ascii_digit()));
    }

    // --- assemble_export ---
    // Split out from format_thread_export so we can test the truncation
    // boundary without constructing serenity::model::channel::Message values.

    #[test]
    fn assemble_export_empty_entries_returns_header_only() {
        let (out, written, truncated) = assemble_export("HDR\n", &[], 1024);
        assert_eq!(out, "HDR\n");
        assert_eq!(written, 0);
        assert!(!truncated);
    }

    #[test]
    fn assemble_export_single_oversized_entry_writes_zero_and_marks_truncated() {
        let entries = vec!["x".repeat(200)];
        let (out, written, truncated) = assemble_export("h\n", &entries, 50);
        assert_eq!(written, 0);
        assert!(truncated);
        // Footer needs ~56 bytes; max_bytes 50 leaves ≤48 of room, so it is
        // intentionally omitted (it can't be appended without exceeding the
        // limit). The header is still present.
        assert!(out.starts_with("h\n"));
        assert!(!out.contains("xx"));
    }

    #[test]
    fn assemble_export_entry_at_exact_boundary_is_included() {
        // header(2) + entry(3) == max_bytes(5); the strict-greater check
        // keeps the entry in.
        let (out, written, truncated) = assemble_export("h\n", &["abc".to_string()], 5);
        assert_eq!(written, 1);
        assert!(!truncated);
        assert_eq!(out, "h\nabc");
    }

    #[test]
    fn assemble_export_entry_one_byte_over_boundary_is_excluded() {
        // header(2) + entry(4) == 6 > max_bytes(5); entry is dropped.
        let (out, written, truncated) = assemble_export("h\n", &["abcd".to_string()], 5);
        assert_eq!(written, 0);
        assert!(truncated);
        assert!(out.starts_with("h\n"));
        assert!(!out.contains("abcd"));
    }

    #[test]
    fn assemble_export_appends_footer_when_room_remains() {
        // First two short entries fit; the long third entry would overflow,
        // and the remaining headroom is enough for the truncation footer.
        let entries = vec!["a\n".to_string(), "b\n".to_string(), "c".repeat(500)];
        let (out, written, truncated) = assemble_export("h\n", &entries, 200);
        assert_eq!(written, 2);
        assert!(truncated);
        assert!(out.contains("[Export truncated"));
    }

    // --- snowflake conversion ---

    #[test]
    fn timestamp_ms_to_snowflake_known_value() {
        // 2026-05-10 00:00:00 UTC = 1778572800000 ms since Unix epoch
        // Discord ms = 1778572800000 - 1420070400000 = 358502400000
        // Snowflake = 358502400000 << 22 = 1503238553600000000 (approx)
        let ts_ms: u64 = 1_778_572_800_000;
        let snowflake = timestamp_ms_to_snowflake(ts_ms);
        // Verify round-trip: extract timestamp back from snowflake
        let extracted_ms = (snowflake.get() >> 22) + DISCORD_EPOCH_MS;
        assert_eq!(extracted_ms, ts_ms);
    }

    #[test]
    fn timestamp_ms_to_snowflake_at_discord_epoch_is_one() {
        // At exactly the Discord epoch, discord_ms=0, shifted=0, clamped to 1
        let snowflake = timestamp_ms_to_snowflake(DISCORD_EPOCH_MS);
        assert_eq!(snowflake.get(), 1);
    }

    #[test]
    fn timestamp_ms_to_snowflake_before_epoch_saturates() {
        // Timestamp before Discord epoch should saturate to 1
        let snowflake = timestamp_ms_to_snowflake(1_000_000_000_000);
        assert_eq!(snowflake.get(), 1);
    }

    // --- ExportFilter cap logic ---

    #[test]
    fn export_filter_default_cap_is_100() {
        // Default (no params) uses Limit(100)
        let filter = ExportFilter::Limit(100);
        let cap = match &filter {
            ExportFilter::Limit(n) => *n,
            _ => THREAD_EXPORT_MESSAGE_LIMIT,
        };
        assert_eq!(cap, 100);
    }

    #[test]
    fn export_filter_all_cap_is_5000() {
        let filter = ExportFilter::All;
        let cap = match &filter {
            ExportFilter::Limit(n) => *n,
            _ => THREAD_EXPORT_MESSAGE_LIMIT,
        };
        assert_eq!(cap, THREAD_EXPORT_MESSAGE_LIMIT);
        assert_eq!(cap, 5000);
    }

    #[test]
    fn export_filter_limit_uses_custom_cap() {
        let filter = ExportFilter::Limit(250);
        let cap = match &filter {
            ExportFilter::Limit(n) => *n,
            _ => THREAD_EXPORT_MESSAGE_LIMIT,
        };
        assert_eq!(cap, 250);
    }

    #[test]
    fn export_filter_after_uses_global_cap() {
        let filter = ExportFilter::After(MessageId::new(123456789));
        let cap = match &filter {
            ExportFilter::Limit(n) => *n,
            _ => THREAD_EXPORT_MESSAGE_LIMIT,
        };
        assert_eq!(cap, THREAD_EXPORT_MESSAGE_LIMIT);
    }

    // --- should_process_user_message tests (GIVEN/WHEN/THEN) ---
    // Tests the multibot-mentions gating logic extracted from EventHandler::message.
    // The bug in #481 was that other bots' messages were filtered by bot gating
    // before multibot detection could run, so the bot never learned the thread
    // was multi-bot and responded without @mention.

    /// GIVEN: multibot-mentions mode, single-bot thread, bot is involved
    /// WHEN:  human sends message without @mention
    /// THEN:  bot responds (natural conversation)
    #[test]
    fn multibot_mentions_single_bot_thread_no_mention() {
        assert!(should_process_user_message(
            AllowUsers::MultibotMentions,
            false, // is_mentioned
            true,  // in_thread
            true,  // involved
            false, // other_bot_present
        ));
    }

    /// GIVEN: multibot-mentions mode, multi-bot thread (other bot has posted)
    /// WHEN:  human sends message without @mention
    /// THEN:  bot does NOT respond (requires @mention in multi-bot thread)
    /// This is the exact scenario from bug #481.
    #[test]
    fn multibot_mentions_multi_bot_thread_no_mention() {
        assert!(!should_process_user_message(
            AllowUsers::MultibotMentions,
            false, // is_mentioned
            true,  // in_thread
            true,  // involved
            true,  // other_bot_present ← another bot posted
        ));
    }

    /// GIVEN: multibot-mentions mode, multi-bot thread
    /// WHEN:  human sends message WITH @mention
    /// THEN:  bot responds (explicit @mention always works)
    #[test]
    fn multibot_mentions_multi_bot_thread_with_mention() {
        assert!(should_process_user_message(
            AllowUsers::MultibotMentions,
            true, // is_mentioned
            true, // in_thread
            true, // involved
            true, // other_bot_present
        ));
    }

    /// GIVEN: multibot-mentions mode, not in a thread (main channel)
    /// WHEN:  human sends message without @mention
    /// THEN:  bot does NOT respond (main channel always requires @mention)
    #[test]
    fn multibot_mentions_main_channel_no_mention() {
        assert!(!should_process_user_message(
            AllowUsers::MultibotMentions,
            false, // is_mentioned
            false, // in_thread (main channel)
            false, // involved
            false, // other_bot_present
        ));
    }

    /// GIVEN: multibot-mentions mode, in thread but bot is NOT involved
    /// WHEN:  human sends message without @mention
    /// THEN:  bot does NOT respond (not participating in this thread)
    #[test]
    fn multibot_mentions_not_involved() {
        assert!(!should_process_user_message(
            AllowUsers::MultibotMentions,
            false, // is_mentioned
            true,  // in_thread
            false, // involved ← bot hasn't posted here
            false, // other_bot_present
        ));
    }

    /// GIVEN: involved mode, multi-bot thread
    /// WHEN:  human sends message without @mention
    /// THEN:  bot responds (involved mode ignores multi-bot status)
    #[test]
    fn involved_mode_ignores_multibot() {
        assert!(should_process_user_message(
            AllowUsers::Involved,
            false, // is_mentioned
            true,  // in_thread
            true,  // involved
            true,  // other_bot_present ← ignored in involved mode
        ));
    }

    /// GIVEN: mentions mode
    /// WHEN:  human sends message without @mention (even in own thread)
    /// THEN:  bot does NOT respond (always requires @mention)
    #[test]
    fn mentions_mode_always_requires_mention() {
        assert!(!should_process_user_message(
            AllowUsers::Mentions,
            false, // is_mentioned
            true,  // in_thread
            true,  // involved
            false, // other_bot_present
        ));
    }

    /// After soft limit fires once (n==20), subsequent bot messages still return
    /// SoftLimit but with n>20. The caller warns only when n==max (exact hit),
    /// preventing warning messages from ping-ponging between bots.
    #[test]
    fn soft_limit_warn_once_semantics() {
        let mut t = BotTurnTracker::new(20);
        for _ in 0..19 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        // n==20: exact hit — caller should send warning
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(20));
        // n==21: past limit — caller should silently return (no warning)
        assert_eq!(t.on_bot_message("t1"), TurnResult::Throttled);
        // n==22: still past — still silent
        assert_eq!(t.on_bot_message("t1"), TurnResult::Throttled);
    }

    /// Hard limit also carries count for warn-once semantics.
    #[test]
    fn hard_limit_warn_once_semantics() {
        let mut t = BotTurnTracker::new(HARD_BOT_TURN_LIMIT + 1); // soft > hard so hard fires first
        for _ in 0..HARD_BOT_TURN_LIMIT - 1 {
            assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        }
        // Exact hit — warn
        assert_eq!(t.on_bot_message("t1"), TurnResult::HardLimit);
        // Past — silent
        assert_eq!(t.on_bot_message("t1"), TurnResult::Stopped);
    }

    /// Regression test for #497: system messages (thread created, pin, etc.)
    /// should NOT reset the bot turn counter. The filtering happens at the
    /// call site (MessageType check); this verifies the counter stays put
    /// when on_human_message is never called.
    #[test]
    fn system_message_does_not_reset_counter() {
        let mut t = BotTurnTracker::new(3);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        assert_eq!(t.on_bot_message("t1"), TurnResult::Ok);
        // No on_human_message (system message filtered out at call site)
        assert_eq!(t.on_bot_message("t1"), TurnResult::SoftLimit(3));
    }

    // --- build_sender_context tests (regression for #581 → #584) ---
    // PR #583 fixed SenderContext to use parent channel_id when in a thread.
    // These tests verify the pure function extracted from EventHandler::message.

    /// In-thread message: channel_id = parent, thread_id = thread channel ID.
    #[test]
    fn build_sender_context_in_thread() {
        let ctx = build_sender_context(
            "user1",
            "alice",
            "Alice",
            "thread_ch",
            Some("parent_ch"),
            false,
            "2026-05-01T00:00:00Z",
            "msg123",
            "bot99",
        );
        assert_eq!(ctx.channel_id, "parent_ch");
        assert_eq!(ctx.thread_id, Some("thread_ch".to_string()));
        assert_eq!(ctx.channel, "discord");
        assert_eq!(ctx.sender_id, "user1");
        assert!(!ctx.is_bot);
        assert_eq!(ctx.receiver_id, Some("bot99".to_string()));
        assert!(ctx
            .output_instructions
            .as_ref()
            .is_some_and(|items| items.iter().any(|item| item.contains("[[attach:"))));
    }

    /// Non-thread message: channel_id = message channel, thread_id = None.
    #[test]
    fn build_sender_context_not_in_thread() {
        let ctx = build_sender_context(
            "user1",
            "alice",
            "Alice",
            "main_ch",
            None,
            false,
            "2026-05-01T00:00:00Z",
            "msg456",
            "bot99",
        );
        assert_eq!(ctx.channel_id, "main_ch");
        assert_eq!(ctx.thread_id, None);
    }

    /// Bot sender: is_bot flag propagated correctly.
    #[test]
    fn build_sender_context_bot_sender() {
        let ctx = build_sender_context(
            "bot1",
            "mybot",
            "MyBot",
            "ch",
            Some("parent"),
            true,
            "2026-05-01T00:00:00Z",
            "msg789",
            "bot99",
        );
        assert!(ctx.is_bot);
        assert_eq!(ctx.channel_id, "parent");
        assert_eq!(ctx.thread_id, Some("ch".to_string()));
    }

    // --- detect_thread tests (regression for #506 → #518 → #519) ---
    // PR #506 used parent_id.is_some() to detect threads, but category text
    // channels also have parent_id (pointing to the category). This caused
    // the bot to skip thread creation for normal channels inside categories.
    //
    // detect_thread() uses thread_metadata.is_some() — the canonical check
    // per Discord API docs. Table-driven to cover all channel scenarios.

    const BOT: u64 = 1000;
    const OTHER: u64 = 2000;
    const PARENT_CH: u64 = 100;
    const CATEGORY: u64 = 200;

    /// Helper: build an allowed_channels set from a slice.
    fn allowed(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    /// Table-driven: each row is a realistic Discord channel scenario.
    #[test]
    fn detect_thread_table() {
        struct Case {
            name: &'static str,
            has_thread_metadata: bool,
            parent_id: Option<u64>,
            owner_id: Option<u64>,
            bot_id: u64,
            allowed_channels: HashSet<u64>,
            allow_all: bool,
            in_allowed: bool,
            expect: (bool, Option<bool>), // (in_thread, bot_owns)
        }

        let cases = vec![
            // --- Non-thread channels: thread_metadata = None ---
            Case {
                name: "text channel under category (regression #506)",
                has_thread_metadata: false,
                parent_id: Some(CATEGORY), // points to category, NOT a thread
                owner_id: None,
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: true,
                expect: (false, None),
            },
            Case {
                name: "top-level text channel (no category)",
                has_thread_metadata: false,
                parent_id: None,
                owner_id: None,
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: true,
                expect: (false, None),
            },
            Case {
                name: "voice channel under category",
                has_thread_metadata: false,
                parent_id: Some(CATEGORY),
                owner_id: None,
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: false,
                expect: (false, None),
            },
            // --- Thread channels: thread_metadata = Some ---
            Case {
                name: "public thread, parent in allowlist, bot owns",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: Some(BOT),
                bot_id: BOT,
                allowed_channels: allowed(&[PARENT_CH]),
                allow_all: false,
                in_allowed: false,
                expect: (true, Some(true)),
            },
            Case {
                name: "public thread, parent in allowlist, other user owns",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: Some(OTHER),
                bot_id: BOT,
                allowed_channels: allowed(&[PARENT_CH]),
                allow_all: false,
                in_allowed: false,
                expect: (true, Some(false)),
            },
            Case {
                name: "thread, parent NOT in allowlist, not allow_all",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: Some(BOT),
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: false,
                expect: (false, Some(true)),
            },
            Case {
                name: "thread, allow_all_channels = true",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: Some(OTHER),
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: true,
                in_allowed: false,
                expect: (true, Some(false)),
            },
            Case {
                name: "thread, in_allowed_channel = true (parent is the allowed channel)",
                has_thread_metadata: true,
                parent_id: Some(PARENT_CH),
                owner_id: None,
                bot_id: BOT,
                allowed_channels: allowed(&[]),
                allow_all: false,
                in_allowed: true,
                expect: (true, Some(false)),
            },
            // --- Defensive: partial data ---
            Case {
                name: "thread with parent_id = None (defensive, partial API data)",
                has_thread_metadata: true,
                parent_id: None,
                owner_id: Some(BOT),
                bot_id: BOT,
                allowed_channels: allowed(&[PARENT_CH]),
                allow_all: false,
                in_allowed: false,
                expect: (false, Some(true)), // can't verify parent → not allowed, but bot still owns
            },
        ];

        for c in &cases {
            let result = detect_thread(
                c.has_thread_metadata,
                c.parent_id,
                c.owner_id,
                c.bot_id,
                &c.allowed_channels,
                c.allow_all,
                c.in_allowed,
            );
            assert_eq!(result, c.expect, "FAILED: {}", c.name);
        }
    }

    // --- WarnAndStop regression test (#633) ---
    // The WarnAndStop path now delegates to detect_thread(). This test pins
    // the exact scenario from #633: a category child channel whose category
    // ID is in another bot's allowed_channels must NOT be treated as allowed.
    #[test]
    fn detect_thread_rejects_category_child_in_warn_and_stop() {
        let category_id: u64 = 200;
        let allowed = HashSet::from([category_id]);
        // Category child: has parent_id (the category) but NO thread_metadata.
        let (in_thread, _) =
            detect_thread(false, Some(category_id), None, 1000, &allowed, false, false);
        assert!(
            !in_thread,
            "category child must not match allowed_channels via parent_id"
        );
    }

    // --- Per-thread streaming tests (#534) ---
    // Streaming ON by default, OFF when another bot is detected in the thread.

    /// Single bot thread: streaming enabled.
    #[test]
    fn discord_streams_when_no_other_bot() {
        let adapter = super::DiscordAdapter::new(Arc::new(super::Http::new("")));
        assert!(adapter.use_streaming(false));
    }

    /// Multi-bot thread: send-once to avoid edit interference.
    #[test]
    fn discord_no_stream_when_other_bot_present() {
        let adapter = super::DiscordAdapter::new(Arc::new(super::Http::new("")));
        assert!(!adapter.use_streaming(true));
    }

    // --- resolve_channel tests ---

    #[test]
    fn resolve_channel_uses_channel_id_when_no_thread() {
        let ch = ChannelRef {
            platform: "discord".into(),
            channel_id: "111".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: None,
        };
        assert_eq!(DiscordAdapter::resolve_channel(&ch), "111");
    }

    #[test]
    fn resolve_channel_prefers_thread_id_when_set() {
        let ch = ChannelRef {
            platform: "discord".into(),
            channel_id: "111".into(),
            thread_id: Some("222".into()),
            parent_id: None,
            origin_event_id: None,
        };
        assert_eq!(DiscordAdapter::resolve_channel(&ch), "222");
    }

    // --- is_denied_user tests (regression for #604) ---

    /// Human not in allowlist → denied.
    #[test]
    fn denied_user_human_not_in_allowlist() {
        let allowed = HashSet::from([100]);
        assert!(is_denied_user(false, false, &allowed, 999));
    }

    /// Human in allowlist → allowed.
    #[test]
    fn denied_user_human_in_allowlist() {
        let allowed = HashSet::from([100]);
        assert!(!is_denied_user(false, false, &allowed, 100));
    }

    /// Bot not in allowlist → allowed (bots skip user gate). This is the #604 fix.
    #[test]
    fn denied_user_bot_skips_allowlist() {
        let allowed = HashSet::from([100]);
        assert!(!is_denied_user(true, false, &allowed, 999));
    }

    #[test]
    fn l3_gate_skips_bots_admits_humans() {
        // Regression guard (#1270 F1): the shared L3 identity gate must NOT run
        // for bots — mirrors is_denied_user's !is_bot bypass. Otherwise trusted /
        // mode-admitted bots would be denied when allow_all_users=false.
        assert!(!l3_gate_applies(true)); // bot → gate skipped
        assert!(l3_gate_applies(false)); // human → gate applies
    }

    // --- Trusted bot mention bypass tests ---
    // A trusted bot @mentioning this bot bypasses allow_bot_messages mode,
    // treating the mention the same as a human @mention.

    /// GIVEN: trusted bot @mentions this bot
    /// THEN:  bypass is granted (treated as human mention)
    #[test]
    fn trusted_bot_mention_bypasses_gate() {
        let trusted = HashSet::from([42]);
        assert!(is_trusted_bot_mention(true, &trusted, 42));
    }

    /// GIVEN: untrusted bot @mentions this bot
    /// THEN:  no bypass (normal bot gating applies)
    #[test]
    fn untrusted_bot_mention_no_bypass() {
        let trusted = HashSet::from([42]);
        assert!(!is_trusted_bot_mention(true, &trusted, 99));
    }

    /// GIVEN: trusted bot sends message WITHOUT @mention
    /// THEN:  no bypass (must explicitly @mention)
    #[test]
    fn trusted_bot_no_mention_no_bypass() {
        let trusted = HashSet::from([42]);
        assert!(!is_trusted_bot_mention(false, &trusted, 42));
    }

    /// GIVEN: empty trusted_bot_ids (feature not configured)
    /// THEN:  no bypass regardless of mention
    #[test]
    fn empty_trusted_ids_no_bypass() {
        let trusted: HashSet<u64> = HashSet::new();
        assert!(!is_trusted_bot_mention(true, &trusted, 42));
    }

    // --- Trusted bot admission integration tests ---
    // These test the full bot gating decision path: allow_bot_messages mode +
    // trusted_bot_ids + trusted mention bypass, mirroring the actual logic in
    // EventHandler::message.

    /// Simulates the bot admission decision from EventHandler::message.
    /// Returns `true` if the bot message would be processed (not dropped).
    fn should_admit_bot_message(
        allow_bot_messages: AllowBots,
        is_mentioned: bool,
        trusted_bot_ids: &HashSet<u64>,
        author_id: u64,
    ) -> bool {
        let trusted_mention =
            is_mentioned && !trusted_bot_ids.is_empty() && trusted_bot_ids.contains(&author_id);

        if !trusted_mention {
            match allow_bot_messages {
                AllowBots::Off => return false,
                AllowBots::Mentions => {
                    if !is_mentioned {
                        return false;
                    }
                }
                AllowBots::All => {} // would check consecutive cap, skip for unit test
            }

            if !trusted_bot_ids.is_empty() && !trusted_bot_ids.contains(&author_id) {
                return false;
            }
        }
        true
    }

    /// GIVEN: allow_bot_messages=Off, trusted bot @mentions this bot
    /// THEN:  admitted (trusted mention overrides Off mode)
    #[test]
    fn bot_admission_trusted_mention_overrides_off() {
        let trusted = HashSet::from([42]);
        assert!(should_admit_bot_message(AllowBots::Off, true, &trusted, 42));
    }

    /// GIVEN: allow_bot_messages=Off, untrusted bot @mentions this bot
    /// THEN:  rejected (Off mode blocks)
    #[test]
    fn bot_admission_untrusted_mention_blocked_by_off() {
        let trusted = HashSet::from([42]);
        assert!(!should_admit_bot_message(
            AllowBots::Off,
            true,
            &trusted,
            99
        ));
    }

    /// GIVEN: allow_bot_messages=Off, trusted bot without @mention
    /// THEN:  rejected (no mention = no bypass)
    #[test]
    fn bot_admission_trusted_no_mention_blocked_by_off() {
        let trusted = HashSet::from([42]);
        assert!(!should_admit_bot_message(
            AllowBots::Off,
            false,
            &trusted,
            42
        ));
    }

    /// GIVEN: allow_bot_messages=Off, empty trusted_bot_ids, bot @mentions
    /// THEN:  rejected (feature not configured)
    #[test]
    fn bot_admission_empty_trusted_ids_off_mode() {
        let trusted: HashSet<u64> = HashSet::new();
        assert!(!should_admit_bot_message(
            AllowBots::Off,
            true,
            &trusted,
            42
        ));
    }

    /// GIVEN: allow_bot_messages=Mentions, trusted bot @mentions
    /// THEN:  admitted (would pass anyway, but bypass also works)
    #[test]
    fn bot_admission_mentions_mode_trusted_mention() {
        let trusted = HashSet::from([42]);
        assert!(should_admit_bot_message(
            AllowBots::Mentions,
            true,
            &trusted,
            42
        ));
    }

    /// GIVEN: allow_bot_messages=All, untrusted bot (not in trusted_bot_ids)
    /// THEN:  rejected by trusted_bot_ids filter
    #[test]
    fn bot_admission_all_mode_untrusted_bot_rejected() {
        let trusted = HashSet::from([42]);
        assert!(!should_admit_bot_message(
            AllowBots::All,
            false,
            &trusted,
            99
        ));
    }

    // --- DM gating tests (#656) ---
    // DMs are gated by `allow_dm` config. When allowed, DMs bypass
    // `allowed_channels` and treat the message as implicit @mention.

    /// GIVEN: allow_dm = false
    /// WHEN:  user sends a DM
    /// THEN:  DM is rejected
    #[test]
    fn dm_rejected_when_allow_dm_false() {
        assert!(!should_process_dm(false));
    }

    /// GIVEN: allow_dm = true
    /// WHEN:  user sends a DM
    /// THEN:  DM is accepted
    #[test]
    fn dm_accepted_when_allow_dm_true() {
        assert!(should_process_dm(true));
    }

    /// GIVEN: allow_dm = true, user NOT in allowed_users
    /// WHEN:  user sends a DM
    /// THEN:  user is denied (allowed_users still enforced in DMs)
    #[test]
    fn dm_denied_user_still_enforced() {
        let allowed = HashSet::from([100]);
        // DM passes allow_dm gate, but user gate still applies
        assert!(should_process_dm(true));
        assert!(is_denied_user(false, false, &allowed, 999));
    }

    /// GIVEN: allow_dm = true, user in allowed_users
    /// WHEN:  user sends a DM
    /// THEN:  user is allowed
    #[test]
    fn dm_allowed_user_passes() {
        let allowed = HashSet::from([100]);
        assert!(should_process_dm(true));
        assert!(!is_denied_user(false, false, &allowed, 100));
    }

    /// DMs are treated as implicit @mention — should_process_user_message
    /// is never called for DMs (the `!is_dm` guard skips it).
    /// This test verifies the Involved mode would reject a non-thread,
    /// non-mentioned message — confirming DMs MUST bypass this check.
    #[test]
    fn dm_must_bypass_user_message_gating() {
        // Without the `!is_dm` bypass, a DM would be rejected by Involved mode
        // because is_mentioned=false and in_thread=false.
        assert!(!should_process_user_message(
            AllowUsers::Involved,
            false, // is_mentioned (DMs don't have @mention)
            false, // in_thread (DMs are not threads)
            false, // involved
            false, // other_bot_present
        ));
    }

    // --- Thread creation skip tests (regression for #656 DM bug) ---
    // Pins the invariant: DMs must never call get_or_create_thread().
    // Discord DM channels do not support thread creation.

    /// GIVEN: is_dm = true, not in a thread
    /// THEN:  skip thread creation (use DM channel directly)
    #[test]
    fn dm_skips_thread_creation() {
        assert!(should_skip_thread_creation(false, true));
    }

    /// GIVEN: already in a thread, not a DM
    /// THEN:  skip thread creation (reuse existing thread)
    #[test]
    fn existing_thread_skips_thread_creation() {
        assert!(should_skip_thread_creation(true, false));
    }

    /// GIVEN: not in a thread, not a DM (normal channel message)
    /// THEN:  do NOT skip — create a new thread
    #[test]
    fn normal_channel_creates_thread() {
        assert!(!should_skip_thread_creation(false, false));
    }

    // --- WarnAndStop dedup tests (#530) ---

    #[test]
    fn dedup_detects_existing_bot_warning() {
        let msg = format!(
            "{} (20/20). A human must reply.",
            BOT_TURN_LIMIT_WARNING_PREFIX
        );
        assert!(turn_limit_warning_present(&[(true, &msg)]));
    }

    #[test]
    fn dedup_ignores_human_warning_text() {
        let msg = format!(
            "{} (20/20). A human must reply.",
            BOT_TURN_LIMIT_WARNING_PREFIX
        );
        assert!(!turn_limit_warning_present(&[(false, &msg)]));
    }

    #[test]
    fn dedup_returns_false_when_no_warning() {
        assert!(!turn_limit_warning_present(&[
            (true, "hello"),
            (false, "world")
        ]));
    }

    #[test]
    fn dedup_returns_false_for_empty_messages() {
        assert!(!turn_limit_warning_present(&[]));
    }

    // --- should_process_reaction tests ---
    // Pins the reaction gating logic to prevent regressions (F1/F2/F3 review cycle).

    /// GIVEN: Mentions mode (reactions cannot @mention)
    /// THEN:  always rejected
    #[test]
    fn reaction_mentions_mode_always_rejected() {
        assert!(!should_process_reaction(
            AllowUsers::Mentions,
            true,
            true,
            false,
            false,
        ));
    }

    /// GIVEN: Involved mode, non-thread channel
    /// THEN:  rejected (participation check never runs for non-threads)
    #[test]
    fn reaction_involved_non_thread_rejected() {
        assert!(!should_process_reaction(
            AllowUsers::Involved,
            false, // is_thread
            false, // bot_involved (irrelevant for non-thread)
            false,
            false,
        ));
    }

    /// GIVEN: Involved mode, thread, bot NOT involved
    /// THEN:  rejected
    #[test]
    fn reaction_involved_thread_not_participated_rejected() {
        assert!(!should_process_reaction(
            AllowUsers::Involved,
            true,  // is_thread
            false, // bot_involved
            false,
            false,
        ));
    }

    /// GIVEN: Involved mode, thread, bot IS involved
    /// THEN:  accepted
    #[test]
    fn reaction_involved_thread_participated_accepted() {
        assert!(should_process_reaction(
            AllowUsers::Involved,
            true, // is_thread
            true, // bot_involved
            false,
            false,
        ));
    }

    /// GIVEN: MultibotMentions mode, single-bot thread, bot involved
    /// THEN:  accepted (no multibot contention)
    #[test]
    fn reaction_multibot_single_bot_thread_accepted() {
        assert!(should_process_reaction(
            AllowUsers::MultibotMentions,
            true,  // is_thread
            true,  // bot_involved
            false, // other_bot_present
            false, // targets_this_bot (irrelevant when no other bot)
        ));
    }

    /// GIVEN: MultibotMentions mode, multi-bot thread, reaction targets THIS bot's message
    /// THEN:  accepted
    #[test]
    fn reaction_multibot_targets_this_bot_accepted() {
        assert!(should_process_reaction(
            AllowUsers::MultibotMentions,
            true, // is_thread
            true, // bot_involved
            true, // other_bot_present
            true, // targets_this_bot
        ));
    }

    /// GIVEN: MultibotMentions mode, multi-bot thread, reaction targets OTHER bot's message
    /// THEN:  rejected
    #[test]
    fn reaction_multibot_targets_other_bot_rejected() {
        assert!(!should_process_reaction(
            AllowUsers::MultibotMentions,
            true,  // is_thread
            true,  // bot_involved
            true,  // other_bot_present
            false, // targets_this_bot
        ));
    }

    /// GIVEN: MultibotMentions mode, non-thread channel
    /// THEN:  rejected
    #[test]
    fn reaction_multibot_non_thread_rejected() {
        assert!(!should_process_reaction(
            AllowUsers::MultibotMentions,
            false, // is_thread
            false,
            false,
            false,
        ));
    }
}
