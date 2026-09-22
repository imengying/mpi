//! OpenAI-兼容 providers differ in a handful of details. Rather than branching on
//! `if provider == "..."`, the differences live in this table: defaults are derived
//! from the base URL, and the config only writes the exceptions.

use serde::{Deserialize, Serialize};

/// How a provider's hosted search is turned on. The model runs it upstream; pi never
/// executes the call. `off` is the config's way to clear a detected format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchFormat {
    /// Responses API tool `{ "type": "web_search" }`.
    WebSearch,
    /// Grok on the Responses API: `web_search` and `x_search`.
    WebAndX,
    /// Anthropic server tool `web_search_20250305`.
    Anthropic,
    /// xAI Chat Completions `search_parameters`.
    Xai,
    /// Qwen-compatible `enable_search`.
    Qwen,
    /// Zhipu tool `{ "type": "web_search" }`.
    Zhipu,
    /// Explicitly no hosted search, even when the host would otherwise have one.
    Off,
}

impl SearchFormat {
    /// Whether this shape belongs on `api`. A completions flag sent to the Responses
    /// endpoint is rejected, so a mismatch is not a search.
    pub fn fits(self, api: crate::llm::Api) -> bool {
        match self {
            SearchFormat::Off => false,
            SearchFormat::Anthropic => api == crate::llm::Api::AnthropicMessages,
            SearchFormat::WebSearch | SearchFormat::WebAndX => api == crate::llm::Api::OpenAiResponses,
            SearchFormat::Xai | SearchFormat::Qwen | SearchFormat::Zhipu => {
                api == crate::llm::Api::OpenAiCompletions
            }
        }
    }
}

/// How a provider expects thinking/reasoning to be turned on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingFormat {
    /// `reasoning_effort: "<level>"` — OpenAI, Kimi and friends.
    Openai,
    /// `thinking: {type: "enabled"}` plus a `thinking_token_budget`.
    Deepseek,
    /// `thinking: {type: "enabled"}` — Z.ai / GLM.
    Zai,
    /// `enable_thinking: true` plus `thinking_budget`.
    Qwen,
    /// `thinking_budget_tokens` only (llama.cpp server).
    Llamacpp,
    /// Anthropic-style `thinking.budget_tokens`.
    Anthropic,
    /// The model takes no reasoning parameter at all.
    None,
}

/// Concrete, resolved switches. Build with [`Compat::from_base_url`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compat {
    /// `max_tokens` or `max_completion_tokens`.
    pub max_tokens_field: &'static str,
    /// `developer` role instead of `system` for the system prompt.
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub thinking_format: ThinkingFormat,
    /// Thinking must be replayed as `<thinking>` text rather than a thinking block.
    pub requires_thinking_as_text: bool,
    /// Assistant messages must carry a (possibly empty) `reasoning_content` once any
    /// message in the history has one, or the upstream rejects the request.
    pub requires_reasoning_content_on_assistant: bool,
    /// A tool result must be followed by an assistant message.
    pub requires_assistant_after_tool_result: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_strict_mode: bool,
    /// Accepts an explicit `cache_control` marker on content blocks.
    pub supports_cache_control: bool,
    /// Send `x-session-affinity` so a session sticks to one backend (better cache hits).
    pub send_session_affinity: bool,
    /// Server keeps prompt caches longer than the default; enables the 1h/24h hints.
    pub supports_long_cache: bool,
    /// Hosted search this provider can speak, when it has one. `None` means pi must not
    /// invent a search field: an unknown flag is a 400, not a silent no-op.
    pub search_format: Option<SearchFormat>,
}

impl Default for Compat {
    fn default() -> Self {
        Compat {
            max_tokens_field: "max_tokens",
            supports_developer_role: false,
            supports_reasoning_effort: true,
            thinking_format: ThinkingFormat::Openai,
            requires_thinking_as_text: false,
            requires_reasoning_content_on_assistant: false,
            requires_assistant_after_tool_result: false,
            supports_usage_in_streaming: true,
            supports_strict_mode: false,
            supports_cache_control: false,
            send_session_affinity: true,
            supports_long_cache: false,
            search_format: None,
        }
    }
}

