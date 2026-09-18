//! HTTP plumbing shared by both providers: one streaming call, one buffered call.
//!
//! SSE framing is parsed by hand: both providers send `data:` lines that feed straight
//! into the provider-specific assemblers.

use super::{Delta, LlmError, Request, anthropic, openai};
use crate::config::Provider;

pub struct Client {
    http: reqwest::Client,
}

/// One decoded SSE frame, ready to be handed to the matching assembler.
enum Frame {
    Anthropic(Box<anthropic::StreamEvent>),
    OpenAi(Box<openai::StreamChunk>),
}

struct AnthropicState(anthropic::Assembler);
struct OpenAiState(openai::Assembler);

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
        let api_key = Self::api_key(provider)?;
        let anthropic_style = provider.api == "anthropic-messages";
        let mut request = if anthropic_style {
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
            let body = openai::build_request(req, true);
            self.http
                .post(openai::endpoint(&provider.base_url))
                .bearer_auth(&api_key)
                .json(&body)
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
        let mut anthropic_state = AnthropicState(anthropic::Assembler::default());
        let mut openai_state = OpenAiState(openai::Assembler::default());
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
                dispatch(&frame, anthropic_style, &mut anthropic_state, &mut openai_state, on_delta)?;
            }
        }
        if !buffer.trim().is_empty() {
            dispatch(&buffer, anthropic_style, &mut anthropic_state, &mut openai_state, on_delta)?;
        }
        Ok(if anthropic_style {
            anthropic_state.0.finish()
        } else {
            openai_state.0.finish()
        })
    }

    /// One buffered completion, used for compaction summaries: they are never rendered
    /// token by token and must not burn cache writes.
    pub async fn complete(&self, req: &Request<'_>) -> Result<super::Completion, LlmError> {
        let provider = req.provider;
        let api_key = Self::api_key(provider)?;
        let anthropic_style = provider.api == "anthropic-messages";
        let response = if anthropic_style {
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
            let body = openai::build_request(req, false);
            self.http
                .post(openai::endpoint(&provider.base_url))
                .bearer_auth(&api_key)
                .json(&body)
                .send()
                .await
                .map_err(|err| LlmError::Transport(err.to_string()))?
        };
        let status = response.status();
        let text = response.text().await.map_err(|err| LlmError::Transport(err.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Api { status: status.as_u16(), message: describe_error(&text) });
        }
        let decode = |err: serde_json::Error| {
            LlmError::Decode(format!("{err}: {}", crate::util::truncate(&text, 300, "…")))
        };
        if anthropic_style {
            serde_json::from_str::<anthropic::FullResponse>(&text)
                .map(anthropic::FullResponse::assemble)
                .map_err(decode)
        } else {
            serde_json::from_str::<openai::FullResponse>(&text)
                .map(openai::FullResponse::assemble)
                .map_err(decode)
        }
    }
}

fn dispatch(
    frame: &str,
    anthropic_style: bool,
    anthropic_state: &mut AnthropicState,
    openai_state: &mut OpenAiState,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<(), LlmError> {
    match parse_sse_frame(frame, anthropic_style)? {
        Some(Frame::Anthropic(event)) => anthropic_state.0.feed(*event, on_delta),
        Some(Frame::OpenAi(chunk)) => chunk.feed(&mut openai_state.0, on_delta),
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
fn parse_sse_frame(frame: &str, anthropic_style: bool) -> Result<Option<Frame>, LlmError> {
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
    if anthropic_style {
        Ok(anthropic::parse_event(event_name.as_deref(), &data)?.map(|e| Frame::Anthropic(Box::new(e))))
    } else {
        Ok(openai::parse_frame(&data)?.map(|c| Frame::OpenAi(Box::new(c))))
    }
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
        match parse_sse_frame(frame, true).unwrap() {
            Some(Frame::Anthropic(event)) => assert_eq!(event.kind, "content_block_delta"),
            _ => panic!("expected an anthropic event"),
        }
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        match parse_sse_frame(frame, false).unwrap() {
            Some(Frame::OpenAi(_)) => {}
            _ => panic!("expected an openai chunk"),
        }
        assert!(parse_sse_frame("data: [DONE]\n\n", false).unwrap().is_none());
        assert!(parse_sse_frame("\n", false).unwrap().is_none());
    }
}
