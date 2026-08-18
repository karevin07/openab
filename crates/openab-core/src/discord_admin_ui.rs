//! Discord UI for the isolated admin control plane.
//!
//! Split out of `discord.rs`: every card, modal and select menu for the admin
//! flows, plus the interaction handlers that drive them. The control-plane
//! client and its wire types live in [`crate::discord_admin`]; this module is
//! only the Discord-facing surface on top of them.
//!
//! The handlers are `impl Handler` blocks rather than free functions because
//! they need the same three fields the rest of the Discord adapter uses for
//! authorization (`admin_control`, `allow_all_users`, `allowed_users`) — Rust
//! lets the impl live in whichever module owns the behaviour.

use crate::discord::{
    first_role_select, first_string_select, inline_code, is_denied_user, modal_input_value,
    suppress_mentions, truncate_for_discord, Handler, InteractionCard, SELECT_MENU_PAGE_SIZE,
    SELECT_OPTION_TEXT_MAX,
};
use crate::discord_admin::{
    AdminInventory, AdminStatus, CategoryPlan, ChannelPlan, CleanupCandidates, CreatedCategory,
    CreatedTextChannel, DeletedResource, DeletionPlan, MutationOutcome, MutationPlan,
};
use serenity::builder::{
    CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInputText,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateModal, CreateSelectMenu,
    CreateSelectMenuKind, CreateSelectMenuOption,
};
use serenity::model::application::{ButtonStyle, ComponentInteractionDataKind, InputTextStyle};
use serenity::prelude::*;

