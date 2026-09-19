//! OpenAI Chat Completions (and anything that speaks the same dialect).
//!
//! Request bodies are built from structs with a fixed field order so the JSON prefix
//! is byte-identical between turns — that is what makes the gateway's implicit prompt
//! cache hit. `prompt_cache_key` carries the session id for the same reason.

use serde::{Deserialize, Serialize};

use super::compat::{Compat, ThinkingFormat};
use super::{Block, Completion, Delta, LlmError, Message, Request, StopReason, plan_thinking};
use crate::config::Usage;

#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<&'static str>,
}

impl CacheControl {
    fn ephemeral(long: bool) -> Self {
        CacheControl { kind: "ephemeral", ttl: long.then_some("1h") }
    }
}

/// One part of a message body. OpenAI shapes a text part and an image part differently, so
/// the two are separate variants rather than one struct with optional fields.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum ContentBlock {
    Text {
        #[serde(rename = "type")]
        kind: &'static str,
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        #[serde(rename = "type")]
        kind: &'static str,
        image_url: ImageUrl,
    },
}

/// The data-URI or plain URL form OpenAI expects for an image part.
#[derive(Debug, Clone, Serialize)]
struct ImageUrl {
    url: String,
}

impl ContentBlock {
    fn text(text: impl Into<String>, cache: bool) -> Self {
        ContentBlock::Text {
            kind: "text",
            text: text.into(),
            cache_control: cache.then(|| CacheControl::ephemeral(false)),
        }
    }

    /// Inline image as a data URI, which is what both OpenAI and the gateways in front of
    /// it accept; a separate upload endpoint would need a second round trip and a place to
    /// keep the uploaded blob.
    fn image(media_type: &str, base64_data: &str) -> Self {
        ContentBlock::Image {
            kind: "image_url",
            image_url: ImageUrl {
                url: format!("data:{media_type};base64,{base64_data}"),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum ChatContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize)]
struct FunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize)]
struct ToolCallOut {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: FunctionCall,
}

#[derive(Debug, Clone, Serialize)]
struct ToolDef {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolFunction,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct ToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// One outgoing message. A single struct with fixed field order covers all roles;
/// unused fields are omitted, which keeps the serialised prefix stable.
#[derive(Debug, Clone, Serialize, Default)]
struct ChatMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ChatContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// Some gateways demand the field be present (even empty) on every assistant message
    /// once the history contains reasoning.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

impl ChatMessage {
    fn new(role: &'static str) -> Self {
        ChatMessage { role, ..Default::default() }
    }

    fn content(mut self, content: ChatContent) -> Self {
        self.content = Some(content);
        self
    }

