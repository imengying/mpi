//! Provider-independent request/response types plus the thinking-level mapping.
//!
//! Message serialisation is done with structs (never hand-built `Value`s) so field
//! order is stable and prompt-cache prefixes stay byte-identical between turns.

pub mod anthropic;
pub mod client;
pub mod compat;
pub mod openai;
pub mod responses;

use serde::{Deserialize, Serialize};

use crate::config::{ModelConfig, Provider};
use crate::util;

/// The wire protocols pi speaks. The config's `api` field names one of these directly, so
/// this is also where a typo in it is caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    AnthropicMessages,
    OpenAiCompletions,
    OpenAiResponses,
}

impl Api {
    /// Every protocol, in the order the config's docs list them.
    pub const ALL: [Api; 3] =
        [Api::AnthropicMessages, Api::OpenAiCompletions, Api::OpenAiResponses];

    /// The name the config writes, which is also what the error message quotes.
    ///
    /// One word, because it is a value the user types: `completions` names the protocol as
    /// precisely as a longer `openai-` prefix would, next to a `base_url` that already says
    /// whose API it is.
    pub fn name(self) -> &'static str {
        match self {
            Api::AnthropicMessages => "messages",
            Api::OpenAiCompletions => "completions",
            Api::OpenAiResponses => "responses",
        }
    }

    pub fn from_name(name: &str) -> Option<Api> {
        Api::ALL.into_iter().find(|api| api.name() == name)
    }

    /// The endpoint a provider with no `base_url` writes to.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Api::AnthropicMessages => "https://api.anthropic.com",
            Api::OpenAiCompletions | Api::OpenAiResponses => "https://api.openai.com/v1",
        }
    }
}

