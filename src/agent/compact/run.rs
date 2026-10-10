//! Overflow detection, the retry budget, and the flow a compaction takes.
//!
//! [`run`] is the single implementation the three triggers share. Around it sit the two
//! decisions that make recovery safe: whether an upstream error really was an overflow
//! ([`detect_overflow`] — a rate limit that happens to mention tokens is not one, and
//! summarising for it would spend a request to fix nothing), and how many times this turn
//! may retry ([`RetryBudget`] — once, so a context that cannot shrink does not loop).

use crate::llm::{Completion, Message, StopReason, client::Client};

use super::CompactError;
use super::checkpoint::{
    CheckpointFacts, CutPoint, find_cut_point, format_file_blocks, is_user, messages_to_keep,
    messages_to_summarize, replacement_history, user_request_block,
};
use super::summary::{SummaryRequest, summarize};

// Overflow detection
// ---------------------------------------------------------------------------

/// Errors that look like an overflow but are not, and would waste a summary request.
const NOT_OVERFLOW: [&str; 8] = [
    "rate limit",
    "rate_limit",
    "too many requests",
    "throttling error",
    "429",
    "insufficient_quota",
    "quota exceeded",
    "billing",
];

/// Wording providers use when the prompt does not fit.
const OVERFLOW_PATTERNS: [&str; 16] = [
    "prompt is too long",
    "prompt too long",
    "context length",
    "context_length_exceeded",
    "exceeds the context window",
    "exceeded the context",
    "maximum context length",
    "max context length",
    "too many tokens",
    "input is too long",
    "input too long",
    "reduce the length of the messages",
    "request entity too large",
    "context window exceeded",
    "string too long",
    "超过了最大长度",
];

/// Does this error text mean "the prompt did not fit"?
pub fn looks_like_overflow(error: &str) -> bool {
    let lower = error.to_lowercase();
    if NOT_OVERFLOW.iter().any(|pattern| lower.contains(pattern)) {
        return false;
    }
    OVERFLOW_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

/// Why a turn should be compacted and retried. Each variant is a distinct symptom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverflowSignal {
    /// The provider said so outright.
    ExplicitError,
    /// The request succeeded but the prompt alone filled (or overfilled) the window, which
    /// some gateways do silently.
    SilentOverflow {
        prompt_tokens: u64,
        context_window: u64,
    },
}

/// Detect an overflow from a finished turn.
///
/// Output exhaustion is handled separately; a short answer is not proof of input overflow.
pub fn detect_overflow(
    completion: &Completion,
    context_window: Option<u64>,
) -> Option<OverflowSignal> {
    if let Some(error) = &completion.error
        && looks_like_overflow(error)
    {
        return Some(OverflowSignal::ExplicitError);
    }
    if completion.stop_reason == StopReason::Error {
        return None;
    }
    let prompt_tokens =
        completion.usage.input + completion.usage.cache_read + completion.usage.cache_write;
    if let Some(window) = context_window
        && window > 0
        && prompt_tokens > window
    {
        return Some(OverflowSignal::SilentOverflow {
            prompt_tokens,
            context_window: window,
        });
    }

    None
}

/// Tracks the one-retry-per-turn rule for the overflow path.
#[derive(Debug, Default)]
pub struct RetryBudget {
    used: bool,
}

impl RetryBudget {
    /// Consume the retry. Returns false when it has already been spent this turn.
    pub fn spend(&mut self) -> bool {
        if self.used {
            false
        } else {
            self.used = true;
            true
        }
    }

    /// A new user turn, or a normal response, resets the budget.
    pub fn reset(&mut self) {
        self.used = false;
    }

    pub fn available(&self) -> bool {
        !self.used
    }
}

/// Build the replacement history for a compaction, given the current messages.
pub fn plan(
    messages: &[Message],
    keep_recent_tokens: u64,
    replay: crate::llm::ThinkingReplay,
) -> Option<(CutPoint, Vec<Message>, Vec<Message>)> {
    let cut = find_cut_point(messages, keep_recent_tokens, replay)?;
    let summarized = messages_to_summarize(messages, cut);
    let kept = messages_to_keep(messages, cut);
    Some((cut, summarized, kept))
}

/// Everything a compaction needs, so the caller in `loop.rs` stays readable.
pub struct CompactionOutcome {
    pub compaction_id: String,
    pub summary: String,
    pub replacement: Vec<Message>,
    pub replacement_ids: Vec<String>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub shadowed_ids: Vec<String>,
    pub token_before: u64,
    pub token_after: u64,
    pub usage: crate::config::Usage,
}

/// Run one compaction: cut, summarise, and assemble the replacement history.
///
/// Previous checkpoints already live in the selected history prefix.
pub async fn run(
    client: &Client,
    request: SummaryRequest<'_>,
    messages: &[Message],
    context_ids: &[String],
    compaction_id: &str,
    keep_recent_tokens: u64,
    facts: CheckpointFacts,
) -> Result<CompactionOutcome, CompactError> {
    if context_ids.len() != messages.len() {
        return Err(CompactError::Session("上下文记录与消息数量不一致".into()));
    }
    // Bound the request itself, not just the caps inside it: a long session can still add up
    // to more than the model's window, and a summary request that does not fit cannot be
    // sent at all.
    let replay = crate::llm::ThinkingReplay::for_provider(request.provider);
    let (cut, summarized, kept) =
        plan(messages, keep_recent_tokens, replay).ok_or(CompactError::TooShort)?;
    let system_prompt = request.system_prompt.unwrap_or_default();
    let tools = request.tools;
    // The before/after pair only has to be comparable to each other — the check is that the
    // replacement is smaller — but it is also the number the checkpoint records, so it uses
    // the same protocol-aware reading as the threshold check that triggered it.
    let token_before = crate::llm::estimate_request_context(messages, system_prompt, tools, replay);
    let (summary, usage) = summarize(client, request, &summarized, cut.is_split_turn()).await?;
    let (read_files, modified_files) = facts.files.lists();
    let summary = format!(
        "{summary}{}{}",
        format_file_blocks(&read_files, &modified_files),
        user_request_block(&facts.user_requests)
    );
    let replacement = replacement_history(&summarized, &kept, &summary);
    let token_after =
        crate::llm::estimate_request_context(&replacement, system_prompt, tools, replay);
    if token_after >= token_before {
        return Err(CompactError::NotSmaller);
    }
    let mut replacement_ids = vec![format!("summary:{compaction_id}")];
    if !kept.first().is_some_and(is_user)
        && let Some(index) = summarized.iter().rposition(is_user)
    {
        replacement_ids.push(context_ids[index].clone());
    }
    replacement_ids.extend_from_slice(&context_ids[cut.first_kept..]);
    let retained: std::collections::HashSet<_> = replacement_ids.iter().collect();
    let shadowed_ids = context_ids[..cut.first_kept]
        .iter()
        .filter(|id| !retained.contains(id))
        .cloned()
        .collect();
    Ok(CompactionOutcome {
        compaction_id: compaction_id.to_string(),
        summary,
        replacement,
        replacement_ids,
        read_files,
        modified_files,
        shadowed_ids,
        token_before,
        token_after,
        usage,
    })
}

/// Track compaction activity so the footer can show it and a second compaction cannot
/// start while one is running.
#[derive(Debug, Default)]
pub struct CompactionState {
    pub running: bool,
}

impl CompactionState {
    pub fn begin(&mut self) -> Result<(), CompactError> {
        if self.running {
            return Err(CompactError::InProgress);
        }
        self.running = true;
        Ok(())
    }

    pub fn finish(&mut self) {
        self.running = false;
    }
}
