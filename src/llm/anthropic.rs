//! Anthropic Messages API.
//!
//! Prompt caching is explicit here: `cache_control` breakpoints go on the system
//! prompt, the last tool definition and the last content block of the conversation.
//! Anthropic allows four, so three is safely inside the limit — and it makes the
//! "system prompt must not change between turns" rule load-bearing.

use serde::{Deserialize, Serialize};

use super::{Block, Completion, Delta, LlmError, Message, Request, StopReason, plan_thinking};
use crate::config::Usage;

const ANTHROPIC_VERSION: &str = "2023-06-01";

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

/// Where an image block's bytes come from.
#[derive(Debug, Clone, Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    kind: &'static str,
    media_type: String,
    data: String,
}

impl ImageSource {
    fn base64(media_type: &str, data: &str) -> Self {
        ImageSource {
            kind: "base64",
            media_type: media_type.to_string(),
            data: data.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// An image block. Anthropic takes base64 in a nested `source` object, unlike OpenAI's
    /// single data-URI string.
    Image {
        source: ImageSource,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Clone, Serialize)]
struct OutMessage {
    role: &'static str,
    content: Vec<OutBlock>,
}

#[derive(Debug, Clone, Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct ToolDef {
    name: String,
    description: String,
    input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemBlock>>,
    messages: Vec<OutMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    pub stream: bool,
}

pub fn build_request(req: &Request<'_>) -> MessagesRequest {
    let model = req.model;
    let provider = req.provider;
    let compat = provider.compat(model);
    let max_tokens = model.max_tokens();
    let plan = plan_thinking(model, req.level, max_tokens);
    // Thinking tokens come out of `max_tokens`, so the budget has to be subtracted
    // from it rather than sent on top.
    let thinking_budget = plan.budget_tokens.map(|b| b.min(max_tokens.saturating_sub(1024)));
    let effective_max = match thinking_budget {
        Some(budget) => max_tokens.saturating_sub(budget).max(1024),
        None => max_tokens,
    };

    let mut system = None;
    let mut messages: Vec<OutMessage> = Vec::new();
    let mut pending_results: Vec<OutBlock> = Vec::new();
    let mut cache = CacheControl::ephemeral(compat.supports_long_cache);

    for message in req.messages {
        match message {
            Message::System { content } => {
                // Breakpoint 1: the system prompt. It must be byte-identical every
                // turn, which is why cwd/time/model never live here.
                system = Some(vec![SystemBlock {
                    kind: "text",
                    text: content.clone(),
                    cache_control: req.cache_hints.then(|| cache.clone()),
                }]);
            }
            Message::User { content: blocks } => {
                flush_results(&mut messages, &mut pending_results);
                messages.push(OutMessage {
                    role: "user",
                    content: blocks
                        .iter()
                        .filter_map(|block| match block {
                            Block::Text { text } => {
                                Some(OutBlock::Text { text: text.clone(), cache_control: None })
                            }
                            Block::Image { media_type, data } => Some(OutBlock::Image {
                                source: ImageSource::base64(media_type, data),
                            }),
                            _ => None,
                        })
                        .collect(),
                });
            }
            Message::Assistant { content: blocks, .. } => {
                flush_results(&mut messages, &mut pending_results);
                let mut out = Vec::new();
                for block in blocks {
                    match block {
                        Block::Text { text } => {
                            out.push(OutBlock::Text { text: text.clone(), cache_control: None })
                        }
                        Block::Thinking { thinking, signature } => out.push(OutBlock::Thinking {
                            thinking: thinking.clone(),
                            signature: signature.clone(),
                        }),
                        Block::ToolCall { id, name, arguments } => out.push(OutBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: arguments.clone(),
                        }),
                        // A model cannot send an image back, so this never occurs in a
                        // well-formed history; dropped rather than sent as an empty block.
                        Block::Image { .. } => {}
                    }
                }
                if !out.is_empty() {
                    messages.push(OutMessage { role: "assistant", content: out });
                }
            }
            Message::Tool { tool_call_id, content, .. } => {
                // Tool results are user-role blocks in the Anthropic shape, and all of
                // them for one turn have to arrive together.
                pending_results.push(OutBlock::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: content.clone(),
                    cache_control: None,
                });
            }
        }
    }
    flush_results(&mut messages, &mut pending_results);

    // Breakpoint 3: the very end of the conversation, so the whole history is cached.
    if req.cache_hints
        && let Some(last) = messages.last_mut()
        && let Some(block) = last.content.last_mut()
    {
        match block {
            OutBlock::Text { cache_control, .. } | OutBlock::ToolResult { cache_control, .. } => {
                *cache_control = Some(cache.clone())
            }
            _ => {}
        }
    }
    let _ = &mut cache;

    let tools: Vec<ToolDef> = req
        .tools
        .iter()
        .enumerate()
        .map(|(index, tool)| ToolDef {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.parameters.clone(),
            // Breakpoint 2: the tool list is stable, so it is a reliable boundary.
            cache_control: (req.cache_hints && index + 1 == req.tools.len())
                .then(|| CacheControl::ephemeral(compat.supports_long_cache)),
        })
        .collect();

    MessagesRequest {
        model: model.id.clone(),
        max_tokens: effective_max,
        system,
        messages,
        tools,
        thinking: thinking_budget.map(|budget_tokens| ThinkingConfig { kind: "enabled", budget_tokens }),
        stream: true,
    }
}

