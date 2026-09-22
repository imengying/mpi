//! Context compaction.
//!
//! Four things in here are easy to get wrong and are therefore spelled out:
//!
//! * **What is kept.** A checkpoint is not "a summary string replacing history". It is
//!   the full replacement message list, built by keeping every user message (the
//!   *intent*, which is small and must not be lost) and dropping nearly all assistant
//!   and tool traffic (the *process*, which is most of the bytes and is disposable).
//! * **Where the cut goes.** Never on a tool result — that would leave an orphan result
//!   with no call. Cutting on an assistant message with tool calls keeps its results.
//! * **When it is not allowed.** Too little history, a stream in flight, or a compaction
//!   already running are all errors with distinct messages, not silent no-ops.
//! * **Recursion.** The summarisation request itself must never trigger a compaction.

use crate::config::{Defaults, ModelConfig, Provider};
use crate::llm::{Completion, Message, Request, StopReason, ToolSpec, client::Client};
use crate::util;

/// Why a compaction is happening. The three paths share this implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The user typed `/compact`.
    Manual,
    /// Usage crossed `context_window - reserve_tokens` before a request.
    Threshold,
    /// The upstream reported (or silently caused) an overflow.
    Overflow,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::Manual => "manual",
            Reason::Threshold => "threshold",
            Reason::Overflow => "overflow",
        }
    }

    /// The line shown above the transcript while the summary is generated.
    pub fn banner(self) -> &'static str {
        match self {
            Reason::Manual => "正在压缩上下文…",
            Reason::Threshold => "上下文接近上限，正在压缩…",
            Reason::Overflow => "上下文超限，正在压缩后重试…",
        }
    }
}

/// Errors that must be reported rather than swallowed.
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("历史太短，没有可以压缩的内容（最近 {keep_recent_tokens} token 会原样保留）")]
    TooShort { keep_recent_tokens: u64 },
    #[error("正在流式输出，无法压缩；等这一轮结束后再试")]
    Streaming,
    #[error("上一次压缩尚未完成")]
    InProgress,
    #[error("摘要请求失败：{0}")]
    Summarize(String),
    #[error("摘要被长度限制截断，未写入检查点（请重试或换用更小的历史）")]
    Truncated,
    #[error("摘要模型调用了工具，结果不可用；未写入检查点")]
    ToolCallInSummary,
    #[error("会话写入失败：{0}")]
    Session(String),
}

/// How much of the request is reserved for the answer.
///
/// The nominal reserve is 16k, which is the right size for a large window. On a model with a
/// small window that reserve would swallow the whole thing (`6000 - 16384` saturates to 0) and
/// compaction would fire on every single turn, so the reserve is capped at a quarter of the
/// window. The cap only ever applies to small windows: above 64k the nominal value wins.
pub fn reserve_for(context_window: u64) -> u64 {
    Defaults::RESERVE_TOKENS.min(context_window / 4)
}

/// The keep-recent window, capped the same way and for the same reason: keeping 20k tokens
/// untouched is meaningless when the whole window is 6k, and it makes compaction produce a
/// checkpoint that is *larger* than what it replaced.
pub fn keep_recent_for(context_window: u64) -> u64 {
    Defaults::KEEP_RECENT_TOKENS.min(context_window / 3)
}

/// The point at which automatic compaction fires.
pub fn threshold(context_window: u64, reserve: u64) -> u64 {
    context_window.saturating_sub(reserve)
}

/// Convenience: the threshold for a model, with the reserve capped for small windows.
pub fn threshold_for(context_window: u64) -> u64 {
    threshold(context_window, reserve_for(context_window))
}

/// Estimate the tokens in a message list, preferring real usage when it is still valid.
///
/// `real_usage` must only be passed when it describes the *current* context: usage from
/// before the last checkpoint measures the old, larger history and would make the
/// threshold fire again immediately after a compaction.
pub fn estimate_context(messages: &[Message], system: &str, real_usage: Option<u64>) -> u64 {
    match real_usage {
        Some(tokens) if tokens > 0 => tokens,
        _ => crate::llm::estimate_context(messages, system),
    }
}

/// Where to cut, and whether the cut lands in the middle of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPoint {
    /// Index of the first message that survives **as-is**.
    pub first_kept: usize,
    /// When cutting mid-turn, the user message that opened that turn.
    pub turn_start: Option<usize>,
}

impl CutPoint {
    pub fn is_split_turn(&self) -> bool {
        self.turn_start.is_some()
    }
}

/// Can a cut land here? User and assistant messages can; a tool result cannot, because
/// it must follow the call that produced it.
pub fn is_cut_point(message: &Message) -> bool {
    matches!(message, Message::User { .. } | Message::Assistant { .. })
}

fn is_turn_start(message: &Message) -> bool {
    matches!(message, Message::User { .. })
}

fn is_user(message: &Message) -> bool {
    matches!(message, Message::User { .. })
}

/// Walk backwards from the newest message, accumulating estimated size, and stop once
/// `keep_recent_tokens` is reached. The cut is then moved to the nearest valid point,
/// preferring one just after that position and falling back to one just before it (which
/// keeps more history than asked for, but never breaks the tool-call pairing).
///
/// Returns `None` when no usable cut exists: either the history is shorter than the
/// budget, or the only candidate sits in the first turn, which would leave nothing to
/// summarise.
pub fn find_cut_point(messages: &[Message], keep_recent_tokens: u64) -> Option<CutPoint> {
    if messages.is_empty() {
        return None;
    }
    let mut accumulated = 0u64;
    let mut target = 0usize;
    for index in (0..messages.len()).rev() {
        let tokens = messages[index].estimate_tokens();
        if tokens == 0 {
            continue;
        }
        accumulated += tokens;
        if accumulated >= keep_recent_tokens {
            target = index;
            break;
        }
    }
    let usable = |cut: usize| -> Option<CutPoint> {
        if cut == 0 || !is_cut_point(&messages[cut]) {
            return None;
        }
        let turn_start = if is_turn_start(&messages[cut]) {
            None
        } else {
            (0..cut).rev().find(|index| is_turn_start(&messages[*index]))
        };
        // A cut inside the first turn would summarise nothing.
        if turn_start == Some(0) {
            return None;
        }
        Some(CutPoint { first_kept: cut, turn_start })
    };
    // Nearest valid cut at or after the budget boundary: the tool results that belong to
    // an assistant cut stay attached to it automatically, because they come after.
    for cut in target..messages.len() {
        if let Some(point) = usable(cut) {
            return Some(point);
        }
    }
    // Nothing usable at or after the boundary, so keep more than the budget rather than
    // refuse to compact.
    for cut in (0..target).rev() {
        if let Some(point) = usable(cut) {
            return Some(point);
        }
    }
    None
}