pub(crate) fn admin_navigation_buttons() -> Vec<CreateActionRow> {
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
            CreateButton::new("oab_admin:rename")
                .label("✏ Rename")
                .style(ButtonStyle::Secondary),
            CreateButton::new("oab_admin:move")
                .label("↕ Move")
                .style(ButtonStyle::Secondary),
            CreateButton::new("oab_admin:permissions")
                .label("🔐 Permissions")
                .style(ButtonStyle::Secondary),
            CreateButton::new("oab_admin:structure")
                .label("📐 Structure")
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
                "Phase A-D: inspect, provision, rename/move, permissions, structure, cleanup",
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

pub(crate) fn admin_channel_category_card(inventory: &AdminInventory) -> InteractionCard {
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

pub(crate) fn admin_channel_modal(category_id: u64) -> CreateModal {
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

pub(crate) fn admin_cleanup_card(cleanup: &CleanupCandidates) -> InteractionCard {
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

const ADMIN_PERMISSION_TEMPLATES: &[(&str, &str)] = &[
    ("inherit", "Inherit category permissions"),
    ("public", "Everyone can read and write"),
    ("announcement", "Everyone reads; owner posts"),
    ("private-project", "Selected role plus owner"),
    ("admin-only", "Owner and Admin Bot only"),
];

const ADMIN_STRUCTURE_BLUEPRINTS: &[(&str, &str)] = &[
    ("openab", "OpenAB projects and operations"),
    ("development", "Compact development workspace"),
    ("community", "Announcements and feedback"),
];

fn admin_text_channel_options(
    inventory: &AdminInventory,
) -> Vec<(String, String, String)> {
    let mut options = Vec::new();
    for category in &inventory.categories {
        for channel in &category.channels {
            if channel.kind != "text" {
                continue;
            }
            options.push((
                channel.id.clone(),
                format!("#{}", channel.name),
                category.name.clone(),
            ));
        }
    }
    for channel in &inventory.uncategorized {
        if channel.kind == "text" {
            options.push((
                channel.id.clone(),
                format!("#{}", channel.name),
                "Uncategorized".into(),
            ));
        }
    }
    options
}

fn admin_permission_channel_options(
    inventory: &AdminInventory,
) -> Vec<(String, String, String)> {
    let mut options = Vec::new();
    for category in &inventory.categories {
        for channel in &category.channels {
            if channel.kind != "text" && channel.kind != "forum" {
                continue;
            }
            options.push((
                channel.id.clone(),
                format!("#{}", channel.name),
                format!("{} · {}", category.name, channel.kind),
            ));
        }
    }
    options
}

fn admin_rename_target_options(
    inventory: &AdminInventory,
) -> Vec<(String, String, String)> {
    let mut options = Vec::new();
    for category in &inventory.categories {
        options.push((
            format!("category:{}", category.id),
            format!("🗂️ {}", category.name),
            "category".into(),
        ));
        for channel in &category.channels {
            if channel.kind != "text" && channel.kind != "forum" {
                continue;
            }
            options.push((
                format!("{}:{}", if channel.kind == "forum" { "forum" } else { "text_channel" }, channel.id),
                format!("#{}", channel.name),
                format!("{} · {}", category.name, channel.kind),
            ));
        }
    }
    options
}

fn admin_select_row(
    custom_id: impl Into<String>,
    placeholder: &str,
    options: Vec<(String, String, String)>,
) -> Option<CreateActionRow> {
    if options.is_empty() {
        return None;
    }
    let menu_options = options
        .into_iter()
        .take(SELECT_MENU_PAGE_SIZE)
        .map(|(value, label, description)| {
            CreateSelectMenuOption::new(
                truncate_for_discord(&suppress_mentions(&label), SELECT_OPTION_TEXT_MAX),
                value,
            )
            .description(truncate_for_discord(
                &suppress_mentions(&description),
                SELECT_OPTION_TEXT_MAX,
            ))
        })
        .collect();
    Some(CreateActionRow::SelectMenu(
        CreateSelectMenu::new(custom_id, CreateSelectMenuKind::String { options: menu_options })
            .placeholder(placeholder.to_string()),
    ))
}

fn admin_rename_card(inventory: &AdminInventory) -> InteractionCard {
    let options = admin_rename_target_options(inventory);
    let mut embed = CreateEmbed::new()
        .title("✏ Rename")
        .description("Choose a category, text channel, or Forum. A form asks for the new name; nothing changes before confirmation.")
        .colour(0x5865F2);
    let mut components = Vec::new();
    if let Some(row) = admin_select_row("oab_admin_rename_target", "Select something to rename", options) {
        components.push(row);
    } else {
        embed = embed.field("Nothing to rename", "No categories or text/Forum channels were found.", false);
    }
    components.extend(admin_navigation_buttons());
    InteractionCard {
        content: String::new(),
        embed,
        components,
    }
}

fn admin_rename_modal(target_type: &str, target_id: u64) -> CreateModal {
    CreateModal::new(
        format!("oab_admin_rename:{target_type}:{target_id}"),
        "Preview rename",
    )
    .components(vec![CreateActionRow::InputText(
        CreateInputText::new(InputTextStyle::Short, "New name", "name")
            .min_length(1)
            .max_length(100),
    )])
}

fn admin_move_channel_card(inventory: &AdminInventory) -> InteractionCard {
    let options = admin_text_channel_options(inventory);
    let mut embed = CreateEmbed::new()
        .title("↕ Move channel")
        .description("Choose a text channel, then the destination category. Forum channels stay where they are.")
        .colour(0x5865F2);
    let mut components = Vec::new();
    if let Some(row) = admin_select_row("oab_admin_move_channel", "Select a text channel", options) {
        components.push(row);
    } else {
        embed = embed.field("No text channels", "Create a text channel first.", false);
    }
    components.extend(admin_navigation_buttons());
    InteractionCard {
        content: String::new(),
        embed,
        components,
    }
}

fn admin_move_category_card(inventory: &AdminInventory, channel_id: u64) -> InteractionCard {
    let options = inventory
        .categories
        .iter()
        .map(|category| {
            (
                category.id.clone(),
                format!("🗂️ {}", category.name),
                format!("{} channel(s)", category.channels.len()),
            )
        })
        .collect();
    let mut embed = CreateEmbed::new()
        .title("↕ Choose destination category")
        .description("The channel inherits the destination category's permissions unless it has custom overwrites.")
        .colour(0x5865F2);
    let mut components = Vec::new();
    if let Some(row) = admin_select_row(
        format!("oab_admin_move_category:{channel_id}"),
        "Select destination category",
        options,
    ) {
        components.push(row);
    } else {
        embed = embed.field("No categories", "Create a category first.", false);
    }
    components.extend(admin_navigation_buttons());
    InteractionCard {
        content: String::new(),
        embed,
        components,
    }
}

fn admin_permission_channel_card(inventory: &AdminInventory) -> InteractionCard {
    let options = admin_permission_channel_options(inventory);
    let mut embed = CreateEmbed::new()
        .title("🔐 Permission template")
        .description("Choose a text or Forum channel, then a template. `private-project` also asks for an access role. This replaces current overwrites.")
        .colour(0x5865F2);
    let mut components = Vec::new();
    if let Some(row) = admin_select_row("oab_admin_perm_channel", "Select a channel", options) {
        components.push(row);
    } else {
        embed = embed.field("No channels", "Create a text or Forum channel first.", false);
    }
    components.extend(admin_navigation_buttons());
    InteractionCard {
        content: String::new(),
        embed,
        components,
    }
}

fn admin_permission_template_card(channel_id: u64) -> InteractionCard {
    let options = ADMIN_PERMISSION_TEMPLATES
        .iter()
        .map(|(value, description)| ((*value).to_string(), (*value).to_string(), (*description).to_string()))
        .collect();
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("🔐 Choose permission template")
            .description("Existing channel overwrites will be replaced after you confirm.")
            .colour(0x5865F2),
        components: {
            let mut components = Vec::new();
            if let Some(row) = admin_select_row(
                format!("oab_admin_perm_template:{channel_id}"),
                "Select a template",
                options,
            ) {
                components.push(row);
            }
            components.extend(admin_navigation_buttons());
            components
        },
    }
}

fn admin_permission_role_card(channel_id: u64, template: &str) -> InteractionCard {
    let mut components = vec![CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            format!("oab_admin_perm_role:{channel_id}:{template}"),
            CreateSelectMenuKind::Role { default_roles: None },
        )
        .placeholder("Select access role"),
    )];
    components.extend(admin_navigation_buttons());
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("🔐 Select access role")
            .description("`private-project` needs a non-@everyone role that can see this channel.")
            .colour(0x5865F2),
        components,
    }
}