/// One block of assistant output or user input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    /// An image the user pasted. `data` is the raw file bytes (not base64): each provider
    /// wants a different envelope, and the session file stores it as-is so a resumed
    /// session keeps the picture.
    Image {
        /// IANA media type, e.g. `image/png`. Both APIs want this spelled out.
        media_type: String,
        /// Base64 of the file bytes, ready to drop into either provider's envelope.
        data: String,
    },
    Thinking {
        thinking: String,
        /// Provider-supplied signature, when the API returns one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// A hosted-search item, replayed verbatim and never executed locally.
    ///
    /// `provider` and `model` are where it was issued. A `/model` switch drops it: the
    /// payload is private to that upstream, the same way an encrypted reasoning item is.
    Hosted {
        provider: String,
        model: String,
        payload: serde_json::Value,
    },
    /// A citation from a hosted search. Display only — it is not sent back.
    Citation {
        url: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        title: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: Vec<Block>,
    },
    Assistant {
        content: Vec<Block>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
    },
    Tool {
        tool_call_id: String,
        name: String,
        content: String,
    },
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message::User { content: vec![Block::Text { text: text.into() }] }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Message::Assistant { content: vec![Block::Text { text: text.into() }], stop_reason: None }
    }

    pub fn text(&self) -> String {
        let blocks = match self {
            Message::System { content } => return content.clone(),
            Message::User { content } | Message::Assistant { content, .. } => content,
            Message::Tool { content, .. } => return content.clone(),
        };
        blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn thinking(&self) -> String {
        let Message::Assistant { content, .. } = self else { return String::new() };
        content
            .iter()
            .filter_map(|block| match block {
                Block::Thinking { thinking, .. } => Some(thinking.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn tool_calls(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        let Message::Assistant { content, .. } = self else { return Vec::new() };
        content
            .iter()
            .filter_map(|block| match block {
                Block::ToolCall { id, name, arguments } => Some((id.as_str(), name.as_str(), arguments)),
                _ => None,
            })
            .collect()
    }

    /// chars/4 estimate, counting text, thinking and tool-call arguments.
    ///
    /// An image is charged a fixed cost rather than its byte length: providers bill by
    /// tiles or by a visual-token formula, not by how well a PNG compressed, so measuring
    /// the encoded bytes would be wildly wrong in both directions. The constant comes from
    /// the observed cost of a typical screenshot.
    pub fn estimate_tokens(&self) -> u64 {
        const IMAGE_TOKENS: u64 = 1_200;
        let mut chars = 0usize;
        let mut images = 0u64;
        match self {
            Message::System { content } | Message::Tool { content, .. } => chars += content.len(),
            Message::User { content } | Message::Assistant { content, .. } => {
                for block in content {
                    match block {
                        Block::Text { text } => chars += text.len(),
                        Block::Thinking { thinking, .. } => chars += thinking.len(),
                        Block::ToolCall { name, arguments, .. } => {
                            chars += name.len() + arguments.to_string().len()
                        }
                        Block::Hosted { payload, .. } => chars += payload.to_string().len(),
                        Block::Citation { url, title } => chars += url.len() + title.len(),
                        Block::Image { .. } => images += 1,
                    }
                }
            }
        }
        (chars as u64).div_ceil(4) + images * IMAGE_TOKENS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    /// The user stopped the turn with Esc. Kept distinct from `Error` so a resumed session
    /// and the transcript agree that the answer was cut short on purpose.
    Aborted,
    /// The upstream paused a hosted tool (Anthropic `pause_turn`) and must be sent the
    /// same assistant message again. Nothing is executed locally.
    Pause,
}

/// A tool as advertised to the model. Ordering is fixed by the tool registry so the
/// tool block is a stable cache prefix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Streaming events surfaced to the UI.
#[derive(Debug, Clone)]
pub enum Delta {
    Text(String),
    Thinking(String),
    /// A one-line status that is not part of the answer, such as a hosted search starting.
    Notice(String),
}

/// The outcome of one assistant turn.
#[derive(Debug, Clone)]
pub struct Completion {
    pub message: Message,
    pub usage: crate::config::Usage,
    pub stop_reason: StopReason,
    pub error: Option<String>,
}

impl Completion {
    pub fn text(&self) -> String {
        self.message.text()
    }

    pub fn tool_calls(&self) -> Vec<(String, String, serde_json::Value)> {
        self.message
            .tool_calls()
            .into_iter()
            .map(|(id, name, args)| (id.to_string(), name.to_string(), args.clone()))
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("HTTP 请求失败：{0}")]
    Transport(String),
    #[error("上游返回错误（HTTP {status}）：{message}")]
    Api { status: u16, message: String },
    #[error("响应解析失败：{0}")]
    Decode(String),
    #[error("provider「{0}」没有可用的 API key（设置对应环境变量或配置 api_key）")]
    MissingKey(String),
    #[error("provider「{0}」的 api「{1}」无法识别")]
    UnknownApi(String, String),
}

impl LlmError {
    /// Feed the raw error text through the overflow classifier later; this only
    /// exposes the message for pattern matching.
    pub fn message(&self) -> String {
        self.to_string()
    }
}

/// One request, in a shape both providers can render.
pub struct Request<'a> {
    pub model: &'a ModelConfig,
    pub provider: &'a Provider,
    /// First message is the system prompt (kept byte-identical every turn).
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub level: &'a str,
    pub session_id: &'a str,
    /// Disable the compression/compaction hints for side requests (summaries).
    pub cache_hints: bool,
}

/// The hosted-search shape to send on this request, if the model asked for it and the
/// provider has one that fits its protocol.
pub fn hosted_search(model: &ModelConfig, provider: &Provider) -> Option<compat::SearchFormat> {
    if !model.search {
        return None;
    }
    let format = provider.compat(model).search_format?;
    let api = provider.api().unwrap_or(Api::OpenAiCompletions);
    format.fits(api).then_some(format)
}

/// One citation, the way it is shown under an answer.
pub fn citation_line(title: &str, url: &str) -> String {
    if title.is_empty() { url.to_string() } else { format!("{title}  {url}") }
}

/// Citation lines on an assistant message, in order, without the "searched" header.
pub fn citation_lines(message: &Message) -> Vec<String> {
    let Message::Assistant { content, .. } = message else { return Vec::new() };
    content
        .iter()
        .filter_map(|block| match block {
            Block::Citation { url, title } => Some(citation_line(title, url)),
            _ => None,
        })
        .collect()
}

/// Map a pi level to the concrete knobs a provider wants.
///
/// The three-step mapping is: pi level → this struct → provider-specific field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingPlan {
    pub effort: Option<String>,
    /// Anthropic-style budget in tokens.
    pub budget_tokens: Option<u64>,
}

/// Concrete thinking budgets per level. The values are chosen so reasoning cannot eat
/// the whole answer budget; they are scaled down when `max_tokens` is small.
pub fn thinking_budget(level: &str, max_tokens: u64) -> u64 {
    let fraction = match level {
        "low" => 0.15,
        "medium" => 0.3,
        "high" => 0.5,
        "xhigh" => 0.7,
        "max" => 0.85,
        _ => 0.0,
    };
    let budget = (max_tokens as f64 * fraction) as u64;
    // Always leave room for an answer (and a tool call).
    let ceiling = max_tokens.saturating_sub(1024);
    budget.min(ceiling)
}

pub fn plan_thinking(model: &ModelConfig, level: &str, max_tokens: u64) -> ThinkingPlan {
    if !model.reasoning || level.is_empty() || level == "off" {
        return ThinkingPlan { effort: None, budget_tokens: None };
    }
    ThinkingPlan {
        effort: Some(level.to_string()),
        budget_tokens: Some(thinking_budget(level, max_tokens)),
    }
}

/// Clamp a level to the nearest one the model supports, reporting whether it moved.
pub fn clamp_level(model: &ModelConfig, level: &str) -> (String, bool) {
    let levels = model.levels();
    if levels.is_empty() {
        return (String::new(), !level.is_empty());
    }
    if levels.iter().any(|l| l == level) {
        return (level.to_string(), false);
    }
    let clamped = model.clamp_level(level);
    (clamped, true)
}

/// Rough token estimate for a whole request, used by the threshold compaction path.
pub fn estimate_context(messages: &[Message], system: &str) -> u64 {
    let system = util::estimate_tokens(system);
    messages.iter().map(Message::estimate_tokens).sum::<u64>() + system
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ModelConfig {
        serde_json::from_str(r#"{"id":"m","reasoning":true,"max_tokens":100000}"#).unwrap()
    }

    #[test]
    fn level_mapping_clamps_to_what_the_model_accepts() {
        let mut m = model();
        m.thinking_levels = vec!["low".into(), "high".into()];
        assert_eq!(clamp_level(&m, "high"), ("high".into(), false));
        assert_eq!(clamp_level(&m, "max"), ("high".into(), true));
        assert_eq!(clamp_level(&m, "low"), ("low".into(), false));
    }

    #[test]
    fn a_non_reasoning_model_has_no_levels() {
        let mut m = model();
        m.reasoning = false;
        assert_eq!(clamp_level(&m, "high"), (String::new(), true));
    }

    #[test]
    fn thinking_budget_never_consumes_the_whole_answer_budget() {
        assert!(thinking_budget("max", 8192) < 8192);
        assert_eq!(thinking_budget("low", 8192), 1228);
        assert_eq!(thinking_budget("off", 8192), 0);
    }

    #[test]
    fn plan_is_empty_without_reasoning() {
        let mut m = model();
        m.reasoning = false;
        let plan = plan_thinking(&m, "high", 1000);
        assert_eq!(plan.effort, None);
    }

    #[test]
    fn usage_cache_hit_rate_uses_prompt_tokens() {
        let usage = crate::config::Usage { input: 100, output: 50, cache_read: 900, cache_write: 0 };
        assert_eq!(usage.hit_rate(), Some(90.0));
        assert_eq!(crate::config::Usage::default().hit_rate(), None);
    }
}