/// The messages folded into the summary: everything before the cut.
///
/// A mid-turn cut does not lose the user message that opened that turn, even though it
/// sits on the summarised side. [`replacement_history`] re-attaches every user message
/// verbatim, so the intent survives both as prose in the summary and as the actual text.
pub fn messages_to_summarize(messages: &[Message], cut: CutPoint) -> Vec<Message> {
    messages[..cut.first_kept.min(messages.len())].to_vec()
}

/// The messages kept verbatim after the checkpoint.
pub fn messages_to_keep(messages: &[Message], cut: CutPoint) -> Vec<Message> {
    messages[cut.first_kept.min(messages.len())..].to_vec()
}

/// Build the replacement history: **every user message from the summarised part**, plus
/// the kept window. Assistant and tool traffic from the summarised part is dropped —
/// that is what makes compaction actually shrink a session.
pub fn replacement_history(
    summarized: &[Message],
    kept: &[Message],
    summary: &str,
) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    out.push(Message::user_text(format!(
        "以下是本次会话此前工作的上下文检查点，请把它当作已经发生过的历史继续工作。\n\n{summary}"
    )));
    for message in summarized {
        if is_user(message) {
            let text = message.text();
            if !text.trim().is_empty() {
                out.push(Message::user_text(text));
            }
        }
    }
    out.extend(kept.iter().cloned());
    out
}

/// File operations collected for the `<read-files>` / `<modified-files>` blocks, so a
/// resumed session knows what to re-read instead of guessing from prose.
#[derive(Debug, Default, Clone)]
pub struct FileOps {
    pub read: std::collections::BTreeSet<String>,
    pub modified: std::collections::BTreeSet<String>,
}

impl FileOps {
    pub fn observe(&mut self, message: &Message) {
        for (_, name, arguments) in message.tool_calls() {
            let Some(path) = arguments.get("path").and_then(|value| value.as_str()) else { continue };
            match name {
                "read" | "grep" | "find" | "ls" => {
                    self.read.insert(path.to_string());
                }
                "write" | "edit" => {
                    self.modified.insert(path.to_string());
                }
                _ => {}
            }
        }
    }

    /// Files that were read and later modified stay in the modified list only.
    pub fn lists(&self) -> (Vec<String>, Vec<String>) {
        let read_only: Vec<String> = self
            .read
            .iter()
            .filter(|path| !self.modified.contains(*path))
            .cloned()
            .collect();
        (read_only, self.modified.iter().cloned().collect())
    }
}

pub fn format_file_blocks(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read_files.join("\n")));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        return String::new();
    }
    format!("\n\n{}", sections.join("\n\n"))
}

/// A tool result longer than this is cut before it reaches the summariser, or one giant
/// output could blow up the summary request itself.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// How much of one assistant message's prose the summariser sees.
///
/// A turn can carry a long write-up, and the *summary* of a turn does not need it in full —
/// the sections that matter (goal, decisions, next steps) are a reduction of it, and a
/// verbatim copy of the input is the one thing a summariser has to read but never produces.
const ASSISTANT_TEXT_MAX_CHARS: usize = 20_000;

/// Flatten a conversation into text. Serialising rather than sending the messages keeps
/// the summariser from treating them as a conversation to continue.
///
/// **Reasoning traces are left out** (see the note in the match arm), along with anything
/// else already bounded. What is sent is the conversation as it reads on screen: what the
/// user asked, what the assistant answered, what tools ran and what they returned.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for message in messages {
        match message {
            Message::User { .. } => {
                let text = message.text();
                if !text.trim().is_empty() {
                    parts.push(format!("[用户]: {text}"));
                }
            }
            Message::Assistant { .. } => {
                let text = message.text();
                if !text.trim().is_empty() {
                    parts.push(format!(
                        "[助手]: {}",
                        truncate_chars(&text, ASSISTANT_TEXT_MAX_CHARS)
                    ));
                }
                // The reasoning trace is deliberately not serialised.
                //
                // It is the model thinking aloud, and it is enormous: in a real long session
                // it was 3.4M of the 4.4M characters sent to the summariser — 78% of the
                // request, against 481 characters of user text. That is what pushed the
                // summary request past the model's own window ("prompt is too long: 1113399
                // tokens > 1048576 maximum"): the request to shrink the conversation was
                // itself larger than any conversation it could be asked to shrink.
                //
                // It is also the part a summary does not need. The trace is scratch work on
                // the way to the answer; the answer, the tool calls that produced it and the
                // results they returned are what the next model has to know. Sending the
                // scratch work back in asks the summariser to re-derive what it is being
                // handed the conclusion of.
                let calls: Vec<String> = message
                    .tool_calls()
                    .into_iter()
                    .map(|(_, name, arguments)| format!("{name}({arguments})"))
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[助手工具调用]: {}", calls.join("; ")));
                }
            }
            Message::Tool { name, content, .. } => {
                if !content.trim().is_empty() {
                    parts.push(format!("[工具结果 {}]: {}", name, truncate_for_summary(content)));
                }
            }
            Message::System { .. } => {}
        }
    }
    parts.join("\n\n")
}

