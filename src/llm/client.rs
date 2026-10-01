//! HTTP plumbing shared by both providers: one streaming call, one buffered call.
//!
//! SSE framing is parsed by hand: both providers send `data:` lines that feed straight
//! into the provider-specific assemblers.

use super::{Api, Delta, LlmError, Request, anthropic, openai, responses};
use crate::config::Provider;

pub struct Client {
    http: reqwest::Client,
}

/// One decoded SSE frame, ready to be handed to the matching assembler.
enum Frame {
    Anthropic(Box<anthropic::StreamEvent>),
    OpenAi(Box<openai::StreamChunk>),
    Responses(Box<responses::StreamEvent>),
}

#[derive(Default)]
struct Assemblers {
    anthropic: anthropic::Assembler,
    openai: openai::Assembler,
    responses: responses::Assembler,
}

impl Assemblers {
    fn finish(self, api: Api) -> super::Completion {
        match api {
            Api::AnthropicMessages => self.anthropic.finish_stream(),
            Api::OpenAiCompletions => self.openai.finish_stream(),
            Api::OpenAiResponses => self.responses.finish_stream(),
        }
    }
}

/// Which body/parser pair a provider uses. Read once per request so the dispatch is a
/// single match rather than a string comparison scattered through the call.
fn api_of(provider: &Provider) -> Result<Api, LlmError> {
    provider
        .api()
        .ok_or_else(|| LlmError::UnknownApi(provider.name.clone(), provider.api.clone()))
}

impl Client {
    #[cfg(test)]
    pub(crate) fn local_test_client() -> Self {
        Self { http: reqwest::Client::builder().no_proxy().build().unwrap() }
    }

