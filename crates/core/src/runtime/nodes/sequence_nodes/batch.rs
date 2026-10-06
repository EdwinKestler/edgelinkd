// Licensed under the Apache License, Version 2.0
// Copyright EdgeLink contributors
// Based on Node-RED 19-batch.js

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{Duration, interval};

use crate::N2linkError;
use crate::runtime::flow::Flow;
use crate::runtime::nodes::*;
use n2link_macro::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchNodeConfig {
    #[serde(default = "default_mode")]
    pub mode: String, // "count", "interval", or "concat"
    #[serde(default = "default_count")]
    pub count: usize, // Number of messages to batch (count mode)
    #[serde(default)]
    pub overlap: usize, // Overlap between batches (count mode)
    #[serde(default)]
    pub interval: f64, // Interval in seconds (interval mode)
    #[serde(rename = "allowEmptySequence", default)]
    pub allow_empty_sequence: bool, // Whether to send empty sequences (interval mode)
    #[serde(default)]
    pub topics: Vec<TopicConfig>, // Topics to batch (concat mode)
    #[serde(rename = "honourParts", default)]
    pub honour_parts: bool, // Whether to honor msg.parts info
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicConfig {
    pub topic: String,
}

fn default_mode() -> String {
    "count".to_string()
}

fn default_count() -> usize {
    1
}

impl Default for BatchNodeConfig {
    fn default() -> Self {
        Self {
            mode: "count".to_string(),
            count: 1,
            overlap: 0,
            interval: 1.0,
            allow_empty_sequence: false,
            topics: Vec::new(),
            honour_parts: false,
        }
    }
}

#[derive(Debug)]
struct PendingGroup {
    messages: Vec<MsgHandle>, // Messages in this group
    count: Option<usize>,     // Expected count for this group
}

#[derive(Debug)]
struct TopicGroups {
    groups: HashMap<String, PendingGroup>, // group_id -> group
    group_ids: Vec<String>,                // Ordered list of group IDs
}

#[derive(Debug)]
#[flow_node("batch", red_name = "batch")]
pub struct BatchNode {
    base: BaseFlowNodeState,
    config: BatchNodeConfig,
    /// Node-RED's `nodeMessageBufferMaxLength`. `0` means the buffer is not capped.
    max_kept_msgs: usize,
    // Pending messages for count mode
    count_pending: Arc<Mutex<Vec<MsgHandle>>>,
    // Pending messages for interval mode
    interval_pending: Arc<Mutex<Vec<MsgHandle>>>,
    interval_task: Mutex<Option<JoinHandle<()>>>,

    // Pending messages for concat mode
    concat_pending: Arc<Mutex<HashMap<String, TopicGroups>>>,
    pending_count: Arc<Mutex<usize>>,
}

impl BatchNode {
    pub fn build(
        flow: &Flow,
        base_node: BaseFlowNodeState,
        config: &RedFlowNodeConfig,
        _options: Option<&config::Config>,
    ) -> crate::Result<Box<dyn FlowNodeBehavior>> {
        let config = BatchNodeConfig::deserialize(&config.rest)?;
        let max_kept_msgs = flow.settings().node_message_buffer_max_length;
        Ok(Box::new(BatchNode {
            base: base_node,
            config,
            max_kept_msgs,
            count_pending: Arc::new(Mutex::new(Vec::new())),
            interval_pending: Arc::new(Mutex::new(Vec::new())),
            interval_task: Mutex::new(None),
            concat_pending: Arc::new(Mutex::new(HashMap::new())),
            pending_count: Arc::new(Mutex::new(0)),
        }))
    }

    /// Create parts info for a batch of messages
    fn create_parts_info(messages: &[MsgHandle], msg_id: &str) -> Vec<(String, usize, usize)> {
        let count = messages.len();
        (0..count).map(|i| (msg_id.to_string(), i, count)).collect()
    }

    /// Set msg.parts for each message in the batch.
    ///
    /// `clone_messages` mirrors upstream's `send_msgs(..., clone_msg)`: when a message is going to
    /// stay in the buffer for the next sequence (count mode with `overlap`), the copy that is sent
    /// has to be a clone, or the next batch would rewrite the `parts` of a message that was
    /// already emitted.
    async fn send_batch(&self, messages: Vec<MsgHandle>, clone_messages: bool) -> Result<Vec<MsgHandle>, N2linkError> {
        if messages.is_empty() {
            return Ok(vec![]);
        }

        let msg_id = {
            let first_msg = messages[0].read().await;
            first_msg.id().map(|id| id.to_string()).unwrap_or_else(|| "unknown".to_string())
        };

        let parts_info = Self::create_parts_info(&messages, &msg_id);
        let mut result = Vec::new();

        for (i, msg_handle) in messages.into_iter().enumerate() {
            // `deep_clone(false)` keeps `_msgid`, which is what Node-RED's `cloneMessage` does and
            // what the sequence id is derived from.
            let msg_handle = if clone_messages { msg_handle.deep_clone(false).await } else { msg_handle };
            let (id, index, count) = &parts_info[i];
            // Set parts property
            {
                let mut msg = msg_handle.write().await;
                let mut parts = BTreeMap::new();
                parts.insert("id".to_string(), Variant::String(id.clone()));
                parts.insert("index".to_string(), Variant::Number(serde_json::Number::from(*index)));
                parts.insert("count".to_string(), Variant::Number(serde_json::Number::from(*count)));
                msg.set("parts".to_string(), Variant::Object(parts));
            }
            result.push(msg_handle);
        }

        Ok(result)
    }

    /// `batch.too-many` from the Node-RED catalog.
    const TOO_MANY: &str = "too many pending messages in batch node";

    async fn finish_ok(&self, msg: MsgHandle, cancel: CancellationToken) {
        self.notify_uow_completed(msg, cancel).await;
    }

    async fn finish_too_many(&self, msg: MsgHandle, cancel: CancellationToken) {
        // `done(error)` reaches the catch node and does not also notify complete.
        self.report_error(Self::TOO_MANY.to_string(), msg, cancel).await;
    }

    /// The last message is the one that crossed the cap.
    async fn finish_overflow(&self, mut pending: Vec<MsgHandle>, cancel: CancellationToken) {
        if let Some(cause) = pending.pop() {
            self.finish_too_many(cause, cancel.clone()).await;
        }
        for msg in pending {
            self.finish_ok(msg, cancel.clone()).await;
        }
    }

    /// Concat groups are not a single queue, so the causing message is matched by id.
    async fn finish_overflow_cause(&self, pending: Vec<MsgHandle>, cause: &MsgHandle, cancel: CancellationToken) {
        let cause_id = cause.read().await.id();
        let mut errored = false;
        for msg in pending {
            let is_cause = !errored && msg.read().await.id() == cause_id;
            if is_cause {
                errored = true;
                self.finish_too_many(msg, cancel.clone()).await;
            } else {
                self.finish_ok(msg, cancel.clone()).await;
            }
        }
    }

    /// `reset` completes every buffered message, then the reset message itself.
    async fn finish_reset(&self, reset_msg: MsgHandle, cancel: CancellationToken) {
        let count_msgs: Vec<MsgHandle> = self.count_pending.lock().await.drain(..).collect();
        let interval_msgs: Vec<MsgHandle> = self.interval_pending.lock().await.drain(..).collect();
        let mut concat_msgs = Vec::new();
        {
            let mut pending = self.concat_pending.lock().await;
            for topic_groups in pending.values() {
                for group in topic_groups.groups.values() {
                    concat_msgs.extend(group.messages.iter().cloned());
                }
            }
            pending.clear();
        }
        *self.pending_count.lock().await = 0;
        for msg in count_msgs.into_iter().chain(interval_msgs).chain(concat_msgs) {
            self.finish_ok(msg, cancel.clone()).await;
        }
        self.finish_ok(reset_msg, cancel).await;
    }

    /// Count mode: batch by count or end-of-sequence.
    ///
    /// A buffered message is completed when it is sent, reset, or dropped by the buffer cap,
    /// which is when Node-RED calls `done()`.
    async fn process_count_mode(
        &self,
        msg_handle: MsgHandle,
        cancel: CancellationToken,
    ) -> Result<Vec<MsgHandle>, N2linkError> {
        let mut eof = false;
        if self.config.honour_parts {
            let msg = msg_handle.read().await;
            if let Some(Variant::Object(parts)) = msg.get("parts")
                && let (Some(Variant::Number(index)), Some(Variant::Number(count))) =
                    (parts.get("index"), parts.get("count"))
            {
                let idx = index.as_u64().unwrap_or(0) as usize;
                let cnt = count.as_u64().unwrap_or(0) as usize;
                if idx + 1 == cnt {
                    eof = true;
                }
            }
        }

        let mut pending = self.count_pending.lock().await;
        let mut pending_count = self.pending_count.lock().await;
        pending.push(msg_handle);
        *pending_count += 1;

        if pending.len() >= self.config.count || eof {
            let batch_size = if eof { pending.len() } else { self.config.count };
            let overlap = self.config.overlap.min(batch_size.saturating_sub(1));
            let batch: Vec<MsgHandle> = pending.drain(..batch_size).collect();

            // Keep overlapping messages for the next batch. An end-of-sequence still emits the
            // whole buffer: that is the behaviour the existing count specs assert.
            if overlap > 0 && !eof {
                let overlap_start = batch_size.saturating_sub(overlap);
                for handle in batch.iter().skip(overlap_start) {
                    pending.push(handle.clone());
                }
            }
            // Upstream resets the counter even when the overlap tail stays buffered.
            *pending_count = 0;
            let leave = if eof || overlap == 0 { batch.len() } else { batch.len().saturating_sub(overlap) };
            let finished: Vec<MsgHandle> = batch.iter().take(leave).cloned().collect();
            drop(pending_count);
            drop(pending);
            // With overlap the tail of this batch stays in the buffer, so the copies that are
            // emitted have to be clones (upstream's is_overlap argument).
            let sent = self.send_batch(batch, overlap > 0).await?;
            for msg in finished {
                self.finish_ok(msg, cancel.clone()).await;
            }
            return Ok(sent);
        }

        if self.max_kept_msgs > 0 && *pending_count > self.max_kept_msgs {
            let drained: Vec<MsgHandle> = pending.drain(..).collect();
            *pending_count = 0;
            drop(pending_count);
            drop(pending);
            self.finish_overflow(drained, cancel).await;
        }

        Ok(vec![])
    }

    /// Send what the interval timer accumulated, or the empty sequence upstream emits when
    /// the node is configured with `allowEmptySequence`.
    async fn flush_interval(self: &Arc<Self>, cancel: CancellationToken) {
        let pending: Vec<MsgHandle> = {
            let mut pending = self.interval_pending.lock().await;
            let mut pending_count = self.pending_count.lock().await;
            *pending_count = 0;
            pending.drain(..).collect()
        };

        if !pending.is_empty() {
            match self.send_batch(pending.clone(), false).await {
                Ok(msgs) => {
                    for msg in msgs {
                        let _ = self.fan_out_one(Envelope { port: 0, msg }, cancel.child_token()).await;
                    }
                    // Upstream calls `done()` after the interval send, not when the message arrived.
                    for msg in pending {
                        self.finish_ok(msg, cancel.clone()).await;
                    }
                }
                Err(e) => log::error!("Failed to send an interval batch: {e}"),
            }
        } else if self.config.allow_empty_sequence {
            let mut parts = BTreeMap::new();
            parts.insert("id".to_string(), Variant::String(Msg::generate_id().to_string()));
            parts.insert("index".to_string(), Variant::Number(serde_json::Number::from(0)));
            parts.insert("count".to_string(), Variant::Number(serde_json::Number::from(1)));
            let msg = MsgHandle::with_payload(Variant::Null);
            msg.write().await.set("parts".to_string(), Variant::Object(parts));
            let _ = self.fan_out_one(Envelope { port: 0, msg }, cancel.child_token()).await;
        }
    }

    /// Start the interval timer for interval mode
    async fn start_interval_timer(self: &Arc<Self>, cancel: CancellationToken) -> JoinHandle<()> {
        let duration = Duration::from_secs_f64(self.config.interval);
        let this = Arc::clone(self);
        let token = cancel.child_token();

        tokio::spawn(async move {
            let mut interval = interval(duration);
            // `interval` fires immediately; Node-RED's `setInterval` waits a full period first.
            interval.tick().await;

            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => this.flush_interval(token.clone()).await,
                }
            }
        })
    }

    /// Interval mode: every message joins the sequence the timer will flush.
    async fn process_interval_mode(
        &self,
        msg_handle: MsgHandle,
        cancel: CancellationToken,
    ) -> Result<Vec<MsgHandle>, N2linkError> {
        let mut pending = self.interval_pending.lock().await;
        let mut pending_count = self.pending_count.lock().await;
        pending.push(msg_handle);
        *pending_count += 1;
        if self.max_kept_msgs > 0 && *pending_count > self.max_kept_msgs {
            let drained: Vec<MsgHandle> = pending.drain(..).collect();
            *pending_count = 0;
            drop(pending_count);
            drop(pending);
            self.finish_overflow(drained, cancel).await;
        }
        Ok(vec![])
    }

    /// Concat mode: batch by topic and group id
    async fn process_concat_mode(
        &self,
        msg_handle: MsgHandle,
        cancel: CancellationToken,
    ) -> Result<Vec<MsgHandle>, N2linkError> {
        let (topic, group_id, has_parts) = {
            let msg = msg_handle.read().await;

            let topic = msg
                .get("topic")
                .and_then(|v| match v {
                    Variant::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default();

            let (group_id, has_parts) = if let Some(Variant::Object(parts)) = msg.get("parts") {
                if let Some(Variant::String(id)) = parts.get("id") {
                    (id.clone(), true)
                } else {
                    (String::new(), false)
                }
            } else {
                (String::new(), false)
            };

            (topic, group_id, has_parts)
        };

        // A message this mode does not buffer is finished immediately. Upstream calls
        // `done(batch.no-parts)` when the topic matches but `parts` is missing; this node
        // has always completed that message without an error, and the output is still empty.
        let topic_exists = self.config.topics.iter().any(|t| t.topic == topic);
        if !topic_exists || !has_parts {
            self.finish_ok(msg_handle, cancel).await;
            return Ok(vec![]);
        }

        let mut pending = self.concat_pending.lock().await;
        let mut pending_count = self.pending_count.lock().await;

        // Get or create topic groups
        let topic_groups = pending
            .entry(topic.clone())
            .or_insert_with(|| TopicGroups { groups: HashMap::new(), group_ids: Vec::new() });

        // Get or create group
        if !topic_groups.groups.contains_key(&group_id) {
            topic_groups.groups.insert(group_id.clone(), PendingGroup { messages: Vec::new(), count: None });
            topic_groups.group_ids.push(group_id.clone());
        }

        let group = topic_groups.groups.get_mut(&group_id).unwrap();
        group.messages.push(msg_handle.clone());
        *pending_count += 1;

        // Update count if available: upstream keeps the first `parts.count` it sees for a
        // group, which is what makes a group complete when the matching messages arrive.
        {
            let msg = msg_handle.read().await;
            if group.count.is_none()
                && let Some(Variant::Object(parts)) = msg.get("parts")
                && let Some(Variant::Number(count)) = parts.get("count")
            {
                group.count = Some(count.as_u64().unwrap_or(0) as usize);
            }
        }

        if self.max_kept_msgs > 0 && *pending_count > self.max_kept_msgs {
            let mut drained = Vec::new();
            for topic_groups in pending.values() {
                for group in topic_groups.groups.values() {
                    drained.extend(group.messages.iter().cloned());
                }
            }
            pending.clear();
            *pending_count = 0;
            drop(pending);
            drop(pending_count);
            self.finish_overflow_cause(drained, &msg_handle, cancel).await;
            return Ok(vec![]);
        }

        // Check if all topics have complete groups
        let can_concat = self.config.topics.iter().all(|topic_config| {
            if let Some(topic_groups) = pending.get(&topic_config.topic)
                && let Some(first_group_id) = topic_groups.group_ids.first()
                && let Some(group) = topic_groups.groups.get(first_group_id)
                && let Some(expected_count) = group.count
            {
                return group.messages.len() == expected_count;
            }
            false
        });

        if can_concat {
            // Collect messages from all topics
            let mut all_messages = Vec::new();

            // Upstream collects from every configured topic *before* removing anything, so a
            // topic that is listed twice contributes its messages to the sequence twice.
            for topic_config in &self.config.topics {
                if let Some(topic_groups) = pending.get(&topic_config.topic)
                    && let Some(first_group_id) = topic_groups.group_ids.first()
                    && let Some(group) = topic_groups.groups.get(first_group_id)
                {
                    all_messages.extend(group.messages.iter().cloned());
                }
            }

            for topic_config in &self.config.topics {
                if let Some(topic_groups) = pending.get_mut(&topic_config.topic)
                    && let Some(first_group_id) = topic_groups.group_ids.first().cloned()
                {
                    if let Some(group) = topic_groups.groups.remove(&first_group_id) {
                        *pending_count = pending_count.saturating_sub(group.messages.len());
                    }
                    topic_groups.group_ids.remove(0);
                }
            }

            drop(pending);
            drop(pending_count);
            let finished = all_messages.clone();
            // Upstream always clones the messages a concat emits, then calls `done()` on them.
            let sent = self.send_batch(all_messages, true).await?;
            for msg in finished {
                self.finish_ok(msg, cancel.clone()).await;
            }
            return Ok(sent);
        }

        Ok(vec![])
    }
}

#[async_trait]
impl FlowNodeBehavior for BatchNode {
    fn get_base(&self) -> &BaseFlowNodeState {
        &self.base
    }
    async fn run(self: Arc<Self>, stop_token: CancellationToken) {
        // Node-RED starts the interval timer when the node is created, not when the first
        // message arrives: the sequence is flushed on the timer, and with
        // `allowEmptySequence` the very first flush can be an empty one.
        let is_interval_mode = self.config.mode == "interval" && self.config.interval > 0.0;
        if is_interval_mode {
            let mut task = self.interval_task.lock().await;
            *task = Some(self.start_interval_timer(stop_token.clone()).await);
        }

        while !stop_token.is_cancelled() {
            let cancel = stop_token.clone();
            // Completion is deferred until the message is sent, reset, or dropped. `with_uow`
            // would call `done()` as soon as this loop accepted the message.
            let msg = match self.recv_msg(cancel.clone()).await {
                Ok(msg) => msg,
                Err(err) if err.is_cancelled() => break,
                Err(err) => {
                    log::warn!("[batch:{}] {err}", self.name());
                    continue;
                }
            };

            if msg.read().await.contains("reset") {
                // Restart the interval timer, as upstream's reset does, then complete the
                // messages the reset discarded.
                let mut task = self.interval_task.lock().await;
                if let Some(handle) = task.take() {
                    handle.abort();
                }
                if is_interval_mode {
                    *task = Some(self.start_interval_timer(stop_token.clone()).await);
                }
                drop(task);
                self.finish_reset(msg, cancel).await;
                continue;
            }

            let dispatched = match self.config.mode.as_str() {
                "count" => self.process_count_mode(msg.clone(), cancel.clone()).await,
                "interval" => self.process_interval_mode(msg.clone(), cancel.clone()).await,
                "concat" => self.process_concat_mode(msg.clone(), cancel.clone()).await,
                _ => {
                    self.finish_ok(msg.clone(), cancel.clone()).await;
                    Ok(vec![])
                }
            };
            match dispatched {
                Ok(out) => {
                    for m in out {
                        if let Err(err) = self.fan_out_one(Envelope { port: 0, msg: m }, cancel.child_token()).await {
                            log::error!("[batch:{}] {err}", self.name());
                        }
                    }
                }
                Err(err) => {
                    self.report_error(err.to_string(), msg.clone(), cancel.clone()).await;
                    self.finish_ok(msg, cancel).await;
                }
            }
        }
        log::debug!("BatchNode process() task has been terminated.");
    }
}