fn truncate_for_summary(text: &str) -> String {
    truncate_chars(text, TOOL_RESULT_MAX_CHARS)
}

/// Cut `text` to `max` characters, saying so when anything was dropped.
///
/// Counted in characters rather than bytes: the limit is about how much a model reads, and
/// slicing a multi-byte character in half would send invalid UTF-8.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}\n[... 已截断]")
}

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "你是一个上下文摘要助手。读取用户与 AI 助手之间的对话，\
按指定格式产出结构化摘要。不要继续对话，不要回答对话中的任何问题，只输出结构化摘要。";

/// The initial seven-section prompt.
pub fn summary_prompt(
    conversation: &str,
    custom: Option<&str>,
    previous: Option<&str>,
    split_turn: bool,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("<conversation>\n");
    prompt.push_str(conversation);
    prompt.push_str("\n</conversation>\n\n");
    if split_turn {
        prompt.push_str(
            "注意：最后一条用户消息开启的那一轮还没有结束，只总结到它已经开始的部分，并在 Progress 的 In Progress 里写清这一步做到哪里。\n\n",
        );
    }
    if let Some(previous) = previous {
        // Re-compaction merges into the previous summary instead of rewriting it.
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(previous);
        prompt.push_str("\n</previous-summary>\n\n");
        prompt.push_str(
            "上面的消息是需要合并进已有摘要的新对话。规则：\n\
             - PRESERVE：完整保留已有摘要里的全部信息\n\
             - ADD：把新消息里的进展、决策与上下文补进去\n\
             - UPDATE：Progress 里已完成的事项从 In Progress 移到 Done\n\
             - UPDATE：根据实际完成情况更新 Next Steps\n\
             - PRESERVE：原样保留文件路径、函数名与报错信息\n\
             - 已经不再相关的内容可以删除\n\n",
        );
    } else {
        prompt.push_str(
            "上面的消息是需要总结的对话。请产出一份结构化的上下文检查点摘要，供另一个模型接着工作。\n\n",
        );
    }
    prompt.push_str("严格使用以下格式：\n\n");
    prompt.push_str(crate::config::SUMMARY_SECTIONS);
    if let Some(custom) = custom
        && !custom.trim().is_empty()
    {
        prompt.push_str(&format!("\n\n额外关注：{custom}"));
    }
    prompt
}

/// A summary that came back truncated must never be stored as a checkpoint.
pub fn check_summary(completion: &Completion) -> Result<String, CompactError> {
    match completion.stop_reason {
        StopReason::Error => Err(CompactError::Summarize(
            completion.error.clone().unwrap_or_else(|| "未知错误".into()),
        )),
        StopReason::Length => Err(CompactError::Truncated),
        StopReason::ToolUse => Err(CompactError::ToolCallInSummary),
        // A summary request is never driven by the turn loop, so it cannot be stopped by
        // Esc; treating it as an error keeps the variant from being silently accepted.
        StopReason::Aborted => Err(CompactError::Summarize("摘要被中断".into())),
        // Search is forced off for this request. A pause here means that failed.
        StopReason::Pause => Err(CompactError::Summarize("摘要请求不应触发搜索".into())),
        StopReason::Stop => {
            let text = completion.text();
            if text.trim().is_empty() {
                Err(CompactError::Summarize("摘要为空".into()))
            } else {
                Ok(text)
            }
        }
    }
}

/// Everything the summariser needs for one run.
pub struct SummaryRequest<'a> {
    pub provider: &'a Provider,
    pub model: &'a ModelConfig,
    pub session_id: &'a str,
    pub previous_summary: Option<&'a str>,
    pub custom_instructions: Option<&'a str>,
    pub level: &'a str,
}

/// Run the summarisation request. It goes through the same provider as everything else,
/// but never through the compaction path, so it cannot recurse.
pub async fn summarize(
    client: &Client,
    request: SummaryRequest<'_>,
    summarized: &[Message],
    split_turn: bool,
) -> Result<(String, crate::config::Usage), CompactError> {
    let conversation = serialize_conversation(summarized);
    let prompt = summary_prompt(
        &conversation,
        request.custom_instructions,
        request.previous_summary,
        split_turn,
    );
    let messages = vec![
        Message::System { content: SUMMARIZATION_SYSTEM_PROMPT.to_string() },
        Message::user_text(prompt),
    ];
    let no_tools: Vec<ToolSpec> = Vec::new();
    // The summary is not a turn of the conversation. Hosted search on it would spend a
    // search on the summary itself, and the tool list would no longer match the session.
    let mut model = request.model.clone();
    model.search = false;
    let body = Request {
        model: &model,
        provider: request.provider,
        messages: &messages,
        tools: &no_tools,
        level: request.level,
        session_id: request.session_id,
        // A one-off summary must not write cache entries for the main conversation.
        cache_hints: false,
    };
    let completion = client
        .complete(&body)
        .await
        .map_err(|err| CompactError::Summarize(err.message()))?;
    let summary = check_summary(&completion)?;
    Ok((summary, completion.usage))
}

// ---------------------------------------------------------------------------
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
    OVERFLOW_PATTERNS.iter().any(|pattern| lower.contains(pattern))
}

/// Why a turn should be compacted and retried. Each variant is a distinct symptom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverflowSignal {
    /// The provider said so outright.
    ExplicitError,
    /// The request succeeded but the prompt alone filled (or overfilled) the window, which
    /// some gateways do silently.
    SilentOverflow { prompt_tokens: u64, context_window: u64 },
    /// The server truncated the prompt to fit, leaving no room to answer.
    LengthCut { output_tokens: u64, max_tokens: u64 },
}

