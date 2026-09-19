//! Opening a thread and starting an agent turn in it, without a human message
//! to hang it off.
//!
//! Two callers need this: the cron scheduler, and incident triage. Keeping one
//! implementation matters more than the line count saved — the pieces that are
//! easy to get subtly wrong (the synthetic `SenderContext` shape, the
//! parent-vs-thread id distinction, trigger truncation) are exactly the ones
//! that would drift apart in two copies.
//!
//! Split in two on purpose: callers need to do their own bookkeeping with the
//! newly created thread — cron persists a sticky mapping, triage records the
//! thread so failures inside it cannot recurse — before the turn is submitted.

use std::sync::Arc;

use chrono::Utc;
use tracing::error;

use crate::adapter::{ChannelRef, ChatAdapter, MessageRef, SenderContext};
use crate::format;

/// Post the visible trigger message, and open a thread under it when asked.
///
/// Returns the channel the agent should reply into, plus the trigger message
/// itself (the router needs it to anchor streaming edits).
pub async fn open_trigger_thread(
    adapter: &Arc<dyn ChatAdapter>,
    channel: &ChannelRef,
    prefix: &str,
    trigger_text: &str,
    thread_name: Option<&str>,
) -> anyhow::Result<(ChannelRef, MessageRef)> {
    let max_limit = adapter.message_limit();
    let allowed_body_len = max_limit.saturating_sub(prefix.chars().count());
    let trigger_body = format::truncate_chars_head(trigger_text, allowed_body_len);
    let trigger_msg = adapter
        .send_message(channel, &format!("{prefix}{trigger_body}"))
        .await
        .map_err(|error| anyhow::anyhow!("failed to send trigger message: {error}"))?;

    let Some(thread_name) = thread_name else {
        return Ok((channel.clone(), trigger_msg));
    };
    match adapter.create_thread(channel, &trigger_msg, thread_name).await {
        Ok(reply_channel) => Ok((reply_channel, trigger_msg)),
        Err(error) => Err(anyhow::anyhow!("failed to create thread: {error}")),
    }
}

/// The thread identifier to put on a synthetic sender: an explicit thread id
/// when there is one, otherwise the channel id once we know we are inside a
/// thread (because a parent exists).
pub fn sender_thread_id(channel: &ChannelRef) -> Option<String> {
    channel
        .thread_id
        .clone()
        .or_else(|| channel.parent_id.as_ref().map(|_| channel.channel_id.clone()))
}

/// Drive one agent turn in `reply_channel` on behalf of a non-human sender.
pub async fn submit_agent_turn(
    adapter: &Arc<dyn ChatAdapter>,
    router: &Arc<crate::adapter::AdapterRouter>,
    reply_channel: &ChannelRef,
    trigger_msg: MessageRef,
    sender_id: &str,
    sender_name: &str,
    platform: &str,
    prompt: String,
) -> anyhow::Result<()> {
    let sender = SenderContext {
        schema: "openab.sender.v1".into(),
        sender_id: sender_id.to_string(),
        sender_name: sender_name.to_string(),
        display_name: sender_name.to_string(),
        channel: platform.to_string(),
        channel_id: reply_channel
            .parent_id
            .as_deref()
            .unwrap_or(&reply_channel.channel_id)
            .to_string(),
        thread_id: sender_thread_id(reply_channel),
        is_bot: true,
        timestamp: Some(Utc::now().to_rfc3339()),
        // Self-triggered: no originating message, no external receiver.
        message_id: None,
        receiver_id: None,
        output_instructions: None,
    };
    let sender_json = serde_json::to_string(&sender)
        .map_err(|error| anyhow::anyhow!("failed to serialize sender context: {error}"))?;

    router
        .handle_message(
            adapter,
            crate::adapter::MessageContext {
                thread_channel: reply_channel.clone(),
                sender_json,
                prompt,
                extra_blocks: vec![],
                trigger_msg,
                other_bot_present: false,
            },
        )
        .await
        .map_err(|error| {
            error!("thread session handle_message error: {error}");
            anyhow::anyhow!("{error}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(channel_id: &str, thread_id: Option<&str>, parent_id: Option<&str>) -> ChannelRef {
        ChannelRef {
            platform: "discord".into(),
            channel_id: channel_id.into(),
            thread_id: thread_id.map(str::to_string),
            parent_id: parent_id.map(str::to_string),
            origin_event_id: None,
        }
    }

    #[test]
    fn googlechat_top_level_sender_has_no_thread_id() {
        assert_eq!(sender_thread_id(&channel("spaces/TEST", None, None)), None);
    }

    #[test]
    fn an_explicit_thread_id_is_preserved() {
        assert_eq!(
            sender_thread_id(&channel(
                "spaces/TEST",
                Some("spaces/TEST/threads/THREAD"),
                None
            ))
            .as_deref(),
            Some("spaces/TEST/threads/THREAD")
        );
    }

    #[test]
    fn thread_platforms_fall_back_to_the_child_channel_id() {
        // Inside a Discord thread the thread *is* the channel, and the parent
        // holds the enclosing channel — the same distinction incident triage
        // relies on to recognise its own threads.
        assert_eq!(
            sender_thread_id(&channel("thread-456", None, Some("channel-123"))).as_deref(),
            Some("thread-456")
        );
    }
}