fn flush_results(messages: &mut Vec<OutMessage>, pending: &mut Vec<OutBlock>) {
    if pending.is_empty() {
        return;
    }
    messages.push(OutMessage { role: "user", content: std::mem::take(pending) });
}

pub fn endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1/messages") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/messages")
    } else {
        format!("{trimmed}/v1/messages")
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    message: Option<WireMessage>,
    #[serde(default)]
    content_block: Option<WireBlock>,
    #[serde(default)]
    delta: Option<WireDelta>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
struct WireBlock {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WireDelta {
    #[serde(default)]
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Clone)]
enum Partial {
    Text(String),
    Thinking { text: String, signature: Option<String> },
    ToolUse { id: String, name: String, json: String },
}

impl Partial {
    fn into_block(self) -> Option<Block> {
        match self {
            Partial::Text(text) => (!text.is_empty()).then_some(Block::Text { text }),
            Partial::Thinking { text, signature } => {
                (!text.is_empty()).then_some(Block::Thinking { thinking: text, signature })
            }
            Partial::ToolUse { id, name, json } => {
                let arguments = if json.trim().is_empty() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&json).unwrap_or(serde_json::Value::String(json))
                };
                Some(Block::ToolCall { id, name, arguments })
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Assembler {
    blocks: Vec<Partial>,
    usage: Usage,
    stop_reason: Option<String>,
    error: Option<String>,
}

impl Assembler {
    fn slot(&mut self, index: usize) -> &mut Partial {
        while self.blocks.len() <= index {
            self.blocks.push(Partial::Text(String::new()));
        }
        &mut self.blocks[index]
    }

    fn start(&mut self, index: usize, block: &WireBlock) {
        while self.blocks.len() <= index {
            self.blocks.push(Partial::Text(String::new()));
        }
        self.blocks[index] = match block {
            WireBlock { thinking: Some(_), .. } | WireBlock { signature: Some(_), .. } => {
                Partial::Thinking { text: String::new(), signature: None }
            }
            WireBlock { id: Some(_), name: Some(_), .. } => {
                Partial::ToolUse { id: String::new(), name: String::new(), json: String::new() }
            }
            _ => Partial::Text(String::new()),
        };
    }

    pub fn feed(&mut self, event: StreamEvent, on_delta: &mut dyn FnMut(Delta)) {
        match event.kind.as_str() {
            "error" => {
                self.error = Some(
                    event.error.map(|e| e.message).unwrap_or_else(|| "上游返回未知错误".into()),
                );
            }
            "message_start" => {
                if let Some(message) = &event.message {
                    if let Some(usage) = &message.usage {
                        self.set_usage(usage);
                    }
                    if let Some(reason) = &message.stop_reason {
                        self.stop_reason = Some(reason.clone());
                    }
                }
            }
            "content_block_start" => {
                if let (Some(index), Some(block)) = (event.index, &event.content_block) {
                    self.start(index, block);
                    if let Some(id) = block.id.clone()
                        && let Partial::ToolUse { id: slot, .. } = &mut self.blocks[index]
                    {
                        *slot = id;
                    }
                }
            }
            "content_block_delta" => {
                let (Some(index), Some(delta)) = (event.index, event.delta) else { return };
                let slot = self.slot(index);
                // `delta.kind` is redundant with which key is present, but it guards
                // against a frame that carries an unexpected payload.
                let kind = delta.kind.as_deref();
                if kind == Some("thinking_delta")
                    && let Some(thinking) = delta.thinking
                {
                    if let Partial::Thinking { text, .. } = slot {
                        text.push_str(&thinking);
                    }
                    on_delta(Delta::Thinking(thinking));
                }
                if let Some(text) = delta.text {
                    if let Partial::Text(current) = slot {
                        current.push_str(&text);
                    }
                    on_delta(Delta::Text(text));
                } else if let Some(signature) = delta.signature {
                    if let Partial::Thinking { signature: slot, .. } = slot {
                        *slot = Some(signature);
                    }
                } else if let Some(json) = delta.partial_json
                    && let Partial::ToolUse { json: slot, .. } = slot
                {
                    slot.push_str(&json);
                }
            }
            "message_delta" => {
                if let Some(delta) = event.delta
                    && let Some(reason) = delta.stop_reason
                {
                    self.stop_reason = Some(reason);
                }
                if let Some(usage) = &event.usage {
                    self.set_usage(usage);
                }
            }
            _ => {}
        }
    }

    fn set_usage(&mut self, usage: &WireUsage) {
        // Anthropic reports cumulative numbers, so later events overwrite earlier
        // ones rather than summing.
        if usage.input_tokens > 0 || usage.cache_read_input_tokens > 0 || usage.cache_creation_input_tokens > 0
        {
            self.usage.input = usage.input_tokens;
            self.usage.cache_read = usage.cache_read_input_tokens;
            self.usage.cache_write = usage.cache_creation_input_tokens;
        }
        if usage.output_tokens > 0 {
            self.usage.output = usage.output_tokens;
        }
    }

    pub fn finish(self) -> Completion {
        let content: Vec<Block> = self.blocks.into_iter().filter_map(Partial::into_block).collect();
        if let Some(message) = self.error {
            return Completion {
                message: Message::Assistant { content, stop_reason: Some(StopReason::Error) },
                usage: self.usage,
                stop_reason: StopReason::Error,
                error: Some(message),
            };
        }
        let stop_reason = match self.stop_reason.as_deref() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::Length,
            _ => {
                if content.iter().any(|b| matches!(b, Block::ToolCall { .. })) {
                    StopReason::ToolUse
                } else {
                    StopReason::Stop
                }
            }
        };
        Completion {
            message: Message::Assistant { content, stop_reason: Some(stop_reason) },
            usage: self.usage,
            stop_reason,
            error: None,
        }
    }
}

/// Parse `event:`/`data:` pairs. Returns the event name and decoded payload.
pub fn parse_event(event_name: Option<&str>, payload: &str) -> Result<Option<StreamEvent>, LlmError> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|err| LlmError::Decode(format!("{err}: {}", crate::util::truncate(trimmed, 300, "…"))))?;
    if let Some(name) = event_name
        && value.get("type").is_none()
        && let Some(object) = value.as_object_mut()
    {
        object.insert("type".into(), serde_json::Value::String(name.to_string()));
    }
    serde_json::from_value(value).map(Some).map_err(|err| LlmError::Decode(err.to_string()))
}