    fn text(self, text: &str, cache: bool) -> Self {
        self.content(text_content(text, cache))
    }
}

#[derive(Debug, Serialize)]
pub struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingToggle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enable_thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_token_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_retention: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ThinkingToggle {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    clear_thinking: Option<bool>,
}

/// Build the request body for one turn.
pub fn build_request(req: &Request<'_>, stream: bool) -> ChatRequest {
    let model = req.model;
    let provider = req.provider;
    let compat: Compat = provider.compat(model);
    let max_tokens = model.max_tokens();
    let plan = plan_thinking(model, req.level, max_tokens);
    // Only gateways that accept the marker get block-shaped content at all; for the
    // rest the request stays in the plain-string shape they expect.
    let cache_marker = req.cache_hints && compat.supports_cache_control;

    let mut messages: Vec<ChatMessage> = Vec::new();
    for (index, message) in req.messages.iter().enumerate() {
        let is_last = index + 1 == req.messages.len();
        let last_block_hint = is_last && cache_marker;
        match message {
            Message::System { content } => {
                let role = if compat.supports_developer_role { "developer" } else { "system" };
                messages.push(ChatMessage::new(role).text(content, last_block_hint));
            }
            Message::User { content } => {
                messages.push(ChatMessage::new("user").content(user_content(content, last_block_hint)));
            }
            Message::Assistant { content, .. } => {
                let text = join_text(content);
                let calls: Vec<ToolCallOut> = message
                    .tool_calls()
                    .into_iter()
                    .map(|(id, name, args)| ToolCallOut {
                        id: id.to_string(),
                        kind: "function",
                        function: FunctionCall {
                            name: name.to_string(),
                            arguments: args.to_string(),
                        },
                    })
                    .collect();
                let thinking = message.thinking();
                let has_thinking = !thinking.is_empty();
                // The field has to be present (even empty) once the history contains
                // reasoning, or the upstream rejects the request.
                let reasoning_content = compat
                    .requires_reasoning_content_on_assistant
                    .then(|| if has_thinking { thinking.clone() } else { String::new() });
                // Providers that cannot replay thinking blocks want the reasoning as
                // plain text instead, inside `<thinking>` tags.
                let text = if compat.requires_thinking_as_text && has_thinking {
                    format!("<thinking>\n{thinking}\n</thinking>\n\n{text}")
                } else {
                    text
                };
                let mut message = ChatMessage::new("assistant");
                // Providers reject an assistant message with neither content nor tool calls,
                // so the content field is dropped when only calls remain.
                if !(text.is_empty() && !calls.is_empty()) {
                    message = message.text(&text, last_block_hint);
                }
                message.tool_calls = (!calls.is_empty()).then_some(calls);
                message.reasoning_content = reasoning_content;
                messages.push(message);
            }
            Message::Tool { tool_call_id, content, .. } => {
                let mut message = ChatMessage::new("tool");
                message.content = Some(ChatContent::Text(content.clone()));
                message.tool_call_id = Some(tool_call_id.clone());
                messages.push(message);
            }
        }
    }

    // Cache breakpoint: the very end of the conversation, so the whole history is
    // reusable on the next turn (only for gateways that accept the marker).
    if cache_marker
        && let Some(last) = messages.last_mut()
    {
        last.cache_control = Some(CacheControl::ephemeral(compat.supports_long_cache));
    }

    let tools: Vec<ToolDef> = req
        .tools
        .iter()
        .enumerate()
        .map(|(index, tool)| ToolDef {
            kind: "function",
            function: ToolFunction {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
                strict: compat.supports_strict_mode.then_some(true),
            },
            cache_control: (cache_marker && index + 1 == req.tools.len())
                .then(|| CacheControl::ephemeral(compat.supports_long_cache)),
        })
        .collect();

    let mut request = ChatRequest {
        model: model.id.clone(),
        messages,
        tools,
        tool_choice: (!req.tools.is_empty()).then_some("auto"),
        stream,
        stream_options: stream.then_some(StreamOptions { include_usage: compat.supports_usage_in_streaming }),
        max_tokens: None,
        max_completion_tokens: None,
        reasoning_effort: None,
        thinking: None,
        enable_thinking: None,
        thinking_token_budget: None,
        thinking_budget: None,
        thinking_budget_tokens: None,
        prompt_cache_key: req.cache_hints.then(|| req.session_id.to_string()),
        prompt_cache_retention: (req.cache_hints && compat.supports_long_cache)
            .then(|| "24h".to_string()),
    };

    if compat.max_tokens_field == "max_completion_tokens" {
        request.max_completion_tokens = Some(max_tokens);
    } else {
        request.max_tokens = Some(max_tokens);
    }

    let effort = plan.effort.clone();
    if let Some(level) = &effort {
        match compat.thinking_format {
            ThinkingFormat::Openai => {
                if compat.supports_reasoning_effort {
                    request.reasoning_effort = Some(level.clone());
                }
            }
            ThinkingFormat::Deepseek => {
                request.thinking = Some(ThinkingToggle { kind: "enabled", clear_thinking: None });
                request.reasoning_effort = Some(level.clone());
            }
            ThinkingFormat::Zai => {
                request.thinking = Some(ThinkingToggle { kind: "enabled", clear_thinking: Some(false) });
                if compat.supports_reasoning_effort {
                    request.reasoning_effort = Some(level.clone());
                }
            }
            ThinkingFormat::Qwen => {
                request.enable_thinking = Some(true);
                if compat.supports_reasoning_effort {
                    request.reasoning_effort = Some(level.clone());
                }
            }
            ThinkingFormat::Llamacpp => {
                request.thinking_budget_tokens = plan.budget_tokens;
            }
            ThinkingFormat::Anthropic => {}
            ThinkingFormat::None => {}
        }
        // Cap the reasoning phase: reasoning and the answer share `max_tokens`, so an
        // uncapped budget can leave no room for text or a tool call.
        match compat.thinking_format {
            ThinkingFormat::Qwen => request.thinking_budget = plan.budget_tokens,
            ThinkingFormat::Openai | ThinkingFormat::Deepseek | ThinkingFormat::Zai => {
                request.thinking_token_budget = plan.budget_tokens
            }
            _ => {}
        }
    }

    request
}

fn join_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.clone()),
            // An image has no text form. Callers that must preserve it use `user_content`,
            // which builds parts; this function is for the places that want prose only.
            Block::Image { .. } => None,
            Block::Thinking { .. } => None,
            Block::ToolCall { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// A plain string, or a single text block carrying the cache marker when the request has
/// one to place here.
fn text_content(text: &str, cache: bool) -> ChatContent {
    if cache {
        ChatContent::Blocks(vec![ContentBlock::text(text, true)])
    } else {
        ChatContent::Text(text.to_string())
    }
}

/// A user message body: the text plus any pasted images, in that order.
///
/// The plain-string shape is kept when there is only text — some gateways reject the
/// block-array form outright, and there is no reason to send it for an ordinary message.
/// Once an image is present the array form is unavoidable, and the text goes in as its own
/// part so the two do not run together.
fn user_content(blocks: &[Block], last_block_hint: bool) -> ChatContent {
    let has_image = blocks.iter().any(|block| matches!(block, Block::Image { .. }));
    if !has_image {
        return text_content(&join_text(blocks), last_block_hint);
    }
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            Block::Text { text } => {
                if !text.is_empty() {
                    parts.push(ContentBlock::text(text.clone(), false));
                }
            }
            Block::Image { media_type, data } => {
                parts.push(ContentBlock::image(media_type, data));
            }
            Block::Thinking { .. } | Block::ToolCall { .. } => {}
        }
    }
    // The cache marker rides on the last part, which is what the prefix actually ends on.
    if last_block_hint
        && let Some(ContentBlock::Text { cache_control, .. }) = parts.last_mut()
    {
        *cache_control = Some(CacheControl::ephemeral(false));
    }
    ChatContent::Blocks(parts)
}

