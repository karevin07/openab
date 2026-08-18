//! Turn-boundary message batching dispatcher.
//!
//! See ADR: docs/adr/turn-boundary-batching.md for full design rationale.
//!
//! # Invariants
//! - I1: First message after idle has zero added latency.
//! - I2: At most one in-flight ACP turn per thread.
//! - I3: Broker structural fidelity — no merging, splitting, reordering, or
//!   semantic transformation of arrival events.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, info_span, warn};

use crate::acp::ContentBlock;
use crate::adapter::{
    AdapterRouter, ChannelRef, ChatAdapter, MessageRef, TaskLifecycleEvent,
};
use crate::config::ReactionsConfig;
use crate::error_display::format_user_error;
use crate::reactions::StatusReactionController;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// One arrival event buffered for a future ACP turn.
#[derive(Clone)]
pub struct BufferedMessage {
    /// Serialised SenderContext JSON (already built by the platform adapter).
    pub sender_json: String,
    /// Author display name — denormalised from `sender_json` so observability
    /// fields (per-event tracing in `dispatch_batch`) don't pay a JSON parse.
    /// Per ADR §2.3 each arrival event carries its sender name.
    pub sender_name: String,
    /// User-visible prompt text (verbatim, never transformed).
    pub prompt: String,
    /// Attachment blocks (images, STT transcripts) in arrival order.
    pub extra_blocks: Vec<ContentBlock>,
    /// Anchor for reactions (👀 / ❌).
    pub trigger_msg: MessageRef,
    /// Broker receive time — used for `buffer_wait_ms` observability.
    pub arrived_at: Instant,
    /// Rough token estimate for `max_batch_tokens` cap.
    pub estimated_tokens: usize,
    /// Snapshot at submit time. Captured per-message so a batch reflects the
    /// freshest known state; `dispatch_batch` reads `batch.last()`.
    pub other_bot_present: bool,
    /// Slack streaming recipient `(user_id, team_id)` for `chat.startStream`,
    /// captured at message-arrival time (after allow-list) and bound to this turn
    /// — no shared thread cache, so no cross-turn race. Populated for real-user
    /// Slack turns regardless of `assistant_mode`; only *consumed* when assistant
    /// mode's native streaming is active. `None` for non-Slack platforms and
    /// bot-authored turns.
    pub recipient: Option<(String, String)>,
}

/// User-facing snapshot of one message that has not been handed to the ACP
/// agent yet. Queue payloads remain private to the dispatcher; adapters only
/// receive the fields required to render queue-management controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMessage {
    pub id: u64,
    pub sender_id: String,
    pub sender_name: String,
    pub prompt: String,
    pub attachment_count: usize,
    pub waiting_seconds: u64,
    /// True when this request was active during the previous process lifetime
    /// and was conservatively requeued after restart.
    pub recovered_from_active: bool,
}

/// Snapshot of one request that has crossed the queue boundary and is being
/// dispatched to the agent. The prompt is retained only for control-plane UI;
/// the authoritative payload remains in the in-flight consumer frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveMessage {
    pub id: u64,
    pub sender_id: String,
    pub sender_name: String,
    pub prompt: String,
    pub attachment_count: usize,
    /// True when this request was active during a previous process lifetime
    /// and is now being replayed after restart.
    pub recovered_from_active: bool,
}

/// How `thread_key` is built for the dispatcher's per-thread map.
///
/// - `Thread`: one mpsc per thread → all senders in a thread share one batch → one
///   ACP turn per batch (cheaper, but risks silent drop when the agent's single reply
///   forgets to address some senders).
/// - `Lane`: one mpsc per (thread, sender) → each sender batches independently and
///   gets a dedicated ACP turn. Sessions are still shared per-thread; turns serialise
///   through the shared session.
///
/// Derived from `config::MessageProcessingMode` in `main.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchGrouping {
    Thread,
    Lane,
}

/// Error returned by `Dispatcher::submit`.
#[derive(Debug)]
pub enum DispatchError {
    /// The per-thread consumer task has exited unexpectedly.
    ConsumerDead,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConsumerDead => write!(f, "dispatch consumer exited unexpectedly"),
        }
    }
}

impl std::error::Error for DispatchError {}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PendingQueue {
    inner: Mutex<PendingQueueInner>,
}

#[derive(Default)]
struct PendingQueueInner {
    entries: HashMap<u64, BufferedMessage>,
    order: VecDeque<u64>,
    recovered_from_active: HashSet<u64>,
}

#[derive(Clone)]
struct QueuedMessage {
    id: u64,
    message: BufferedMessage,
    recovered_from_active: bool,
}

enum BatchTake {
    Taken(Box<QueuedMessage>),
    TooLarge,
    Missing,
}

impl PendingQueue {
    fn insert(&self, id: u64, message: BufferedMessage, front: bool) {
        self.insert_with_recovery(id, message, front, false);
    }

    fn insert_with_recovery(
        &self,
        id: u64,
        message: BufferedMessage,
        front: bool,
        recovered_from_active: bool,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.insert(id, message);
        if recovered_from_active {
            inner.recovered_from_active.insert(id);
        } else {
            inner.recovered_from_active.remove(&id);
        }
        if front {
            inner.order.push_front(id);
        } else {
            inner.order.push_back(id);
        }
    }

    fn take(&self, id: u64) -> Option<BufferedMessage> {
        let mut inner = self.inner.lock().unwrap();
        let message = inner.entries.remove(&id)?;
        inner.recovered_from_active.remove(&id);
        inner.order.retain(|queued_id| *queued_id != id);
        Some(message)
    }

    fn take_first(&self) -> Option<QueuedMessage> {
        let mut inner = self.inner.lock().unwrap();
        while let Some(id) = inner.order.pop_front() {
            if let Some(message) = inner.entries.remove(&id) {
                let recovered_from_active = inner.recovered_from_active.remove(&id);
                return Some(QueuedMessage {
                    id,
                    message,
                    recovered_from_active,
                });
            }
        }
        None
    }

    fn take_front_for_batch(&self, current_tokens: usize, max_tokens: usize) -> BatchTake {
        let mut inner = self.inner.lock().unwrap();
        let Some(id) = inner.order.front().copied() else {
            return BatchTake::Missing;
        };
        let Some(message) = inner.entries.get(&id) else {
            inner.order.pop_front();
            return BatchTake::Missing;
        };
        if current_tokens.saturating_add(message.estimated_tokens) > max_tokens {
            return BatchTake::TooLarge;
        }
        inner.order.pop_front();
        let message = inner
            .entries
            .remove(&id)
            .expect("pending queue entry disappeared while locked");
        let recovered_from_active = inner.recovered_from_active.remove(&id);
        BatchTake::Taken(Box::new(QueuedMessage {
            id,
            message,
            recovered_from_active,
        }))
    }

    fn list(&self) -> Vec<PendingMessage> {
        let inner = self.inner.lock().unwrap();
        inner
            .order
            .iter()
            .filter_map(|id| {
                inner.entries.get(id).map(|message| PendingMessage {
                    id: *id,
                    sender_id: sender_id_from_json(&message.sender_json),
                    sender_name: message.sender_name.clone(),
                    prompt: message.prompt.clone(),
                    attachment_count: message.extra_blocks.len(),
                    waiting_seconds: message.arrived_at.elapsed().as_secs(),
                    recovered_from_active: inner.recovered_from_active.contains(id),
                })
            })
            .collect()
    }

    fn edit(&self, id: u64, prompt: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(message) = inner.entries.get_mut(&id) else {
            return false;
        };
        message.prompt = prompt.to_string();
        message.estimated_tokens = estimate_tokens(prompt, &message.extra_blocks);
        true
    }

    fn remove(&self, id: u64) -> bool {
        self.take(id).is_some()
    }

    fn move_to_front(&self, id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if !inner.entries.contains_key(&id) {
            return false;
        }
        inner.order.retain(|queued_id| *queued_id != id);
        inner.order.push_front(id);
        true
    }

    fn clear_through(&self, max_id: u64) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let ids = inner
            .order
            .iter()
            .copied()
            .filter(|id| *id <= max_id)
            .collect::<Vec<_>>();
        for id in &ids {
            inner.entries.remove(id);
            inner.recovered_from_active.remove(id);
        }
        inner.order.retain(|id| *id > max_id);
        ids.len()
    }

    fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    fn snapshot(&self) -> Vec<(QueuedMessage, bool)> {
        let inner = self.inner.lock().unwrap();
        inner
            .order
            .iter()
            .filter_map(|id| {
                inner.entries.get(id).map(|message| {
                    (
                        QueuedMessage {
                            id: *id,
                            message: message.clone(),
                            recovered_from_active: inner.recovered_from_active.contains(id),
                        },
                        inner.recovered_from_active.contains(id),
                    )
                })
            })
            .collect()
    }
}

fn sender_id_from_json(sender_json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(sender_json)
        .ok()
        .and_then(|value| value.get("sender_id")?.as_str().map(str::to_string))
        .unwrap_or_default()
}

const QUEUE_STORE_VERSION: u32 = 1;

/// Aggregate persisted work waiting for one logical platform thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedQueueSummary {
    pub platform: String,
    pub thread_id: String,
    pub queued_messages: usize,
    pub recovered_active_messages: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedQueueFile {
    version: u32,
    next_message_id: u64,
    #[serde(default)]
    lanes: Vec<PersistedLane>,
}