    pub fn new() -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            // Long generations: time out the connect and the gaps between chunks, never
            // the whole response.
            .connect_timeout(std::time::Duration::from_secs(20))
            .read_timeout(std::time::Duration::from_secs(300))
            .build()?;
        Ok(Client { http })
    }

    fn api_key(provider: &Provider) -> Result<String, LlmError> {
        provider
            .api_key()
            .ok_or_else(|| LlmError::MissingKey(provider.name.clone()))
    }

    /// Stream one assistant turn, invoking `on_delta` for every token.
    pub async fn stream(
        &self,
        req: &Request<'_>,
        on_delta: &mut dyn FnMut(Delta),
    ) -> Result<super::Completion, LlmError> {
        super::validate_tool_history(req.messages)?;
        let provider = req.provider;
        let api = api_of(provider)?;
        let api_key = Self::api_key(provider)?;
        let mut request = if api == Api::AnthropicMessages {
            let body = anthropic::build_request(req);
            let mut request = self
                .http
                .post(anthropic::endpoint(&provider.base_url))
                .json(&body);
            for (name, value) in
                anthropic::headers(&api_key, provider.compat(req.model).supports_long_cache)
            {
                request = request.header(name, value);
            }
            request
        } else {
            let mut request = match api {
                Api::OpenAiResponses => self
                    .http
                    .post(responses::endpoint(&provider.base_url))
                    .json(&responses::build_request(req, true)),
                _ => self
                    .http
                    .post(openai::endpoint(&provider.base_url))
                    .json(&openai::build_request(req, true)),
            };
            request = request.bearer_auth(&api_key);
            request
        };
        if provider.compat(req.model).send_session_affinity {
            // Only meaningful behind a load balancer, but harmless elsewhere, and it
            // is what keeps a session pinned to the backend holding its cache.
            request = request
                .header("x-session-affinity", req.session_id)
                .header("x-session-id", req.session_id);
        }
        let response = request
            .send()
            .await
            .map_err(|err| LlmError::Transport(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::Api { status: status.as_u16(), message: describe_error(&text) });
        }
        let mut buffer = Vec::new();
        let mut assemblers = Assemblers::default();
        // Where the reasoning payloads about to arrive were issued from. A signature has to
        // be checked against the provider and model before being replayed, and this is the
        // only place that knows both.
        assemblers.responses.issued_by(&provider.base_url, &req.model.id);
        assemblers.anthropic.issued_by(&provider.base_url, &req.model.id);
        let mut response = response;
        // `chunk()` is inherent on `Response`, so no extra stream-trait dependency is
        // needed just to read the body incrementally.
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(err) => {
                    return Ok(assemblers
                        .finish(api)
                        .failed(format!("流式传输中断：{err}")));
                }
            };
            buffer.extend_from_slice(&chunk);
            if let Err(err) = drain_sse(&mut buffer, api, &mut assemblers, on_delta) {
                return Ok(assemblers.finish(api).failed(err.to_string()));
            }
            if buffer.len() > 8 * 1024 * 1024 {
                return Ok(assemblers.finish(api).failed("SSE 单帧超过 8 MiB"));
            }
        }
        if !buffer.is_empty() {
            let result = std::str::from_utf8(&buffer)
                .map_err(|err| LlmError::Decode(err.to_string()))
                .and_then(|frame| dispatch(frame, api, &mut assemblers, on_delta));
            if let Err(err) = result {
                return Ok(assemblers.finish(api).failed(err.to_string()));
            }
        }
        Ok(assemblers.finish(api))
    }

    /// Buffered summary completion with the normal cache/routing identity and tools disabled.
    pub async fn complete(&self, req: &Request<'_>) -> Result<super::Completion, LlmError> {
        super::validate_tool_history(req.messages)?;
        let provider = req.provider;
        let api = api_of(provider)?;
        let api_key = Self::api_key(provider)?;
        let body = summary_request_body(req)?;
        let endpoint = match api {
            Api::AnthropicMessages => anthropic::endpoint(&provider.base_url),
            Api::OpenAiCompletions => openai::endpoint(&provider.base_url),
            Api::OpenAiResponses => responses::endpoint(&provider.base_url),
        };
        let mut request = self.http.post(endpoint).json(&body);
        if api == Api::AnthropicMessages {
            for (name, value) in anthropic::headers(&api_key, provider.compat(req.model).supports_long_cache) {
                request = request.header(name, value);
            }
        } else {
            request = request.bearer_auth(&api_key);
        }
        if provider.compat(req.model).send_session_affinity {
            request = request.header("x-session-affinity", req.session_id)
                .header("x-session-id", req.session_id);
        }
        let response = request.send().await.map_err(|err| LlmError::Transport(err.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|err| LlmError::Transport(err.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Api { status: status.as_u16(), message: describe_error(&text) });
        }
        let decode = |err: serde_json::Error| {
            LlmError::Decode(format!("{err}: {}", crate::util::truncate(&text, 300, "…")))
        };
        match api {
            Api::AnthropicMessages => serde_json::from_str::<anthropic::FullResponse>(&text)
                .map(|full| full.assemble(&provider.base_url, &req.model.id))
                .map_err(decode),
            Api::OpenAiCompletions => serde_json::from_str::<openai::FullResponse>(&text)
                .map(openai::FullResponse::assemble)
                .map_err(decode),
            Api::OpenAiResponses => {
                let mut assembler = responses::Assembler::default();
                assembler.issued_by(&provider.base_url, &req.model.id);
                serde_json::from_str::<responses::FullResponse>(&text)
                    .map(|full| full.assemble(assembler))
                    .map_err(decode)
            }
        }
    }
}

/// Append a path to a base URL, unless the user already configured the full one.
///
/// A base URL is written by hand in the config, and people write it both ways — the API root
/// and the endpoint itself — so the suffix is added only when it is missing. Trailing slashes
/// are tolerated for the same reason.
///
/// Shared rather than repeated per dialect: the rule is about how a URL is written down, which
/// has nothing to do with which provider is on the other end.
pub fn endpoint(base_url: &str, path: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with(path) {
        trimmed.to_string()
    } else {
        format!("{trimmed}{path}")
    }
}

