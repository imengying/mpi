//! Shrinking tool results before a summary is considered.
//!
//! This is the cheap path: a tool result over the threshold keeps its head and its tail and
//! gets a reference to the session file in the middle. No model call, and in the common case
//! it is enough — one enormous `read` or command output is usually what pushed the context
//! over the threshold in the first place.
//!
//! Deterministic on purpose: the same conversation prunes to the same bytes, so the
//! prompt-cache prefix stays stable across the turns that both needed pruning.

use crate::llm::Message;
use crate::util;

pub struct PruneOutcome {
    pub replacement: Vec<Message>,
    pub tool_results: usize,
    pub saved_tokens: u64,
}

/// Run only under token pressure. This changes result text, never calls, status or user input.
/// Each replacement points to the append-only session containing the complete original.
pub fn prune_tool_results(
    messages: &[Message],
    session_path: &std::path::Path,
) -> Option<PruneOutcome> {
    const THRESHOLD: usize = 8192;
    const HEAD: usize = 4096;
    const TAIL: usize = 1024;
    let mut edits = Vec::new();
    let mut saved_tokens = 0;
    for (index, message) in messages.iter().enumerate() {
        let Message::Tool {
            tool_call_id,
            content,
            ..
        } = message
        else {
            continue;
        };
        let chars = content.chars().count();
        if chars <= THRESHOLD {
            continue;
        }
        let head: String = content.chars().take(HEAD).collect();
        let tail: String = content
            .chars()
            .rev()
            .take(TAIL)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let trimmed = format!(
            "{head}\n\n[工具结果中间已裁剪；原始内容保存在 {}，tool_call_id={tool_call_id}]\n\n{tail}",
            session_path.display()
        );
        if trimmed.chars().count() >= chars {
            continue;
        }
        saved_tokens +=
            util::estimate_tokens(content).saturating_sub(util::estimate_tokens(&trimmed));
        edits.push((index, trimmed));
    }
    if edits.is_empty() {
        return None;
    }
    let tool_results = edits.len();
    let mut replacement = messages.to_vec();
    for (index, trimmed) in edits {
        if let Message::Tool { content, .. } = &mut replacement[index] {
            *content = trimmed;
        }
    }
    Some(PruneOutcome {
        replacement,
        tool_results,
        saved_tokens,
    })
}