/// Borrowed mirror of [`PersistedQueueFile`] used only for writing. Serialising
/// from references keeps a snapshot from deep-copying every lane — and every
/// base64 image block inside it — on each queue mutation. Field names and order
/// must stay identical to `PersistedQueueFile` so the on-disk shape is unchanged.
#[derive(Serialize)]
struct PersistedQueueFileRef<'a> {
    version: u32,
    next_message_id: u64,
    lanes: Vec<&'a PersistedLane>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedLane {
    key: String,
    adapter_kind: String,
    thread_channel: PersistedChannelRef,
    #[serde(default)]
    pending: Vec<PersistedMessage>,
    #[serde(default)]
    active: Vec<PersistedMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedMessage {
    id: u64,
    sender_json: String,
    sender_name: String,
    prompt: String,
    #[serde(default)]
    extra_blocks: Vec<PersistedContentBlock>,
    trigger_msg: PersistedMessageRef,
    queued_at_unix_ms: u64,
    other_bot_present: bool,
    recipient: Option<(String, String)>,
    #[serde(default)]
    recovered_from_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PersistedContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedChannelRef {
    platform: String,
    channel_id: String,
    thread_id: Option<String>,
    parent_id: Option<String>,
    origin_event_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedMessageRef {
    channel: PersistedChannelRef,
    message_id: String,
}

struct QueueStore {
    path: PathBuf,
    lanes: Mutex<HashMap<String, PersistedLane>>,
    next_message_id: AtomicU64,
    /// Monotonic snapshot counter, assigned while `lanes` is held. Serialising
    /// under that lock but writing outside it means two mutations can reach
    /// `write_snapshot` out of order; the sequence lets the older one notice it
    /// has been superseded instead of rolling the file back.
    snapshot_seq: AtomicU64,
    /// Guards the file write and records the newest sequence already on disk.
    /// Deliberately separate from `lanes` so an fsync never blocks `submit` or
    /// the queue-management APIs.
    writer: Mutex<u64>,
}

impl QueueStore {
    fn load(path: PathBuf) -> Self {
        let file = match std::fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<PersistedQueueFile>(&data) {
                Ok(file) if file.version == QUEUE_STORE_VERSION => file,
                Ok(file) => {
                    warn!(
                        version = file.version,
                        expected = QUEUE_STORE_VERSION,
                        path = %path.display(),
                        "unsupported queue store version, starting empty"
                    );
                    PersistedQueueFile {
                        version: QUEUE_STORE_VERSION,
                        next_message_id: 1,
                        lanes: Vec::new(),
                    }
                }
                Err(error) => {
                    warn!(%error, path = %path.display(), "failed to parse queue store, starting empty");
                    PersistedQueueFile {
                        version: QUEUE_STORE_VERSION,
                        next_message_id: 1,
                        lanes: Vec::new(),
                    }
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => PersistedQueueFile {
                version: QUEUE_STORE_VERSION,
                next_message_id: 1,
                lanes: Vec::new(),
            },
            Err(error) => {
                warn!(%error, path = %path.display(), "failed to read queue store, starting empty");
                PersistedQueueFile {
                    version: QUEUE_STORE_VERSION,
                    next_message_id: 1,
                    lanes: Vec::new(),
                }
            }
        };
        let max_id = file
            .lanes
            .iter()
            .flat_map(|lane| lane.pending.iter().chain(&lane.active))
            .map(|message| message.id)
            .max()
            .unwrap_or(0);
        let next_message_id = file.next_message_id.max(max_id.saturating_add(1)).max(1);
        let lanes = file
            .lanes
            .into_iter()
            .map(|lane| (lane.key.clone(), lane))
            .collect::<HashMap<_, _>>();
        info!(
            count = lanes.len(),
            path = %path.display(),
            "loaded persistent dispatch queue"
        );
        Self {
            path,
            lanes: Mutex::new(lanes),
            next_message_id: AtomicU64::new(next_message_id),
            snapshot_seq: AtomicU64::new(0),
            writer: Mutex::new(0),
        }
    }

    fn next_message_id(&self) -> u64 {
        self.next_message_id.load(Ordering::Relaxed)
    }

    fn record_next_message_id(&self, next_message_id: u64) {
        self.next_message_id
            .fetch_max(next_message_id, Ordering::Relaxed);
    }

    fn summaries(&self) -> Vec<PersistedQueueSummary> {
        let lanes = self.lanes.lock().unwrap();
        let mut summaries = HashMap::<(String, String), PersistedQueueSummary>::new();
        for lane in lanes.values() {
            let platform = lane.thread_channel.platform.clone();
            let thread_id = lane
                .thread_channel
                .thread_id
                .clone()
                .unwrap_or_else(|| lane.thread_channel.channel_id.clone());
            let summary = summaries
                .entry((platform.clone(), thread_id.clone()))
                .or_insert(PersistedQueueSummary {
                    platform,
                    thread_id,
                    queued_messages: 0,
                    recovered_active_messages: 0,
                });
            summary.queued_messages += lane.pending.len() + lane.active.len();
            summary.recovered_active_messages += lane.active.len();
        }
        let mut summaries = summaries.into_values().collect::<Vec<_>>();
        summaries.sort_by(|a, b| {
            a.platform
                .cmp(&b.platform)
                .then_with(|| a.thread_id.cmp(&b.thread_id))
        });
        summaries
    }

    fn lanes_for_adapter(&self, adapter_kind: &str) -> Vec<PersistedLane> {
        let lanes = self.lanes.lock().unwrap();
        let mut result = lanes
            .values()
            .filter(|lane| lane.adapter_kind == adapter_kind)
            .cloned()
            .collect::<Vec<_>>();
        result.sort_by(|a, b| a.key.cmp(&b.key));
        result
    }

    fn update_pending(
        &self,
        key: &str,
        adapter_kind: &str,
        thread_channel: &ChannelRef,
        pending: Vec<(QueuedMessage, bool)>,
    ) {
        self.mutate(|lanes| {
            let pending = pending
                .into_iter()
                .map(|(message, recovered)| PersistedMessage::from_queued(message, recovered))
                .collect::<Vec<_>>();
            let lane = lanes.entry(key.to_string()).or_insert_with(|| PersistedLane {
                key: key.to_string(),
                adapter_kind: adapter_kind.to_string(),
                thread_channel: PersistedChannelRef::from(thread_channel),
                pending: Vec::new(),
                active: Vec::new(),
            });
            lane.adapter_kind = adapter_kind.to_string();
            lane.thread_channel = PersistedChannelRef::from(thread_channel);
            lane.pending = pending;
            if lane.pending.is_empty() && lane.active.is_empty() {
                lanes.remove(key);
            }
        });
    }

    fn mark_active(
        &self,
        key: &str,
        adapter_kind: &str,
        thread_channel: &ChannelRef,
        pending: Vec<(QueuedMessage, bool)>,
        active: &[QueuedMessage],
    ) {
        self.mutate(|lanes| {
            lanes.insert(
                key.to_string(),
                PersistedLane {
                    key: key.to_string(),
                    adapter_kind: adapter_kind.to_string(),
                    thread_channel: PersistedChannelRef::from(thread_channel),
                    pending: pending
                        .into_iter()
                        .map(|(message, recovered)| {
                            PersistedMessage::from_queued(message, recovered)
                        })
                        .collect(),
                    active: active
                        .iter()
                        .cloned()
                        .map(|message| {
                            let recovered_from_active = message.recovered_from_active;
                            PersistedMessage::from_queued(message, recovered_from_active)
                        })
                        .collect(),
                },
            );
        });
    }

    fn complete_active(&self, key: &str, pending: Vec<(QueuedMessage, bool)>) {
        self.mutate(|lanes| {
            let Some(lane) = lanes.get_mut(key) else {
                return;
            };
            lane.active.clear();
            lane.pending = pending
                .into_iter()
                .map(|(message, recovered)| PersistedMessage::from_queued(message, recovered))
                .collect();
            if lane.pending.is_empty() {
                lanes.remove(key);
            }
        });
    }

    fn mark_requeued(&self, lane: &PersistedLane) {
        self.mutate(|lanes| {
            let mut restored = lane.clone();
            let mut pending = restored
                .active
                .drain(..)
                .map(|mut message| {
                    message.recovered_from_active = true;
                    message
                })
                .collect::<Vec<_>>();
            pending.append(&mut restored.pending);
            restored.pending = pending;
            lanes.insert(restored.key.clone(), restored);
        });
    }

    fn remove_lane(&self, key: &str) {
        self.mutate(|lanes| {
            lanes.remove(key);
        });
    }

    /// Apply `mutate` to the in-memory lanes, then persist the resulting
    /// snapshot. Serialisation runs under the `lanes` lock because it borrows
    /// the map; the write and its two fsyncs deliberately do not, so a slow
    /// disk cannot stall `submit` or the queue-management APIs.
    ///
    /// Durability is unchanged: when this returns, a snapshot at least as new
    /// as this mutation is on disk — either the one written here, or a later
    /// one that superseded it.
    fn mutate(&self, mutate: impl FnOnce(&mut HashMap<String, PersistedLane>)) {
        let (seq, data) = {
            let mut lanes = self.lanes.lock().unwrap();
            mutate(&mut lanes);
            // Assigned under the lock, so sequence order matches the order in
            // which mutations were applied.
            let seq = self
                .snapshot_seq
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            match self.serialize_locked(&lanes) {
                Ok(data) => (seq, data),
                Err(error) => {
                    warn!(%error, path = %self.path.display(), "failed to serialize dispatch queue");
                    return;
                }
            }
        };
        self.write_snapshot(seq, &data);
    }

    fn serialize_locked(&self, lanes: &HashMap<String, PersistedLane>) -> anyhow::Result<Vec<u8>> {
        let mut entries = lanes.values().collect::<Vec<_>>();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        let file = PersistedQueueFileRef {
            version: QUEUE_STORE_VERSION,
            next_message_id: self.next_message_id(),
            lanes: entries,
        };
        Ok(serde_json::to_vec_pretty(&file)?)
    }

    /// Write one serialised snapshot, skipping it when a newer snapshot already
    /// reached disk. Sequence order equals lock-acquisition order, so a higher
    /// sequence always contains every earlier mutation — dropping the stale
    /// write loses nothing and stops it from rolling the file back.
    fn write_snapshot(&self, seq: u64, data: &[u8]) {
        let mut last_written = self.writer.lock().unwrap();
        if *last_written >= seq {
            return;
        }
        match atomic_write(&self.path, data) {
            Ok(()) => *last_written = seq,
            Err(error) => {
                warn!(%error, path = %self.path.display(), "failed to persist dispatch queue");
            }
        }
    }
}

impl PersistedMessage {
    fn from_queued(queued: QueuedMessage, recovered_from_active: bool) -> Self {
        let queued_at_unix_ms = unix_time_ms().saturating_sub(
            u64::try_from(queued.message.arrived_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        Self {
            id: queued.id,
            sender_json: queued.message.sender_json,
            sender_name: queued.message.sender_name,
            prompt: queued.message.prompt,
            extra_blocks: queued
                .message
                .extra_blocks
                .into_iter()
                .map(PersistedContentBlock::from)
                .collect(),
            trigger_msg: PersistedMessageRef::from(&queued.message.trigger_msg),
            queued_at_unix_ms,
            other_bot_present: queued.message.other_bot_present,
            recipient: queued.message.recipient,
            recovered_from_active,
        }
    }

    fn into_queued(self) -> (QueuedMessage, bool) {
        let age = Duration::from_millis(unix_time_ms().saturating_sub(self.queued_at_unix_ms));
        let now = Instant::now();
        let arrived_at = now.checked_sub(age).unwrap_or(now);
        let extra_blocks = self
            .extra_blocks
            .into_iter()
            .map(ContentBlock::from)
            .collect::<Vec<_>>();
        let estimated_tokens = estimate_tokens(&self.prompt, &extra_blocks);
        (
            QueuedMessage {
                id: self.id,
                message: BufferedMessage {
                    sender_json: self.sender_json,
                    sender_name: self.sender_name,
                    prompt: self.prompt,
                    extra_blocks,
                    trigger_msg: self.trigger_msg.into(),
                    arrived_at,
                    estimated_tokens,
                    other_bot_present: self.other_bot_present,
                    recipient: self.recipient,
                },
                recovered_from_active: self.recovered_from_active,
            },
            self.recovered_from_active,
        )
    }
}

impl From<ContentBlock> for PersistedContentBlock {
    fn from(value: ContentBlock) -> Self {
        match value {
            ContentBlock::Text { text } => Self::Text { text },
            ContentBlock::Image { media_type, data } => Self::Image { media_type, data },
        }
    }
}

impl From<PersistedContentBlock> for ContentBlock {
    fn from(value: PersistedContentBlock) -> Self {
        match value {
            PersistedContentBlock::Text { text } => Self::Text { text },
            PersistedContentBlock::Image { media_type, data } => Self::Image { media_type, data },
        }
    }
}

impl From<&ChannelRef> for PersistedChannelRef {
    fn from(value: &ChannelRef) -> Self {
        Self {
            platform: value.platform.clone(),
            channel_id: value.channel_id.clone(),
            thread_id: value.thread_id.clone(),
            parent_id: value.parent_id.clone(),
            origin_event_id: value.origin_event_id.clone(),
        }
    }
}

impl From<PersistedChannelRef> for ChannelRef {
    fn from(value: PersistedChannelRef) -> Self {
        Self {
            platform: value.platform,
            channel_id: value.channel_id,
            thread_id: value.thread_id,
            parent_id: value.parent_id,
            origin_event_id: value.origin_event_id,
        }
    }
}

impl From<&MessageRef> for PersistedMessageRef {
    fn from(value: &MessageRef) -> Self {
        Self {
            channel: PersistedChannelRef::from(&value.channel),
            message_id: value.message_id.clone(),
        }
    }
}

impl From<PersistedMessageRef> for MessageRef {
    fn from(value: PersistedMessageRef) -> Self {
        Self {
            channel: value.channel.into(),
            message_id: value.message_id,
        }
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn atomic_write(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp_path = path.with_extension("json.tmp");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp_path)?;
    #[cfg(unix)]
    std::fs::set_permissions(&temp_path, {
        use std::os::unix::fs::PermissionsExt;
        std::fs::Permissions::from_mode(0o600)
    })?;
    file.write_all(data)?;
    file.sync_all()?;
    std::fs::rename(&temp_path, path)?;
    if let Some(parent) = path.parent() {
        let directory = std::fs::File::open(parent)?;
        directory.sync_all()?;
    }
    Ok(())
}

#[derive(Default)]
struct QueueActivity {
    inner: Mutex<QueueActivityInner>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct QueueActivityInner {
    active: Vec<ActiveMessage>,
    replace_paused: bool,
}

impl QueueActivity {
    fn set_active(&self, batch: &[QueuedMessage]) {
        let mut inner = self.inner.lock().unwrap();
        inner.active = batch
            .iter()
            .map(|item| ActiveMessage {
                id: item.id,
                sender_id: sender_id_from_json(&item.message.sender_json),
                sender_name: item.message.sender_name.clone(),
                prompt: item.message.prompt.clone(),
                attachment_count: item.message.extra_blocks.len(),
                recovered_from_active: item.recovered_from_active,
            })
            .collect();
    }

    fn clear_active(&self) {
        self.inner.lock().unwrap().active.clear();
    }

    fn list(&self) -> Vec<ActiveMessage> {
        self.inner.lock().unwrap().active.clone()
    }

    fn claim_replace(&self, id: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.replace_paused || !inner.active.iter().any(|item| item.id == id) {
            return false;
        }
        inner.replace_paused = true;
        true
    }

    fn release_replace(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.replace_paused = false;
        drop(inner);
        self.changed.notify_waiters();
    }

    async fn wait_until_unpaused(&self) {
        loop {
            let notified = self.changed.notified();
            if !self.inner.lock().unwrap().replace_paused {
                return;
            }
            notified.await;
        }
    }
}

struct ReplaceClaim {
    key: String,
    queue: Arc<PendingQueue>,
    activity: Arc<QueueActivity>,
    adapter_kind: String,
    thread_channel: ChannelRef,
}

struct ThreadHandle {
    tx: tokio::sync::mpsc::Sender<u64>,
    queue: Arc<PendingQueue>,
    activity: Arc<QueueActivity>,
    consumer: tokio::task::JoinHandle<()>,
    /// Race-safe eviction counter (§2.5). Plain u64 — all reads/writes under per_thread lock.
    generation: u64,
    channel_id: String,
    thread_channel: ChannelRef,
    adapter_kind: String,
}

impl ThreadHandle {
    /// Exact number of messages whose payload is still pending — used for
    /// shutdown / cancel logging and queue-management UI reconciliation.
    fn pending_count(&self) -> usize {
        self.queue.len()
    }
}

// ---------------------------------------------------------------------------
// DispatchTarget — trait seam between Dispatcher and AdapterRouter
// ---------------------------------------------------------------------------

/// Surface that `consumer_loop` / `dispatch_batch` need from the underlying
/// router. Extracted as a trait so the dispatcher can be unit-tested without
/// spinning up a real `SessionPool` (which forks ACP CLI subprocesses).
/// `AdapterRouter` is the production implementor; tests use a mock that
/// records calls.
#[async_trait]
pub trait DispatchTarget: Send + Sync + 'static {
    fn reactions_config(&self) -> &ReactionsConfig;

    /// Workspace aliases from config (for `[[ws:@alias]]` resolution).
    fn workspace_aliases(&self) -> std::collections::HashMap<String, String>;

    /// Bot home directory used for `~` expansion.
    fn bot_home(&self) -> std::path::PathBuf;

    /// Canonical security boundary for all workspace resolution.
    fn workspace_root(&self) -> std::path::PathBuf;

    /// Workspace spec bound to this platform channel, if configured.
    fn channel_workspace_spec(&self, channel: &ChannelRef) -> Option<String>;

    /// Ensure the ACP session for `session_key` exists (idempotent).
    /// Returns `true` if a new session was created, `false` if it already existed.
    async fn ensure_session(&self, session_key: &str, working_dir: Option<&str>) -> Result<bool>;

    /// Destroy the session for `session_key` (used to rollback on directive failure).
    async fn reset_session(&self, session_key: &str);

    /// Drive one ACP turn with the pre-packed `content_blocks`.
    #[allow(clippy::too_many_arguments)]
    async fn stream_prompt_blocks(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        session_key: &str,
        content_blocks: Vec<ContentBlock>,
        thread_channel: &ChannelRef,
        reactions: Arc<StatusReactionController>,
        other_bot_present: bool,
        recipient: Option<(String, String)>,
    ) -> Result<()>;
}

#[async_trait]
impl DispatchTarget for AdapterRouter {
    fn reactions_config(&self) -> &ReactionsConfig {
        AdapterRouter::reactions_config(self)
    }

    fn workspace_aliases(&self) -> std::collections::HashMap<String, String> {
        self.workspace_aliases_map()
    }

    fn bot_home(&self) -> std::path::PathBuf {
        self.bot_home_path()
    }

    fn workspace_root(&self) -> std::path::PathBuf {
        self.workspace_root_path()
    }

    fn channel_workspace_spec(&self, channel: &ChannelRef) -> Option<String> {
        AdapterRouter::channel_workspace_spec(self, channel)
    }

    async fn ensure_session(&self, session_key: &str, working_dir: Option<&str>) -> Result<bool> {
        self.pool().get_or_create(session_key, working_dir).await
    }

    async fn reset_session(&self, session_key: &str) {
        let _ = self.pool().reset_session(session_key).await;
    }

    async fn stream_prompt_blocks(
        &self,
        adapter: &Arc<dyn ChatAdapter>,
        session_key: &str,
        content_blocks: Vec<ContentBlock>,
        thread_channel: &ChannelRef,
        reactions: Arc<StatusReactionController>,
        other_bot_present: bool,
        recipient: Option<(String, String)>,
    ) -> Result<()> {
        AdapterRouter::stream_prompt_blocks(
            self,
            adapter,
            session_key,
            content_blocks,
            thread_channel,
            reactions,
            other_bot_present,
            recipient,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Dispatcher
// ---------------------------------------------------------------------------

/// Default idle timeout for per-thread consumer tasks in batched modes (Thread / Lane).
/// When no message arrives within this window the consumer exits, allowing `per_thread`
/// map cleanup on the next `submit` (via `SendError` → `try_evict_locked`). Prevents
/// unbounded task/memory growth from one-shot thread keys (e.g. Slack non-thread messages).
///
/// Batched modes need a longer window so a lane that's between trigger arrivals isn't
/// torn down and respawned on every message.
pub const DEFAULT_CONSUMER_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Idle timeout for per-message mode (cap=1, no batching). Per-message dispatchers
/// don't benefit from holding consumers across message gaps — there is no batch
/// window to preserve — so a much shorter timeout reduces idle resource footprint
/// from one-shot thread keys (Little's Law: steady-state idle count = arrival rate
/// × idle window).
pub const PER_MESSAGE_CONSUMER_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve `(cap, grouping, idle_timeout)` for a given processing mode.
///
/// Per-message mode forces cap=1 + Thread grouping + the short per-message idle
/// (one-shot threads shouldn't pin a consumer for 5 min); Thread / Lane modes
/// use the configured `max_buffered` and the default idle window.
pub fn dispatch_params(
    mode: &crate::config::MessageProcessingMode,
    max_buffered: usize,
) -> (usize, BatchGrouping, Duration) {
    use crate::config::MessageProcessingMode;
    match mode {
        MessageProcessingMode::Message => {
            (1, BatchGrouping::Thread, PER_MESSAGE_CONSUMER_IDLE_TIMEOUT)
        }
        MessageProcessingMode::Thread => (
            max_buffered,
            BatchGrouping::Thread,
            DEFAULT_CONSUMER_IDLE_TIMEOUT,
        ),
        MessageProcessingMode::Lane => (
            max_buffered,
            BatchGrouping::Lane,
            DEFAULT_CONSUMER_IDLE_TIMEOUT,
        ),
    }
}

/// Per-thread message dispatcher for batched mode.
///
/// Constructed once in `main.rs` and shared via `Arc`. Platform adapters call
/// `submit()` from their per-message `tokio::spawn`'d tasks.
pub struct Dispatcher {
    /// std::sync::Mutex — critical section has no .await; tokio::Mutex buys nothing here.
    per_thread: Mutex<HashMap<String, ThreadHandle>>,
    /// Monotonic counter for `ThreadHandle.generation` (§2.5). Pre-fetched on
    /// every `submit` and consumed only when a fresh handle is inserted; wasted
    /// values are fine because generations need only be monotonic, not contiguous.
    next_generation: AtomicU64,
    /// Stable IDs for pending messages exposed through queue-management UIs.
    next_message_id: AtomicU64,
    replace_claims: Mutex<HashMap<u64, ReplaceClaim>>,
    queue_store: Option<Arc<QueueStore>>,
    target: Arc<dyn DispatchTarget>,
    max_buffered_messages: usize,
    max_batch_tokens: usize,
    grouping: BatchGrouping,
    idle_timeout: Duration,
}

impl Dispatcher {
    /// Construct a dispatcher with an explicit consumer idle timeout. Per-mode
    /// callers in `main.rs` pass `PER_MESSAGE_CONSUMER_IDLE_TIMEOUT` for cap=1
    /// dispatchers and `DEFAULT_CONSUMER_IDLE_TIMEOUT` for batched modes.
    pub fn with_idle_timeout(
        target: Arc<dyn DispatchTarget>,
        max_buffered_messages: usize,
        max_batch_tokens: usize,
        grouping: BatchGrouping,
        idle_timeout: Duration,
    ) -> Self {
        Self {
            per_thread: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(0),
            next_message_id: AtomicU64::new(1),
            replace_claims: Mutex::new(HashMap::new()),
            queue_store: None,
            target,
            max_buffered_messages,
            max_batch_tokens,
            grouping,
            idle_timeout,
        }
    }

    /// Enable durable queue snapshots. The store is loaded synchronously so
    /// callers can reconcile external UI metadata before restored consumers
    /// are started with `restore_persisted`.
    pub fn with_persistence(mut self, path: PathBuf) -> Self {
        let store = Arc::new(QueueStore::load(path));
        self.next_message_id
            .store(store.next_message_id(), Ordering::Relaxed);
        self.queue_store = Some(store);
        self
    }

    pub fn persisted_queue_summaries(&self) -> Vec<PersistedQueueSummary> {
        self.queue_store
            .as_ref()
            .map(|store| store.summaries())
            .unwrap_or_default()
    }

    /// Requeue work loaded from disk and lazily restart one consumer per lane.
    /// Requests that were active at shutdown are placed before prior pending
    /// work and marked as recovered, providing at-least-once delivery.
    pub fn restore_persisted(&self, adapter: Arc<dyn ChatAdapter>) -> usize {
        let Some(store) = &self.queue_store else {
            return 0;
        };
        let lanes = store.lanes_for_adapter(adapter.platform());
        let mut restored_count = 0;
        for mut lane in lanes {
            let stored_lane = lane.clone();
            let mut restored = lane
                .active
                .drain(..)
                .map(|mut message| {
                    message.recovered_from_active = true;
                    message
                })
                .collect::<Vec<_>>();
            restored.append(&mut lane.pending);
            if restored.is_empty() {
                store.remove_lane(&lane.key);
                continue;
            }

            if self.per_thread.lock().unwrap().contains_key(&lane.key) {
                continue;
            }

            let queue = Arc::new(PendingQueue::default());
            let lane_count = restored.len();
            for message in restored {
                let (queued, recovered) = message.into_queued();
                queue.insert_with_recovery(queued.id, queued.message, false, recovered);
            }
            let thread_channel: ChannelRef = lane.thread_channel.clone().into();
            // Capacity must cover what was restored: a lane larger than the
            // configured cap could not wake its own consumer.
            let cap = self.max_buffered_messages.max(queue.len()).max(1);
            let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
            store.mark_requeued(&stored_lane);
            let handle =
                self.spawn_lane(&lane.key, &thread_channel, &adapter, generation, queue, cap);
            let mut map = self.per_thread.lock().unwrap();
            if let std::collections::hash_map::Entry::Vacant(entry) = map.entry(lane.key) {
                entry.insert(handle);
                restored_count += lane_count;
            } else {
                handle.consumer.abort();
            }
        }
        restored_count
    }

    /// Build the dispatcher key for a (platform, thread, sender) tuple.
    ///
    /// In `Thread` mode the sender is ignored; in `Lane` mode the sender is appended
    /// so each (thread, sender) pair gets its own mpsc and consumer.
    ///
    /// Note: this is the *dispatcher* key, not the *session pool* key. Session pool keys
    /// are always `<platform>:<thread_id>` regardless of grouping (the ACP session is
    /// shared per-thread by design).
    pub fn key(&self, platform: &str, thread_id: &str, sender_id: &str) -> String {
        match self.grouping {
            BatchGrouping::Thread => format!("{platform}:{thread_id}"),
            BatchGrouping::Lane => format!("{platform}:{thread_id}:{sender_id}"),
        }
    }

    /// Build the shared session pool key for a routed channel.
    ///
    /// Unlike dispatcher keys, session keys never include sender identity.
    /// They track the logical conversation thread across all grouping modes.
    fn session_key(thread_channel: &ChannelRef) -> String {
        let logical_thread_id = thread_channel
            .thread_id
            .as_deref()
            .unwrap_or(&thread_channel.channel_id);
        format!("{}:{}", thread_channel.platform, logical_thread_id)
    }

    fn allocate_message_id(&self) -> u64 {
        let id = self.next_message_id.fetch_add(1, Ordering::Relaxed);
        if let Some(store) = &self.queue_store {
            store.record_next_message_id(id.saturating_add(1));
        }
        id
    }

    fn persist_pending_queue(
        &self,
        key: &str,
        adapter_kind: &str,
        thread_channel: &ChannelRef,
        queue: &PendingQueue,
    ) {
        if let Some(store) = &self.queue_store {
            store.update_pending(
                key,
                adapter_kind,
                thread_channel,
                queue.snapshot(),
            );
        }
    }

    /// Create a fresh lane — pending queue, activity gate, mpsc wakeup channel
    /// and consumer task — and return its handle.
    ///
    /// `submit` (first attempt and retry) and `restore_persisted` all need
    /// exactly this wiring. Keeping it in one place means the eleven-argument
    /// `consumer_loop` call exists once, so a new consumer parameter cannot be
    /// threaded through one path and silently forgotten in another.
    ///
    /// `queue` is a parameter because `restore_persisted` hands over a queue
    /// already populated from disk while `submit` starts empty. This spawns the
    /// consumer task, so only call it when a lane is genuinely missing (e.g.
    /// from inside `or_insert_with`).
    fn spawn_lane(
        &self,
        thread_key: &str,
        thread_channel: &ChannelRef,
        adapter: &Arc<dyn ChatAdapter>,
        generation: u64,
        queue: Arc<PendingQueue>,
        cap: usize,
    ) -> ThreadHandle {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        let activity = Arc::new(QueueActivity::default());
        let consumer = tokio::spawn(consumer_loop(
            thread_key.to_string(),
            thread_channel.clone(),
            rx,
            Arc::clone(&queue),
            Arc::clone(&activity),
            Arc::clone(&self.target),
            Arc::clone(adapter),
            cap,
            self.max_batch_tokens,
            self.idle_timeout,
            self.queue_store.clone(),
        ));
        ThreadHandle {
            tx,
            queue,
            activity,
            consumer,
            generation,
            channel_id: thread_channel.channel_id.clone(),
            thread_channel: thread_channel.clone(),
            adapter_kind: adapter.platform().to_string(),
        }
    }

    /// Submit one arrival event for the given thread.
    ///
    /// - If the thread has no active consumer, one is spawned lazily.
    /// - If the channel is full, this future parks until space is available
    ///   (backpressure — no data loss, no error).
    /// - If the consumer has died (`SendError`), surfaces ❌ + ⚠️ and returns
    ///   `Err(DispatchError::ConsumerDead)` (§2.5).
    ///
    /// `adapter` is passed per-call (not stored on `Dispatcher`) because the
    /// Discord adapter is constructed inside serenity's `ready` callback via
    /// `OnceLock` — after the Dispatcher is built in `main.rs`.
    pub async fn submit(
        &self,
        thread_key: String,
        thread_channel: ChannelRef,
        adapter: Arc<dyn ChatAdapter>,
        msg: BufferedMessage,
    ) -> Result<(), DispatchError> {
        let cap = self.max_buffered_messages;

        // Pre-fetch a generation in case we end up inserting a fresh handle.
        // Wasted if the entry already exists; generations need only be monotonic.
        let next_g = self.next_generation.fetch_add(1, Ordering::Relaxed);

        let message_id = self.allocate_message_id();
        let (tx, queue, my_generation) = {
            // SAFETY: no .await while this guard is held — guard drops at end of block.
            let mut map = self.per_thread.lock().unwrap();

            // Proactive stale-entry cleanup: if the consumer has exited (idle
            // timeout or unexpected), remove the entry so `or_insert_with`
            // creates a fresh one. Prevents map leak from one-shot thread keys
            // and avoids the first-message-after-idle being treated as an error.
            if let Some(handle) = map.get(&thread_key) {
                if handle.consumer.is_finished() {
                    map.remove(&thread_key);
                }
            }

            let entry = map.entry(thread_key.clone()).or_insert_with(|| {
                self.spawn_lane(
                    &thread_key,
                    &thread_channel,
                    &adapter,
                    next_g,
                    Arc::new(PendingQueue::default()),
                    cap,
                )
            });
            (entry.tx.clone(), Arc::clone(&entry.queue), entry.generation)
        };

        queue.insert(message_id, msg, false);
        self.persist_pending_queue(
            &thread_key,
            adapter.platform(),
            &thread_channel,
            &queue,
        );

        if let Err(e) = tx.send(message_id).await {
            // Consumer has exited between our check and the send — race-safe
            // eviction under lock (§2.5), then transparent retry once.
            //
            // Safe to re-acquire `per_thread` here: the first lock guard above
            // was dropped before `tx.send().await`, so this acquisition cannot
            // deadlock against the await point. The same property holds for the
            // retry acquisition below.
            {
                // SAFETY: no .await while this guard is held.
                let mut map = self.per_thread.lock().unwrap();
                Self::try_evict_locked(&mut map, &thread_key, my_generation);
            }
            let failed_id = e.0;
            let Some(failed_msg) = queue.take(failed_id) else {
                // The message was removed through the queue manager while this
                // producer was waiting for channel capacity.
                return Ok(());
            };

            // Retry: spawn a fresh consumer and re-send. If this also fails,
            // surface the error to the user.
            let retry_g = self.next_generation.fetch_add(1, Ordering::Relaxed);
            let (retry_tx, retry_queue, retry_gen) = {
                // SAFETY: no .await while this guard is held — guard drops at end of block.
                let mut map = self.per_thread.lock().unwrap();
                let entry = map.entry(thread_key.clone()).or_insert_with(|| {
                    self.spawn_lane(
                        &thread_key,
                        &thread_channel,
                        &adapter,
                        retry_g,
                        Arc::new(PendingQueue::default()),
                        cap,
                    )
                });
                (
                    entry.tx.clone(),
                    Arc::clone(&entry.queue),
                    entry.generation,
                )
            };

            retry_queue.insert(failed_id, failed_msg, false);
            self.persist_pending_queue(
                &thread_key,
                adapter.platform(),
                &thread_channel,
                &retry_queue,
            );

            if let Err(e2) = retry_tx.send(failed_id).await {
                // Retry also failed — truly unexpected. Surface error.
                {
                    // SAFETY: no .await while this guard is held.
                    let mut map = self.per_thread.lock().unwrap();
                    Self::try_evict_locked(&mut map, &thread_key, retry_gen);
                }
                let failed_id = e2.0;
                let failed_msg = retry_queue.take(failed_id);
                self.persist_pending_queue(
                    &thread_key,
                    adapter.platform(),
                    &thread_channel,
                    &retry_queue,
                );
                if let Some(failed_msg) = failed_msg.as_ref() {
                    let _ = adapter
                        .add_reaction(
                            &failed_msg.trigger_msg,
                            &self.target.reactions_config().emojis.error,
                        )
                        .await;
                }
                let _ = adapter
                    .send_message(
                        &thread_channel,
                        &format!(
                            "⚠️ {}",
                            format_user_error("dispatch consumer exited unexpectedly")
                        ),
                    )
                    .await;
                return Err(DispatchError::ConsumerDead);
            }
        }
        Ok(())
    }

    /// Return all messages still waiting to be handed to the agent for a
    /// logical platform thread. Entries across sender lanes are ordered by
    /// their process-local monotonic ID.
    pub fn pending_messages(&self, platform: &str, thread_id: &str) -> Vec<PendingMessage> {
        let queues = self.pending_queues(platform, thread_id);
        if queues.len() == 1 {
            return queues[0].list();
        }
        let mut messages = queues
            .iter()
            .flat_map(|queue| queue.list())
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| message.id);
        messages
    }

    /// Edit a pending text prompt. Returns false when the message has already
    /// started, was removed, or does not belong to this thread.
    pub fn edit_pending_message(
        &self,
        platform: &str,
        thread_id: &str,
        message_id: u64,
        prompt: &str,
    ) -> bool {
        for (key, queue, adapter_kind, thread_channel) in
            self.pending_queue_handles(platform, thread_id)
        {
            if queue.edit(message_id, prompt) {
                self.persist_pending_queue(&key, &adapter_kind, &thread_channel, &queue);
                return true;
            }
        }
        false
    }

    /// Remove one pending message without interrupting the active ACP turn.
    pub fn remove_pending_message(
        &self,
        platform: &str,
        thread_id: &str,
        message_id: u64,
    ) -> bool {
        for (key, queue, adapter_kind, thread_channel) in
            self.pending_queue_handles(platform, thread_id)
        {
            if queue.remove(message_id) {
                self.persist_pending_queue(&key, &adapter_kind, &thread_channel, &queue);
                return true;
            }
        }
        false
    }

    /// Move one pending message to the front of its dispatch lane.
    pub fn move_pending_message_to_front(
        &self,
        platform: &str,
        thread_id: &str,
        message_id: u64,
    ) -> bool {
        for (key, queue, adapter_kind, thread_channel) in
            self.pending_queue_handles(platform, thread_id)
        {
            if queue.move_to_front(message_id) {
                self.persist_pending_queue(&key, &adapter_kind, &thread_channel, &queue);
                return true;
            }
        }
        false
    }

    pub fn active_messages(&self, platform: &str, thread_id: &str) -> Vec<ActiveMessage> {
        let prefix = format!("{platform}:{thread_id}");
        let lane_prefix = format!("{prefix}:");
        let activities = self
            .per_thread
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.as_str() == prefix || key.starts_with(&lane_prefix))
            .map(|(_, handle)| Arc::clone(&handle.activity))
            .collect::<Vec<_>>();
        let mut active = activities
            .iter()
            .flat_map(|activity| activity.list())
            .collect::<Vec<_>>();
        active.sort_by_key(|message| message.id);
        active
    }

    /// Atomically verify that `message_id` is still active and pause this lane
    /// before it can begin another turn. The claim must be released on every
    /// caller path.
    pub fn claim_active_for_replace(
        &self,
        platform: &str,
        thread_id: &str,
        message_id: u64,
    ) -> bool {
        let prefix = format!("{platform}:{thread_id}");
        let lane_prefix = format!("{prefix}:");
        let target = self
            .per_thread
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.as_str() == prefix || key.starts_with(&lane_prefix))
            .find_map(|(key, handle)| {
                handle.activity.claim_replace(message_id).then(|| ReplaceClaim {
                    key: key.clone(),
                    queue: Arc::clone(&handle.queue),
                    activity: Arc::clone(&handle.activity),
                    adapter_kind: handle.adapter_kind.clone(),
                    thread_channel: handle.thread_channel.clone(),
                })
            });
        let Some(target) = target else {
            return false;
        };
        self.replace_claims
            .lock()
            .unwrap()
            .insert(message_id, target);
        true
    }

    /// Insert the revised request ahead of existing pending work while an
    /// active replacement claim is held.
    pub fn enqueue_claimed_replacement(
        &self,
        active_message_id: u64,
        message: BufferedMessage,
    ) -> Result<u64, DispatchError> {
        let claims = self.replace_claims.lock().unwrap();
        let Some(claim) = claims.get(&active_message_id) else {
            return Err(DispatchError::ConsumerDead);
        };
        let new_id = self.allocate_message_id();
        claim.queue.insert(new_id, message, true);
        self.persist_pending_queue(
            &claim.key,
            &claim.adapter_kind,
            &claim.thread_channel,
            &claim.queue,
        );
        Ok(new_id)
    }

    pub fn release_active_replace(&self, active_message_id: u64) {
        if let Some(claim) = self
            .replace_claims
            .lock()
            .unwrap()
            .remove(&active_message_id)
        {
            claim.activity.release_replace();
        }
    }

    /// Remove every pending message without interrupting the active ACP turn.
    pub fn clear_pending_messages(&self, platform: &str, thread_id: &str) -> usize {
        self.clear_pending_messages_through(platform, thread_id, u64::MAX)
    }

    /// Remove pending messages that existed at confirmation time while keeping
    /// newer arrivals. Used by Discord's destructive-action confirmation card.
    pub fn clear_pending_messages_through(
        &self,
        platform: &str,
        thread_id: &str,
        max_message_id: u64,
    ) -> usize {
        self.pending_queue_handles(platform, thread_id)
            .into_iter()
            .map(|(key, queue, adapter_kind, thread_channel)| {
                let removed = queue.clear_through(max_message_id);
                if removed > 0 {
                    self.persist_pending_queue(&key, &adapter_kind, &thread_channel, &queue);
                }
                removed
            })
            .sum()
    }

    fn pending_queues(&self, platform: &str, thread_id: &str) -> Vec<Arc<PendingQueue>> {
        self.pending_queue_handles(platform, thread_id)
            .into_iter()
            .map(|(_, queue, _, _)| queue)
            .collect()
    }

    fn pending_queue_handles(
        &self,
        platform: &str,
        thread_id: &str,
    ) -> Vec<(String, Arc<PendingQueue>, String, ChannelRef)> {
        let prefix = format!("{platform}:{thread_id}");
        let lane_prefix = format!("{prefix}:");
        self.per_thread
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.as_str() == prefix || key.starts_with(&lane_prefix))
            .map(|(key, handle)| {
                (
                    key.clone(),
                    Arc::clone(&handle.queue),
                    handle.adapter_kind.clone(),
                    handle.thread_channel.clone(),
                )
            })
            .collect()
    }

    /// Drop all per-thread handles whose key belongs to `(platform, thread_id)`,
    /// regardless of grouping, and abort each consumer (§2.5 / §4.4). Returns
    /// the total number of buffered messages discarded across all lanes.
    ///
    /// Matches both Thread keys (`<platform>:<thread_id>`) and Lane keys
    /// (`<platform>:<thread_id>:<sender_id>`). Used by `/reset` and
    /// `/cancel-all` to clear the entire thread, not just one lane.
    ///
    /// Disjoint from SendError recovery: removal happens *before* abort, so any
    /// fresh `submit` after this returns lands on a lazily-constructed new handle
    /// instead of observing `SendError`.
    pub fn cancel_buffered_thread(&self, platform: &str, thread_id: &str) -> usize {
        let prefix = format!("{platform}:{thread_id}");
        let lane_prefix = format!("{prefix}:");
        // SAFETY: no .await while this guard is held — function is sync.
        let mut map = self.per_thread.lock().unwrap();
        let keys: Vec<String> = map
            .keys()
            .filter(|k| k.as_str() == prefix || k.starts_with(&lane_prefix))
            .cloned()
            .collect();
        let mut dropped = 0;
        for k in keys {
            if let Some(handle) = map.remove(&k) {
                // Clear payloads before aborting so producers already parked in
                // `tx.send()` cannot recover and retry messages that the caller
                // explicitly requested to discard.
                dropped += handle.queue.clear_through(u64::MAX);
                if let Some(store) = &self.queue_store {
                    store.remove_lane(&k);
                }
                for active in handle.activity.list() {
                    if let Some(claim) = self
                        .replace_claims
                        .lock()
                        .unwrap()
                        .remove(&active.id)
                    {
                        claim.activity.release_replace();
                    }
                }
                handle.consumer.abort();
            }
        }
        dropped
    }

    /// §2.5 race-safe eviction. Caller must hold the `per_thread` mutex.
    /// Removes the entry only if its generation matches `my_generation` —
    /// protects against evicting a fresh handle that another `submit` lazily
    /// inserted between this caller's failed `tx.send` and this call.
    /// Returns true if the entry was removed.
    fn try_evict_locked(
        map: &mut HashMap<String, ThreadHandle>,
        thread_key: &str,
        my_generation: u64,
    ) -> bool {
        if let Some(handle) = map.get(thread_key) {
            if handle.generation == my_generation {
                map.remove(thread_key);
                return true;
            }
        }
        false
    }

    /// Remove map entries whose consumer task has finished (idle timeout or
    /// unexpected exit). Called periodically from the cleanup task in main.rs
    /// to prevent unbounded map growth from one-shot thread keys that never
    /// receive a second `submit()`. Returns the number of entries swept.
    pub fn sweep_stale(&self) -> usize {
        // SAFETY: no .await while this guard is held — function is sync.
        let mut map = self.per_thread.lock().unwrap();
        let before = map.len();
        map.retain(|_, handle| !handle.consumer.is_finished());
        before - map.len()
    }

    /// Log buffered-message counts and drop all handles (called on SIGTERM).
    pub fn shutdown(&self) {
        // SAFETY: no .await while this guard is held — function is sync.
        let mut map = self.per_thread.lock().unwrap();
        for (thread_id, handle) in map.iter() {
            let pending = handle.pending_count();
            if pending > 0 {
                if self.queue_store.is_some() {
                    info!(
                        thread_id = %thread_id,
                        channel   = %handle.channel_id,
                        adapter   = %handle.adapter_kind,
                        buffered_preserved = pending,
                        "shutdown preserved pending messages for restart",
                    );
                } else {
                    warn!(
                        thread_id = %thread_id,
                        channel   = %handle.channel_id,
                        adapter   = %handle.adapter_kind,
                        buffered_lost = pending,
                        "shutdown dropped pending messages without dispatch",
                    );
                }
            }
            handle.queue.clear_through(u64::MAX);
            handle.consumer.abort();
        }
        for (_, claim) in self.replace_claims.lock().unwrap().drain() {
            claim.activity.release_replace();
        }
        map.clear();
    }
}

// ---------------------------------------------------------------------------
// consumer_loop
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn consumer_loop(
    thread_key: String,
    thread_channel: ChannelRef,
    mut rx: tokio::sync::mpsc::Receiver<u64>,
    queue: Arc<PendingQueue>,
    activity: Arc<QueueActivity>,
    target: Arc<dyn DispatchTarget>,
    adapter: Arc<dyn ChatAdapter>,
    max_batch: usize,
    max_tokens: usize,
    idle_timeout: Duration,
    queue_store: Option<Arc<QueueStore>>,
) {
    loop {
        activity.wait_until_unpaused().await;

        // Prefer the explicit order so queue-manager promotion and claimed
        // replacements take effect even though the mpsc still contains older
        // wakeup IDs. When empty, mpsc remains the zero-latency wakeup edge.
        let first = loop {
            if let Some(first) = queue.take_first() {
                break first;
            }
            match tokio::time::timeout(idle_timeout, rx.recv()).await {
                Ok(Some(_wakeup_id)) => continue,
                Ok(None) => return,
                Err(_elapsed) => {
                    debug!(
                        thread_key = %thread_key,
                        channel = %thread_channel.channel_id,
                        "consumer idle timeout, exiting"
                    );
                    return;
                }
            }
        };

        // Greedy drain up to max_batch messages or max_tokens.
        let mut batch = vec![first];
        let mut cumulative_tokens = batch[0].message.estimated_tokens;

        while batch.len() < max_batch {
            match rx.try_recv() {
                Ok(_wakeup_id) => match queue.take_front_for_batch(cumulative_tokens, max_tokens) {
                    BatchTake::Taken(more) => {
                        cumulative_tokens += more.message.estimated_tokens;
                        batch.push(*more);
                    }
                    BatchTake::TooLarge => break,
                    BatchTake::Missing => continue,
                },
                Err(_) => break,
            }
        }

        activity.set_active(&batch);
        if let Some(store) = &queue_store {
            store.mark_active(
                &thread_key,
                adapter.platform(),
                &thread_channel,
                queue.snapshot(),
                &batch,
            );
        }

        // §2.6: read the freshest snapshot in the batch (batch is non-empty).
        let bot_present = batch.last().unwrap().message.other_bot_present;

        let batch = batch
            .into_iter()
            .map(|queued| queued.message)
            .collect::<Vec<_>>();

        dispatch_batch(
            &thread_key,
            &thread_channel,
            &target,
            &adapter,
            batch,
            bot_present,
        )
        .await;
        activity.clear_active();
        if let Some(store) = &queue_store {
            store.complete_active(&thread_key, queue.snapshot());
        }
    }
}

// ---------------------------------------------------------------------------
// dispatch_batch
// ---------------------------------------------------------------------------

async fn dispatch_batch(
    thread_key: &str,
    thread_channel: &ChannelRef,
    target: &Arc<dyn DispatchTarget>,
    adapter: &Arc<dyn ChatAdapter>,
    batch: Vec<BufferedMessage>,
    other_bot_present: bool,
) {
    let dispatch_start = Instant::now();
    let batch_size = batch.len();
    let session_key = Dispatcher::session_key(thread_channel);
    if let Err(error) = adapter
        .update_task_lifecycle(
            thread_channel,
            TaskLifecycleEvent::Started { batch_size },
        )
        .await
    {
        warn!(%error, "failed to mark task as running");
    }

    // Apply 👀 reaction to every message in the batch before dispatch (§6.7).
    // Skip when assistant status API is active — uses
    // assistant.threads.setStatus instead of emoji reactions.
    let assistant_status = adapter.uses_assistant_status();
    if !assistant_status {
        let queued_emoji = &target.reactions_config().emojis.queued;
        for msg in batch.iter() {
            let _ = adapter.add_reaction(&msg.trigger_msg, queued_emoji).await;
        }
    }

    // Collect per-event observability data (before consuming the batch).
    let tokens_per_event: Vec<usize> = batch.iter().map(|m| m.estimated_tokens).collect();
    let wait_ms: Vec<u128> = batch
        .iter()
        .map(|m| m.arrived_at.elapsed().as_millis())
        .collect();
    let senders: Vec<String> = batch.iter().map(|m| m.sender_name.clone()).collect();

    // Native-streaming recipient is bound to the turn (captured per-message). A
    // batch attributes to the most recent sender; None for non-Slack/bot turns.
    let recipient: Option<(String, String)> = batch.last().and_then(|m| m.recipient.clone());

    // Anchor reactions on the last message in the batch (before consuming).
    let trigger_msg = batch.last().unwrap().trigger_msg.clone();
    let dispatch_channel = ChannelRef {
        // Reply correlation is event-scoped, but the dispatcher consumer is
        // thread-scoped. Rebuild the per-dispatch channel from the stable
        // thread route plus the freshest event ID so gateway replies (e.g.
        // LINE reply-token lookup) target the current inbound event.
        origin_event_id: trigger_msg.channel.origin_event_id.clone(),
        ..thread_channel.clone()
    };

    // Pack all arrival events into one Vec<ContentBlock> (§3.3).
    // Uses into_iter() to avoid deep-copying extra_blocks (may contain base64 image data).
    let mut content_blocks: Vec<ContentBlock> = Vec::new();

    // Parse control directives from the first message in the batch (ADR: control-directives).
    // Directives are only processed on the session's first message (§2.2).
    //
    // Strategy:
    //   1. Parse directives (cheap text extraction — no mutation, no I/O)
    //   2. Resolve explicit [[ws:...]], otherwise the platform-channel binding
    //   3. Call ensure_session with resolved workspace — returns created_now
    //   4. Only strip prompt and apply title/workspace if created_now == true
    //   5. If created_now == false, the [[...]] text is preserved verbatim
    let mut batch = batch;
    let parse_result = batch
        .first()
        .map(|first_msg| crate::directives::parse_directives(&first_msg.prompt));

    // An explicit directive overrides the channel default. Discord threads use
    // their parent channel binding through `channel_workspace_spec`.
    let directive_workspace = parse_result
        .as_ref()
        .and_then(|pr| pr.metadata.raw.get("ws").cloned());
    let workspace_spec =
        directive_workspace.or_else(|| target.channel_workspace_spec(thread_channel));

    // Tentatively resolve the workspace — if resolution fails and the session
    // turns out to be new, we abort. Existing sessions keep their immutable
    // persisted workspace, so a later config error cannot move them.
    let ws_resolved: Option<Result<String, String>> = workspace_spec.map(|ws_value| {
        let aliases = target.workspace_aliases();
        let bot_home = target.bot_home();
        let workspace_root = target.workspace_root();
        crate::directives::resolve_workspace(&ws_value, &aliases, &bot_home, &workspace_root)
            .map(|p| p.display().to_string())
    });

    // Extract workspace path for ensure_session (None if no directive or resolution failed).
    let workspace_override: Option<String> =
        ws_resolved.as_ref().and_then(|r| r.as_ref().ok().cloned());

    // Ensure session exists. The create_gate mutex inside get_or_create serializes
    // concurrent callers — only the winner gets created_now == true.
    let created_now = match target
        .ensure_session(&session_key, workspace_override.as_deref())
        .await
    {
        Ok(created) => created,
        Err(e) => {
            let user_msg = format_user_error(&e.to_string());
            let _ = adapter
                .send_message(&dispatch_channel, &format!("⚠️ {user_msg}"))
                .await;
            error!("pool error in dispatch_batch: {e}");
            let _ = adapter
                .update_task_lifecycle(
                    thread_channel,
                    TaskLifecycleEvent::Failed {
                        message: user_msg,
                    },
                )
                .await;
            return;
        }
    };

    // Only apply directives/bindings if this is genuinely the first message.
    if created_now {
        let title_to_apply = parse_result
            .as_ref()
            .and_then(|pr| pr.metadata.title.clone());

        // If workspace resolution failed on a NEW session, rollback and abort.
        // Reset FIRST to minimize the TOCTOU window, then rename.
        if let Some(Err(e)) = &ws_resolved {
            target.reset_session(&session_key).await;
            if let Some(ref title) = title_to_apply {
                if !title.is_empty() {
                    let _ = adapter.rename_thread(&dispatch_channel, title).await;
                }
            }
            let _ = adapter
                .send_message(&dispatch_channel, &format!("⚠️ {e}"))
                .await;
            error!(session_key, error = %e, "workspace selection rejected");
            let _ = adapter
                .update_task_lifecycle(
                    thread_channel,
                    TaskLifecycleEvent::Failed {
                        message: e.to_string(),
                    },
                )
                .await;
            return;
        }

        if let Some(pr) = parse_result {
            if !pr.metadata.raw.is_empty() {
                // Strip directives from the prompt
                if let Some(first_msg) = batch.first_mut() {
                    first_msg.prompt = pr.prompt;
                }
            }
        }

        // Apply title on success path.
        if let Some(ref title) = title_to_apply {
            if !title.is_empty() {
                if let Err(e) = adapter.rename_thread(&dispatch_channel, title).await {
                    warn!(session_key, error = %e, "failed to apply title directive");
                }
            }
        }
    }

    for msg in batch {
        let mut event_blocks =
            AdapterRouter::pack_arrival_event(&msg.sender_json, &msg.prompt, msg.extra_blocks);
        content_blocks.append(&mut event_blocks);
    }
    let packed_block_count = content_blocks.len();

    let reactions_config = target.reactions_config().clone();
    let reactions = Arc::new(StatusReactionController::new(
        reactions_config.enabled,
        adapter.clone(),
        trigger_msg,
        reactions_config.emojis.clone(),
        reactions_config.timing.clone(),
    ));
    // 👀 already applied above; skip set_queued() to avoid double-reaction.

    let result = target
        .stream_prompt_blocks(
            adapter,
            &session_key,
            content_blocks,
            &dispatch_channel,
            reactions.clone(),
            other_bot_present,
            recipient,
        )
        .await;

    // In assistant status mode, all status is conveyed via
    // assistant.threads.setStatus — skip emoji reactions entirely.
    if !assistant_status {
        match &result {
            Ok(()) => reactions.set_done().await,
            Err(_) => reactions.set_error().await,
        }

        let hold_ms = if result.is_ok() {
            reactions_config.timing.done_hold_ms
        } else {
            reactions_config.timing.error_hold_ms
        };
        if reactions_config.remove_after_reply {
            let reactions = reactions;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
                reactions.clear().await;
            });
        }
    }

    if let Err(ref e) = result {
        let _ = adapter
            .send_message(&dispatch_channel, &format!("⚠️ {e}"))
            .await;
    }

    let lifecycle = match &result {
        Ok(()) => TaskLifecycleEvent::Finished,
        Err(error) => TaskLifecycleEvent::Failed {
            message: format_user_error(&error.to_string()),
        },
    };
    if let Err(error) = adapter
        .update_task_lifecycle(thread_channel, lifecycle)
        .await
    {
        warn!(%error, "failed to update completed task UI");
    }

    let agent_dispatch_ms = dispatch_start.elapsed().as_millis();
    let span = info_span!(
        "dispatch",
        channel = %thread_channel.channel_id,
        adapter = adapter.platform(),
    );
    let _enter = span.enter();
    info!(
        thread_key         = %thread_key,
        events_per_dispatch = batch_size,
        packed_block_count  = packed_block_count,
        agent_dispatch_ms   = agent_dispatch_ms,
        tokens_per_event    = ?tokens_per_event,
        wait_ms             = ?wait_ms,
        senders             = ?senders,
        "batch dispatched",
    );
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Rough char-to-token ratio for English-ish text. Coarse on purpose — the goal
/// is a guard rail for `max_batch_tokens`, not an exact pre-flight.
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;
/// Conservative per-image token budget. Larger than typical Claude image cost
/// so the cap trips before we hand the model an oversized batch.
const TOKENS_PER_IMAGE_ESTIMATE: usize = 512;

/// Rough token estimate for a buffered message (used for `max_batch_tokens` cap).
/// Intentionally coarse — the goal is a guard rail, not an exact pre-flight.
pub fn estimate_tokens(prompt: &str, extra_blocks: &[ContentBlock]) -> usize {
    let text_tokens = prompt.len() / CHARS_PER_TOKEN_ESTIMATE + 1;
    let block_tokens: usize = extra_blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.len() / CHARS_PER_TOKEN_ESTIMATE + 1,
            ContentBlock::Image { .. } => TOKENS_PER_IMAGE_ESTIMATE,
        })
        .sum();
    text_tokens + block_tokens
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_empty() {
        assert!(estimate_tokens("", &[]) >= 1);
    }

    #[test]
    fn estimate_tokens_text() {
        // 400 chars ≈ 100 tokens
        let s = "a".repeat(400);
        assert_eq!(estimate_tokens(&s, &[]), 101);
    }

    #[test]
    fn estimate_tokens_image_block() {
        let blocks = vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: "base64data".into(),
        }];
        assert_eq!(estimate_tokens("", &blocks), 1 + 512);
    }

    #[test]
    fn pack_arrival_event_single() {
        let blocks =
            AdapterRouter::pack_arrival_event(r#"{"schema":"openab.sender.v1"}"#, "hello", vec![]);
        // sender_context delimiter + prompt = 2 blocks
        assert_eq!(blocks.len(), 2);
        if let ContentBlock::Text { text } = &blocks[0] {
            assert!(text.contains("<sender_context>"));
            assert!(text.contains("</sender_context>"));
            // Header is delimiter only — prompt lives in its own block.
            assert!(!text.contains("hello"));
        } else {
            panic!("expected Text delimiter block");
        }
        if let ContentBlock::Text { text } = &blocks[1] {
            assert_eq!(text, "hello");
        } else {
            panic!("expected Text prompt block");
        }
    }

    #[test]
    fn pack_arrival_event_with_extra_blocks() {
        let extra = vec![
            ContentBlock::Text {
                text: "[Voice transcript]: hi".into(),
            },
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "abc".into(),
            },
        ];
        let blocks = AdapterRouter::pack_arrival_event("{}", "prompt", extra);
        // delimiter + transcript + prompt + image = 4 blocks
        assert_eq!(blocks.len(), 4);
        assert!(
            matches!(&blocks[0], ContentBlock::Text { text } if text.contains("<sender_context>"))
        );
        assert!(
            matches!(&blocks[1], ContentBlock::Text { text } if text.contains("Voice transcript"))
        );
        assert!(matches!(&blocks[2], ContentBlock::Text { text } if text == "prompt"));
        assert!(matches!(&blocks[3], ContentBlock::Image { .. }));
    }

    #[test]
    fn pack_arrival_event_batch_n2() {
        // Two arrival events concatenated → 2 (header + prompt) pairs = 4 blocks.
        let mut all: Vec<ContentBlock> = Vec::new();
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"ts":"T1"}"#,
            "msg1",
            vec![],
        ));
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"ts":"T2"}"#,
            "msg2",
            vec![],
        ));
        assert_eq!(all.len(), 4);
        if let ContentBlock::Text { text } = &all[0] {
            assert!(text.contains(r#""ts":"T1""#));
            assert!(!text.contains("msg1"));
        }
        if let ContentBlock::Text { text } = &all[1] {
            assert_eq!(text, "msg1");
        }
        if let ContentBlock::Text { text } = &all[2] {
            assert!(text.contains(r#""ts":"T2""#));
            assert!(!text.contains("msg2"));
        }
        if let ContentBlock::Text { text } = &all[3] {
            assert_eq!(text, "msg2");
        }
    }

    // ADR §3.6 Scenario B — text in one message, image in the next, same author.
    // Broker preserves structural truth: image stays in M2 alone, both messages
    // carry the same sender_id so the agent can semantically link them.
    #[test]
    fn pack_arrival_event_scenario_b_image_in_separate_message() {
        let mut all: Vec<ContentBlock> = Vec::new();
        // M1 (alice): "see this image"
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"sender_id":"A","ts":"T1"}"#,
            "see this image",
            vec![],
        ));
        // M2 (alice): image, no text
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"sender_id":"A","ts":"T2"}"#,
            "",
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "imgB".into(),
            }],
        ));
        // header(M1) + prompt(M1) + header(M2) + image(M2) = 4 blocks
        // (M2 has empty prompt, so its prompt block is omitted)
        assert_eq!(all.len(), 4);
        if let ContentBlock::Text { text } = &all[0] {
            assert!(text.contains(r#""sender_id":"A""#));
            assert!(text.contains(r#""ts":"T1""#));
        } else {
            panic!("expected Text delimiter for M1");
        }
        if let ContentBlock::Text { text } = &all[1] {
            assert_eq!(text, "see this image");
        } else {
            panic!("expected Text prompt for M1");
        }
        if let ContentBlock::Text { text } = &all[2] {
            assert!(text.contains(r#""ts":"T2""#));
        } else {
            panic!("expected Text delimiter for M2");
        }
        // M2's image follows immediately after its delimiter (no prompt block).
        assert!(matches!(&all[3], ContentBlock::Image { .. }));
    }

    // ADR §3.6 Scenario C — fragmented multi-author batch.
    // Repeated sender_id is preserved across non-adjacent messages; bob's interjection
    // is kept as-is (no silent drop, no temporal reorder).
    #[test]
    fn pack_arrival_event_scenario_c_multi_author_interleaved() {
        let mut all: Vec<ContentBlock> = Vec::new();
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"sender_id":"A","ts":"T1"}"#,
            "see this image",
            vec![],
        ));
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"sender_id":"B","ts":"T2"}"#,
            "what?",
            vec![],
        ));
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"sender_id":"A","ts":"T3"}"#,
            "",
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "imgC".into(),
            }],
        ));
        // M1: header + prompt = 2 blocks
        // M2: header + prompt = 2 blocks
        // M3: header + image = 2 blocks (empty prompt → no prompt block)
        // total = 6
        assert_eq!(all.len(), 6);
        let h1 = match &all[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected Text delimiter for M1"),
        };
        let p1 = match &all[1] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected Text prompt for M1"),
        };
        let h2 = match &all[2] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected Text delimiter for M2"),
        };
        let p2 = match &all[3] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected Text prompt for M2"),
        };
        let h3 = match &all[4] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected Text delimiter for M3"),
        };
        assert!(h1.contains(r#""sender_id":"A""#) && h1.contains(r#""ts":"T1""#));
        assert_eq!(p1, "see this image");
        assert!(h2.contains(r#""sender_id":"B""#) && h2.contains(r#""ts":"T2""#));
        assert_eq!(p2, "what?");
        assert!(h3.contains(r#""sender_id":"A""#) && h3.contains(r#""ts":"T3""#));
        // M3's image attached to M3 only.
        assert!(matches!(&all[5], ContentBlock::Image { .. }));
    }

    // ADR §3.6 Scenario D — voice-only message in a batch.
    // Within each arrival, transcript Text blocks precede the prompt block so the
    // agent sees voice content before any typed text. The sender_context delimiter
    // still opens each arrival.
    #[test]
    fn pack_arrival_event_scenario_d_voice_only() {
        let mut all: Vec<ContentBlock> = Vec::new();
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"sender_id":"A","ts":"T1"}"#,
            "look at this",
            vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "scr".into(),
            }],
        ));
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"sender_id":"A","ts":"T2"}"#,
            "",
            vec![ContentBlock::Text {
                text: "[Voice message transcript]: hey can we sync about the deploy".into(),
            }],
        ));
        all.extend(AdapterRouter::pack_arrival_event(
            r#"{"sender_id":"B","ts":"T3"}"#,
            "what?",
            vec![],
        ));
        // M1: header + prompt + image = 3
        // M2: header + transcript = 2 (empty prompt → no prompt block)
        // M3: header + prompt = 2
        // total = 7
        assert_eq!(all.len(), 7);
        if let ContentBlock::Text { text } = &all[0] {
            assert!(text.contains(r#""ts":"T1""#));
            assert!(!text.contains("look at this"));
        }
        if let ContentBlock::Text { text } = &all[1] {
            assert_eq!(text, "look at this");
        }
        assert!(matches!(&all[2], ContentBlock::Image { .. }));
        if let ContentBlock::Text { text } = &all[3] {
            assert!(text.contains(r#""ts":"T2""#));
        }
        // Transcript precedes prompt (and prompt is omitted here because empty).
        if let ContentBlock::Text { text } = &all[4] {
            assert!(text.contains("Voice message transcript"));
            assert!(text.contains("sync about the deploy"));
        } else {
            panic!("expected transcript Text block after M2 delimiter");
        }
        if let ContentBlock::Text { text } = &all[5] {
            assert!(text.contains(r#""sender_id":"B""#));
        }
        if let ContentBlock::Text { text } = &all[6] {
            assert_eq!(text, "what?");
        }
    }

    // Token-cap math: a single message that already exceeds max_batch_tokens still
    // dispatches alone (the consumer_loop logic admits the first message before
    // checking the cap). Verifies estimate_tokens scales with input length.
    #[test]
    fn estimate_tokens_oversized_single_message() {
        // ~24k token text (96000 chars / 4 chars-per-token).
        let big = "x".repeat(96_000);
        let est = estimate_tokens(&big, &[]);
        assert!(est > 24_000, "expected >24k tokens, got {est}");
    }

    // Cumulative token math: two messages whose sum exceeds max_batch_tokens.
    // The consumer_loop reads first, then peeks at the next; if cumulative tokens
    // > cap, the second is held over to the next batch (FIFO preserved).
    #[test]
    fn estimate_tokens_cumulative_exceeds_cap() {
        let max_tokens = 24_000_usize;
        let m1 = estimate_tokens(&"a".repeat(80_000), &[]);
        let m2 = estimate_tokens(&"b".repeat(50_000), &[]);
        assert!(m1 < max_tokens);
        assert!(m1 + m2 > max_tokens, "{m1} + {m2} should exceed cap");
    }

    // ADR §2.5 race-safe eviction. The full SendError path requires a real
    // AdapterRouter (concrete struct, not a trait — no easy mock seam), so we
    // unit-test the eviction predicate in isolation. End-to-end consumer-death
    // recovery is exercised by the manual staging smoke documented in the ADR.
    fn dummy_handle(generation: u64) -> ThreadHandle {
        let (tx, _rx) = tokio::sync::mpsc::channel::<u64>(1);
        let consumer = tokio::spawn(async {});
        ThreadHandle {
            tx,
            queue: Arc::new(PendingQueue::default()),
            activity: Arc::new(QueueActivity::default()),
            consumer,
            generation,
            channel_id: "C".into(),
            thread_channel: make_channel("C"),
            adapter_kind: "discord".into(),
        }
    }

    #[tokio::test]
    async fn try_evict_locked_removes_when_generation_matches() {
        let mut map: HashMap<String, ThreadHandle> = HashMap::new();
        map.insert("t".into(), dummy_handle(7));
        assert!(Dispatcher::try_evict_locked(&mut map, "t", 7));
        assert!(map.is_empty());
    }

    // The bug §2.5 prevents: a stale producer (my_gen=7) observing SendError
    // must not remove a freshly inserted handle (gen=8) created by another
    // submit between the failed send and the eviction attempt.
    #[tokio::test]
    async fn try_evict_locked_keeps_when_generation_differs() {
        let mut map: HashMap<String, ThreadHandle> = HashMap::new();
        map.insert("t".into(), dummy_handle(8));
        assert!(!Dispatcher::try_evict_locked(&mut map, "t", 7));
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("t").unwrap().generation, 8);
    }

    #[tokio::test]
    async fn try_evict_locked_returns_false_when_absent() {
        let mut map: HashMap<String, ThreadHandle> = HashMap::new();
        assert!(!Dispatcher::try_evict_locked(&mut map, "missing", 0));
    }

    // BatchGrouping → thread_key shape.
    fn make_dispatcher(grouping: BatchGrouping) -> Dispatcher {
        // The router is wrapped in Arc but never used by `key()` itself; we use
        // a dummy AdapterRouter built via the same path main.rs would use.
        // For a pure-keying test we'd ideally not need it, but the constructor demands one.
        // Construct a minimal router via the public test helpers in adapter.rs if available;
        // otherwise we fall back to building one with a dummy SessionPool.
        use crate::acp::SessionPool;
        let agent_cfg = crate::config::AgentConfig {
            command: "/bin/true".into(),
            args: vec![],
            working_dir: "/tmp".into(),
            env: std::collections::HashMap::new(),
            inherit_env: vec![],
            command_explicit: true,
        };
        let pool = Arc::new(SessionPool::new(
            agent_cfg,
            1,
            crate::config::default_prompt_hard_timeout_secs()
                .saturating_add(crate::config::default_hung_grace_secs()),
            HashMap::new(),
        ));
        let router = Arc::new(AdapterRouter::new(
            pool,
            crate::config::ReactionsConfig::default(),
            crate::markdown::TableMode::Off,
            crate::config::default_prompt_hard_timeout_secs(),
            crate::config::default_liveness_check_secs(),
            crate::adapter::WorkspaceRouting {
                aliases: std::collections::HashMap::new(),
                channels: std::collections::HashMap::new(),
                bot_home: std::path::PathBuf::from("/tmp"),
                root: std::path::PathBuf::from("/tmp"),
            },
        ));
        Dispatcher::with_idle_timeout(router, 10, 24_000, grouping, DEFAULT_CONSUMER_IDLE_TIMEOUT)
    }

    #[tokio::test]
    async fn key_per_thread_ignores_sender() {
        let d = make_dispatcher(BatchGrouping::Thread);
        assert_eq!(d.key("discord", "T1", "userA"), "discord:T1");
        assert_eq!(d.key("discord", "T1", "userB"), "discord:T1");
    }

    #[tokio::test]
    async fn key_per_lane_includes_sender() {
        let d = make_dispatcher(BatchGrouping::Lane);
        assert_eq!(d.key("discord", "T1", "userA"), "discord:T1:userA");
        assert_eq!(d.key("discord", "T1", "userB"), "discord:T1:userB");
        // Different threads remain distinct.
        assert_eq!(d.key("slack", "T2", "userA"), "slack:T2:userA");
    }

    fn insert_dummy_handle(d: &Dispatcher, key: &str) -> Arc<PendingQueue> {
        let (tx, _rx) = tokio::sync::mpsc::channel::<u64>(10);
        let consumer = tokio::spawn(async {});
        let queue = Arc::new(PendingQueue::default());
        let handle = ThreadHandle {
            tx,
            queue: Arc::clone(&queue),
            activity: Arc::new(QueueActivity::default()),
            consumer,
            generation: 0,
            channel_id: "c".into(),
            thread_channel: make_channel("c"),
            adapter_kind: "discord".into(),
        };
        d.per_thread.lock().unwrap().insert(key.to_string(), handle);
        queue
    }

    #[tokio::test]
    async fn cancel_buffered_thread_drops_per_thread_key() {
        let d = make_dispatcher(BatchGrouping::Thread);
        let queue = insert_dummy_handle(&d, "discord:T1");
        let _ = insert_dummy_handle(&d, "discord:T2"); // different thread, must survive
        queue.insert(1, make_msg("drop me", 10), false);
        let activity = d
            .per_thread
            .lock()
            .unwrap()
            .get("discord:T1")
            .map(|handle| Arc::clone(&handle.activity))
            .unwrap();
        activity.set_active(&[QueuedMessage {
            id: 2,
            message: make_msg("active", 10),
            recovered_from_active: false,
        }]);
        assert!(d.claim_active_for_replace("discord", "T1", 2));
        assert_eq!(d.cancel_buffered_thread("discord", "T1"), 1);
        assert!(queue.list().is_empty());
        assert!(!activity.inner.lock().unwrap().replace_paused);
        assert!(d.replace_claims.lock().unwrap().is_empty());
        let map = d.per_thread.lock().unwrap();
        assert!(!map.contains_key("discord:T1"));
        assert!(map.contains_key("discord:T2"));
    }

    #[tokio::test]
    async fn cancel_buffered_thread_drops_all_lanes() {
        let d = make_dispatcher(BatchGrouping::Lane);
        let _ = insert_dummy_handle(&d, "discord:T1:userA");
        let _ = insert_dummy_handle(&d, "discord:T1:userB");
        let _ = insert_dummy_handle(&d, "discord:T2:userA"); // different thread
        let _ = insert_dummy_handle(&d, "slack:T1:userA"); // different platform
        d.cancel_buffered_thread("discord", "T1");
        let map = d.per_thread.lock().unwrap();
        assert!(!map.contains_key("discord:T1:userA"));
        assert!(!map.contains_key("discord:T1:userB"));
        assert!(map.contains_key("discord:T2:userA"));
        assert!(map.contains_key("slack:T1:userA"));
    }

    #[tokio::test]
    async fn cancel_buffered_thread_does_not_match_thread_id_prefix() {
        // T1 must not match T10 / T11 (substring trap).
        let d = make_dispatcher(BatchGrouping::Lane);
        let _ = insert_dummy_handle(&d, "discord:T1:userA");
        let _ = insert_dummy_handle(&d, "discord:T10:userA");
        d.cancel_buffered_thread("discord", "T1");
        let map = d.per_thread.lock().unwrap();
        assert!(!map.contains_key("discord:T1:userA"));
        assert!(map.contains_key("discord:T10:userA"));
    }

    #[tokio::test]
    async fn pending_queue_can_list_edit_remove_and_clear_across_lanes() {
        let d = make_dispatcher(BatchGrouping::Lane);
        let queue_a = insert_dummy_handle(&d, "discord:T1:userA");
        let queue_b = insert_dummy_handle(&d, "discord:T1:userB");
        queue_a.insert(2, make_msg("second", 10), false);
        queue_b.insert(1, make_msg("first", 10), false);

        let pending = d.pending_messages("discord", "T1");
        assert_eq!(
            pending.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(d.edit_pending_message("discord", "T1", 2, "updated"));
        assert_eq!(
            d.pending_messages("discord", "T1")[1].prompt,
            "updated"
        );
        assert!(d.remove_pending_message("discord", "T1", 1));
        assert!(!d.remove_pending_message("discord", "T1", 1));
        queue_b.insert(3, make_msg("new arrival", 10), false);
        assert_eq!(
            d.clear_pending_messages_through("discord", "T1", 2),
            1
        );
        assert_eq!(d.pending_messages("discord", "T1")[0].id, 3);
        assert_eq!(d.clear_pending_messages("discord", "T1"), 1);
        assert!(d.pending_messages("discord", "T1").is_empty());
    }

    #[tokio::test]
    async fn pending_queue_move_to_front_changes_thread_dispatch_order() {
        let d = make_dispatcher(BatchGrouping::Thread);
        let queue = insert_dummy_handle(&d, "discord:T1");
        queue.insert(1, make_msg("first", 10), false);
        queue.insert(2, make_msg("second", 10), false);
        queue.insert(3, make_msg("third", 10), false);

        assert!(d.move_pending_message_to_front("discord", "T1", 3));
        assert_eq!(
            d.pending_messages("discord", "T1")
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
    }

    #[tokio::test]
    async fn active_replace_claim_pauses_lane_and_inserts_replacement_first() {
        let d = make_dispatcher(BatchGrouping::Thread);
        let queue = insert_dummy_handle(&d, "discord:T1");
        queue.insert(20, make_msg("existing pending", 10), false);
        let activity = d
            .per_thread
            .lock()
            .unwrap()
            .get("discord:T1")
            .map(|handle| Arc::clone(&handle.activity))
            .unwrap();
        activity.set_active(&[QueuedMessage {
            id: 10,
            message: make_msg("active", 10),
            recovered_from_active: false,
        }]);

        assert_eq!(d.active_messages("discord", "T1")[0].id, 10);
        assert!(d.claim_active_for_replace("discord", "T1", 10));
        assert!(!d.claim_active_for_replace("discord", "T1", 10));
        let replacement_id = d
            .enqueue_claimed_replacement(10, make_msg("replacement", 10))
            .unwrap();
        assert_eq!(
            d.pending_messages("discord", "T1")[0].id,
            replacement_id
        );
        assert!(activity.inner.lock().unwrap().replace_paused);
        let waiter_activity = Arc::clone(&activity);
        let waiter = tokio::spawn(async move { waiter_activity.wait_until_unpaused().await });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        d.release_active_replace(10);
        assert!(!activity.inner.lock().unwrap().replace_paused);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("replace release should wake the lane")
            .unwrap();
    }

    #[test]
    fn active_queue_snapshot_preserves_restart_recovery_marker() {
        let activity = QueueActivity::default();
        activity.set_active(&[QueuedMessage {
            id: 10,
            message: make_msg("replayed request", 10),
            recovered_from_active: true,
        }]);

        let active = activity.list();
        assert_eq!(active.len(), 1);
        assert!(active[0].recovered_from_active);
    }

    #[test]
    fn persistent_active_snapshot_preserves_restart_recovery_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let store = QueueStore::load(path.clone());
        let channel = make_channel("T1");
        store.mark_active(
            "mock:T1",
            "mock",
            &channel,
            Vec::new(),
            &[QueuedMessage {
                id: 10,
                message: make_msg("replayed request", 10),
                recovered_from_active: true,
            }],
        );
        drop(store);

        let restored = QueueStore::load(path);
        let lane = restored.lanes_for_adapter("mock").remove(0);
        assert!(lane.active[0].recovered_from_active);
    }

    #[test]
    fn persistent_queue_requeues_active_before_pending_and_preserves_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let store = QueueStore::load(path.clone());
        let channel = make_channel("T1");
        let queue = PendingQueue::default();
        let mut pending = make_msg("pending", 10);
        pending.extra_blocks.push(ContentBlock::Image {
            media_type: "image/png".into(),
            data: "base64-data".into(),
        });
        queue.insert(12, pending, false);
        let active = vec![QueuedMessage {
            id: 11,
            message: make_msg("active", 10),
            recovered_from_active: false,
        }];
        store.record_next_message_id(13);
        store.mark_active("mock:T1", "mock", &channel, queue.snapshot(), &active);
        drop(store);

        let restored = QueueStore::load(path.clone());
        assert_eq!(
            restored.summaries(),
            vec![PersistedQueueSummary {
                platform: "mock".into(),
                thread_id: "T1".into(),
                queued_messages: 2,
                recovered_active_messages: 1,
            }]
        );
        let lane = restored.lanes_for_adapter("mock").remove(0);
        restored.mark_requeued(&lane);
        drop(restored);

        let reloaded = QueueStore::load(path);
        let lane = reloaded.lanes_for_adapter("mock").remove(0);
        assert!(lane.active.is_empty());
        assert_eq!(
            lane.pending
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert!(lane.pending[0].recovered_from_active);
        assert!(!lane.pending[1].recovered_from_active);
        assert!(matches!(
            lane.pending[1].extra_blocks.first(),
            Some(PersistedContentBlock::Image { media_type, data })
                if media_type == "image/png" && data == "base64-data"
        ));
        assert_eq!(reloaded.next_message_id(), 13);
        assert!(!dir.path().join("queue.json.tmp").exists());
    }

    #[test]
    fn write_snapshot_skips_a_superseded_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let store = QueueStore::load(path.clone());
        let channel = make_channel("T1");
        let queue = PendingQueue::default();
        queue.insert(1, make_msg("queued", 10), false);

        // Writes sequence 1 and records it as the newest state on disk.
        store.update_pending("mock:T1", "mock", &channel, queue.snapshot());
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("queued"));

        // Serialisation now happens outside the lanes lock, so an older
        // snapshot can reach `write_snapshot` after a newer one already
        // landed. It must be dropped rather than roll the file back.
        let empty = serde_json::to_vec_pretty(&PersistedQueueFile {
            version: QUEUE_STORE_VERSION,
            next_message_id: 1,
            lanes: Vec::new(),
        })
        .unwrap();
        store.write_snapshot(1, &empty);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), persisted);

        // A genuinely newer sequence still writes.
        store.write_snapshot(2, &empty);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("queued"));
    }

    #[tokio::test]
    async fn dispatcher_restores_and_drains_persistent_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let store = QueueStore::load(path.clone());
        let channel = make_channel("T1");
        let queue = PendingQueue::default();
        queue.insert(2, make_msg("pending", 10), false);
        store.mark_active(
            "mock:T1",
            "mock",
            &channel,
            queue.snapshot(),
            &[QueuedMessage {
                id: 1,
                message: make_msg("interrupted", 10),
                recovered_from_active: false,
            }],
        );
        drop(store);

        let mock = Arc::new(MockDispatchTarget::new());
        let target: Arc<dyn DispatchTarget> = mock.clone();
        let dispatcher = Dispatcher::with_idle_timeout(
            target,
            1,
            24_000,
            BatchGrouping::Thread,
            Duration::from_secs(60),
        )
        .with_persistence(path);
        let adapter: Arc<dyn ChatAdapter> = Arc::new(MockChatAdapter);
        assert_eq!(dispatcher.restore_persisted(adapter), 2);

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.calls().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("restored requests should dispatch");
        let calls = mock.calls();
        assert!(calls[0]
            .text_blocks
            .iter()
            .any(|text| text == "interrupted"));
        assert!(calls[1]
            .text_blocks
            .iter()
            .any(|text| text == "pending"));

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if dispatcher.persisted_queue_summaries().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("completed restored requests should leave no durable queue");
        dispatcher.shutdown();
    }

    #[tokio::test]
    async fn shutdown_preserves_persistent_pending_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let dispatcher = make_dispatcher(BatchGrouping::Thread).with_persistence(path.clone());
        let queue = insert_dummy_handle(&dispatcher, "discord:T1");
        queue.insert(1, make_msg("keep after shutdown", 10), false);
        dispatcher.persist_pending_queue(
            "discord:T1",
            "discord",
            &make_channel("T1"),
            &queue,
        );

        dispatcher.shutdown();

        let restored = QueueStore::load(path);
        assert_eq!(restored.summaries()[0].queued_messages, 1);
        assert_eq!(
            restored.lanes_for_adapter("discord")[0].pending[0].prompt,
            "keep after shutdown"
        );
    }

    #[tokio::test]
    async fn cancel_removes_persistent_pending_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let dispatcher = make_dispatcher(BatchGrouping::Thread).with_persistence(path.clone());
        let queue = insert_dummy_handle(&dispatcher, "discord:T1");
        queue.insert(1, make_msg("discard", 10), false);
        dispatcher.persist_pending_queue(
            "discord:T1",
            "discord",
            &make_channel("T1"),
            &queue,
        );

        assert_eq!(dispatcher.cancel_buffered_thread("discord", "T1"), 1);
        assert!(QueueStore::load(path).summaries().is_empty());
    }

    // Long-running consumer that parks until aborted — used by sweep_stale /
    // shutdown tests to exercise the "still alive" path.
    fn alive_consumer_handle() -> ThreadHandle {
        let (tx, _rx) = tokio::sync::mpsc::channel::<u64>(10);
        let consumer = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        ThreadHandle {
            tx,
            queue: Arc::new(PendingQueue::default()),
            activity: Arc::new(QueueActivity::default()),
            consumer,
            generation: 0,
            channel_id: "c".into(),
            thread_channel: make_channel("c"),
            adapter_kind: "discord".into(),
        }
    }

    #[tokio::test]
    async fn sweep_stale_removes_finished_consumers() {
        let d = make_dispatcher(BatchGrouping::Thread);
        let _ = insert_dummy_handle(&d, "discord:T1");
        let _ = insert_dummy_handle(&d, "discord:T2");
        // Yield so the empty-body spawned tasks actually run to completion
        // before is_finished() is checked.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let swept = d.sweep_stale();
        assert_eq!(swept, 2);
        assert!(d.per_thread.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn sweep_stale_keeps_running_consumers() {
        let d = make_dispatcher(BatchGrouping::Thread);
        let abort = {
            let h = alive_consumer_handle();
            let a = h.consumer.abort_handle();
            d.per_thread.lock().unwrap().insert("alive".into(), h);
            a
        };
        let swept = d.sweep_stale();
        assert_eq!(swept, 0);
        assert!(d.per_thread.lock().unwrap().contains_key("alive"));
        // Cleanup so the parked task doesn't linger across tests.
        abort.abort();
    }

    #[tokio::test]
    async fn shutdown_clears_all_handles() {
        let d = make_dispatcher(BatchGrouping::Thread);
        let _ = insert_dummy_handle(&d, "k1");
        let _ = insert_dummy_handle(&d, "k2");
        let _ = insert_dummy_handle(&d, "k3");
        d.shutdown();
        assert!(d.per_thread.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn shutdown_aborts_running_consumers() {
        let d = make_dispatcher(BatchGrouping::Thread);
        let abort = {
            let h = alive_consumer_handle();
            let a = h.consumer.abort_handle();
            d.per_thread.lock().unwrap().insert("k".into(), h);
            a
        };
        d.shutdown();
        // Give the runtime a tick to process abort + map drop.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(abort.is_finished());
    }

    // -----------------------------------------------------------------------
    // consumer_loop / dispatch_batch integration tests (NIT 2)
    //
    // These drive `consumer_loop` directly with a pre-populated mpsc, using
    // `MockDispatchTarget` to record the calls that would otherwise hit a
    // real `AdapterRouter` (and through it, ACP CLI subprocesses). This
    // gives deterministic coverage of the orchestration paths the existing
    // unit tests don't reach: greedy drain, token-cap overflow, idle timeout.
    // -----------------------------------------------------------------------

    /// One recorded `stream_prompt_blocks` invocation.
    #[derive(Clone)]
    struct RecordedDispatch {
        block_count: usize,
        text_blocks: Vec<String>,
        other_bot_present: bool,
        dispatch_channel: ChannelRef,
    }

    /// Mock `DispatchTarget` — records calls; never touches a real session pool.
    struct MockDispatchTarget {
        reactions: ReactionsConfig,
        calls: Mutex<Vec<RecordedDispatch>>,
        aliases: std::collections::HashMap<String, String>,
        bot_home: std::path::PathBuf,
        workspace_root: std::path::PathBuf,
        channel_workspace: Option<String>,
        ensured_workdirs: Mutex<Vec<Option<String>>>,
        /// If set, `ensure_session` returns this error once.
        ensure_err: Mutex<Option<String>>,
        /// If set, `stream_prompt_blocks` returns this error once.
        stream_err: Mutex<Option<String>>,
    }

    impl MockDispatchTarget {
        fn new() -> Self {
            Self {
                reactions: ReactionsConfig::default(),
                calls: Mutex::new(Vec::new()),
                aliases: std::collections::HashMap::new(),
                bot_home: std::path::PathBuf::from("/tmp"),
                workspace_root: std::path::PathBuf::from("/tmp"),
                channel_workspace: None,
                ensured_workdirs: Mutex::new(Vec::new()),
                ensure_err: Mutex::new(None),
                stream_err: Mutex::new(None),
            }
        }

        fn calls(&self) -> Vec<RecordedDispatch> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DispatchTarget for MockDispatchTarget {
        fn reactions_config(&self) -> &ReactionsConfig {
            &self.reactions
        }

        fn workspace_aliases(&self) -> std::collections::HashMap<String, String> {
            self.aliases.clone()
        }

        fn bot_home(&self) -> std::path::PathBuf {
            self.bot_home.clone()
        }

        fn workspace_root(&self) -> std::path::PathBuf {
            self.workspace_root.clone()
        }

        fn channel_workspace_spec(&self, _channel: &ChannelRef) -> Option<String> {
            self.channel_workspace.clone()
        }

        async fn ensure_session(
            &self,
            _session_key: &str,
            working_dir: Option<&str>,
        ) -> Result<bool> {
            self.ensured_workdirs
                .lock()
                .unwrap()
                .push(working_dir.map(str::to_string));
            if let Some(msg) = self.ensure_err.lock().unwrap().take() {
                return Err(anyhow::anyhow!(msg));
            }
            Ok(true)
        }

        async fn reset_session(&self, _session_key: &str) {}

        async fn stream_prompt_blocks(
            &self,
            _adapter: &Arc<dyn ChatAdapter>,
            _session_key: &str,
            content_blocks: Vec<ContentBlock>,
            thread_channel: &ChannelRef,
            _reactions: Arc<StatusReactionController>,
            other_bot_present: bool,
            _recipient: Option<(String, String)>,
        ) -> Result<()> {
            let text_blocks = content_blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect();
            self.calls.lock().unwrap().push(RecordedDispatch {
                block_count: content_blocks.len(),
                text_blocks,
                other_bot_present,
                dispatch_channel: thread_channel.clone(),
            });
            if let Some(msg) = self.stream_err.lock().unwrap().take() {
                return Err(anyhow::anyhow!(msg));
            }
            Ok(())
        }
    }

    /// Mock `ChatAdapter` — every method is a no-op success. The dispatch loop
    /// invokes `add_reaction` (queued 👀), `platform`, and on the error path
    /// `send_message`; nothing else needs real behavior here.
    struct MockChatAdapter;

    #[async_trait]
    impl ChatAdapter for MockChatAdapter {
        fn platform(&self) -> &'static str {
            "mock"
        }
        fn message_limit(&self) -> usize {
            2000
        }

        async fn send_message(&self, channel: &ChannelRef, _content: &str) -> Result<MessageRef> {
            Ok(MessageRef {
                channel: channel.clone(),
                message_id: "mock-msg".into(),
            })
        }

        async fn create_thread(
            &self,
            channel: &ChannelRef,
            _trigger_msg: &MessageRef,
            _title: &str,
        ) -> Result<ChannelRef> {
            Ok(channel.clone())
        }

        async fn add_reaction(&self, _msg: &MessageRef, _emoji: &str) -> Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _msg: &MessageRef, _emoji: &str) -> Result<()> {
            Ok(())
        }
        fn use_streaming(&self, _other_bot_present: bool) -> bool {
            false
        }
    }

    fn make_channel(thread: &str) -> ChannelRef {
        ChannelRef {
            platform: "mock".into(),
            channel_id: thread.into(),
            thread_id: Some(thread.into()),
            parent_id: None,
            origin_event_id: None,
        }
    }

    fn make_msg(prompt: &str, tokens: usize) -> BufferedMessage {
        BufferedMessage {
            sender_json: r#"{"schema":"openab.sender.v1","sender_id":"u","sender_name":"u"}"#
                .into(),
            sender_name: "u".into(),
            prompt: prompt.into(),
            extra_blocks: vec![],
            trigger_msg: MessageRef {
                channel: make_channel("T"),
                message_id: format!("m-{prompt}"),
            },
            arrived_at: Instant::now(),
            estimated_tokens: tokens,
            other_bot_present: false,
            recipient: None,
        }
    }

    /// Pre-load `msgs` into a fresh mpsc, drop the sender, and run
    /// `consumer_loop` to completion. Returns the recorded dispatches.
    async fn run_consumer_with_messages(
        msgs: Vec<BufferedMessage>,
        max_batch: usize,
        max_tokens: usize,
    ) -> Vec<RecordedDispatch> {
        let mock = Arc::new(MockDispatchTarget::new());
        let target: Arc<dyn DispatchTarget> = mock.clone();
        let adapter: Arc<dyn ChatAdapter> = Arc::new(MockChatAdapter);
        let (tx, rx) = tokio::sync::mpsc::channel::<u64>(msgs.len().max(1));
        let queue = Arc::new(PendingQueue::default());
        for (index, message) in msgs.into_iter().enumerate() {
            let id = index as u64 + 1;
            queue.insert(id, message, false);
            tx.send(id).await.unwrap();
        }
        drop(tx);

        consumer_loop(
            "mock:T".into(),
            make_channel("T"),
            rx,
            queue,
            Arc::new(QueueActivity::default()),
            target,
            adapter,
            max_batch,
            max_tokens,
            Duration::from_secs(60),
            None,
        )
        .await;

        mock.calls()
    }

    #[tokio::test]
    async fn consumer_skips_queue_ids_removed_before_dispatch() {
        let mock = Arc::new(MockDispatchTarget::new());
        let target: Arc<dyn DispatchTarget> = mock.clone();
        let adapter: Arc<dyn ChatAdapter> = Arc::new(MockChatAdapter);
        let queue = Arc::new(PendingQueue::default());
        let (tx, rx) = tokio::sync::mpsc::channel::<u64>(2);
        queue.insert(1, make_msg("removed", 10), false);
        queue.insert(2, make_msg("kept", 10), false);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        assert!(queue.remove(1));
        drop(tx);

        consumer_loop(
            "mock:T".into(),
            make_channel("T"),
            rx,
            queue,
            Arc::new(QueueActivity::default()),
            target,
            adapter,
            10,
            24_000,
            Duration::from_secs(60),
            None,
        )
        .await;

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].block_count, 2);
    }

    #[tokio::test]
    async fn consumer_dispatches_promoted_message_next() {
        let mock = Arc::new(MockDispatchTarget::new());
        let target: Arc<dyn DispatchTarget> = mock.clone();
        let adapter: Arc<dyn ChatAdapter> = Arc::new(MockChatAdapter);
        let queue = Arc::new(PendingQueue::default());
        let (tx, rx) = tokio::sync::mpsc::channel::<u64>(3);
        queue.insert(1, make_msg("first", 10), false);
        queue.insert(2, make_msg("second", 10), false);
        queue.insert(3, make_msg("promoted", 10), false);
        assert!(queue.move_to_front(3));
        for id in 1..=3 {
            tx.send(id).await.unwrap();
        }
        drop(tx);

        consumer_loop(
            "mock:T".into(),
            make_channel("T"),
            rx,
            queue,
            Arc::new(QueueActivity::default()),
            target,
            adapter,
            1,
            24_000,
            Duration::from_secs(60),
            None,
        )
        .await;

        let calls = mock.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].text_blocks.iter().any(|text| text == "promoted"));
    }

    #[tokio::test]
    async fn consumer_dispatches_single_message_as_one_batch() {
        let calls = run_consumer_with_messages(vec![make_msg("hi", 10)], 10, 24_000).await;
        assert_eq!(calls.len(), 1);
        // pack_arrival_event with no extra_blocks → delimiter + prompt = 2 blocks.
        assert_eq!(calls[0].block_count, 2);
        assert!(!calls[0].other_bot_present);
    }

    #[tokio::test]
    async fn channel_binding_selects_workspace_for_new_session() {
        let tmp = tempfile::tempdir().unwrap();
        let projects_root = tmp.path().join("projects");
        let project = projects_root.join("openab");
        std::fs::create_dir_all(&project).unwrap();

        let mut aliases = std::collections::HashMap::new();
        aliases.insert("openab".to_string(), project.display().to_string());
        let mock = Arc::new(MockDispatchTarget {
            aliases,
            bot_home: tmp.path().to_path_buf(),
            workspace_root: projects_root,
            channel_workspace: Some("@openab".to_string()),
            ..MockDispatchTarget::new()
        });
        let target: Arc<dyn DispatchTarget> = mock.clone();
        let adapter: Arc<dyn ChatAdapter> = Arc::new(MockChatAdapter);
        let channel = ChannelRef {
            platform: "discord".into(),
            channel_id: "thread-1".into(),
            thread_id: None,
            parent_id: Some("project-channel".into()),
            origin_event_id: None,
        };

        dispatch_batch(
            "discord:thread-1",
            &channel,
            &target,
            &adapter,
            vec![make_msg("fix the tests", 10)],
            false,
        )
        .await;

        assert_eq!(
            mock.ensured_workdirs.lock().unwrap().as_slice(),
            &[Some(project.canonicalize().unwrap().display().to_string())]
        );
    }

    #[tokio::test]
    async fn consumer_greedy_drain_combines_queued_messages_into_one_batch() {
        // 3 messages already in the queue when the consumer wakes → greedy
        // drain pulls all 3, packs them into one batch, dispatches once.
        let calls = run_consumer_with_messages(
            vec![make_msg("a", 50), make_msg("b", 50), make_msg("c", 50)],
            10,
            24_000,
        )
        .await;
        assert_eq!(calls.len(), 1, "expected a single batched dispatch");
        // 3 arrivals × (delimiter + prompt) = 6 blocks.
        assert_eq!(calls[0].block_count, 6);
    }

    #[tokio::test]
    async fn consumer_token_cap_splits_batch_preserving_fifo() {
        // max_tokens=100, two 80-token messages → cumulative 160 > 100, so
        // msg2 becomes `pending` and is dispatched in the next batch.
        let calls =
            run_consumer_with_messages(vec![make_msg("a", 80), make_msg("b", 80)], 10, 100).await;
        assert_eq!(calls.len(), 2, "token cap should split into two batches");
        // Each batch holds one arrival → delimiter + prompt = 2 blocks.
        assert_eq!(calls[0].block_count, 2);
        assert_eq!(calls[1].block_count, 2);
    }

    #[tokio::test]
    async fn consumer_dispatch_uses_last_event_origin_event_id_for_merged_batch() {
        let mut first = make_msg("a", 80);
        first.trigger_msg.channel.origin_event_id = Some("evt-first".into());
        let mut second = make_msg("b", 80);
        second.trigger_msg.channel.origin_event_id = Some("evt-second".into());

        let calls = run_consumer_with_messages(vec![first, second], 10, 200).await;
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].dispatch_channel.origin_event_id.as_deref(),
            Some("evt-second")
        );
    }

    #[tokio::test]
    async fn consumer_dispatch_preserves_thread_route_while_refreshing_origin_event_id() {
        let mock = Arc::new(MockDispatchTarget::new());
        let target: Arc<dyn DispatchTarget> = mock.clone();
        let adapter: Arc<dyn ChatAdapter> = Arc::new(MockChatAdapter);
        let (tx, rx) = tokio::sync::mpsc::channel::<u64>(1);
        let queue = Arc::new(PendingQueue::default());

        let mut msg = make_msg("hi", 10);
        msg.trigger_msg.channel = ChannelRef {
            platform: "mock".into(),
            channel_id: "parent-channel".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: Some("evt-fresh".into()),
        };
        queue.insert(1, msg, false);
        tx.send(1).await.unwrap();
        drop(tx);

        consumer_loop(
            "mock:topic-42".into(),
            ChannelRef {
                platform: "mock".into(),
                channel_id: "topic-42".into(),
                thread_id: Some("topic-42".into()),
                parent_id: Some("parent-channel".into()),
                origin_event_id: Some("evt-stale".into()),
            },
            rx,
            queue,
            Arc::new(QueueActivity::default()),
            target,
            adapter,
            10,
            24_000,
            Duration::from_secs(60),
            None,
        )
        .await;

        let calls = mock.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].dispatch_channel.channel_id, "topic-42");
        assert_eq!(
            calls[0].dispatch_channel.thread_id.as_deref(),
            Some("topic-42")
        );
        assert_eq!(
            calls[0].dispatch_channel.parent_id.as_deref(),
            Some("parent-channel")
        );
        assert_eq!(
            calls[0].dispatch_channel.origin_event_id.as_deref(),
            Some("evt-fresh")
        );
    }

    #[tokio::test]
    async fn consumer_exits_after_idle_timeout_with_no_messages() {
        // No messages ever arrive; consumer should exit once `idle_timeout`
        // elapses. Keep `tx` alive so the exit path is the timeout, not the
        // "all senders dropped" branch.
        let mock = Arc::new(MockDispatchTarget::new());
        let target: Arc<dyn DispatchTarget> = mock.clone();
        let adapter: Arc<dyn ChatAdapter> = Arc::new(MockChatAdapter);
        let (tx, rx) = tokio::sync::mpsc::channel::<u64>(1);
        let queue = Arc::new(PendingQueue::default());
        let consumer = tokio::spawn(consumer_loop(
            "mock:T".into(),
            make_channel("T"),
            rx,
            queue,
            Arc::new(QueueActivity::default()),
            target,
            adapter,
            10,
            24_000,
            Duration::from_millis(50),
            None,
        ));
        // Wait enough for the timeout branch + a tick for the task to finish.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            consumer.is_finished(),
            "consumer should exit after idle timeout"
        );
        // No dispatches should have been recorded.
        assert!(mock.calls().is_empty());
        drop(tx);
    }

    #[tokio::test]
    async fn submit_evicts_dead_handle_and_retries_with_fresh_consumer() {
        // §2.5: if `tx.send()` returns `SendError` (consumer's rx dropped
        // mid-flight), `submit` evicts the stale entry under lock and spawns
        // a fresh consumer. Manufacture this state by inserting a handle
        // whose consumer is still parked but whose rx has been dropped.
        let mock = Arc::new(MockDispatchTarget::new());
        let target: Arc<dyn DispatchTarget> = mock.clone();
        let d = Dispatcher::with_idle_timeout(
            target,
            10,
            24_000,
            BatchGrouping::Thread,
            DEFAULT_CONSUMER_IDLE_TIMEOUT,
        );
        let adapter: Arc<dyn ChatAdapter> = Arc::new(MockChatAdapter);

        let key = "mock:T".to_string();
        let parked = {
            let (tx, rx) = tokio::sync::mpsc::channel::<u64>(10);
            drop(rx); // closes the channel → next tx.send() yields SendError
            let consumer = tokio::spawn(std::future::pending::<()>());
            let abort = consumer.abort_handle();
            let handle = ThreadHandle {
                tx,
                queue: Arc::new(PendingQueue::default()),
                activity: Arc::new(QueueActivity::default()),
                consumer,
                generation: 999,
                channel_id: "T".into(),
                thread_channel: make_channel("T"),
                adapter_kind: "mock".into(),
            };
            d.per_thread.lock().unwrap().insert(key.clone(), handle);
            abort
        };

        d.submit(key, make_channel("T"), adapter, make_msg("hello", 10))
            .await
            .expect("retry should spawn a fresh consumer");
        // Give the freshly spawned consumer time to drain + dispatch.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let calls = mock.calls();
        assert_eq!(
            calls.len(),
            1,
            "fresh consumer should have dispatched the retry"
        );
        // pack_arrival_event with no extra_blocks → delimiter + prompt = 2 blocks.
        assert_eq!(calls[0].block_count, 2);

        parked.abort();
    }
}
