//! HTTP plumbing shared by both providers: one streaming call, one buffered call.
//!
//! SSE framing is parsed by hand: both providers send `data:` lines that feed straight
//! into the provider-specific assemblers.

use std::time::Duration;

use super::{Api, Delta, LlmError, Request, anthropic, openai, responses};
use crate::config::Provider;

pub struct Client {
    http: reqwest::Client,
}

/// How many times one request may be sent again before the failure is reported.
///
/// Five, like codex. The count is per request, not per turn: a turn that makes three calls
/// gets five for each, because the failures are independent.
const RETRY_LIMIT: u32 = 5;

/// The first wait, doubling per attempt: 0.5s, 1s, 2s, 4s, 8s. About 15s of patience.
///
/// Under `cfg(test)` it is a millisecond, so a test that exhausts the budget — or proves a
/// `Retry-After` is honoured — does not spend fifteen seconds proving it. The shape of the
/// backoff is asserted directly, on an explicit base.
#[cfg(not(test))]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
#[cfg(test)]
const RETRY_BASE_DELAY: Duration = Duration::from_millis(1);

/// A ceiling on any single wait, including one a server asked for with `Retry-After`.
///
/// A header is a number a server chose; without a ceiling, `Retry-After: 86400` would park
/// the session for a day with nothing on screen but a notice. Past a minute, failing is the
/// more useful answer: the user can decide, and the request can be sent again by typing.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(60);

/// Whether a failed attempt may be sent again, and how long to wait first.
enum Retry {
    /// The request will fail the same way again: a bad request, a bad key, a bad model.
    Never,
    /// Worth another try. `Duration::ZERO` means "as soon as the backoff allows".
    After(Duration),
}

/// How one attempt ended.
enum Attempt {
    /// The round trip finished — including a stream that broke after delivering text, which
    /// comes back as a completion carrying the error rather than as a failure.
    Finished(super::Completion),
    /// Nothing usable came of it.
    Failed { error: LlmError, retry: Retry },
}

/// The wait before retry number `attempt` (1-based).
///
/// Doubling from [`RETRY_BASE_DELAY`], with no jitter: the client is one session on one
/// machine, so there is no thundering herd to spread out, and a predictable delay is easier
/// to reason about when a run feels slow.
fn retry_delay(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(1u32 << (attempt - 1).min(8))
        .min(RETRY_MAX_DELAY)
}

/// Whether a status is worth retrying, and what the server asked us to wait.
///
/// 429 and the 5xx family are the transient ones. 529 is Anthropic's "overloaded", which is
/// not standard but is the most likely 5xx-shaped answer from that API. Every other 4xx is a
/// statement about the request itself, and resending it would spend the budget to produce
/// the same refusal.
fn retry_for_status(status: u16, retry_after: Option<Duration>) -> Retry {
    let transient = status == 429 || status == 529 || (500..600).contains(&status);
    if !transient {
        return Retry::Never;
    }
    Retry::After(retry_after.unwrap_or(Duration::ZERO).min(RETRY_MAX_DELAY))
}

/// The `Retry-After` header, in the only form providers send it: whole seconds.
///
/// The HTTP-date form is legal and unused in practice; anything that is not a number is
/// ignored, which falls back to the backoff. A wrong guess here costs a few hundred
/// milliseconds, so parsing it loosely is better than failing the request over it.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get(reqwest::header::RETRY_AFTER)?;
    parse_retry_after(value.to_str().ok()?)
}

/// Split out because a `Response` cannot be built in a test, and this is the half that
/// decides what the number means.
fn parse_retry_after(value: &str) -> Option<Duration> {
    Some(Duration::from_secs(value.trim().parse::<u64>().ok()?))
}

/// One decoded SSE frame, ready to be handed to the matching assembler.
enum Frame {
    Anthropic(Box<anthropic::StreamEvent>),
    OpenAi(Box<openai::StreamChunk>),
    Responses(Box<responses::StreamEvent>),
}

