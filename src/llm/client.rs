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

/// Which body/parser pair a provider uses. Read once per request so the dispatch is a
/// single match rather than a string comparison scattered through the call.
fn api_of(provider: &Provider) -> Result<Api, LlmError> {
    provider
        .api()
        .ok_or_else(|| LlmError::UnknownApi(provider.name.clone(), provider.api.clone()))
}

impl Client {
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
        let mut buffer = String::new();
        let mut assemblers = Assemblers::default();
        // Where the reasoning payloads about to arrive were issued from. A signature has to
        // be checked against the provider and model before being replayed, and this is the
        // only place that knows both.
        assemblers.responses.issued_by(&provider.base_url, &req.model.id);
        let mut response = response;
        // `chunk()` is inherent on `Response`, so no extra stream-trait dependency is
        // needed just to read the body incrementally.
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| LlmError::Transport(err.to_string()))?
        {
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            // Frames end at a blank line; the trailing partial frame stays buffered.
            while let Some(index) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..index + 2).collect();
                dispatch(&frame, api, &mut assemblers, on_delta)?;
            }
        }
        if !buffer.trim().is_empty() {
            dispatch(&buffer, api, &mut assemblers, on_delta)?;
        }
        Ok(match api {
            Api::AnthropicMessages => assemblers.anthropic.finish(),
            Api::OpenAiCompletions => assemblers.openai.finish(),
            Api::OpenAiResponses => assemblers.responses.finish(),
        })
    }

    /// One buffered completion, used for compaction summaries: they are never rendered
    /// token by token and must not burn cache writes.
    pub async fn complete(&self, req: &Request<'_>) -> Result<super::Completion, LlmError> {
        let provider = req.provider;
        let api = api_of(provider)?;
        let api_key = Self::api_key(provider)?;
        let response = if api == Api::AnthropicMessages {
            let mut body = anthropic::build_request(req);
            body.stream = false;
            let mut request = self
                .http
                .post(anthropic::endpoint(&provider.base_url))
                .json(&body);
            for (name, value) in
                anthropic::headers(&api_key, provider.compat(req.model).supports_long_cache)
            {
                request = request.header(name, value);
            }
            request.send().await.map_err(|err| LlmError::Transport(err.to_string()))?
        } else {
            let mut request = match api {
                Api::OpenAiResponses => self
                    .http
                    .post(responses::endpoint(&provider.base_url))
                    .json(&responses::build_request(req, false)),
                _ => self
                    .http
                    .post(openai::endpoint(&provider.base_url))
                    .json(&openai::build_request(req, false)),
            };
            request = request.bearer_auth(&api_key);
            request.send().await.map_err(|err| LlmError::Transport(err.to_string()))?
        };
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
                .map(anthropic::FullResponse::assemble)
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

#[cfg(test)]
mod tests {
    use super::*;

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