pub fn endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_text: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// Accumulator shared by the streaming and non-streaming paths.
#[derive(Debug, Default)]
pub struct Assembler {
    text: String,
    thinking: String,
    calls: Vec<(String, String, String)>,
    usage: Usage,
    stop_reason: Option<String>,
    error: Option<String>,
}

impl Assembler {
    pub(crate) fn push_delta(&mut self, delta: &StreamDelta, on_delta: &mut dyn FnMut(Delta)) {
        let thinking = delta
            .reasoning_content
            .as_deref()
            .or(delta.reasoning.as_deref())
            .or(delta.reasoning_text.as_deref())
            .unwrap_or("");
        if !thinking.is_empty() {
            self.thinking.push_str(thinking);
            on_delta(Delta::Thinking(thinking.to_string()));
        }
        if let Some(content) = &delta.content
            && !content.is_empty()
        {
            self.text.push_str(content);
            on_delta(Delta::Text(content.clone()));
        }
        for call in &delta.tool_calls {
            while self.calls.len() <= call.index {
                self.calls.push((String::new(), String::new(), String::new()));
            }
            let slot = &mut self.calls[call.index];
            if let Some(id) = &call.id
                && !id.is_empty()
            {
                slot.0 = id.clone();
            }
            if let Some(function) = &call.function {
                if let Some(name) = &function.name
                    && !name.is_empty()
                {
                    slot.1 = name.clone();
                }
                if let Some(args) = &function.arguments {
                    slot.2.push_str(args);
                }
            }
        }
    }

    pub(crate) fn set_usage(&mut self, usage: &WireUsage) {
        let cached = usage.prompt_tokens_details.as_ref().map(|d| d.cached_tokens).unwrap_or(0);
        self.usage = Usage {
            input: usage.prompt_tokens.saturating_sub(cached),
            output: usage.completion_tokens,
            cache_read: cached,
            cache_write: 0,
        };
        let _ = usage.total_tokens;
    }

    pub fn finish(mut self) -> Completion {
        let mut content: Vec<Block> = Vec::new();
        if !self.thinking.is_empty() {
            content.push(Block::Thinking { thinking: std::mem::take(&mut self.thinking), signature: None });
        }
        if !self.text.is_empty() {
            content.push(Block::Text { text: std::mem::take(&mut self.text) });
        }
        for (index, (id, name, args)) in self.calls.into_iter().enumerate() {
            if name.is_empty() {
                continue;
            }
            let arguments = if args.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&args).unwrap_or(serde_json::Value::String(args))
            };
            content.push(Block::ToolCall {
                id: if id.is_empty() { format!("call_{index}") } else { id },
                name,
                arguments,
            });
        }
        let stop_reason = match self.error {
            Some(message) => {
                return Completion {
                    message: Message::Assistant { content, stop_reason: Some(StopReason::Error) },
                    usage: self.usage,
                    stop_reason: StopReason::Error,
                    error: Some(message),
                };
            }
            None => match self.stop_reason.as_deref() {
                Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
                Some("length") | Some("max_tokens") => StopReason::Length,
                _ => {
                    if content.iter().any(|b| matches!(b, Block::ToolCall { .. })) {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    }
                }
            },
        };
        Completion {
            message: Message::Assistant { content, stop_reason: Some(stop_reason) },
            usage: self.usage,
            stop_reason,
            error: None,
        }
    }
}

