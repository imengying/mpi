//! OpenAI Responses API (`/v1/responses`), including the Codex-style gateways in front of it.
//!
//! The shape differs from Chat Completions in three ways that matter here:
//!
//! * The system prompt is a top-level `instructions` string, not a message.
//! * The conversation is a flat `input` list of items — a tool result is its own
//!   `function_call_output` item rather than a message with a role — so one historical
//!   message can expand to several items.
//! * Reasoning arrives as a `reasoning` item with an opaque `encrypted_content`, and
//!   `store: false` means the server keeps nothing: the item has to be replayed by the
//!   client on the next turn or the model loses its own train of thought.
//!
//! Fields are serialised from structs with a fixed order for the same reason as the other
//! two providers: the request prefix has to be byte-identical between turns to keep the
//! server-side prompt cache hitting.

use serde::{Deserialize, Serialize};

use super::compat::Compat;
use super::{Block, Completion, Delta, LlmError, Message, Request, StopReason, plan_thinking};
use crate::config::Usage;

/// The Responses API rejects `max_output_tokens` below 16.
const MIN_OUTPUT_TOKENS: u64 = 16;

/// One part of a user message body.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum ContentPart {
    Text {
        #[serde(rename = "type")]
        kind: &'static str,
        text: String,
    },
    Image {
        #[serde(rename = "type")]
        kind: &'static str,
        image_url: String,
    },
}

impl ContentPart {
    fn text(text: impl Into<String>) -> Self {
        ContentPart::Text { kind: "input_text", text: text.into() }
    }

    fn image(media_type: &str, data: &str) -> Self {
        // The same inline data URI the Chat Completions path uses; there is no separate
        // upload endpoint, and one would need somewhere to keep the blob.
        ContentPart::Image {
            kind: "input_image",
            image_url: format!("data:{media_type};base64,{data}"),
        }
    }
}

/// One entry of the flat `input` list. `role` is absent on the item kinds that carry their
/// own `type`, so the field is only serialised where it applies.
#[derive(Debug, Clone, Serialize)]
struct InputItem {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Vec<ContentPart>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    /// A replayed reasoning item: the summary is the text the UI showed, and the encrypted
    /// payload is what lets the server pick the thought back up.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<Vec<SummaryPart>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SummaryPart {
    #[serde(rename = "type")]
    kind: String,
    text: String,
}

impl InputItem {
    fn message(role: &'static str, content: Vec<ContentPart>) -> Self {
        InputItem {
            kind: None,
            role: Some(role),
            content: Some(content),
            ..InputItem::empty()
        }
    }

    fn empty() -> Self {
        InputItem {
            kind: None,
            role: None,
            content: None,
            id: None,
            call_id: None,
            name: None,
            arguments: None,
            output: None,
            summary: None,
            encrypted_content: None,
        }
    }
}