/// Headers shared by streaming and non-streaming requests.
pub fn headers(api_key: &str, long_cache: bool) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("x-api-key", api_key.to_string()),
        ("anthropic-version", ANTHROPIC_VERSION.to_string()),
        ("content-type", "application/json".to_string()),
    ];
    if long_cache {
        headers.push(("anthropic-beta", "extended-cache-ttl-2025-04-11".to_string()));
    }
    headers
}

/// Non-streaming response shape, used for compaction requests that do not need to be
/// rendered token by token.
#[derive(Debug, Deserialize)]
pub struct FullResponse {
    #[serde(default)]
    content: Vec<WireBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<WireError>,
}

impl FullResponse {
    pub fn assemble(self) -> Completion {
        let usage = self
            .usage
            .as_ref()
            .map(|u| Usage {
                input: u.input_tokens,
                output: u.output_tokens,
                cache_read: u.cache_read_input_tokens,
                cache_write: u.cache_creation_input_tokens,
            })
            .unwrap_or_default();
        if let Some(error) = self.error {
            return Completion {
                message: Message::Assistant { content: Vec::new(), stop_reason: Some(StopReason::Error) },
                usage,
                stop_reason: StopReason::Error,
                error: Some(error.message),
            };
        }
        let mut content = Vec::new();
        for block in self.content {
            if let Some(thinking) = block.thinking {
                content.push(Block::Thinking { thinking, signature: block.signature });
            } else if let Some(text) = block.text {
                content.push(Block::Text { text });
            } else if let (Some(id), Some(name)) = (block.id, block.name) {
                let arguments = serde_json::Value::Object(serde_json::Map::new());
                content.push(Block::ToolCall { id, name, arguments });
            }
        }
        let stop_reason = match self.stop_reason.as_deref() {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::Length,
            _ => StopReason::Stop,
        };
        Completion {
            message: Message::Assistant { content, stop_reason: Some(stop_reason) },
            usage,
            stop_reason,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, Provider};

    fn fixtures() -> (ModelConfig, Provider) {
        let model: ModelConfig = serde_json::from_str(
            r#"{"id":"claude-x","reasoning":true,"max_tokens":16000,"thinking_levels":["low","high"]}"#,
        )
        .unwrap();
        let provider: Provider =
            serde_json::from_str(r#"{"name":"a","api":"anthropic-messages"}"#).unwrap();
        (model, provider)
    }

    #[test]
    fn cache_breakpoints_land_on_system_tools_and_the_last_block() {
        let (model, provider) = fixtures();
        let tools = vec![
            crate::llm::ToolSpec { name: "read".into(), description: "r".into(), parameters: serde_json::json!({}) },
            crate::llm::ToolSpec { name: "write".into(), description: "w".into(), parameters: serde_json::json!({}) },
        ];
        let messages = vec![
            Message::System { content: "system".into() },
            Message::user_text("hello"),
        ];
        let req = Request { model: &model, provider: &provider, messages: &messages, tools: &tools, level: "high", session_id: "s", cache_hints: true };
        let body = serde_json::to_value(build_request(&req)).unwrap();
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["tools"][0]["cache_control"].is_null());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn thinking_budget_is_subtracted_from_max_tokens() {
        let (model, provider) = fixtures();
        let messages = vec![Message::user_text("hi")];
        let req = Request { model: &model, provider: &provider, messages: &messages, tools: &[], level: "high", session_id: "s", cache_hints: false };
        let body = serde_json::to_value(build_request(&req)).unwrap();
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert_eq!(budget, 8000);
        assert_eq!(body["max_tokens"], 8000);
    }

    #[test]
    fn a_non_reasoning_model_sends_no_thinking_block() {
        let (mut model, provider) = fixtures();
        model.reasoning = false;
        let messages = vec![Message::user_text("hi")];
        let req = Request { model: &model, provider: &provider, messages: &messages, tools: &[], level: "high", session_id: "s", cache_hints: false };
        let body = serde_json::to_value(build_request(&req)).unwrap();
        assert!(body["thinking"].is_null());
        assert_eq!(body["max_tokens"], 16000);
    }

    #[test]
    fn tool_results_are_grouped_into_one_user_message() {
        let (model, provider) = fixtures();
        let messages = vec![
            Message::Assistant {
                content: vec![
                    Block::ToolCall { id: "a".into(), name: "read".into(), arguments: serde_json::json!({}) },
                    Block::ToolCall { id: "b".into(), name: "read".into(), arguments: serde_json::json!({}) },
                ],
                stop_reason: Some(StopReason::ToolUse),
            },
            Message::Tool { tool_call_id: "a".into(), name: "read".into(), content: "A".into() },
            Message::Tool { tool_call_id: "b".into(), name: "read".into(), content: "B".into() },
        ];
        let req = Request { model: &model, provider: &provider, messages: &messages, tools: &[], level: "high", session_id: "s", cache_hints: false };
        let body = serde_json::to_value(build_request(&req)).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn streaming_events_assemble_thinking_text_and_tool_use() {
        let mut assembler = Assembler::default();
        let mut text = String::new();
        let mut sink = |delta: Delta| match delta {
            Delta::Text(t) => text.push_str(&t),
            Delta::Thinking(_) => {}
        };
        let events = [
            ("message_start", r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":90}}}"#),
            ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#),
            ("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi"}}"#),
            ("content_block_start", r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"read"}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#),
            ("content_block_delta", r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}"#),
            ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#),
        ];
        for (name, frame) in events {
            let event = parse_event(Some(name), frame).unwrap().unwrap();
            assembler.feed(event, &mut sink);
        }
        let completion = assembler.finish();
        assert_eq!(completion.text(), "hi");
        assert_eq!(completion.message.thinking(), "hmm");
        assert_eq!(completion.usage.input, 10);
        assert_eq!(completion.usage.cache_read, 90);
        assert_eq!(completion.usage.output, 7);
        assert_eq!(completion.stop_reason, StopReason::ToolUse);
        let calls = completion.tool_calls();
        assert_eq!(calls[0].2["path"], "a");
    }

    #[test]
    fn an_error_event_produces_an_error_completion() {
        let mut assembler = Assembler::default();
        let mut sink = |_: Delta| {};
        let event = parse_event(None, r#"{"type":"error","error":{"message":"prompt is too long"}}"#).unwrap().unwrap();
        assembler.feed(event, &mut sink);
        let completion = assembler.finish();
        assert_eq!(completion.stop_reason, StopReason::Error);
        assert!(completion.error.unwrap().contains("too long"));
    }

    #[test]
    fn endpoint_handles_both_base_url_shapes() {
        assert_eq!(endpoint("https://api.anthropic.com"), "https://api.anthropic.com/v1/messages");
        assert_eq!(endpoint("https://x/v1"), "https://x/v1/messages");
        assert_eq!(endpoint("https://x/v1/messages"), "https://x/v1/messages");
    }
}