fn admin_structure_blueprint_card() -> InteractionCard {
    let options = ADMIN_STRUCTURE_BLUEPRINTS
        .iter()
        .map(|(value, description)| {
            ((*value).to_string(), (*value).to_string(), (*description).to_string())
        })
        .collect();
    let mut components = Vec::new();
    if let Some(row) = admin_select_row("oab_admin_struct_blueprint", "Select a structure", options)
    {
        components.push(row);
    }
    components.extend(admin_navigation_buttons());
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("📐 Apply structure")
            .description("Creates missing categories and channels from a built-in blueprint. Existing same-name items are skipped, never overwritten.")
            .colour(0x5865F2),
        components,
    }
}

fn admin_structure_template_card(blueprint: &str) -> InteractionCard {
    let options = ADMIN_PERMISSION_TEMPLATES
        .iter()
        .map(|(value, description)| {
            ((*value).to_string(), (*value).to_string(), (*description).to_string())
        })
        .collect();
    let mut components = Vec::new();
    if let Some(row) = admin_select_row(
        format!("oab_admin_struct_template:{blueprint}"),
        "Select permissions for new channels",
        options,
    ) {
        components.push(row);
    }
    components.extend(admin_navigation_buttons());
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("📐 Structure permissions")
            .description(format!("Blueprint `{}`. New channels receive this template; existing channels keep their current overwrites.", blueprint))
            .colour(0x5865F2),
        components,
    }
}

fn admin_structure_role_card(blueprint: &str, template: &str) -> InteractionCard {
    let mut components = vec![CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            format!("oab_admin_struct_role:{blueprint}:{template}"),
            CreateSelectMenuKind::Role { default_roles: None },
        )
        .placeholder("Select access role"),
    )];
    components.extend(admin_navigation_buttons());
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("📐 Select access role")
            .description("`private-project` needs a non-@everyone role for newly created channels.")
            .colour(0x5865F2),
        components,
    }
}

fn admin_mutation_preview_card(plan: &MutationPlan, confirm_prefix: &str) -> InteractionCard {
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("⚠️ Confirm server change")
            .description(truncate_for_discord(&plan.summary, 3500))
            .colour(0xF1C40F)
            .field("Operation", inline_code(&plan.operation), false)
            .footer(CreateEmbedFooter::new(format!(
                "This preview expires in {} seconds",
                plan.expires_in_seconds
            ))),
        components: vec![CreateActionRow::Buttons(vec![
            CreateButton::new(format!("oab_admin:{confirm_prefix}:{}", plan.id))
                .label("Confirm")
                .style(ButtonStyle::Success),
            CreateButton::new("oab_admin:cancel")
                .label("Cancel")
                .style(ButtonStyle::Secondary),
        ])],
    }
}