/// Detect an overflow from a finished turn.
///
/// `context_window` is only consulted for the silent case; the length-cut check
/// deliberately does **not** depend on it, because the whole point is that a provider can
/// squeeze the output to nothing while the configured capacity still looks fine.
pub fn detect_overflow(
    completion: &Completion,
    context_window: Option<u64>,
    max_tokens: u64,
) -> Option<OverflowSignal> {
    if let Some(error) = &completion.error
        && looks_like_overflow(error)
    {
        return Some(OverflowSignal::ExplicitError);
    }
    if completion.stop_reason == StopReason::Error {
        return None;
    }
    let prompt_tokens = completion.usage.input + completion.usage.cache_read + completion.usage.cache_write;
    if let Some(window) = context_window
        && window > 0
        && prompt_tokens > window
    {
        return Some(OverflowSignal::SilentOverflow { prompt_tokens, context_window: window });
    }
    if completion.stop_reason == StopReason::Length {
        // Zero output is the classic "prompt ate the whole window" symptom. A non-zero but
        // far-too-short answer counts too: the model was cut off early.
        let output = completion.usage.output;
        if output < max_tokens / 4 {
            return Some(OverflowSignal::LengthCut { output_tokens: output, max_tokens });
        }
    }
    None
}

/// Whether a finished response leaves nothing to retry: a successful turn cannot be
/// "continued" by re-sending, so it is compacted without a retry.
pub fn compact_without_retry(completion: &Completion) -> bool {
    completion.stop_reason == StopReason::Stop
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

/// True when the conversation is long enough that a cut exists at all.
#[cfg(test)]
fn can_compact(messages: &[Message], keep_recent_tokens: u64) -> bool {
    find_cut_point(messages, keep_recent_tokens).is_some()
}

/// Build the replacement history for a compaction, given the current messages.
pub fn plan(messages: &[Message], keep_recent_tokens: u64) -> Option<(CutPoint, Vec<Message>, Vec<Message>)> {
    let cut = find_cut_point(messages, keep_recent_tokens)?;
    let summarized = messages_to_summarize(messages, cut);
    let kept = messages_to_keep(messages, cut);
    Some((cut, summarized, kept))
}

/// Decide what one summary request will contain: where the cut goes and what is sent.
///
/// Split out from [`run`] so the wiring — budget applied to the messages actually sent, not
/// merely available as a helper — is what the tests exercise. Getting that wiring wrong is
/// silent: the caps all look right and the request is still unbounded.
pub struct SummaryPlan {
    /// The messages folded into the summary.
    pub summarized: Vec<Message>,
    /// The messages kept verbatim after the checkpoint.
    pub kept: Vec<Message>,
    pub cut: CutPoint,
    /// The oldest turns were left out to fit the model's window.
    pub trimmed: bool,
    /// What the request will cost, so the caller can act on it if it still does not fit.
    pub request_tokens: u64,
}

/// Plan a summary request against the model's window.
pub fn plan_summary(
    messages: &[Message],
    keep_recent_tokens: u64,
    context_window: Option<u64>,
) -> Result<SummaryPlan, CompactError> {
    let Some((cut, summarized, kept)) = plan(messages, keep_recent_tokens) else {
        return Err(CompactError::TooShort { keep_recent_tokens });
    };
    let budget = summary_budget_for(context_window.unwrap_or(0));
    let (summarized, trimmed) = limit_for_summary(&summarized, budget);
    // Measured through the same serialisation the request uses, so the number means what it
    // says: the conversation text plus the fixed instructions around it.
    let conversation = serialize_conversation(&summarized);
    let request_tokens = util::estimate_tokens(&conversation)
        + util::estimate_tokens(SUMMARIZATION_SYSTEM_PROMPT)
        + util::estimate_tokens(crate::config::SUMMARY_SECTIONS);
    Ok(SummaryPlan { summarized, kept, cut, trimmed, request_tokens })
}

/// Everything a compaction needs, so the caller in `loop.rs` stays readable.
pub struct CompactionOutcome {
    pub summary: String,
    pub replacement: Vec<Message>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub usage: crate::config::Usage,
    pub tokens_before: u64,
    /// Token estimate after the swap, so the user can see what was saved.
    pub tokens_after: u64,
    /// Whether the oldest turns were left out of the summary request to keep it under the
    /// model's window. The checkpoint still carries their user messages, so nothing the user
    /// asked is lost — but the summary describes less, and that is worth saying.
    pub trimmed_for_summary: bool,
}

/// How much of the summarised conversation may go into one summary request.
///
/// The request has to fit the model's own window along with the answer it is asked for. It
/// is not "context_window minus reserve" but a good deal less: the summary is a *reduction*,
/// and a summariser given the full window has to read all of it to produce a fraction of it.
/// A third of the window leaves room for the seven-section answer and keeps the request
/// comfortably inside the limit the provider enforces.
///
/// Without a bound the request is whatever the conversation happens to be, and a long session
/// makes it larger than the window — the request to shrink the conversation is refused for
/// being too large, and `/compact` is impossible exactly when it is needed. `/compact` on a
/// real 1M-token session failed this way: `prompt is too long: 1113399 tokens > 1048576`.
pub fn summary_budget_for(context_window: u64) -> u64 {
    // A model with no declared window gets a conservative fixed budget rather than none:
    // an unbounded request is the failure this exists to prevent.
    match context_window {
        0 => 120_000,
        window => (window / 3).max(4_096),
    }
}

/// Cut the messages to summarise down to `budget_tokens`, newest-first.
///
/// The oldest turns are dropped, because a summary of the recent history is worth more than
/// one of the start of the session: the newest messages are what the next model continues
/// from. What is dropped is *not* lost from the conversation — the checkpoint keeps every
/// user message verbatim regardless (see [`replacement_history`]), so the intent of a
/// dropped turn survives even when its prose does not reach the summariser.
///
/// Returns the messages to send and whether anything was dropped, so the caller can say so.
pub fn limit_for_summary(messages: &[Message], budget_tokens: u64) -> (Vec<Message>, bool) {
    let total: u64 = messages.iter().map(Message::estimate_tokens).sum();
    if total <= budget_tokens {
        return (messages.to_vec(), false);
    }
    let mut kept: Vec<Message> = Vec::new();
    let mut used = 0u64;
    for message in messages.iter().rev() {
        let tokens = message.estimate_tokens();
        if used + tokens > budget_tokens && !kept.is_empty() {
            break;
        }
        used += tokens;
        kept.push(message.clone());
    }
    kept.reverse();
    // A cut that lands on a tool result would hand the summariser an orphan result with no
    // call, the same pairing rule the checkpoint's own cut follows.
    while kept.first().is_some_and(|m| matches!(m, Message::Tool { .. })) {
        kept.remove(0);
    }
    (kept, true)
}

/// Run one compaction: cut, summarise, and assemble the replacement history.
///
/// `previous_summary` comes from the last checkpoint so a re-compaction merges instead
/// of starting over.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    client: &Client,
    request: SummaryRequest<'_>,
    messages: &[Message],
    system_prompt: &str,
    keep_recent_tokens: u64,
    real_usage: Option<u64>,
) -> Result<CompactionOutcome, CompactError> {
    let tokens_before = estimate_context(messages, system_prompt, real_usage);
    let mut file_ops = FileOps::default();
    for message in messages {
        file_ops.observe(message);
    }
    // Bound the request itself, not just the caps inside it: a long session can still add up
    // to more than the model's window, and a summary request that does not fit cannot be
    // sent at all.
    let SummaryPlan { cut, summarized, kept, trimmed, request_tokens } =
        plan_summary(messages, keep_recent_tokens, request.model.context_window)?;
    debug_assert!(
        request.model.context_window.is_none_or(|window| request_tokens < window),
        "a summary request must fit the window: {request_tokens} >= {:?}",
        request.model.context_window
    );
    let (summary, usage) = summarize(client, request, &summarized, cut.is_split_turn()).await?;
    let (read_files, modified_files) = file_ops.lists();
    let summary = format!("{summary}{}", format_file_blocks(&read_files, &modified_files));
    let replacement = replacement_history(&summarized, &kept, &summary);
    let tokens_after = replacement.iter().map(Message::estimate_tokens).sum::<u64>()
        + util::estimate_tokens(system_prompt);
    Ok(CompactionOutcome {
        summary,
        replacement,
        read_files,
        modified_files,
        usage,
        tokens_before,
        tokens_after,
        trimmed_for_summary: trimmed,
    })
}