impl Compat {
    pub fn from_base_url(base_url: &str, api: crate::llm::Api) -> Self {
        let mut compat = Compat::default();
        let url = base_url.to_lowercase();
        if api == crate::llm::Api::AnthropicMessages {
            compat.thinking_format = ThinkingFormat::Anthropic;
            compat.send_session_affinity = true;
            compat.search_format = Some(SearchFormat::Anthropic);
            return compat;
        }
        if api == crate::llm::Api::OpenAiResponses {
            // The Responses tool is the same shape on every host. xAI's extra `x_search`
            // is layered on below, where the host is known.
            compat.search_format = Some(SearchFormat::WebSearch);
        }
        let host = url.split("//").nth(1).unwrap_or(&url);
        let host = host.split('/').next().unwrap_or(host);
        if host.ends_with("api.openai.com") {
            compat.max_tokens_field = "max_completion_tokens";
            compat.supports_developer_role = true;
            compat.supports_strict_mode = true;
            compat.supports_long_cache = true;
        } else if host.contains("deepseek") {
            compat.thinking_format = ThinkingFormat::Deepseek;
            compat.supports_reasoning_effort = false;
            compat.requires_reasoning_content_on_assistant = true;
            // The official API does not run a search. Responses accepts `web_search` and
            // then ignores it, which would look like a search that never happened. Chat
            // completions has no search field at all. Leave the format unset so `search:
            // true` fails at startup instead of sending a no-op.
            compat.search_format = None;
        } else if host.contains("bigmodel") || host.contains("z.ai") || host.contains("zhipu") {
            compat.thinking_format = ThinkingFormat::Zai;
            compat.supports_reasoning_effort = false;
            compat.requires_reasoning_content_on_assistant = true;
            if api == crate::llm::Api::OpenAiCompletions {
                compat.search_format = Some(SearchFormat::Zhipu);
            }
        } else if host.contains("dashscope") || host.contains("aliyuncs") || host.contains("qwen") {
            compat.thinking_format = ThinkingFormat::Qwen;
            compat.supports_reasoning_effort = false;
            if api == crate::llm::Api::OpenAiCompletions {
                compat.search_format = Some(SearchFormat::Qwen);
            }
        } else if host == "x.ai" || host.ends_with(".x.ai") {
            compat.search_format = Some(match api {
                crate::llm::Api::OpenAiResponses => SearchFormat::WebAndX,
                _ => SearchFormat::Xai,
            });
        } else if host.contains("moonshot") {
            compat.thinking_format = ThinkingFormat::Openai;
            compat.max_tokens_field = "max_completion_tokens";
        } else if host.contains("openrouter") {
            // OpenRouter normalises caching itself; the markers are not accepted.
            compat.thinking_format = ThinkingFormat::Openai;
            compat.send_session_affinity = false;
        } else if host.contains("localhost") || host.contains("127.0.0.1") {
            // A local gateway (vLLM, llama.cpp, LM Studio, one-api…). Keep the most
            // permissive shape: unknown options are usually ignored there.
            compat.send_session_affinity = false;
        }
        compat
    }

    /// Layer a config-supplied patch on top. Only the fields the user wrote change.
    pub fn apply(&mut self, patch: &CompatPatch) {
        if let Some(value) = &patch.max_tokens_field {
            self.max_tokens_field = if value == "max_completion_tokens" {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
        }
        if let Some(value) = patch.supports_developer_role {
            self.supports_developer_role = value;
        }
        if let Some(value) = patch.supports_reasoning_effort {
            self.supports_reasoning_effort = value;
        }
        if let Some(value) = patch.thinking_format {
            self.thinking_format = value;
        }
        if let Some(value) = patch.requires_thinking_as_text {
            self.requires_thinking_as_text = value;
        }
        if let Some(value) = patch.requires_reasoning_content_on_assistant {
            self.requires_reasoning_content_on_assistant = value;
        }
        if let Some(value) = patch.requires_assistant_after_tool_result {
            self.requires_assistant_after_tool_result = value;
        }
        if let Some(value) = patch.supports_usage_in_streaming {
            self.supports_usage_in_streaming = value;
        }
        if let Some(value) = patch.supports_strict_mode {
            self.supports_strict_mode = value;
        }
        if let Some(value) = patch.supports_cache_control {
            self.supports_cache_control = value;
        }
        if let Some(value) = patch.send_session_affinity {
            self.send_session_affinity = value;
        }
        if let Some(value) = patch.supports_long_cache {
            self.supports_long_cache = value;
        }
        if let Some(value) = patch.search_format {
            // `off` clears a format the host would otherwise have been given.
            self.search_format = (value != SearchFormat::Off).then_some(value);
        }
    }
}

/// The same switches, all optional, as they appear in `config.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CompatPatch {
    pub max_tokens_field: Option<String>,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub thinking_format: Option<ThinkingFormat>,
    pub requires_thinking_as_text: Option<bool>,
    pub requires_reasoning_content_on_assistant: Option<bool>,
    pub requires_assistant_after_tool_result: Option<bool>,
    pub supports_usage_in_streaming: Option<bool>,
    pub supports_strict_mode: Option<bool>,
    pub supports_cache_control: Option<bool>,
    pub send_session_affinity: Option<bool>,
    pub supports_long_cache: Option<bool>,
    pub search_format: Option<SearchFormat>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Api;