fn admin_mutation_done_card(result: &MutationOutcome) -> InteractionCard {
    InteractionCard {
        content: String::new(),
        embed: CreateEmbed::new()
            .title("✅ Server change applied")
            .description(truncate_for_discord(&result.summary, 3500))
            .colour(0x2ECC71)
            .field("Operation", inline_code(&result.operation), false),
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

impl Handler {
    pub(crate) async fn handle_admin_component(
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
        if comp.data.custom_id == "oab_admin_rename_target" {
            let Some((target_type, target_id)) = first_string_select(&comp.data.kind)
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
                        .content("⚠️ This rename selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if let Err(error) = comp
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Modal(admin_rename_modal(target_type, target_id)),
                )
                .await
            {
                tracing::error!(%error, "failed to open admin rename modal");
            }
            return;
        }
        if comp.data.custom_id == "oab_admin_move_channel" {
            let Some(channel_id) = first_string_select(&comp.data.kind)
                .and_then(|value| value.parse::<u64>().ok())
            else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This channel selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer move channel selection");
                return;
            }
            let card = match client.inventory(comp.user.id.get(), guild_id).await {
                Ok(inventory) => admin_move_category_card(&inventory, channel_id),
                Err(error) => admin_error_card(&error),
            };
            let _ = comp.edit_response(&ctx.http, card.into_edit()).await;
            return;
        }
        if let Some(channel_id) = comp
            .data
            .custom_id
            .strip_prefix("oab_admin_move_category:")
            .and_then(|value| value.parse::<u64>().ok())
        {
            let Some(category_id) = first_string_select(&comp.data.kind)
                .and_then(|value| value.parse::<u64>().ok())
            else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This category selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer move preview");
                return;
            }
            let card = match client
                .preview_move(comp.user.id.get(), guild_id, channel_id, category_id)
                .await
            {
                Ok(preview) => admin_mutation_preview_card(&preview.plan, "confirm_move"),
                Err(error) => admin_error_card(&error),
            };
            let _ = comp.edit_response(&ctx.http, card.into_edit()).await;
            return;
        }
        if comp.data.custom_id == "oab_admin_perm_channel" {
            let Some(channel_id) = first_string_select(&comp.data.kind)
                .and_then(|value| value.parse::<u64>().ok())
            else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This channel selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            let response = CreateInteractionResponse::UpdateMessage(
                admin_permission_template_card(channel_id).into_message(),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        if let Some(channel_id) = comp
            .data
            .custom_id
            .strip_prefix("oab_admin_perm_template:")
            .and_then(|value| value.parse::<u64>().ok())
        {
            let Some(template) = first_string_select(&comp.data.kind).map(str::to_owned) else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This template selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if template == "private-project" {
                let response = CreateInteractionResponse::UpdateMessage(
                    admin_permission_role_card(channel_id, &template).into_message(),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            }
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer permission preview");
                return;
            }
            let card = match client
                .preview_permission(comp.user.id.get(), guild_id, channel_id, &template, None)
                .await
            {
                Ok(preview) => admin_mutation_preview_card(&preview.plan, "confirm_perm"),
                Err(error) => admin_error_card(&error),
            };
            let _ = comp.edit_response(&ctx.http, card.into_edit()).await;
            return;
        }
        if let Some(rest) = comp.data.custom_id.strip_prefix("oab_admin_perm_role:") {
            let mut parts = rest.splitn(2, ':');
            let channel_id = parts.next().and_then(|value| value.parse::<u64>().ok());
            let template = parts.next().map(str::to_owned);
            let role_id = first_role_select(&comp.data.kind);
            let (Some(channel_id), Some(template), Some(role_id)) = (channel_id, template, role_id)
            else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This role selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer permission role preview");
                return;
            }
            let card = match client
                .preview_permission(
                    comp.user.id.get(),
                    guild_id,
                    channel_id,
                    &template,
                    Some(role_id),
                )
                .await
            {
                Ok(preview) => admin_mutation_preview_card(&preview.plan, "confirm_perm"),
                Err(error) => admin_error_card(&error),
            };
            let _ = comp.edit_response(&ctx.http, card.into_edit()).await;
            return;
        }
        if comp.data.custom_id == "oab_admin_struct_blueprint" {
            let Some(blueprint) = first_string_select(&comp.data.kind).map(str::to_owned) else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This structure selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            let response = CreateInteractionResponse::UpdateMessage(
                admin_structure_template_card(&blueprint).into_message(),
            );
            let _ = comp.create_response(&ctx.http, response).await;
            return;
        }
        if let Some(blueprint) = comp
            .data
            .custom_id
            .strip_prefix("oab_admin_struct_template:")
            .map(str::to_owned)
        {
            let Some(template) = first_string_select(&comp.data.kind).map(str::to_owned) else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This template selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if template == "private-project" {
                let response = CreateInteractionResponse::UpdateMessage(
                    admin_structure_role_card(&blueprint, &template).into_message(),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            }
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer structure preview");
                return;
            }
            let card = match client
                .preview_structure(comp.user.id.get(), guild_id, &blueprint, &template, None)
                .await
            {
                Ok(preview) => admin_mutation_preview_card(&preview.plan, "confirm_struct"),
                Err(error) => admin_error_card(&error),
            };
            let _ = comp.edit_response(&ctx.http, card.into_edit()).await;
            return;
        }
        if let Some(rest) = comp.data.custom_id.strip_prefix("oab_admin_struct_role:") {
            let mut parts = rest.splitn(2, ':');
            let blueprint = parts.next().map(str::to_owned);
            let template = parts.next().map(str::to_owned);
            let role_id = first_role_select(&comp.data.kind);
            let (Some(blueprint), Some(template), Some(role_id)) = (blueprint, template, role_id)
            else {
                let response = CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content("⚠️ This role selection is invalid. Refresh the card.")
                        .ephemeral(true),
                );
                let _ = comp.create_response(&ctx.http, response).await;
                return;
            };
            if let Err(error) = comp.defer(&ctx.http).await {
                tracing::error!(%error, "failed to defer structure role preview");
                return;
            }
            let card = match client
                .preview_structure(
                    comp.user.id.get(),
                    guild_id,
                    &blueprint,
                    &template,
                    Some(role_id),
                )
                .await
            {
                Ok(preview) => admin_mutation_preview_card(&preview.plan, "confirm_struct"),
                Err(error) => admin_error_card(&error),
            };
            let _ = comp.edit_response(&ctx.http, card.into_edit()).await;
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
        } else if action == "rename" {
            client
                .inventory(comp.user.id.get(), guild_id)
                .await
                .map(|inventory| admin_rename_card(&inventory))
        } else if action == "move" {
            client
                .inventory(comp.user.id.get(), guild_id)
                .await
                .map(|inventory| admin_move_channel_card(&inventory))
        } else if action == "permissions" {
            client
                .inventory(comp.user.id.get(), guild_id)
                .await
                .map(|inventory| admin_permission_channel_card(&inventory))
        } else if action == "structure" {
            Ok(admin_structure_blueprint_card())
        } else if let Some(plan_id) = action.strip_prefix("confirm_rename:") {
            client
                .apply_rename(comp.user.id.get(), guild_id, plan_id)
                .await
                .map(|result| admin_mutation_done_card(&result.result))
        } else if let Some(plan_id) = action.strip_prefix("confirm_move:") {
            client
                .apply_move(comp.user.id.get(), guild_id, plan_id)
                .await
                .map(|result| admin_mutation_done_card(&result.result))
        } else if let Some(plan_id) = action.strip_prefix("confirm_perm:") {
            client
                .apply_permission(comp.user.id.get(), guild_id, plan_id)
                .await
                .map(|result| admin_mutation_done_card(&result.result))
        } else if let Some(plan_id) = action.strip_prefix("confirm_struct:") {
            client
                .apply_structure(comp.user.id.get(), guild_id, plan_id)
                .await
                .map(|result| admin_mutation_done_card(&result.result))
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

    pub(crate) async fn handle_admin_category_modal(
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

    pub(crate) async fn handle_admin_channel_modal(
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

    pub(crate) async fn handle_admin_rename_modal(
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
        let Some((target_type, target_id)) = modal
            .data
            .custom_id
            .strip_prefix("oab_admin_rename:")
            .and_then(|value| value.split_once(':'))
            .and_then(|(target_type, target_id)| {
                target_id
                    .parse::<u64>()
                    .ok()
                    .map(|target_id| (target_type.to_string(), target_id))
            })
        else {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("⚠️ This rename selection is invalid. Start Rename again.")
                    .ephemeral(true),
            );
            let _ = modal.create_response(&ctx.http, response).await;
            return;
        };
        let name = modal_input_value(modal, "name")
            .map(str::trim)
            .unwrap_or_default();
        if let Err(error) = modal.defer_ephemeral(&ctx.http).await {
            tracing::error!(%error, "failed to defer admin rename preview");
            return;
        }
        let card = match client
            .preview_rename(
                modal.user.id.get(),
                guild_id,
                &target_type,
                target_id,
                name,
            )
            .await
        {
            Ok(preview) => admin_mutation_preview_card(&preview.plan, "confirm_rename"),
            Err(error) => {
                tracing::warn!(%error, "Discord Admin rename preview failed");
                admin_error_card(&error)
            }
        };
        if let Err(error) = modal.edit_response(&ctx.http, card.into_edit()).await {
            tracing::error!(%error, "failed to show admin rename preview");
        }
    }
}