/// Parse one SSE payload; `None` for frames that carry no JSON (e.g. `[DONE]`).
pub fn parse_frame(payload: &str) -> Result<Option<StreamChunk>, LlmError> {
    let trimmed = payload.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|err| LlmError::Decode(format!("{err}: {}", crate::util::truncate(trimmed, 300, "…"))))
}

impl StreamChunk {
    pub(crate) fn feed(self, assembler: &mut Assembler, on_delta: &mut dyn FnMut(Delta)) {
        if let Some(error) = self.error {
            assembler.error = Some(error.message);
            return;
        }
        if let Some(usage) = &self.usage {
            assembler.set_usage(usage);
        }
        for choice in &self.choices {
            if let Some(reason) = &choice.finish_reason
                && !reason.is_empty()
            {
                assembler.stop_reason = Some(reason.clone());
            }
            assembler.push_delta(&choice.delta, on_delta);
        }
    }
}

/// Non-streaming response shape.
#[derive(Debug, Deserialize)]
pub struct FullResponse {
    #[serde(default)]
    choices: Vec<FullChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct FullChoice {
    #[serde(default)]
    message: FullMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct FullMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_text: Option<String>,
    #[serde(default)]
    tool_calls: Vec<FullToolCall>,
    #[serde(default)]
    function_call: Option<FullFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct FullToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FullFunctionCall>,
}

#[derive(Debug, Default, Deserialize)]
struct FullFunctionCall {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl FullResponse {
    /// Fold a non-streaming body into the same accumulator the SSE path uses, so both
    /// shapes produce identical messages.
    pub fn assemble(self) -> Completion {
        let mut assembler = Assembler::default();
        let mut sink = |_: Delta| {};
        if let Some(usage) = &self.usage {
            assembler.set_usage(usage);
        }
        for choice in self.choices {
            if let Some(reason) = choice.finish_reason {
                assembler.stop_reason = Some(reason);
            }
            let message = choice.message;
            let thinking = message
                .reasoning_content
                .or(message.reasoning)
                .or(message.reasoning_text)
                .unwrap_or_default();
            let mut calls: Vec<FullToolCall> = message.tool_calls;
            if let Some(function) = message.function_call {
                calls.push(FullToolCall { id: None, function: Some(function) });
            }
            let delta = StreamDelta {
                content: message.content.filter(|c| !c.is_empty()),
                reasoning_content: (!thinking.is_empty()).then_some(thinking),
                reasoning: None,
                reasoning_text: None,
                tool_calls: calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| ToolCallDelta {
                        index,
                        id: call.id,
                        function: call.function.map(|f| FunctionDelta { name: f.name, arguments: f.arguments }),
                    })
                    .collect(),
            };
            assembler.push_delta(&delta, &mut sink);
        }
        assembler.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, Provider};

