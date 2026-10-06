//! Context compaction.
//!
//! A checkpoint replaces a balanced prefix with a bounded summary, retaining recent
//! messages and the active request (including images). Original records remain on disk.
//! A failed, truncated or non-shrinking summary never replaces the live context.
//!
//! # Where things live
//!
//! The three trigger paths (`/compact`, the threshold check before a request, and overflow
//! recovery) all end up in `run::run`, and what distinguishes them is the [`Reason`] they
//! pass. The work that gets them there is split by what it operates on:
//!
//! * `prune` — shrink oversized tool results in place, before deciding whether a summary
//!   is needed at all. Deterministic and cheap; it is what usually saves a request.
//! * `checkpoint` — what a checkpoint *contains*: where to cut the conversation, what the
//!   replacement history is, and the text of the summary prompt.
//! * `summary` — shaping and issuing the one model call a compaction makes.
//! * [`run`] — overflow detection, the retry budget, and the flow that ties the above
//!   together.
//!
//! The shared vocabulary — [`Reason`], [`CompactError`] and the prompt version — is here.

mod checkpoint;
mod prune;
mod run;
mod summary;

pub use checkpoint::{CheckpointFacts, FileOps, format_file_blocks};
pub use prune::{PruneOutcome, prune_tool_results};
pub use run::{
    CompactionOutcome, CompactionState, RetryBudget, detect_overflow, looks_like_overflow, run,
};
pub use summary::{PreparedSummary, SummaryRequest, prepare_summary};

#[cfg(test)]
mod tests;

/// Bumped whenever the summary contract changes. It is stored with each checkpoint so a
/// replay/debugger can tell which projection rules produced the text.
pub const SUMMARY_PROMPT_VERSION: &str = "v2";

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

    /// The footer's busy row while the summary is generated.
    ///
    /// Threshold and manual look identical from here — both just shrink the history — so they
    /// share one word. Overflow adds something the user will otherwise see happen with no
    /// explanation: the turn they already sent is sent again.
    pub fn banner(self) -> &'static str {
        match self {
            Reason::Manual | Reason::Threshold => "正在压缩",
            Reason::Overflow => "正在压缩后重试",
        }
    }
}

/// Errors that must be reported rather than swallowed.
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("历史太短，没有可压缩的内容")]
    TooShort,
    #[error("正在输出，无法压缩")]
    Streaming,
    #[error("上一次压缩尚未完成")]
    InProgress,
    #[error("摘要请求失败：{0}")]
    Summarize(String),
    #[error("摘要被长度限制截断，未写入检查点")]
    Truncated,
    #[error("摘要模型调用了工具，未写入检查点")]
    ToolCallInSummary,
    #[error("摘要没有缩小上下文，保留原历史")]
    NotSmaller,
    #[error("摘要输入超过模型可用窗口")]
    InputTooLarge,
    #[error("会话写入失败：{0}")]
    Session(String),
}