/// Decode one SSE payload into a typed frame, or `None` when it carries no JSON.
///
/// The three dialects differ only in which struct they decode into, so the rule about what
/// an unparseable frame means lives here once rather than three times. `[DONE]` is a
/// terminator rather than an error: OpenAI ends its stream with it, the others simply stop.
///
/// The payload is truncated in the error message because a malformed frame is often a whole
/// HTML error page, and the transcript should say what went wrong rather than reproduce it.
pub fn decode_frame<T: serde::de::DeserializeOwned>(payload: &str) -> Result<Option<T>, LlmError> {
    let trimmed = payload.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|err| LlmError::Decode(format!("{err}: {}", crate::util::truncate(trimmed, 300, "…"))))
}

fn dispatch(
    frame: &str,
    api: Api,
    assemblers: &mut Assemblers,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<(), LlmError> {
    match parse_sse_frame(frame, api)? {
        Some(Frame::Anthropic(event)) => assemblers.anthropic.feed(*event, on_delta),
        Some(Frame::OpenAi(chunk)) => chunk.feed(&mut assemblers.openai, on_delta),
        Some(Frame::Responses(event)) => event.feed(&mut assemblers.responses, on_delta),
        None => {}
    }
    Ok(())
}

/// Pull the interesting part out of an error body without assuming its shape.
pub fn describe_error(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let message = value
            .pointer("/error/message")
            .or_else(|| value.pointer("/error/detail"))
            .or_else(|| value.pointer("/message"))
            .or_else(|| value.pointer("/detail"))
            .and_then(|v| v.as_str());
        if let Some(message) = message {
            return message.to_string();
        }
    }
    crate::util::truncate(body.trim(), 600, "…")
}

/// Split one SSE frame into its `event:` name and concatenated `data:` payload.
fn parse_sse_frame(frame: &str, api: Api) -> Result<Option<Frame>, LlmError> {
    let mut event_name: Option<String> = None;
    let mut data = String::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if data.trim().is_empty() || data.trim() == "[DONE]" {
        return Ok(None);
    }
    Ok(match api {
        Api::AnthropicMessages => {
            anthropic::parse_event(event_name.as_deref(), &data)?.map(|e| Frame::Anthropic(Box::new(e)))
        }
        Api::OpenAiCompletions => openai::parse_frame(&data)?.map(|c| Frame::OpenAi(Box::new(c))),
        Api::OpenAiResponses => {
            responses::parse_frame(&data)?.map(|e| Frame::Responses(Box::new(e)))
        }
    })
}

/// Serialize the same typed body as normal turns, preserving object-field ordering.
#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum SummaryBody {
    Messages(anthropic::MessagesRequest),
    Completions(openai::ChatRequest),
    Responses(responses::ResponsesRequest),
}

/// Keep cached schemas while prohibiting tool invocation for the auxiliary request.
pub fn summary_request_body(req: &Request<'_>) -> Result<SummaryBody, LlmError> {
    Ok(match api_of(req.provider)? {
        Api::AnthropicMessages => {
            let mut body = anthropic::build_request(req);
            body.stream = false;
            body.prohibit_tools();
            SummaryBody::Messages(body)
        }
        Api::OpenAiCompletions => {
            let mut body = openai::build_request(req, false);
            body.prohibit_tools();
            SummaryBody::Completions(body)
        }
        Api::OpenAiResponses => {
            let mut body = responses::build_request(req, false);
            body.prohibit_tools();
            SummaryBody::Responses(body)
        }
    })
}