/// A tool as the Responses API advertises it: flat, not nested under `function`.
#[derive(Debug, Clone, Serialize)]
struct ToolDef {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    description: String,
    parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
struct Reasoning {
    effort: String,
    /// `auto` asks for a summary, which is the only reasoning text this API surfaces by
    /// default — the raw chain of thought is never returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Reasoning>,
    /// Keeps the request stateless, which is what makes the session file the only copy of
    /// the conversation — there is no server-side thread to lose or to leak.
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_key: Option<String>,
    /// Without this the reasoning item comes back without its encrypted payload, and a
    /// stateless client cannot replay what it was never given.
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<&'static str>>,
    stream: bool,
}

/// Build the request body for one turn.
pub fn build_request(req: &Request<'_>, stream: bool) -> ResponsesRequest {
    let model = req.model;
    let provider = req.provider;    let compat: Compat = provider.compat(model);
    let max_tokens = model.max_tokens();
    let plan = plan_thinking(model, req.level, max_tokens);

    let mut instructions = None;
    let mut input: Vec<InputItem> = Vec::new();
    for message in req.messages {
        match message {
            Message::System { content } => instructions = Some(content.clone()),
            Message::User { content: blocks } => {
                let parts: Vec<ContentPart> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } => Some(ContentPart::text(text.clone())),
                        Block::Image { media_type, data } => {
                            Some(ContentPart::image(media_type, data))
                        }
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    input.push(InputItem::message("user", parts));
                }
            }
            Message::Assistant { content: blocks, .. } => {
                // Text, thinking and tool calls are three different item kinds, and their
                // order within the message is the order they were produced in.
                for block in blocks {
                    match block {
                        Block::Thinking { thinking, signature } => {
                            if let Some(item) = reasoning_item(
                                thinking,
                                signature.as_deref(),
                                provider,
                                model,
                            ) {
                                input.push(item);
                            }
                        }
                        Block::Text { text } => {
                            // An assistant message is an item of its own, and its part is
                            // an `output_text` rather than the `input_text` a user turn uses.
                            input.push(InputItem::message(
                                "assistant",
                                vec![ContentPart::Text {
                                    kind: "output_text",
                                    text: text.clone(),
                                }],
                            ));
                        }
                        Block::ToolCall { id, name, arguments } => {
                            input.push(InputItem {
                                kind: Some("function_call"),
                                call_id: Some(id.clone()),
                                name: Some(name.clone()),
                                arguments: Some(arguments.to_string()),
                                ..InputItem::empty()
                            });
                        }
                        Block::Image { .. } => {}
                    }
                }
            }
            Message::Tool { tool_call_id, content, .. } => {
                input.push(InputItem {
                    kind: Some("function_call_output"),
                    call_id: Some(tool_call_id.clone()),
                    output: Some(content.clone()),
                    ..InputItem::empty()
                });
            }
        }
    }

    let tools: Vec<ToolDef> = req
        .tools
        .iter()
        .map(|tool| ToolDef {
            kind: "function",
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
            strict: compat.supports_strict_mode.then_some(true),
        })
        .collect();

    let reasoning = plan.effort.map(|effort| Reasoning {
        effort,
        summary: Some("auto"),
    });

    ResponsesRequest {
        model: model.id.clone(),
        instructions,
        input,
        tools,
        tool_choice: (!req.tools.is_empty()).then_some("auto"),
        max_output_tokens: Some(max_tokens.max(MIN_OUTPUT_TOKENS)),
        reasoning: reasoning.clone(),
        store: false,
        prompt_cache_key: req.cache_hints.then(|| req.session_id.to_string()),
        include: (reasoning.is_some() || model.reasoning)
            .then(|| vec!["reasoning.encrypted_content"]),
        stream,
    }
}

/// Rebuild the item a stored thinking block came from.
///
/// The signature is the item exactly as the server sent it, which is the only shape the
/// server accepts back. A block without one (an older session, or a provider that does not
/// encrypt) is dropped: an empty reasoning item is rejected, and a fabricated one would be
/// worse than none. A block from another provider or model is dropped for the same reason —
/// the encrypted payload is not readable there.
fn reasoning_item(
    thinking: &str,
    signature: Option<&str>,
    provider: &crate::config::Provider,
    model: &crate::config::ModelConfig,
) -> Option<InputItem> {
    let stored = decode_signature(signature?)?;
    if stored.provider != provider.base_url || stored.model != model.id {
        return None;
    }
    Some(InputItem {
        kind: Some("reasoning"),
        id: stored.id,
        summary: Some(vec![SummaryPart {
            kind: "summary_text".into(),
            text: thinking.to_string(),
        }]),
        encrypted_content: stored.encrypted_content,
        ..InputItem::empty()
    })
}

/// What a thinking block carries in its `signature` field for this provider.
///
/// The signature is an envelope rather than the raw item because the encrypted payload is
/// only valid for the provider and model that produced it: `/model` can switch mid-session,
/// and replaying foreign encrypted content is an error the user cannot act on. Recording
/// where it came from is what lets a replay be skipped instead.
///
/// The `pi-responses:` prefix also marks the signature as ours, so the other protocols can
/// tell at a glance that it is not theirs to send (see `anthropic`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredReasoning {
    /// The provider (`base_url` is what decides where encrypted content can be read back).
    #[serde(default)]
    provider: String,
    /// The model id the payload was issued for.
    #[serde(default)]
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted_content: Option<String>,
}

/// Marks a signature as belonging to this provider.
pub const SIGNATURE_PREFIX: &str = "pi-responses:";

/// The signature written into a thinking block.
fn encode_signature(stored: &StoredReasoning) -> Option<String> {
    serde_json::to_string(stored).ok().map(|json| format!("{SIGNATURE_PREFIX}{json}"))
}