    #[test]
    fn openai_uses_max_completion_tokens_and_developer_role() {
        let compat = Compat::from_base_url("https://api.openai.com/v1", Api::OpenAiCompletions);
        assert_eq!(compat.max_tokens_field, "max_completion_tokens");
        assert!(compat.supports_developer_role);
        assert!(compat.supports_strict_mode);
    }

    #[test]
    fn a_local_gateway_keeps_the_permissive_defaults() {
        let compat = Compat::from_base_url("url", Api::OpenAiCompletions);
        assert_eq!(compat.max_tokens_field, "max_tokens");
        assert!(!compat.supports_developer_role);
        assert_eq!(compat.thinking_format, ThinkingFormat::Openai);
    }

    #[test]
    fn anthropic_api_selects_the_budget_format() {
        let compat = Compat::from_base_url("https://api.anthropic.com", Api::AnthropicMessages);
        assert_eq!(compat.thinking_format, ThinkingFormat::Anthropic);
    }

    #[test]
    fn config_patch_overrides_only_what_it_names() {
        let mut compat = Compat::from_base_url("url", Api::OpenAiCompletions);
        let patch: CompatPatch = serde_json::from_str(
            r#"{"thinking_format":"deepseek","requires_reasoning_content_on_assistant":true}"#,
        )
        .unwrap();
        compat.apply(&patch);
        assert_eq!(compat.thinking_format, ThinkingFormat::Deepseek);
        assert!(compat.requires_reasoning_content_on_assistant);
        // Untouched fields keep their detected values.
        assert_eq!(compat.max_tokens_field, "max_tokens");
    }

    #[test]
    fn hosted_search_follows_the_host_and_the_protocol() {
        use crate::llm::Api;
        assert_eq!(
            Compat::from_base_url("https://api.anthropic.com", Api::AnthropicMessages).search_format,
            Some(SearchFormat::Anthropic)
        );
        assert_eq!(
            Compat::from_base_url("https://api.openai.com/v1", Api::OpenAiResponses).search_format,
            Some(SearchFormat::WebSearch)
        );
        assert_eq!(
            Compat::from_base_url("https://api.x.ai/v1", Api::OpenAiResponses).search_format,
            Some(SearchFormat::WebAndX)
        );
        assert_eq!(
            Compat::from_base_url("https://api.x.ai/v1", Api::OpenAiCompletions).search_format,
            Some(SearchFormat::Xai)
        );
        assert_eq!(
            Compat::from_base_url("https://dashscope.aliyuncs.com/compatible-mode/v1", Api::OpenAiCompletions)
                .search_format,
            Some(SearchFormat::Qwen)
        );
        assert_eq!(
            Compat::from_base_url("https://open.bigmodel.cn/api/paas/v4", Api::OpenAiCompletions).search_format,
            Some(SearchFormat::Zhipu)
        );
        assert_eq!(
            Compat::from_base_url("https://api.z.ai/api/paas/v4", Api::OpenAiCompletions).search_format,
            Some(SearchFormat::Zhipu)
        );
        // DeepSeek documents `web_search` as ignored on Responses, and chat completions has
        // no search parameter. Treating that as "no format" is what stops a silent no-op.
        assert_eq!(
            Compat::from_base_url("https://api.deepseek.com", Api::OpenAiResponses).search_format,
            None
        );
        assert_eq!(
            Compat::from_base_url("https://api.deepseek.com/v1", Api::OpenAiCompletions).search_format,
            None
        );
        // An unknown completions gateway has no search field pi is willing to invent.
        assert_eq!(
            Compat::from_base_url("https://example.test/v1", Api::OpenAiCompletions).search_format,
            None
        );
    }

    #[test]
    fn search_format_off_clears_a_detected_format() {
        let mut compat = Compat::from_base_url("https://api.x.ai/v1", crate::llm::Api::OpenAiResponses);
        let patch: CompatPatch = serde_json::from_str(r#"{"search_format":"off"}"#).unwrap();
        compat.apply(&patch);
        assert_eq!(compat.search_format, None);
    }
}