/// Decode only complete frames, so a transport chunk may split any UTF-8 code point.
fn drain_sse(
    buffer: &mut Vec<u8>, api: Api, assemblers: &mut Assemblers,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<(), LlmError> {
    let mut consumed = 0;
    loop {
        let rest = &buffer[consumed..];
        let end = rest.iter().enumerate().find_map(|(i, b)| {
            if *b != b'\n' { return None; }
            if rest.get(i + 1) == Some(&b'\n') { return Some(i + 2); }
            if rest.get(i + 1..i + 3) == Some(b"\r\n") { return Some(i + 3); }
            None
        });
        let Some(end) = end else { break };
        let frame = std::str::from_utf8(&rest[..end]).map_err(|err| LlmError::Decode(err.to_string()))?;
        dispatch(frame, api, assemblers, on_delta)?;
        consumed += end;
    }
    buffer.drain(..consumed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_prefix(api: Api) -> Vec<serde_json::Value> {
        let args = r#"{"path":"must-not-exist","content":"bad"}"#;
        match api {
            Api::OpenAiCompletions => vec![serde_json::json!({"choices":[{"delta":{
                "content":"已生成的正文", "reasoning_content":"已有思考",
                "tool_calls":[{"index":0,"id":"c1","function":{"name":"write","arguments":args}}]
            }}]})],
            Api::OpenAiResponses => vec![
                serde_json::json!({"type":"response.output_text.delta", "output_index":0, "delta":"已生成的正文"}),
                serde_json::json!({"type":"response.output_item.added", "output_index":1, "item":{"type":"function_call", "id":"fc1", "call_id":"c1", "name":"write", "arguments":args}}),
            ],
            Api::AnthropicMessages => vec![
                serde_json::json!({"type":"content_block_start", "index":0, "content_block":{"type":"text","text":""}}),
                serde_json::json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta","text":"已生成的正文"}}),
                serde_json::json!({"type":"content_block_start", "index":1, "content_block":{"type":"tool_use","id":"c1","name":"write","input":{}}}),
                serde_json::json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta","partial_json":args}}),
                serde_json::json!({"type":"content_block_stop", "index":1}),
            ],
        }
    }

    #[test]
    fn every_protocol_requires_a_terminal_event_before_releasing_tools() {
        for api in [
            Api::OpenAiCompletions,
            Api::OpenAiResponses,
            Api::AnthropicMessages,
        ] {
            for terminal in [false, true] {
                let mut assemblers = Assemblers::default();
                let mut sink = |_: Delta| {};
                for event in tool_prefix(api) {
                    dispatch(
                        &format!("data: {event}\n\n"),
                        api,
                        &mut assemblers,
                        &mut sink,
                    )
                    .unwrap();
                }
                if terminal {
                    let events = match api {
                        Api::OpenAiCompletions => vec![
                            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
                        ],
                        Api::OpenAiResponses => vec![
                            serde_json::json!({"type":"response.completed","response":{"status":"completed"}}),
                        ],
                        Api::AnthropicMessages => vec![
                            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
                            serde_json::json!({"type":"message_stop"}),
                        ],
                    };
                    for event in events {
                        dispatch(
                            &format!("data: {event}\n\n"),
                            api,
                            &mut assemblers,
                            &mut sink,
                        )
                        .unwrap();
                    }
                } else {
                    dispatch("data: [DONE]\n\n", api, &mut assemblers, &mut sink).unwrap();
                }
                let completion = assemblers.finish(api);
                assert_eq!(completion.text(), "已生成的正文");
                assert_eq!(
                    completion.stop_reason,
                    if terminal {
                        super::super::StopReason::ToolUse
                    } else {
                        super::super::StopReason::Error
                    }
                );
                assert_eq!(completion.tool_calls().len(), usize::from(terminal));
            }
        }
    }

    #[test]
    fn in_band_errors_discard_tools_and_preserve_partial_text() {
        for api in [
            Api::OpenAiCompletions,
            Api::OpenAiResponses,
            Api::AnthropicMessages,
        ] {
            let mut assemblers = Assemblers::default();
            let mut sink = |_: Delta| {};
            for event in tool_prefix(api) {
                dispatch(
                    &format!("data: {event}\n\n"),
                    api,
                    &mut assemblers,
                    &mut sink,
                )
                .unwrap();
            }
            let error = match api {
                Api::OpenAiCompletions => {
                    serde_json::json!({"error":{"message":"upstream failed"}})
                }
                Api::OpenAiResponses => {
                    serde_json::json!({"type":"error","message":"upstream failed"})
                }
                Api::AnthropicMessages => {
                    serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"upstream failed"}})
                }
            };
            dispatch(
                &format!("data: {error}\n\n"),
                api,
                &mut assemblers,
                &mut sink,
            )
            .unwrap();
            let completion = assemblers.finish(api);
            assert_eq!(completion.text(), "已生成的正文");
            assert!(completion.tool_calls().is_empty());
            assert!(
                completion
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("upstream failed")
            );
        }
    }

    #[test]
    fn incomplete_responses_without_a_reason_are_errors() {
        let mut assemblers = Assemblers::default();
        let api = Api::OpenAiResponses;
        let mut sink = |_: Delta| {};
        for event in tool_prefix(api) {
            dispatch(
                &format!("data: {event}\n\n"),
                api,
                &mut assemblers,
                &mut sink,
            )
            .unwrap();
        }
        dispatch(
            r#"data: {"type":"response.incomplete","response":{"status":"incomplete"}}

"#,
            api,
            &mut assemblers,
            &mut sink,
        )
        .unwrap();
        let completion = assemblers.finish(api);
        assert_eq!(completion.stop_reason, super::super::StopReason::Error);
        assert!(completion.tool_calls().is_empty());
        assert!(completion.error.is_some());
    }

    #[test]
    fn utf8_and_crlf_frames_survive_every_transport_boundary() {
        let bytes = "data: {\"choices\":[{\"delta\":{\"content\":\"中文🙂\"}}]}\r\n\r\ndata: [DONE]\n\n".as_bytes();
        for split in 0..=bytes.len() {
            let mut buffer = Vec::new();
            let mut assemblers = Assemblers::default();
            let mut text = String::new();
            let mut sink = |delta| if let Delta::Text(part) = delta { text.push_str(&part); };
            for part in [&bytes[..split], &bytes[split..]] {
                buffer.extend_from_slice(part);
                drain_sse(&mut buffer, Api::OpenAiCompletions, &mut assemblers, &mut sink).unwrap();
            }
            assert!(buffer.is_empty());
            assert_eq!(text, "中文🙂");
        }
    }

    #[test]
    fn error_bodies_are_unwrapped_from_their_envelope() {
        assert_eq!(
            describe_error(r#"{"error":{"message":"prompt is too long"}}"#),
            "prompt is too long"
        );
        assert_eq!(describe_error(r#"{"message":"bad key"}"#), "bad key");
        assert_eq!(describe_error("plain failure"), "plain failure");
    }

    #[test]
    fn sse_frames_are_split_on_event_and_data_lines() {
        let frame = "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"text\":\"hi\"}}\n\n";
        match parse_sse_frame(frame, Api::AnthropicMessages).unwrap() {
            Some(Frame::Anthropic(event)) => assert_eq!(event.kind, "content_block_delta"),
            _ => panic!("expected an anthropic event"),
        }
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        match parse_sse_frame(frame, Api::OpenAiCompletions).unwrap() {
            Some(Frame::OpenAi(_)) => {}
            _ => panic!("expected an openai chunk"),
        }
        // The Responses API names the event twice — once as an SSE `event:` line and once
        // inside the payload — and the payload is the copy that is trusted.
        let frame = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"x\"}\n\n";
        match parse_sse_frame(frame, Api::OpenAiResponses).unwrap() {
            Some(Frame::Responses(event)) => assert_eq!(event.kind, "response.output_text.delta"),
            _ => panic!("expected a responses event"),
        }
        assert!(parse_sse_frame("data: [DONE]\n\n", Api::OpenAiCompletions).unwrap().is_none());
        assert!(parse_sse_frame("\n", Api::OpenAiResponses).unwrap().is_none());
    }

    #[test]
    fn a_provider_with_an_unknown_api_is_reported_rather_than_misread() {
        // The config validates the name at start-up, so this is the guard behind that: a
        // hand-built provider must not silently fall through to another protocol.
        let provider: Provider = serde_json::from_str(
            r#"{"name":"weird","api":"openai-nonsense","base_url":"url","models":[]}"#,
        )
        .unwrap();
        match api_of(&provider) {
            Err(LlmError::UnknownApi(name, api)) => {
                assert_eq!(name, "weird");
                assert_eq!(api, "openai-nonsense");
            }
            other => panic!("expected an unknown-api error, got {other:?}"),
        }
    }
}