    fn request<'a>(model: &'a ModelConfig, provider: &'a Provider, messages: &'a [Message]) -> Request<'a> {
        Request {
            model,
            provider,
            messages,
            tools: &[],
            level: "high",
            session_id: "sess-1",
            cache_hints: true,
        }
    }

    fn anthropic_style_model() -> (ModelConfig, Provider) {
        let model: ModelConfig = serde_json::from_str(
            r#"{"id":"m","reasoning":true,"max_tokens":64000,"thinking_levels":["low","high","max"]}"#,
        )
        .unwrap();
        let provider: Provider = serde_json::from_str(
            r#"{"name":"name","api":"openai-completions","base_url":"url"}"#,
        )
        .unwrap();
        (model, provider)
    }

    #[test]
    fn session_id_is_sent_as_the_prompt_cache_key() {
        let (model, provider) = anthropic_style_model();
        let messages = vec![Message::user_text("hi")];
        let body = serde_json::to_value(build_request(&request(&model, &provider, &messages), true)).unwrap();
        assert_eq!(body["prompt_cache_key"], "sess-1");
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 64000);
    }

    #[test]
    fn reasoning_style_fields_follow_the_configured_format() {
        let (mut model, mut provider) = anthropic_style_model();
        model.compat = Some(serde_json::from_str(r#"{"thinking_format":"deepseek"}"#).unwrap());
        provider.compat = Some(serde_json::from_str(r#"{"requires_reasoning_content_on_assistant":true}"#).unwrap());
        let messages = vec![
            Message::user_text("hi"),
            Message::Assistant {
                content: vec![Block::Thinking { thinking: "why".into(), signature: None }, Block::Text { text: "ok".into() }],
                stop_reason: Some(StopReason::Stop),
            },
            Message::user_text("again"),
        ];
        let body = serde_json::to_value(build_request(&request(&model, &provider, &messages), true)).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        // 85% of max_tokens would swallow the answer, so the budget is clamped.
        assert!(body["thinking_token_budget"].as_u64().unwrap() < 64000);
        // The replayed assistant message must carry reasoning_content.
        assert_eq!(body["messages"][1]["reasoning_content"], "why");
    }

    #[test]
    fn thinking_can_be_replayed_as_plain_text() {
        let (mut model, mut provider) = anthropic_style_model();
        model.compat = Some(serde_json::from_str(r#"{"thinking_format":"none"}"#).unwrap());
        provider.compat = Some(serde_json::from_str(r#"{"requires_thinking_as_text":true}"#).unwrap());
        let messages = vec![
            Message::user_text("hi"),
            Message::Assistant {
                content: vec![Block::Thinking { thinking: "why".into(), signature: None }, Block::Text { text: "ok".into() }],
                stop_reason: Some(StopReason::Stop),
            },
        ];
        let body = serde_json::to_value(build_request(&request(&model, &provider, &messages), false)).unwrap();
        let text = body["messages"][1]["content"].as_str().unwrap();
        assert!(text.starts_with("<thinking>\nwhy\n</thinking>"));
        assert!(text.ends_with("ok"));
    }

    #[test]
    fn tool_calls_are_serialised_with_their_arguments() {
        let (model, provider) = anthropic_style_model();
        let messages = vec![Message::Assistant {
            content: vec![Block::ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
            stop_reason: Some(StopReason::ToolUse),
        }];
        let body = serde_json::to_value(build_request(&request(&model, &provider, &messages), true)).unwrap();
        assert_eq!(body["messages"][0]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(body["messages"][0]["tool_calls"][0]["function"]["arguments"], "{\"path\":\"a.txt\"}");
        assert!(body["messages"][0]["content"].is_null());
    }

    #[test]
    fn streaming_frames_assemble_text_thinking_and_calls() {
        let mut assembler = Assembler::default();
        let mut text = String::new();
        let mut sink = |delta: Delta| match delta {
            Delta::Text(t) => text.push_str(&t),
            Delta::Thinking(_) => {}
        };
        let frames = [
            r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
            r#"{"choices":[{"delta":{"content":"he"}}]}"#,
            r#"{"choices":[{"delta":{"content":"llo"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4}}}"#,
            "[DONE]",
        ];
        for frame in frames {
            if let Some(chunk) = parse_frame(frame).unwrap() {
                chunk.feed(&mut assembler, &mut sink);
            }
        }
        let completion = assembler.finish();
        assert_eq!(text, "hello");
        assert_eq!(completion.text(), "hello");
        assert_eq!(completion.stop_reason, StopReason::ToolUse);
        assert_eq!(completion.usage.input, 6);
        assert_eq!(completion.usage.cache_read, 4);
        let calls = completion.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "c1");
        assert_eq!(calls[0].2["path"], "a");
    }

    #[test]
    fn an_in_band_error_becomes_an_error_completion() {
        let mut assembler = Assembler::default();
        let mut sink = |_: Delta| {};
        let chunk = parse_frame(r#"{"error":{"message":"context length exceeded"}}"#).unwrap().unwrap();
        chunk.feed(&mut assembler, &mut sink);
        let completion = assembler.finish();
        assert_eq!(completion.stop_reason, StopReason::Error);
        assert!(completion.error.unwrap().contains("context length"));
    }

    #[test]
    fn base_url_gets_the_right_endpoint() {
        assert_eq!(endpoint("http://x/v1"), "http://x/v1/chat/completions");
        assert_eq!(endpoint("http://x/v1/"), "http://x/v1/chat/completions");
        assert_eq!(endpoint("http://x/v1/chat/completions"), "http://x/v1/chat/completions");
    }
}
