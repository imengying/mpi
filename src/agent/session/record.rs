//! What one line of the session file is.
//!
//! Records are typed and separated the way codex separates them: conversation items and
//! environment snapshots are different records, so the environment never enters the
//! transcript and is not treated as conversation by compaction. Each variant here is one
//! line of the file; [`super::Session`] owns the file and decides when one is appended.
//!
//! Every variant carries its own `id`, and `parent_id` links back to the record before it.
//! A checkpoint is a record like any other: it *replaces* a span of the conversation when
//! projected (see [`super::context`]), but the records it stands in for stay in the file.

use serde::{Deserialize, Serialize};

use crate::config::Usage;
use crate::llm::{Message, StopReason};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub model: String,
}

/// One line of the file. `type` is the discriminator, matching codex's shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Record {
    /// First line: id, time, cwd, model.
    SessionMeta {
        #[serde(flatten)]
        header: SessionHeader,
    },
    /// A conversation item (user / assistant / tool result).
    ResponseItem {
        /// Links back to the previous record, forming the chain used by `/resume`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<String>,
        id: String,
        message: Message,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        timestamp: String,
    },
    /// Per-turn environment snapshot. Never sent to the model as conversation.
    TurnContext {
        parent_id: Option<String>,
        id: String,
        cwd: String,
        model: String,
        level: String,
        timestamp: String,
    },
    /// The immutable request projection used for one model call. It is bookkeeping, never
    /// part of the model-visible history, and makes pressure decisions reconstructable.
    RequestContext {
        parent_id: Option<String>,
        /// This record's identity also identifies the request.
        id: String,
        request_id: String,
        provider: String,
        model: String,
        level: String,
        context_window: Option<u64>,
        token_estimate: u64,
        system_prompt_hash: Option<String>,
        tools_hash: String,
        cache_hints: bool,
        timestamp: String,
    },
    /// Opens a durable compaction lifecycle. An unmatched start is an interrupted attempt.
    CompactionStart {
        parent_id: Option<String>,
        id: String,
        compaction_id: String,
        reason: String,
        provider: String,
        model: String,
        level: String,
        token_before: u64,
        timestamp: String,
    },
    /// Closes a compaction lifecycle after its summary/checkpoint has been persisted.
    CompactionEnd {
        parent_id: Option<String>,
        id: String,
        compaction_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        timestamp: String,
    },
    /// The intent of one accepted tool call, committed before its body runs.
    ToolExecutionStart {
        parent_id: Option<String>,
        id: String,
        tool_call_id: String,
        name: String,
        arguments: serde_json::Value,
        replay_safe: bool,
        timestamp: String,
    },
    /// The durable outcome of a tool call.
    ToolExecutionEnd {
        parent_id: Option<String>,
        id: String,
        tool_call_id: String,
        name: String,
        status: crate::llm::ToolStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        timestamp: String,
    },
    /// The session name. Appended, never a rewrite of the header.
    SessionInfo {
        parent_id: Option<String>,
        id: String,
        #[serde(default)]
        name: Option<String>,
        timestamp: String,
    },
    /// A compaction checkpoint: the replacement history plus the summary it came from.
    Compacted {
        parent_id: Option<String>,
        id: String,
        compaction_id: String,
        reason: String,
        provider: String,
        model: String,
        level: String,
        summary: String,
        /// Full message list that supersedes everything before this record.
        replacement_history: Vec<Message>,
        /// Logical message identities aligned with the replacement, including retained items.
        replacement_ids: Vec<String>,
        read_files: Vec<String>,
        modified_files: Vec<String>,
        shadowed_ids: Vec<String>,
        token_before: u64,
        token_after: u64,
        summary_prompt_version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        timestamp: String,
    },
    /// Model-free context reduction. Original response items remain available for replay.
    Pruned {
        parent_id: Option<String>,
        id: String,
        replacement_history: Vec<Message>,
        replacement_ids: Vec<String>,
        tool_results: usize,
        saved_tokens: u64,
        timestamp: String,
    },
}

/// One immutable model-visible projection and the identities needed to checkpoint it.
pub struct ContextSnapshot {
    pub messages: Vec<Message>,
    pub entry_ids: Vec<String>,
}

impl Record {
    pub fn id(&self) -> &str {
        match self {
            Record::SessionMeta { header, .. } => &header.id,
            Record::ResponseItem { id, .. }
            | Record::TurnContext { id, .. }
            | Record::RequestContext { id, .. }
            | Record::CompactionStart { id, .. }
            | Record::CompactionEnd { id, .. }
            | Record::ToolExecutionStart { id, .. }
            | Record::ToolExecutionEnd { id, .. }
            | Record::SessionInfo { id, .. }
            | Record::Compacted { id, .. }
            | Record::Pruned { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Record::SessionMeta { .. } => None,
            Record::ResponseItem { parent_id, .. }
            | Record::TurnContext { parent_id, .. }
            | Record::RequestContext { parent_id, .. }
            | Record::CompactionStart { parent_id, .. }
            | Record::CompactionEnd { parent_id, .. }
            | Record::ToolExecutionStart { parent_id, .. }
            | Record::ToolExecutionEnd { parent_id, .. }
            | Record::SessionInfo { parent_id, .. }
            | Record::Compacted { parent_id, .. }
            | Record::Pruned { parent_id, .. } => parent_id.as_deref(),
        }
    }

    /// Only conversation items contribute to the model's context.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Record::ResponseItem { message, .. } => Some(message),
            _ => None,
        }
    }

    /// Whether this record is the one that makes a session worth keeping.
    ///
    /// The environment block is bookkeeping every session starts with, and a name, a model
    /// switch or a turn context on its own is not a conversation. Only a real message —
    /// what the user said, or what came back to them — earns a file.
    pub(super) fn starts_a_conversation(&self) -> bool {
        match self.message() {
            Some(message) => !crate::agent::r#loop::is_environment_block(message),
            None => false,
        }
    }

    pub fn usage(&self) -> Option<Usage> {
        match self {
            Record::ResponseItem { usage, .. } | Record::Compacted { usage, .. } => *usage,
            _ => None,
        }
    }

    pub(super) fn validate(&self) -> Result<(), &'static str> {
        match self {
            Record::Compacted {
                replacement_history,
                replacement_ids,
                ..
            }
            | Record::Pruned {
                replacement_history,
                replacement_ids,
                ..
            } => {
                if replacement_history.len() != replacement_ids.len() {
                    return Err("检查点消息与记录 ID 数量不一致");
                }
                let mut seen = std::collections::HashSet::new();
                if replacement_ids
                    .iter()
                    .any(|id| id.is_empty() || !seen.insert(id))
                {
                    return Err("检查点记录 ID 不能为空或重复");
                }
            }
            _ => {}
        }
        Ok(())
    }
}