/// Read a stored signature back, and only if it is one of ours.
fn decode_signature(signature: &str) -> Option<StoredReasoning> {
    serde_json::from_str(signature.strip_prefix(SIGNATURE_PREFIX)?).ok()
}

/// `base_url` may already name the endpoint; otherwise `/responses` is appended.
pub fn endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/responses") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/responses")
    }
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

/// One SSE event. Only the fields the assembler needs are kept: the API sends a great deal
/// of bookkeeping (sequence numbers, status objects, the echo of the request) that the
/// agent has no use for.
#[derive(Debug, Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    item: Option<Item>,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    output_index: Option<usize>,
    #[serde(default)]
    response: Option<ResponseBody>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Item {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    summary: Option<Vec<SummaryPart>>,
    #[serde(default)]
    encrypted_content: Option<String>,
    #[serde(default)]
    content: Option<Vec<ItemContent>>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ItemContent {
    /// `output_text` or `refusal`. A refusal has no `text`, so the type is what says which
    /// of the two fields to read.
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
}

impl ItemContent {
    fn body(&self) -> Option<&str> {
        if self.kind == "refusal" {
            self.refusal.as_deref().or(self.text.as_deref())
        } else {
            self.text.as_deref().or(self.refusal.as_deref())
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ResponseBody {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<ProviderError>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    output: Vec<Item>,
}

#[derive(Debug, Clone, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputDetails>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct InputDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// Accumulator shared by the streaming and non-streaming paths.
///
/// Text and thinking are keyed by `output_index` because the API may interleave items; in
/// practice one item is emitted at a time, and the slots keep that assumption from being
/// load-bearing.
#[derive(Debug, Default)]
pub struct Assembler {
    items: Vec<Slot>,
    usage: Usage,
    output_tokens: u64,
    stop_reason: Option<String>,
    incomplete: Option<String>,
    error: Option<String>,
    saw_terminal: bool,
    /// Where the reasoning payloads were issued. Recorded so a signature can be checked
    /// against the provider and model replaying it.
    provider: String,
    model: String,
}

#[derive(Debug, Default)]
struct Slot {
    kind: Option<String>,
    id: Option<String>,
    /// Text of a message item, or the accumulated summary of a reasoning item.
    text: String,
    /// Streaming tool-call arguments, before they parse.
    arguments: String,
    call_id: Option<String>,
    name: Option<String>,
    encrypted_content: Option<String>,
    index: usize,
}

impl Assembler {
    /// Tell the assembler where the reasoning it is about to receive comes from, so the
    /// signatures it writes can be checked before being sent back.
    pub fn issued_by(&mut self, provider: &str, model: &str) {
        self.provider = provider.to_string();
        self.model = model.to_string();
    }

    fn slot(&mut self, index: usize) -> &mut Slot {
        while self.items.len() <= index {
            let next = self.items.len();
            self.items.push(Slot { index: next, ..Slot::default() });
        }
        &mut self.items[index]
    }

    fn apply(
        &mut self,
        event: StreamEvent,
        on_delta: &mut dyn FnMut(Delta),
    ) -> Result<(), LlmError> {
        match event.kind.as_str() {
            "response.output_item.added" => {
                let index = event.output_index.unwrap_or(self.items.len());
                if let Some(item) = event.item {
                    let slot = self.slot(index);
                    slot.kind = item.kind;
                    slot.id = item.id;
                    slot.call_id = item.call_id;
                    slot.name = item.name;
                    slot.arguments = item.arguments.unwrap_or_default();
                    slot.encrypted_content = item.encrypted_content;
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = event.delta
                    && !delta.is_empty()
                {
                    let index = event.output_index.unwrap_or(0);
                    let slot = self.slot(index);
                    slot.kind.get_or_insert_with(|| "message".into());
                    slot.text.push_str(&delta);
                    on_delta(Delta::Text(delta));
                }
            }
            // The raw chain of thought is not returned by default, but a gateway that
            // supports it streams here rather than in the summary channel.
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event.delta
                    && !delta.is_empty()
                {
                    let index = event.output_index.unwrap_or(0);
                    let slot = self.slot(index);
                    slot.kind.get_or_insert_with(|| "reasoning".into());
                    slot.text.push_str(&delta);
                    on_delta(Delta::Thinking(delta));
                }
            }
            // A reasoning item is a run of summary parts that have to stay separated, or
            // the paragraphs run together into one sentence.
            "response.reasoning_summary_part.done" => {
                if let Some(index) = event.output_index {
                    let slot = self.slot(index);
                    slot.text.push_str("\n\n");
                    on_delta(Delta::Thinking("\n\n".into()));
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.delta
                    && !delta.is_empty()
                {
                    let index = event.output_index.unwrap_or(0);
                    self.slot(index).arguments.push_str(&delta);
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(index) = event.output_index
                    && let Some(arguments) = event.arguments
                {
                    // The done event carries the final string; a gateway that streamed
                    // nothing would otherwise leave an empty call.
                    let slot = self.slot(index);
                    if arguments.len() >= slot.arguments.len() {
                        slot.arguments = arguments;
                    }
                }
            }
            // A message item may also arrive whole, without any delta events.
            "response.output_item.done" => {
                let index = event.output_index.unwrap_or(self.items.len());
                if let Some(item) = event.item {
                    let slot = self.slot(index);
                    slot.kind = item.kind.or(slot.kind.take());
                    slot.id = item.id.or(slot.id.take());
                    slot.call_id = item.call_id.or(slot.call_id.take());
                    slot.name = item.name.or(slot.name.take());
                    if let Some(arguments) = item.arguments
                        && !arguments.is_empty()
                    {
                        slot.arguments = arguments;
                    }
                    if let Some(encrypted) = item.encrypted_content {
                        slot.encrypted_content = Some(encrypted);
                    }
                    if let Some(summary) = item.summary {
                        let joined = summary
                            .iter()
                            .map(|part| part.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        if !joined.is_empty() {
                            slot.text = joined;
                        }
                    }
                    if let Some(content) = item.content {
                        let text: String =
                            content.iter().filter_map(ItemContent::body).collect();
                        if !text.is_empty() {
                            slot.text = text;
                        }
                    }
                    let _ = item.role;
                }
            }
            "response.completed" | "response.incomplete" => {
                self.saw_terminal = true;
                if let Some(body) = event.response {
                    self.finish_body(body);
                }
            }
            "response.failed" => {
                self.saw_terminal = true;
                let body = event.response;
                let message = body
                    .as_ref()
                    .and_then(|body| body.error.as_ref())
                    .map(|error| match (&error.code, &error.message) {
                        (Some(code), Some(message)) => format!("{code}: {message}"),
                        (None, Some(message)) => message.clone(),
                        (Some(code), None) => code.clone(),
                        (None, None) => "上游未给出原因".into(),
                    })
                    .or_else(|| {
                        body.as_ref()
                            .and_then(|body| body.incomplete_details.as_ref())
                            .and_then(|details| details.reason.clone())
                    })
                    .unwrap_or_else(|| "上游未给出原因".into());
                self.error = Some(message);
            }
            "error" => {
                self.saw_terminal = true;
                let message = match (event.code, event.message) {
                    (Some(code), Some(message)) => format!("{code}: {message}"),
                    (None, Some(message)) => message,
                    (Some(code), None) => code,
                    (None, None) => "上游未给出原因".into(),
                };
                self.error = Some(message);
            }
            _ => {}
        }
        Ok(())
    }

    fn finish_body(&mut self, body: ResponseBody) {
        if let Some(usage) = &body.usage {
            self.set_usage(usage);
        }
        // A reasoning item's encrypted payload arrives with the terminal response for some
        // gateways; without it the next turn could not replay the item, so it is backfilled
        // from here rather than assumed to have come with the item.
        for item in &body.output {
            if item.kind.as_deref() != Some("reasoning") {
                continue;
            }
            let id = item.id.as_deref();
            if let Some(slot) = self
                .items
                .iter_mut()
                .find(|slot| slot.kind.as_deref() == Some("reasoning") && slot.id.as_deref() == id)
                && slot.encrypted_content.is_none()
            {
                slot.encrypted_content = item.encrypted_content.clone();
            }
        }
        self.stop_reason = Some(body.status.unwrap_or_else(|| "completed".into()));
        if let Some(details) = body.incomplete_details
            && let Some(reason) = details.reason
        {
            self.incomplete = Some(reason);
        }
    }

    fn set_usage(&mut self, usage: &WireUsage) {
        let cached = usage
            .input_tokens_details
            .as_ref()
            .map(|details| details.cached_tokens)
            .unwrap_or(0);
        // The API counts cached tokens inside `input_tokens`, so the uncached part is what
        // the footer's hit rate has to be computed from.
        self.usage = Usage {
            input: usage.input_tokens.saturating_sub(cached),
            output: usage.output_tokens,
            cache_read: cached,
            cache_write: 0,
        };
        self.output_tokens = usage.output_tokens;
    }

    pub fn finish(mut self) -> Completion {
        let mut content: Vec<Block> = Vec::new();
        self.items.sort_by_key(|slot| slot.index);
        for slot in std::mem::take(&mut self.items) {
            let kind = slot.kind.as_deref().unwrap_or_default();
            match kind {
                "reasoning" => {
                    if slot.text.trim().is_empty() {
                        continue;
                    }
                    // The signature is the item itself, in the shape the server will accept
                    // back, plus where it came from; see `reasoning_item`.
                    let stored = StoredReasoning {
                        provider: self.provider.clone(),
                        model: self.model.clone(),
                        id: slot.id,
                        encrypted_content: slot.encrypted_content,
                    };
                    content.push(Block::Thinking {
                        thinking: slot.text,
                        signature: encode_signature(&stored),
                    });
                }
                "function_call" => {
                    let Some(name) = slot.name.filter(|name| !name.is_empty()) else { continue };
                    let arguments = parse_arguments(&slot.arguments);
                    content.push(Block::ToolCall {
                        id: slot
                            .call_id
                            .filter(|id| !id.is_empty())
                            .unwrap_or_else(|| format!("call_{}", slot.index)),
                        name,
                        arguments,
                    });
                }
                _ => {
                    if !slot.text.is_empty() {
                        content.push(Block::Text { text: slot.text });
                    }
                }
            }
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
            None => match self.incomplete.as_deref() {
                Some("max_output_tokens") => StopReason::Length,
                Some(_) => StopReason::Error,
                None => {
                    if content.iter().any(|block| matches!(block, Block::ToolCall { .. })) {
                        StopReason::ToolUse
                    } else {
                        StopReason::Stop
                    }
                }
            },
        };
        let _ = (self.stop_reason, self.output_tokens, self.saw_terminal);
        Completion {
            message: Message::Assistant { content, stop_reason: Some(stop_reason) },
            usage: self.usage,
            stop_reason,
            error: None,
        }
    }
}

fn parse_arguments(raw: &str) -> serde_json::Value {
    if raw.trim().is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// Parse one SSE payload; `None` for frames that carry no JSON (e.g. `[DONE]`).
pub fn parse_frame(payload: &str) -> Result<Option<StreamEvent>, LlmError> {
    let trimmed = payload.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|err| {
            LlmError::Decode(format!("{err}: {}", crate::util::truncate(trimmed, 300, "…")))
        })
}

impl StreamEvent {
    pub(crate) fn feed(self, assembler: &mut Assembler, on_delta: &mut dyn FnMut(Delta)) {
        let _ = assembler.apply(self, on_delta);
    }
}

/// Non-streaming response shape: the `response` object itself.
#[derive(Debug, Deserialize)]
pub struct FullResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<ProviderError>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    output: Vec<Item>,
}

impl FullResponse {
    /// Fold a non-streaming body into the same accumulator the SSE path uses, so both
    /// shapes produce identical messages.
    ///
    /// The caller supplies the accumulator so the identity a signature is stamped with
    /// (`issued_by`) is set in one place for both paths.
    pub fn assemble(self, mut assembler: Assembler) -> Completion {
        let mut sink = |_: Delta| {};
        // The items are fed through the same events the stream would have sent, which keeps
        // one implementation of "what an item means".
        for (index, item) in self.output.iter().enumerate() {
            let event = StreamEvent {
                kind: "response.output_item.done".into(),
                item: Some(item.clone()),
                delta: None,
                arguments: None,
                output_index: Some(index),
                response: None,
                code: None,
                message: None,
            };
            let _ = assembler.apply(event, &mut sink);
        }
        let body = ResponseBody {
            status: self.status,
            incomplete_details: self.incomplete_details,
            error: self.error,
            usage: self.usage,
            output: Vec::new(),
        };
        // A failed non-streaming response has to be reported the same way the stream reports
        // `response.failed`; the failure is not an HTTP status, it is a body.
        if let Some(error) = body.error.as_ref() {
            let message = match (&error.code, &error.message) {
                (Some(code), Some(message)) => format!("{code}: {message}"),
                (None, Some(message)) => message.clone(),
                (Some(code), None) => code.clone(),
                (None, None) => "上游未给出原因".into(),
            };
            assembler.error = Some(message);
        }
        assembler.finish_body(body);
        assembler.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ModelConfig, Provider};
    use crate::llm::{Message, ToolSpec};

    fn fixtures() -> (ModelConfig, Provider) {
        let config: Config = serde_json::from_str(
            r#"{
              "providers": [{
                "name": "name",
                "api": "openai-responses",
                "base_url": "https://api.openai.com/v1",
                "models": [{
                  "id": "gpt-5",
                  "reasoning": true,
                  "context_window": 400000,
                  "max_tokens": 32000
                }]
              }]
            }"#,
        )
        .unwrap();
        let (provider, model) = config.find("name/gpt-5").unwrap();
        (model.clone(), provider.clone())
    }

    /// The signature a thinking block carries for the test provider.
    fn signature(id: &str, encrypted: &str) -> String {
        let (model, provider) = fixtures();
        encode_signature(&StoredReasoning {
            provider: provider.base_url.clone(),
            model: model.id.clone(),
            id: Some(id.into()),
            encrypted_content: Some(encrypted.into()),
        })
        .unwrap()
    }

    fn body(messages: &[Message], level: &str) -> serde_json::Value {
        let (model, provider) = fixtures();
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let request = Request {
            model: &model,
            provider: &provider,
            messages,
            tools: &tools,
            level,
            session_id: "session-id",
            cache_hints: true,
        };
        serde_json::to_value(build_request(&request, true)).unwrap()
    }

    #[test]
    fn the_system_prompt_becomes_instructions_not_a_message() {
        let messages = vec![Message::System { content: "S".into() }, Message::user_text("hi")];
        let body = body(&messages, "high");
        assert_eq!(body["instructions"], "S");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn a_tool_call_and_its_result_become_two_items() {
        let messages = vec![
            Message::user_text("read it"),
            Message::Assistant {
                content: vec![Block::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a.rs"}),
                }],
                stop_reason: Some(StopReason::ToolUse),
            },
            Message::Tool {
                tool_call_id: "call_1".into(),
                name: "read".into(),
                content: "data".into(),
            },
        ];
        let body = body(&messages, "high");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "read");
        assert_eq!(input[1]["arguments"], r#"{"path":"a.rs"}"#);
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "data");
    }

    #[test]
    fn thinking_is_replayed_with_its_encrypted_payload() {
        let messages = vec![Message::Assistant {
            content: vec![Block::Thinking {
                thinking: "because".into(),
                signature: Some(signature("rs_1", "blob")),
            }],
            stop_reason: None,
        }];
        let body = body(&messages, "high");
        let item = &body["input"][0];
        assert_eq!(item["type"], "reasoning");
        assert_eq!(item["id"], "rs_1");
        assert_eq!(item["encrypted_content"], "blob");
        assert_eq!(item["summary"][0]["text"], "because");
        assert_eq!(item["summary"][0]["type"], "summary_text");
    }

    #[test]
    fn a_signature_from_another_provider_or_model_is_not_replayed() {
        // `/model` can switch mid-session. The encrypted payload is only readable by the
        // model that produced it, so sending it elsewhere is rejected upstream — worse,
        // it is rejected with an error the user cannot act on. Dropping it loses context
        // the model no longer has anyway.
        let messages = vec![Message::Assistant {
            content: vec![Block::Thinking {
                thinking: "because".into(),
                signature: Some(
                    encode_signature(&StoredReasoning {
                        provider: "https://elsewhere.example/v1".into(),
                        model: "gpt-5".into(),
                        id: Some("rs_1".into()),
                        encrypted_content: Some("blob".into()),
                    })
                    .unwrap(),
                ),
            }],
            stop_reason: None,
        }];
        assert_eq!(body(&messages, "high")["input"].as_array().unwrap().len(), 0);

        // Same host, different model: dropped too.
        let messages = vec![Message::Assistant {
            content: vec![Block::Thinking {
                thinking: "because".into(),
                signature: Some(
                    encode_signature(&StoredReasoning {
                        provider: "https://api.openai.com/v1".into(),
                        model: "gpt-4o".into(),
                        id: Some("rs_1".into()),
                        encrypted_content: Some("blob".into()),
                    })
                    .unwrap(),
                ),
            }],
            stop_reason: None,
        }];
        assert_eq!(body(&messages, "high")["input"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_foreign_signature_is_not_mistaken_for_one_of_ours() {
        // Anthropic signatures are opaque strings of their own; the prefix is what keeps
        // them from being parsed as ours (and vice versa: see `anthropic`).
        assert!(decode_signature("ErUBCkYIBRgCIkA=").is_none());
        assert!(decode_signature("pi-anthropic:{}").is_none());
        let ours = signature("rs_1", "blob");
        assert!(ours.starts_with(SIGNATURE_PREFIX));
        assert!(decode_signature(&ours).is_some());
    }

    #[test]
    fn thinking_without_a_signature_is_dropped_rather_than_faked() {
        let messages = vec![Message::Assistant {
            content: vec![Block::Thinking { thinking: "hmm".into(), signature: None }],
            stop_reason: None,
        }];
        let body = body(&messages, "high");
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_request_is_stateless_and_asks_for_the_encrypted_reasoning() {
        let messages = vec![Message::user_text("hi")];
        let body = body(&messages, "high");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["prompt_cache_key"], "session-id");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["max_output_tokens"], 32000);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn a_non_reasoning_model_sends_no_reasoning_field() {
        let config: Config = serde_json::from_str(
            r#"{"providers":[{"name":"n","api":"openai-responses","base_url":"url",
                "models":[{"id":"m","reasoning":false,"max_tokens":1000}]}]}"#,
        )
        .unwrap();
        let (provider, model) = config.find("n/m").unwrap();
        let messages = vec![Message::user_text("hi")];
        let request = Request {
            model,
            provider,
            messages: &messages,
            tools: &[],
            level: "",
            session_id: "s",
            cache_hints: true,
        };
        let body = serde_json::to_value(build_request(&request, true)).unwrap();
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
        // The minimum the API accepts, not the configured 1000.
        assert_eq!(body["max_output_tokens"], 1000);
    }

    #[test]
    fn a_tiny_output_budget_is_raised_to_the_api_minimum() {
        let config: Config = serde_json::from_str(
            r#"{"providers":[{"name":"n","api":"openai-responses","base_url":"url",
                "models":[{"id":"m","max_tokens":4}]}]}"#,
        )
        .unwrap();
        let (provider, model) = config.find("n/m").unwrap();
        let messages = vec![Message::user_text("hi")];
        let request = Request {
            model,
            provider,
            messages: &messages,
            tools: &[],
            level: "",
            session_id: "s",
            cache_hints: true,
        };
        let body = serde_json::to_value(build_request(&request, true)).unwrap();
        assert_eq!(body["max_output_tokens"], MIN_OUTPUT_TOKENS);
    }

    #[test]
    fn tools_are_flat_with_no_function_envelope() {
        let messages = vec![Message::user_text("hi")];
        let body = body(&messages, "high");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn the_endpoint_is_appended_once() {
        assert_eq!(endpoint("https://api.openai.com/v1"), "https://api.openai.com/v1/responses");
        assert_eq!(endpoint("https://api.openai.com/v1/"), "https://api.openai.com/v1/responses");
        assert_eq!(
            endpoint("https://gateway/responses"),
            "https://gateway/responses"
        );
    }

    /// Feed a list of raw SSE payloads through the stream path.
    ///
    /// The deltas are collected as they arrive and compared with the assembled message, so
    /// a stream that dropped or duplicated a chunk fails here rather than in the UI.
    fn stream(events: &[&str]) -> Completion {
        let (model, provider) = fixtures();
        let mut assembler = Assembler::default();
        assembler.issued_by(&provider.base_url, &model.id);
        let mut text = String::new();
        let mut thinking = String::new();
        {
            let mut sink = |delta: Delta| match delta {
                Delta::Text(chunk) => text.push_str(&chunk),
                Delta::Thinking(chunk) => thinking.push_str(&chunk),
            };
            for json in events {
                let event = parse_frame(json).unwrap().expect("an event");
                event.feed(&mut assembler, &mut sink);
            }
        }
        let completion = assembler.finish();
        assert_eq!(completion.text(), text, "streamed text does not match the message");
        assert_eq!(
            completion.message.thinking(),
            thinking.trim_end(),
            "streamed thinking does not match the message"
        );
        completion
    }

    #[test]
    fn a_streamed_answer_is_assembled_from_its_deltas() {
        let completion = stream(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"think"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"think"}],"encrypted_content":"blob"}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"delta":"hello"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":2,"input_tokens_details":{"cached_tokens":4}}}}"#,
        ]);
        assert_eq!(completion.stop_reason, StopReason::Stop);
        assert_eq!(completion.usage.cache_read, 4);
        assert_eq!(completion.usage.input, 6);
        let Message::Assistant { content, .. } = &completion.message else { panic!() };
        assert!(matches!(&content[0], Block::Thinking { thinking, .. } if thinking == "think"));
        assert!(matches!(&content[1], Block::Text { text } if text == "hello"));
    }

    #[test]
    fn a_tool_call_streamed_in_pieces_parses_its_arguments() {
        let completion = stream(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"a"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":".rs\"}"}"#,
            r#"{"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"a.rs\"}"}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ]);
        assert_eq!(completion.stop_reason, StopReason::ToolUse);
        let calls = completion.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "call_1");
        assert_eq!(calls[0].1, "read");
        assert_eq!(calls[0].2, serde_json::json!({"path": "a.rs"}));
    }

    #[test]
    fn a_failed_response_reports_the_provider_reason() {
        let completion = stream(&[
            r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"upstream exploded"}}}"#,
        ]);
        assert_eq!(completion.stop_reason, StopReason::Error);
        assert_eq!(completion.error.as_deref(), Some("server_error: upstream exploded"));
    }

    #[test]
    fn an_incomplete_response_is_a_length_cut_only_for_max_output_tokens() {
        let cut = stream(&[
            r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
        ]);
        assert_eq!(cut.stop_reason, StopReason::Length);
        let other = stream(&[
            r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"}}}"#,
        ]);
        assert_eq!(other.stop_reason, StopReason::Error);
    }

    #[test]
    fn a_non_streaming_body_produces_the_same_message_as_the_stream() {
        let json = r#"{
          "status": "completed",
          "usage": {"input_tokens": 10, "output_tokens": 3, "input_tokens_details": {"cached_tokens": 4}},
          "output": [
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "think"}], "encrypted_content": "blob"},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi there"}]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a.rs\"}"}
          ]
        }"#;
        let completion =
            serde_json::from_str::<FullResponse>(json).unwrap().assemble(Assembler::default());
        assert_eq!(completion.text(), "hi there");
        assert_eq!(completion.usage.cache_read, 4);
        let calls = completion.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2, serde_json::json!({"path": "a.rs"}));
        let Message::Assistant { content, .. } = &completion.message else { panic!() };
        assert!(matches!(&content[0], Block::Thinking { thinking, signature }
            if thinking == "think" && signature.as_deref().is_some_and(|s| s.contains("blob"))));
    }

    #[test]
    fn an_encrypted_payload_only_present_at_the_end_is_backfilled() {
        // Some gateways omit `encrypted_content` on the item and send it in the terminal
        // response; without the backfill the next turn could not replay the reasoning.
        let (model, provider) = fixtures();
        let mut assembler = Assembler::default();
        assembler.issued_by(&provider.base_url, &model.id);
        let mut sink = |_: Delta| {};
        for json in [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"think"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"think"}]}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"reasoning","id":"rs_1","encrypted_content":"late"}]}}"#,
        ] {
            parse_frame(json).unwrap().unwrap().feed(&mut assembler, &mut sink);
        }
        let completion = assembler.finish();
        let Message::Assistant { content, .. } = &completion.message else { panic!() };
        let Block::Thinking { signature, .. } = &content[0] else { panic!() };
        let stored = decode_signature(signature.as_deref().unwrap()).unwrap();
        assert_eq!(stored.encrypted_content.as_deref(), Some("late"));
    }

    #[test]
    fn reasoning_summary_parts_stay_separated() {
        let completion = stream(&[
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"one"}"#,
            r#"{"type":"response.reasoning_summary_part.done","output_index":0}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"two"}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ]);
        let Message::Assistant { content, .. } = &completion.message else { panic!() };
        assert!(matches!(&content[0], Block::Thinking { thinking, .. } if thinking == "one\n\ntwo"));
    }
}