/// Track compaction activity so the footer can show it and a second compaction cannot
/// start while one is running.
#[derive(Debug, Default)]
pub struct CompactionState {
    pub running: bool,
    pub reason: Option<Reason>,
    /// Set right after a compaction until a fresh usage reading arrives.
    pub tokens_unknown: bool,
}

impl CompactionState {
    pub fn begin(&mut self, reason: Reason) -> Result<(), CompactError> {
        if self.running {
            return Err(CompactError::InProgress);
        }
        self.running = true;
        self.reason = Some(reason);
        Ok(())
    }

    pub fn finish(&mut self) {
        self.running = false;
        self.reason = None;
        self.tokens_unknown = true;
    }

    /// A real usage reading makes the count trustworthy again.
    pub fn observe_usage(&mut self) {
        self.tokens_unknown = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Usage;
    use crate::llm::Block;

    fn user(text: &str) -> Message {
        Message::user_text(text)
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant { content: vec![Block::Text { text: text.into() }], stop_reason: None }
    }

    fn call(id: &str, name: &str, path: &str) -> Message {
        Message::Assistant {
            content: vec![Block::ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: serde_json::json!({ "path": path }),
            }],
            stop_reason: Some(StopReason::ToolUse),
        }
    }

    fn result(id: &str, content: &str) -> Message {
        Message::Tool { tool_call_id: id.into(), name: "read".into(), content: content.into() }
    }

    /// Roughly `tokens` tokens, given the chars/4 estimate.
    fn sized(text: &str, tokens: usize) -> String {
        format!("{text}{}", "x".repeat(tokens * 4))
    }

