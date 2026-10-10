//! The one model call a compaction makes.
//!
//! A summary request is not a normal turn: tools are disabled, it carries its own cache
//! identity so it cannot poison the main conversation's cached prefix, and its result is
//! checked before use — a summary that called a tool, hit the length limit, or failed to
//! shrink the context is refused rather than written as a checkpoint.

use crate::config::{ModelConfig, Provider};
use crate::llm::{Message, Request, ToolSpec, client::Client};
use crate::util;

use super::CompactError;
use super::checkpoint::{check_summary, serialize_conversation, summary_prompt, truncate_chars};

/// Everything the summariser needs for one run.
pub struct SummaryRequest<'a> {
    pub provider: &'a Provider,
    pub model: &'a ModelConfig,
    pub session_id: &'a str,
    pub system_prompt: Option<&'a str>,
    pub tools: &'a [ToolSpec],
    pub level: &'a str,
    pub custom_instructions: Option<&'a str>,
}

/// Owned request material. The source session is never changed while planning a summary.
pub struct PreparedSummary {
    pub model: ModelConfig,
    pub messages: Vec<Message>,
    pub reuses_history_prefix: bool,
}

impl PreparedSummary {
    pub fn request<'a>(&'a self, settings: &'a SummaryRequest<'_>) -> Request<'a> {
        Request {
            model: &self.model,
            provider: settings.provider,
            messages: &self.messages,
            tools: settings.tools,
            level: settings.level,
            session_id: settings.session_id,
            // A summary is a separate routing operation. Reusing the conversation cache key
            // lets an auxiliary prompt poison the normal-turn prefix and makes a provider pin
            // the summary to the conversation's backend session.
            cache_hints: false,
        }
    }
}

/// Prefer the untouched history prefix. Fall back only when that request cannot fit.
pub fn prepare_summary(
    settings: &SummaryRequest<'_>,
    summarized: &[Message],
    split_turn: bool,
) -> Result<PreparedSummary, CompactError> {
    crate::llm::validate_tool_history(summarized)
        .map_err(|err| CompactError::Summarize(err.to_string()))?;
    let mut model = settings.model.clone();
    let window = model.context_window.unwrap_or(128_000);
    model.max_tokens = Some(model.max_tokens().min(16_384).min(window / 4).max(1));
    let tools_cost = settings
        .tools
        .iter()
        .map(|tool| serde_json::to_string(tool).map(|text| util::estimate_tokens(&text)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| CompactError::Summarize(err.to_string()))?
        .into_iter()
        .sum::<u64>();
    // Reserve room for protocol framing, tool-choice controls and the requested output.
    let budget = window
        .saturating_sub(model.max_tokens())
        .saturating_sub(tools_cost)
        .saturating_sub(512);
    let system = settings.system_prompt.map(|content| Message::System {
        content: content.to_string(),
    });
    let directive = summary_prompt("", settings.custom_instructions, split_turn);
    // Measured the way the request will actually be built from these same messages: the
    // reused-prefix branch sends the history verbatim, so on a protocol that replays
    // thinking (Anthropic, Responses) those blocks are prompt space here too, even though
    // the flattened branch below deliberately leaves them out.
    let replay = crate::llm::ThinkingReplay::for_provider(settings.provider);
    let fixed = system
        .as_ref()
        .map_or(0, |message| message.estimate_tokens(replay))
        + Message::user_text(directive.clone()).estimate_tokens(replay);
    if fixed >= budget {
        return Err(CompactError::InputTooLarge);
    }
    let history_cost = summarized
        .iter()
        .map(|message| message.estimate_tokens(replay))
        .sum::<u64>();
    let mut messages: Vec<Message> = system.into_iter().collect();
    let reuses_history_prefix = history_cost.saturating_add(fixed) <= budget;
    if reuses_history_prefix {
        messages.extend_from_slice(summarized);
        messages.push(Message::user_text(directive));
    } else {
        // Keep the system and tool prefix even when reasoning/images cannot fit. A single
        // bounded history item avoids orphaning calls or duplicating previous checkpoints.
        let conversation = serialize_conversation(summarized);
        let room = budget.saturating_sub(fixed).saturating_sub(256) as usize;
        let conversation = if util::estimate_tokens(&conversation) > room as u64 {
            truncate_chars(&conversation, room)
        } else {
            conversation
        };
        messages.push(Message::user_text(summary_prompt(
            &conversation,
            settings.custom_instructions,
            split_turn,
        )));
    }
    if crate::llm::estimate_request_context(&messages, "", &[], replay) > budget {
        return Err(CompactError::InputTooLarge);
    }
    Ok(PreparedSummary {
        model,
        messages,
        reuses_history_prefix,
    })
}

/// Summary calls use the same routing, prompt, tools and effort, and never execute tools.
pub async fn summarize(
    client: &Client,
    settings: SummaryRequest<'_>,
    summarized: &[Message],
    split_turn: bool,
) -> Result<(String, crate::config::Usage), CompactError> {
    let prepared = prepare_summary(&settings, summarized, split_turn)?;
    let completion = client
        .complete(&prepared.request(&settings))
        .await
        .map_err(|err| CompactError::Summarize(err.message()))?;
    let summary = check_summary(&completion)?;
    Ok((summary, completion.usage))
}