#[derive(Default)]
/// One accumulator per protocol, only one of which the request actually uses.
///
/// Held together rather than selected by `Api` because a frame is decoded before it is
/// attributed: `Frame` carries the protocol it was parsed as, and the matching field is the
/// one that consumes it. Keeping all three here is also what lets every request share one
/// code path — the alternative is three copies of the read loop, each with its own framing
/// and its own timeout handling.
///
/// They are not merged into one type despite the resemblance: see the note in `llm/mod.rs`.
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
        Self {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
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
    ///
    /// A failure that produced **nothing** is retried (see `RETRY_LIMIT`); one that
    /// already delivered text is not. Tokens cannot be un-shown, so a second request would
    /// print the answer twice and the caller would have to choose which copy to keep.
    pub async fn stream(
        &self,
        req: &Request<'_>,
        on_delta: &mut dyn FnMut(Delta),
    ) -> Result<super::Completion, LlmError> {
        super::validate_tool_history(req.messages)?;
        let provider = req.provider;
        let api = api_of(provider)?;
        let api_key = Self::api_key(provider)?;
        let mut attempts = 0u32;
        loop {
            // Reset per attempt: what matters is whether *this* request has already put
            // something on screen. `attempt` maintains it, because the decision it feeds —
            // retry or report — is made there, where the partial completion still exists.
            let mut emitted = false;
            let (error, mut delay) = match self
                .attempt(req, provider, api, &api_key, &mut emitted, on_delta)
                .await
            {
                Attempt::Finished(completion) => return Ok(completion),
                Attempt::Failed { error, retry } => {
                    if emitted {
                        // Something is on screen: this cannot be retried, and the caller is
                        // the one that knows what to do with a half-finished answer.
                        return Err(error);
                    }
                    match retry {
                        Retry::Never => return Err(error),
                        Retry::After(delay) => (error, delay),
                    }
                }
            };
            if attempts >= RETRY_LIMIT {
                return Err(error);
            }
            attempts += 1;
            // The server's own `Retry-After` wins when it asks for longer than the backoff
            // would; a shorter one does not make us ignore the backoff.
            let scheduled = retry_delay(RETRY_BASE_DELAY, attempts);
            if delay < scheduled {
                delay = scheduled;
            }
            on_delta(Delta::Notice(format!(
                "请求失败，重试 {attempts}/{RETRY_LIMIT}：{error}"
            )));
            tokio::time::sleep(delay).await;
        }
    }

    /// One request/response round trip, with no retry of its own.
    ///
    /// Sets `emitted` when the request puts text or thinking on screen. A failure after that
    /// is not an [`Attempt::Failed`]: what already arrived is kept, and the completion comes
    /// back carrying the error, exactly as it did before retries existed.
    async fn attempt(
        &self,
        req: &Request<'_>,
        provider: &crate::config::Provider,
        api: Api,
        api_key: &str,
        emitted: &mut bool,
        on_delta: &mut dyn FnMut(Delta),
    ) -> Attempt {
        let mut request = if api == Api::AnthropicMessages {
            let body = anthropic::build_request(req);
            let mut request = self
                .http
                .post(anthropic::endpoint(&provider.base_url))
                .json(&body);
            for (name, value) in
                anthropic::headers(api_key, provider.compat(req.model).supports_long_cache)
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
            request = request.bearer_auth(api_key);
            request
        };
        if provider.compat(req.model).send_session_affinity {
            // Only meaningful behind a load balancer, but harmless elsewhere, and it
            // is what keeps a session pinned to the backend holding its cache.
            request = request
                .header("x-session-affinity", req.session_id)
                .header("x-session-id", req.session_id);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                // Nothing left the client, or nothing came back: the same request can be sent
                // again. Connect timeouts, resets and DNS failures are all here.
                return Attempt::Failed {
                    error: LlmError::Transport(err.to_string()),
                    retry: Retry::After(Duration::ZERO),
                };
            }
        };
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(&response);
            let text = response.text().await.unwrap_or_default();
            return Attempt::Failed {
                error: LlmError::Api {
                    status: status.as_u16(),
                    message: describe_error(&text),
                },
                retry: retry_for_status(status.as_u16(), retry_after),
            };
        }
        let mut buffer = Vec::new();
        let mut assemblers = Assemblers::default();
        // Where the reasoning payloads about to arrive were issued from. A signature has to
        // be checked against the provider and model before being replayed, and this is the
        // only place that knows both.
        assemblers
            .responses
            .issued_by(&provider.base_url, &req.model.id);
        assemblers
            .anthropic
            .issued_by(&provider.base_url, &req.model.id);
        let mut response = response;
        // `chunk()` is inherent on `Response`, so no extra stream-trait dependency is
        // needed just to read the body incrementally.
        //
        // The interruption is classified once the read loop is over, which is why the loop
        // body only records what went wrong. Inside it, `forward` holds the borrow on
        // `emitted`; afterwards the flag can be read.
        let mut interruption: Option<String> = None;
        {
            // A notice is not output: the status line for a retry must not itself be what
            // makes the next retry impossible.
            let mut forward = |delta: Delta| {
                *emitted |= matches!(delta, Delta::Text(_) | Delta::Thinking(_));
                on_delta(delta);
            };
            loop {
                let chunk = match response.chunk().await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(err) => {
                        interruption = Some(format!("流式传输中断：{err}"));
                        break;
                    }
                };
                buffer.extend_from_slice(&chunk);
                if let Err(err) = drain_sse(&mut buffer, api, &mut assemblers, &mut forward) {
                    interruption = Some(err.to_string());
                    break;
                }
                if buffer.len() > 8 * 1024 * 1024 {
                    interruption = Some("SSE 单帧超过 8 MiB".into());
                    break;
                }
            }
            if interruption.is_none()
                && !buffer.is_empty()
                && let Err(err) = std::str::from_utf8(&buffer)
                    .map_err(|err| LlmError::Decode(err.to_string()))
                    .and_then(|frame| dispatch(frame, api, &mut assemblers, &mut forward))
            {
                interruption = Some(err.to_string());
            }
        }
        // The connection broke partway. If nothing was shown yet this is just a failed
        // attempt and worth retrying; if text already arrived it is the caller's answer,
        // cut short, and pretending a second request would fix it would duplicate it.
        if let Some(reason) = interruption {
            return if *emitted {
                Attempt::Finished(assemblers.finish(api).failed(reason))
            } else {
                Attempt::Failed {
                    error: LlmError::Transport(reason),
                    retry: Retry::After(Duration::ZERO),
                }
            };
        }
        Attempt::Finished(assemblers.finish(api))
    }

    /// Buffered auxiliary completion with its own routing identity and tools disabled.
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
            for (name, value) in
                anthropic::headers(&api_key, provider.compat(req.model).supports_long_cache)
            {
                request = request.header(name, value);
            }
        } else {
            request = request.bearer_auth(&api_key);
        }
        if provider.compat(req.model).send_session_affinity {
            request = request
                .header("x-session-affinity", req.session_id)
                .header("x-session-id", req.session_id);
        }
        let response = request
            .send()
            .await
            .map_err(|err| LlmError::Transport(err.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| LlmError::Transport(err.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Api {
                status: status.as_u16(),
                message: describe_error(&text),
            });
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
    serde_json::from_str(trimmed).map(Some).map_err(|err| {
        LlmError::Decode(format!(
            "{err}: {}",
            crate::util::truncate(trimmed, 300, "…")
        ))
    })
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
        Api::AnthropicMessages => anthropic::parse_event(event_name.as_deref(), &data)?
            .map(|e| Frame::Anthropic(Box::new(e))),
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
    buffer: &mut Vec<u8>,
    api: Api,
    assemblers: &mut Assemblers,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<(), LlmError> {
    let mut consumed = 0;
    loop {
        let rest = &buffer[consumed..];
        let end = rest.iter().enumerate().find_map(|(i, b)| {
            if *b != b'\n' {
                return None;
            }
            if rest.get(i + 1) == Some(&b'\n') {
                return Some(i + 2);
            }
            if rest.get(i + 1..i + 3) == Some(b"\r\n") {
                return Some(i + 3);
            }
            None
        });
        let Some(end) = end else { break };
        let frame =
            std::str::from_utf8(&rest[..end]).map_err(|err| LlmError::Decode(err.to_string()))?;
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
        let bytes =
            "data: {\"choices\":[{\"delta\":{\"content\":\"中文🙂\"}}]}\r\n\r\ndata: [DONE]\n\n"
                .as_bytes();
        for split in 0..=bytes.len() {
            let mut buffer = Vec::new();
            let mut assemblers = Assemblers::default();
            let mut text = String::new();
            let mut sink = |delta| {
                if let Delta::Text(part) = delta {
                    text.push_str(&part);
                }
            };
            for part in [&bytes[..split], &bytes[split..]] {
                buffer.extend_from_slice(part);
                drain_sse(
                    &mut buffer,
                    Api::OpenAiCompletions,
                    &mut assemblers,
                    &mut sink,
                )
                .unwrap();
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
        let frame =
            "event: content_block_delta\ndata: {\"index\":0,\"delta\":{\"text\":\"hi\"}}\n\n";
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
        assert!(
            parse_sse_frame("data: [DONE]\n\n", Api::OpenAiCompletions)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_sse_frame("\n", Api::OpenAiResponses)
                .unwrap()
                .is_none()
        );
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

    #[test]
    fn the_backoff_doubles_and_stops_climbing() {
        let base = Duration::from_millis(500);
        // 0.5s, 1s, 2s, 4s, 8s: about fifteen seconds over the whole budget.
        let expected = [500u64, 1000, 2000, 4000, 8000];
        for (index, millis) in expected.iter().enumerate() {
            assert_eq!(
                retry_delay(base, index as u32 + 1),
                Duration::from_millis(*millis)
            );
        }
        // The ceiling holds no matter how far the shift would go.
        assert_eq!(retry_delay(base, 30), RETRY_MAX_DELAY);
    }

    #[test]
    fn only_the_transient_statuses_are_retried() {
        // Waiting is only worth it for the answers that can come out differently.
        for status in [429u16, 500, 502, 503, 504, 529] {
            assert!(
                matches!(retry_for_status(status, None), Retry::After(_)),
                "{status} should be retried"
            );
        }
        // The rest describe the request, and would describe it again.
        for status in [400u16, 401, 403, 404, 422, 499] {
            assert!(
                matches!(retry_for_status(status, None), Retry::Never),
                "{status} should not be retried"
            );
        }
    }

    #[test]
    fn a_retry_after_longer_than_the_backoff_wins_and_is_capped() {
        // The server asked; that beats the schedule.
        assert!(matches!(
            retry_for_status(429, Some(Duration::from_secs(10))),
            Retry::After(delay) if delay == Duration::from_secs(10)
        ));
        // But not without limit: an hour is a session parked with a notice on screen.
        assert!(matches!(
            retry_for_status(429, Some(Duration::from_secs(3600))),
            Retry::After(delay) if delay == RETRY_MAX_DELAY
        ));
    }

    #[test]
    fn retry_after_is_read_as_seconds_and_stays_quiet_about_anything_else() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after(" 12 "), Some(Duration::from_secs(12)));
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        // The HTTP-date form is legal and unused: ignoring it falls back to the backoff,
        // which costs a fraction of a second and cannot fail the request.
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("later"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    /// Accept one connection and return the request body it carried.
    ///
    /// Both scripted servers below need the same three steps — accept, read the headers,
    /// read the body — and differ only in what they answer with. Returning `None` means the
    /// deadline passed, which is how a server stops waiting for a request the client decided
    /// not to send: that is what makes "the second connection was never attempted" a fact a
    /// test can assert rather than a sleep it hopes is long enough.
    fn accept_request(
        listener: &std::net::TcpListener,
        timeout: std::time::Duration,
    ) -> Option<(std::net::TcpStream, String)> {
        use std::io::{BufRead, Read, Write};
        let deadline = std::time::Instant::now() + timeout;
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(err) => panic!("{err}"),
            }
        };
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).unwrap();
        let _ = socket.flush();
        Some((socket, String::from_utf8_lossy(&body).to_string()))
    }

    /// A response with an explicit body and a `Content-Length` that may overstate it.
    ///
    /// Overstating is how a test asks for a broken stream: the client reads what arrived,
    /// then finds the connection closed before the length it was promised.
    fn respond(socket: &mut std::net::TcpStream, status: u16, body: &str, declared_extra: usize) {
        use std::io::Write;
        let reason = if status == 200 { "OK" } else { "Error" };
        let content_type = if status == 200 {
            "text/event-stream"
        } else {
            "application/json"
        };
        write!(
            socket,
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len() + declared_extra
        )
        .unwrap();
        socket.flush().unwrap();
    }

    /// A server that answers with `statuses` in order, one connection each.
    ///
    /// Every response is a complete SSE body unless the status says otherwise, so a test can
    /// say "429, then a real answer" and watch the client reconnect.
    fn scripted_server(statuses: Vec<u16>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (index, status) in statuses.iter().enumerate() {
                let Some((mut socket, body)) =
                    accept_request(&listener, std::time::Duration::from_secs(10))
                else {
                    panic!("connection {index} never arrived");
                };
                seen.push(body);
                if *status == 200 {
                    respond(
                        &mut socket,
                        200,
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                        0,
                    );
                } else {
                    respond(
                        &mut socket,
                        *status,
                        &format!("{{\"error\":{{\"message\":\"busy {index}\"}}}}"),
                        0,
                    );
                }
            }
            seen
        });
        (format!("http://{address}/v1"), handle)
    }

    fn streaming_request<'a>(
        provider: &'a Provider,
        model: &'a crate::config::ModelConfig,
        messages: &'a [super::super::Message],
        tools: &'a [super::super::ToolSpec],
    ) -> Request<'a> {
        Request {
            model,
            provider,
            messages,
            tools,
            level: "high",
            session_id: "test-session",
            cache_hints: false,
        }
    }

    #[tokio::test]
    async fn a_transient_failure_is_retried_and_the_answer_arrives() {
        let (base_url, server) = scripted_server(vec![429, 429, 200]);
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "providers":[{"name":"test","api":"completions","base_url":base_url,"api_key":"k",
                "models":[{"id":"m"}]}],
            "default_model":"test/m"
        }))
        .unwrap();
        let (provider, model) = config.find("test/m").unwrap();
        let messages = vec![super::super::Message::user_text("hi")];
        let mut notices = Vec::new();
        let client = Client::local_test_client();
        let completion = client
            .stream(
                &streaming_request(provider, model, &messages, &[]),
                &mut |delta| {
                    if let Delta::Notice(text) = delta {
                        notices.push(text);
                    }
                },
            )
            .await
            .unwrap();
        assert_eq!(completion.text(), "hello");
        let seen = server.join().unwrap();
        // Three requests, and the two failures said so.
        assert_eq!(seen.len(), 3);
        assert_eq!(notices.len(), 2, "{notices:?}");
        assert!(notices[0].contains("重试 1/5"), "{notices:?}");
        assert!(notices[1].contains("重试 2/5"), "{notices:?}");
    }

    #[tokio::test]
    async fn the_budget_runs_out_after_five_retries() {
        // Six responses for one request: the original and the five retries.
        let (base_url, server) = scripted_server(vec![503, 503, 503, 503, 503, 503]);
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "providers":[{"name":"test","api":"completions","base_url":base_url,"api_key":"k",
                "models":[{"id":"m"}]}],
            "default_model":"test/m"
        }))
        .unwrap();
        let (provider, model) = config.find("test/m").unwrap();
        let messages = vec![super::super::Message::user_text("hi")];
        let client = Client::local_test_client();
        let error = client
            .stream(
                &streaming_request(provider, model, &messages, &[]),
                &mut |_| {},
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, LlmError::Api { status: 503, .. }),
            "{error:?}"
        );
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), RETRY_LIMIT as usize + 1);
    }

    #[tokio::test]
    async fn a_request_the_server_rejected_outright_is_not_resent() {
        // 400 is a statement about the request, and the retries would produce it five more
        // times. One connection is all the server should see.
        let (base_url, server) = scripted_server(vec![400]);
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "providers":[{"name":"test","api":"completions","base_url":base_url,"api_key":"k",
                "models":[{"id":"m"}]}],
            "default_model":"test/m"
        }))
        .unwrap();
        let (provider, model) = config.find("test/m").unwrap();
        let messages = vec![super::super::Message::user_text("hi")];
        let client = Client::local_test_client();
        let error = client
            .stream(
                &streaming_request(provider, model, &messages, &[]),
                &mut |_| {},
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, LlmError::Api { status: 400, .. }),
            "{error:?}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    /// A server that sends part of an SSE body and then drops the connection.
    ///
    /// `Content-Length` promises more than is written, so the client sees a broken stream
    /// rather than a clean end — which is what a dropped connection looks like.
    fn truncated_server(preamble: &'static str) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            // One request is expected. The second `accept` waits briefly rather than
            // blocking forever, so a retry would be *seen* rather than hang the test.
            let Some((mut socket, body)) =
                accept_request(&listener, std::time::Duration::from_secs(5))
            else {
                return seen;
            };
            seen.push(body);
            respond(&mut socket, 200, &format!("data: {preamble}\n\n"), 500);
            drop(socket);
            if accept_request(&listener, std::time::Duration::from_secs(2)).is_some() {
                panic!("the stream was retried after delivering text");
            }
            seen
        });
        (format!("http://{address}/v1"), handle)
    }

    #[tokio::test]
    async fn a_stream_that_broke_after_delivering_text_is_never_sent_again() {
        // The half-answer is real output the user can read, and the caller records it as the
        // assistant's message. A retry would print a second copy of it.
        let preamble =
            r#"{"choices":[{"delta":{"content":"已经写出来的正文"},"finish_reason":null}]}"#;
        let (base_url, server) = truncated_server(preamble);
        let config: crate::config::Config = serde_json::from_value(serde_json::json!({
            "providers":[{"name":"test","api":"completions","base_url":base_url,"api_key":"k",
                "models":[{"id":"m"}]}],
            "default_model":"test/m"
        }))
        .unwrap();
        let (provider, model) = config.find("test/m").unwrap();
        let messages = vec![super::super::Message::user_text("hi")];
        let mut text = String::new();
        let client = Client::local_test_client();
        let completion = client
            .stream(
                &streaming_request(provider, model, &messages, &[]),
                &mut |delta| {
                    if let Delta::Text(chunk) = delta {
                        text.push_str(&chunk);
                    }
                },
            )
            .await
            .unwrap();
        // Whatever arrived is kept, and the completion says the stream did not finish.
        assert_eq!(text, "已经写出来的正文");
        assert_eq!(completion.text(), "已经写出来的正文");
        assert!(completion.error.is_some(), "{completion:?}");
        assert_eq!(completion.stop_reason, super::super::StopReason::Error);
        // One connection: the answer was on screen, so there was nothing to retry.
        assert_eq!(server.join().unwrap().len(), 1);
    }
}