    #[test]
    fn the_cut_never_lands_on_a_tool_result() {
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(user(&sized(&format!("u{i} "), 4000)));
            messages.push(call(&format!("c{i}"), "read", "a.rs"));
            messages.push(result(&format!("c{i}"), &sized("r", 8000)));
            messages.push(assistant(&sized("a", 4000)));
        }
        let cut = find_cut_point(&messages, 20_000).expect("a cut exists");
        assert!(is_cut_point(&messages[cut.first_kept]), "cut landed on a tool result");
        // The kept window really is around the budget, not wildly off.
        let kept: u64 = messages[cut.first_kept..].iter().map(Message::estimate_tokens).sum();
        assert!(kept >= 20_000, "kept only {kept} tokens");
    }

    #[test]
    fn cutting_on_an_assistant_tool_call_keeps_its_results() {
        // A finished turn, then a long second turn whose assistant message crosses the
        // budget. The cut lands on that assistant message, so its tool results follow it.
        let messages = vec![
            user(&sized("ask ", 1_000)),
            assistant(&sized("answer ", 1_000)),
            user(&sized("ask again ", 40_000)),
            call("c1", "read", "a.rs"),
            result("c1", &sized("r", 40_000)),
        ];
        let cut = find_cut_point(&messages, 30_000).expect("a cut exists");
        assert_eq!(cut.first_kept, 3);
        assert!(cut.is_split_turn(), "an assistant cut has to remember its turn start");
        assert_eq!(cut.turn_start, Some(2));
        let kept = messages_to_keep(&messages, cut);
        // The tool result stays together with its call.
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().any(|m| matches!(m, Message::Tool { .. })));
        let summarized = messages_to_summarize(&messages, cut);
        assert_eq!(summarized.len(), 3);
        // …and the replacement history still contains the user message that opened the
        // split turn, verbatim, so the intent is not lost.
        let replacement = replacement_history(&summarized, &kept, "S");
        let user_texts: Vec<String> = replacement
            .iter()
            .filter(|m| is_user(m))
            .map(Message::text)
            .collect();
        assert!(user_texts.iter().any(|t| t.starts_with("ask again")), "{user_texts:?}");
    }

    #[test]
    fn a_single_open_turn_cannot_be_compacted() {
        let messages = vec![
            user(&sized("start ", 20_000)),
            call("c1", "read", "a.rs"),
            result("c1", &sized("r", 30_000)),
        ];
        // The only cut candidates sit inside the first turn, so there is nothing to
        // summarise: compaction reports "too short" instead of producing an empty
        // checkpoint.
        assert!(find_cut_point(&messages, 10_000).is_none());
    }

    #[test]
    fn a_cut_inside_an_enormous_turn_still_keeps_the_whole_turn_start() {
        // The newest turn alone is far past the budget, so the cut lands *inside* it and
        // the turn is flagged as split. The user message that opened it must survive.
        let messages = vec![
            user(&sized("first ", 1_000)),
            assistant(&sized("answer ", 1_000)),
            user(&sized("second ", 200_000)),
            assistant(&sized("long answer ", 300_000)),
        ];
        let cut = find_cut_point(&messages, 20_000).expect("a cut exists");
        assert!(cut.is_split_turn());
        assert_eq!(cut.turn_start, Some(2));
        let summarized = messages_to_summarize(&messages, cut);
        let kept = messages_to_keep(&messages, cut);
        let replacement = replacement_history(&summarized, &kept, "S");
        let user_texts: Vec<String> =
            replacement.iter().filter(|m| is_user(m)).map(Message::text).collect();
        assert!(user_texts.iter().any(|t| t.starts_with("first")));
        assert!(user_texts.iter().any(|t| t.starts_with("second")), "{user_texts:?}");
    }

    #[test]
    fn a_mid_turn_cut_records_the_user_message_that_opened_it() {
        let messages = vec![
            user("first"),
            assistant(&sized("a", 40_000)),
            user("second"),
            assistant(&sized("b", 40_000)),
        ];
        let cut = find_cut_point(&messages, 25_000).unwrap();
        assert!(cut.is_split_turn());
        assert_eq!(cut.turn_start, Some(2));
        // Everything before the cut goes into the summary, and the split turn is announced
        // to the summariser so an unfinished step is described as such.
        let summarized = messages_to_summarize(&messages, cut);
        assert_eq!(summarized.len(), 3);
        assert!(summary_prompt("C", None, None, cut.is_split_turn()).contains("还没有结束"));
    }

    #[test]
    fn too_little_history_has_no_cut_point() {
        let messages = vec![user("hi"), assistant("hello")];
        assert!(find_cut_point(&messages, 20_000).is_none());
        assert!(!can_compact(&messages, 20_000));
    }

    #[test]
    fn the_replacement_history_keeps_every_user_message() {
        let summarized = vec![
            user("keep me"),
            call("c1", "read", "a.rs"),
            result("c1", "noise"),
            assistant("noise too"),
            user("keep me as well"),
        ];
        let kept = vec![user("recent question"), assistant("recent answer")];
        let replacement = replacement_history(&summarized, &kept, "SUMMARY");
        let text: Vec<String> = replacement.iter().map(Message::text).collect();
        assert!(text[0].contains("SUMMARY"));
        assert!(text.iter().any(|t| t == "keep me"));
        assert!(text.iter().any(|t| t == "keep me as well"));
        assert!(text.iter().any(|t| t == "recent question"));
        // Assistant and tool traffic from the summarised part is gone.
        assert!(!text.iter().any(|t| t.contains("noise")));
        assert!(!replacement.iter().any(|m| matches!(m, Message::Tool { .. })));
    }

    #[test]
    fn file_blocks_separate_read_files_from_modified_ones() {
        let mut ops = FileOps::default();
        ops.observe(&call("1", "read", "src/a.rs"));
        ops.observe(&call("2", "read", "src/b.rs"));
        ops.observe(&call("3", "edit", "src/b.rs"));
        ops.observe(&call("4", "write", "src/c.rs"));
        let (read, modified) = ops.lists();
        assert_eq!(read, vec!["src/a.rs"]);
        assert_eq!(modified, vec!["src/b.rs", "src/c.rs"]);
        let blocks = format_file_blocks(&read, &modified);
        assert!(blocks.contains("<read-files>\nsrc/a.rs\n</read-files>"));
        assert!(blocks.contains("<modified-files>\nsrc/b.rs\nsrc/c.rs\n</modified-files>"));
    }

    #[test]
    fn serialization_flattens_tools_and_caps_a_giant_result() {
        let messages = vec![
            user("do the thing"),
            call("c1", "bash", "x"),
            result("c1", &sized("", 50_000)),
        ];
        let text = serialize_conversation(&messages);
        assert!(text.contains("[用户]: do the thing"));
        assert!(text.contains("[助手工具调用]: bash("));
        assert!(text.contains("已截断"));
        assert!(text.chars().count() < 50_000);
    }

    #[test]
    fn reasoning_traces_are_left_out_of_the_summary_request() {
        // This is the bug that made `/compact` fail on a long session: the reasoning traces
        // were 3.4M of a 4.4M-character request, so the request to shrink the conversation
        // was itself larger than the model's window and was refused. A trace is the model's
        // scratch work; the summary is of the conversation, not of the working.
        let messages = vec![
            user("what changed?"),
            Message::Assistant {
                content: vec![
                    Block::Thinking { thinking: sized("scratch ", 40_000), signature: None },
                    Block::Text { text: "I edited a.rs".into() },
                ],
                stop_reason: Some(StopReason::Stop),
            },
        ];
        let text = serialize_conversation(&messages);
        assert!(!text.contains("scratch "), "the trace must not be sent: {}", text.chars().count());
        assert!(!text.contains("助手思考"), "{text}");
        // The answer itself is still there — that is the part worth summarising.
        assert!(text.contains("[助手]: I edited a.rs"), "{text}");
    }

    #[test]
    fn a_long_assistant_message_is_capped() {
        // The per-message cap is the other half: one very long write-up should not decide how
        // big the request is either.
        let messages = vec![Message::Assistant {
            content: vec![Block::Text { text: sized("word ", 80_000) }],
            stop_reason: Some(StopReason::Stop),
        }];
        let text = serialize_conversation(&messages);
        assert!(text.contains("已截断"), "a capped message says so");
        assert!(text.chars().count() < ASSISTANT_TEXT_MAX_CHARS * 2, "{}", text.chars().count());
    }

    #[test]
    fn the_request_budget_keeps_the_newest_turns() {
        // Over budget, the oldest turns go: a summary of the recent work is what the next
        // model continues from. The user messages of the dropped turns are not lost from the
        // conversation — the checkpoint keeps them verbatim.
        let messages: Vec<Message> = (0..40)
            .flat_map(|i| {
                vec![
                    user(&format!("question {i} {}", sized("x", 4_000))),
                    Message::Assistant {
                        content: vec![Block::Text { text: format!("answer {i}") }],
                        stop_reason: Some(StopReason::Stop),
                    },
                ]
            })
            .collect();
        let budget = summary_budget_for(60_000);
        let (kept, trimmed) = limit_for_summary(&messages, budget);
        assert!(trimmed, "this is over budget");
        let text = serialize_conversation(&kept);
        assert!(text.contains("question 39"), "the newest turn survives");
        assert!(!text.contains("question 0 "), "the oldest turns are dropped: {budget}");
        let used: u64 = kept.iter().map(Message::estimate_tokens).sum();
        assert!(used <= budget, "the budget is respected: {used} > {budget}");
        // Never starts on a tool result: the summariser must not see an orphan.
        assert!(!matches!(kept.first(), Some(Message::Tool { .. })));
    }

    #[test]
    fn a_request_that_fits_is_sent_whole() {
        let messages = vec![user("short"), Message::Assistant {
            content: vec![Block::Text { text: "reply".into() }],
            stop_reason: Some(StopReason::Stop),
        }];
        let (kept, trimmed) = limit_for_summary(&messages, summary_budget_for(1_000_000));
        assert!(!trimmed);
        assert_eq!(kept.len(), messages.len());
    }

    #[test]
    fn the_planned_request_fits_the_window() {
        // The wiring is the part that broke in production: every cap can look right while the
        // request that actually gets built is still unbounded. This measures the plan itself,
        // through the same serialisation the request uses.
        //
        // A session shaped like the real one that failed — a lot of agent traffic, most of it
        // reasoning — over a window of 1M tokens.
        let window = 1_048_576u64;
        let messages: Vec<Message> = (0..400)
            .flat_map(|i| {
                let mut content = vec![
                    Block::Thinking { thinking: sized("scratch ", 30_000), signature: None },
                    Block::Text { text: format!("step {i}") },
                ];
                content.push(Block::ToolCall {
                    id: format!("c{i}"),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": sized("ls ", 1_000)}),
                });
                vec![
                    user(&format!("task {i}")),
                    Message::Assistant { content, stop_reason: Some(StopReason::Stop) },
                    Message::Tool {
                        tool_call_id: format!("c{i}"),
                        name: "bash".into(),
                        content: sized("output ", 20_000),
                    },
                ]
            })
            .collect();

        let plan = plan_summary(&messages, keep_recent_for(window), Some(window)).unwrap();
        let request_tokens = plan.request_tokens;
        assert!(
            request_tokens < window,
            "the summary request must fit the window: {request_tokens} >= {window}"
        );
        // And the reasoning traces are not what it is made of.
        let text = serialize_conversation(&plan.summarized);
        assert!(!text.contains("scratch "), "no traces in the request");
        assert!(text.contains("task 399"), "the newest work is covered");
        // The fixture is over budget, so the cap is what makes the request fit.
        assert!(plan.trimmed, "this session is over the summary budget");
    }

    #[test]
    fn the_budget_leaves_room_for_the_answer() {
        // A third of the window: the request has to fit alongside the summary it asks for.
        assert_eq!(summary_budget_for(1_048_576), 349_525);
        assert_eq!(summary_budget_for(60_000), 20_000);
        // An undeclared window gets a fixed budget, not none: unbounded is the failure mode.
        assert!(summary_budget_for(0) > 0);
        // And a tiny window still gets something usable rather than zero.
        assert_eq!(summary_budget_for(1_000), 4_096);
    }

    #[test]
    fn the_first_prompt_asks_for_the_seven_sections() {
        let prompt = summary_prompt("CONVERSATION", None, None, false);
        for section in [
            "## Goal",
            "## Constraints & Preferences",
            "## Progress",
            "## Key Decisions",
            "## Next Steps",
            "## Critical Context",
        ] {
            assert!(prompt.contains(section), "missing {section}");
        }
        assert!(prompt.contains("<conversation>\nCONVERSATION\n</conversation>"));
        assert!(!prompt.contains("<previous-summary>"));
    }

    #[test]
    fn a_re_compaction_asks_to_preserve_the_previous_summary() {
        let prompt = summary_prompt("NEW", Some("重点保留 API 设计"), Some("OLD SUMMARY"), false);
        assert!(prompt.contains("<previous-summary>\nOLD SUMMARY\n</previous-summary>"));
        assert!(prompt.contains("PRESERVE"));
        assert!(prompt.contains("额外关注：重点保留 API 设计"));
    }

    #[test]
    fn a_split_turn_is_flagged_to_the_summariser() {
        let prompt = summary_prompt("CONVERSATION", None, None, true);
        assert!(prompt.contains("还没有结束"));
        assert!(prompt.contains("In Progress"));
        let plain = summary_prompt("CONVERSATION", None, None, false);
        assert!(!plain.contains("还没有结束"));
    }

    #[test]
    fn a_truncated_summary_is_refused() {
        let completion = Completion {
            message: Message::assistant_text("half a summary"),
            usage: Usage::default(),
            stop_reason: StopReason::Length,
            error: None,
        };
        assert!(matches!(check_summary(&completion), Err(CompactError::Truncated)));
        let failed = Completion {
            message: Message::assistant_text(""),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error: Some("boom".into()),
        };
        assert!(matches!(check_summary(&failed), Err(CompactError::Summarize(_))));
    }

    #[test]
    fn a_complete_summary_is_accepted() {
        let completion = Completion {
            message: Message::assistant_text("## Goal\nstuff"),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
        };
        assert!(check_summary(&completion).is_ok());
    }

    #[test]
    fn overflows_are_recognised_across_providers() {
        assert!(looks_like_overflow("prompt is too long: 210000 tokens > 200000 maximum"));
        assert!(looks_like_overflow("This model's maximum context length is 128000 tokens"));
        assert!(looks_like_overflow("Your request exceeds the context window"));
        assert!(looks_like_overflow("输入超过了最大长度"));
    }

    #[test]
    fn rate_limits_are_not_overflows() {
        assert!(!looks_like_overflow("Rate limit reached for gpt-4 in organization org-x"));
        assert!(!looks_like_overflow("Too many requests, please slow down"));
        assert!(!looks_like_overflow("Throttling error: 429"));
        assert!(!looks_like_overflow("insufficient_quota"));
    }

    #[test]
    fn an_explicit_overflow_error_is_detected() {
        let completion = Completion {
            message: Message::Assistant { content: vec![], stop_reason: Some(StopReason::Error) },
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            error: Some("prompt is too long".into()),
        };
        assert_eq!(detect_overflow(&completion, Some(200_000), 8192), Some(OverflowSignal::ExplicitError));
    }

    #[test]
    fn a_silent_overflow_is_detected_from_usage_alone() {
        // The request "succeeded", but the prompt alone did not fit.
        let completion = Completion {
            message: Message::assistant_text("ok"),
            usage: Usage { input: 210_000, output: 5, cache_read: 0, cache_write: 0 },
            stop_reason: StopReason::Stop,
            error: None,
        };
        assert_eq!(
            detect_overflow(&completion, Some(200_000), 8192),
            Some(OverflowSignal::SilentOverflow { prompt_tokens: 210_000, context_window: 200_000 })
        );
    }

    #[test]
    fn a_length_cut_with_no_room_left_is_detected_without_knowing_the_window() {
        let completion = Completion {
            message: Message::Assistant { content: vec![], stop_reason: Some(StopReason::Length) },
            usage: Usage { input: 99_000, output: 0, cache_read: 0, cache_write: 0 },
            stop_reason: StopReason::Length,
            error: None,
        };
        assert_eq!(
            detect_overflow(&completion, None, 8192),
            Some(OverflowSignal::LengthCut { output_tokens: 0, max_tokens: 8192 })
        );
    }

    #[test]
    fn a_genuine_length_stop_near_the_cap_is_not_an_overflow() {
        let completion = Completion {
            message: Message::Assistant { content: vec![], stop_reason: Some(StopReason::Length) },
            usage: Usage { input: 1000, output: 8000, cache_read: 0, cache_write: 0 },
            stop_reason: StopReason::Length,
            error: None,
        };
        assert_eq!(detect_overflow(&completion, Some(200_000), 8192), None);
    }

    #[test]
    fn a_healthy_turn_is_not_an_overflow() {
        let completion = Completion {
            message: Message::assistant_text("done"),
            usage: Usage { input: 1000, output: 50, cache_read: 0, cache_write: 0 },
            stop_reason: StopReason::Stop,
            error: None,
        };
        assert_eq!(detect_overflow(&completion, Some(200_000), 8192), None);
        assert!(compact_without_retry(&completion));
    }

    #[test]
    fn the_retry_budget_allows_exactly_one_retry_per_turn() {
        let mut budget = RetryBudget::default();
        assert!(budget.spend());
        assert!(!budget.spend());
        budget.reset();
        assert!(budget.spend());
    }

    #[test]
    fn a_second_compaction_cannot_start_while_one_is_running() {
        let mut state = CompactionState::default();
        state.begin(Reason::Manual).unwrap();
        assert!(matches!(state.begin(Reason::Threshold), Err(CompactError::InProgress)));
        state.finish();
        assert!(state.tokens_unknown);
        state.observe_usage();
        assert!(!state.tokens_unknown);
        assert!(state.begin(Reason::Overflow).is_ok());
    }

    #[test]
    fn real_usage_is_preferred_over_the_estimate() {
        let messages = vec![user("short")];
        assert_eq!(estimate_context(&messages, "system", Some(123_456)), 123_456);
        assert!(estimate_context(&messages, "system", None) < 100);
    }

    #[test]
    fn the_threshold_leaves_room_for_the_answer() {
        assert_eq!(threshold(200_000, 16_384), 183_616);
        assert_eq!(threshold_for(1_000_000), 983_616);
    }

    #[test]
    fn a_small_window_still_gets_a_usable_threshold() {
        // The nominal 16k reserve would saturate to zero here, so every turn would compact.
        assert_eq!(reserve_for(6_000), 1_500);
        assert_eq!(threshold_for(6_000), 4_500);
        assert!(threshold_for(4_000) > 0);
        // Below the nominal constant the cap does not apply.
        assert_eq!(reserve_for(1_000_000), 16_384);
    }

    #[test]
    fn a_small_window_keeps_a_proportionally_small_recent_window() {
        // Keeping 20k in a 6k window means compaction cuts nothing and the checkpoint ends up
        // larger than what it replaces.
        assert_eq!(keep_recent_for(6_000), 2_000);
        assert_eq!(keep_recent_for(1_000_000), 20_000);
    }
}
