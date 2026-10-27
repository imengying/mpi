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
    /// Explicitly no hosted search, even when the host would otherwise have one.
    Off,
}

impl SearchFormat {
    /// Whether this shape belongs on `api`. A flag sent to the wrong endpoint is rejected,
    /// so a mismatch is not a search.
    pub fn fits(self, api: crate::llm::Api) -> bool {
        match self {
            SearchFormat::Off => false,
            SearchFormat::Anthropic => api == crate::llm::Api::AnthropicMessages,
            SearchFormat::WebSearch | SearchFormat::WebAndX => {
                api == crate::llm::Api::OpenAiResponses
            }
        }
    }
}

/// Concrete, resolved switches. Build with [`Compat::from_base_url`].
///
/// Only what the two remaining protocols actually read. The rest of the old table described
/// Chat Completions — its `reasoning_content` field, its `max_completion_tokens` spelling, its
/// `developer` role — and nothing consults those any more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compat {
    /// `strict` on a Responses function definition: the endpoint rejects an unknown key, so
    /// this is sent only where the host accepts it.
    pub supports_strict_mode: bool,
    /// Send `x-session-affinity` so a session sticks to one backend (better cache hits).
    pub send_session_affinity: bool,
    /// Server keeps prompt caches longer than the default; enables the 1h/24h hints.
    pub supports_long_cache: bool,
    /// Assistant messages must carry a (possibly empty) reasoning item once any message in
    /// the history has one, or the upstream rejects the request.
    pub requires_assistant_after_tool_result: bool,
    /// Hosted search this provider can speak, when it has one. `None` means pi must not
    /// invent a search field: an unknown flag is a 400, not a silent no-op.
    pub search_format: Option<SearchFormat>,
}

impl Default for Compat {
    fn default() -> Self {
        Compat {
            supports_strict_mode: false,
            send_session_affinity: true,
            supports_long_cache: false,
            requires_assistant_after_tool_result: false,
            search_format: None,
        }
    }
}

impl Compat {
    pub fn from_base_url(base_url: &str, api: crate::llm::Api) -> Self {
        let mut compat = Compat::default();
        let url = base_url.to_lowercase();
        if api == crate::llm::Api::AnthropicMessages {
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
            compat.supports_strict_mode = true;
            compat.supports_long_cache = true;
        } else if host.contains("deepseek") {
            // The official API does not run a search. Responses accepts `web_search` and
            // then ignores it, which would look like a search that never happened. Leave
            // the format unset so `search: true` fails at startup instead of sending a
            // no-op.
            compat.search_format = None;
        } else if host.contains("bigmodel") || host.contains("z.ai") || host.contains("zhipu") {
            // No hosted search on the Responses API for this host: `search: true` fails at
            // startup rather than sending a flag the endpoint ignores.
            compat.search_format = None;
        } else if host.contains("dashscope") || host.contains("aliyuncs") || host.contains("qwen") {
            compat.search_format = None;
        } else if host == "x.ai" || host.ends_with(".x.ai") {
            compat.search_format = Some(SearchFormat::WebAndX);
        } else if host.contains("openrouter") {
            // OpenRouter normalises caching itself; the markers are not accepted.
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
        if let Some(value) = patch.requires_assistant_after_tool_result {
            self.requires_assistant_after_tool_result = value;
        }
        if let Some(value) = patch.supports_strict_mode {
            self.supports_strict_mode = value;
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
///
/// A key this struct does not name is rejected by `config.rs`, which lists the fields it
/// understands: an unrecognised `compat` entry is a typo the user should hear about, not a
/// setting that silently does nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct CompatPatch {
    pub requires_assistant_after_tool_result: Option<bool>,
    pub supports_strict_mode: Option<bool>,
    pub send_session_affinity: Option<bool>,
    pub supports_long_cache: Option<bool>,
    pub search_format: Option<SearchFormat>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Api;

    #[test]
    fn openai_accepts_the_strict_flag_and_the_long_cache() {
        let compat = Compat::from_base_url("https://api.openai.com/v1", Api::OpenAiResponses);
        assert!(compat.supports_strict_mode);
        assert!(compat.supports_long_cache);
    }

    #[test]
    fn a_local_gateway_keeps_the_permissive_defaults() {
        // A local runner is not a load balancer, so pinning a session to one backend would
        // be meaningless; everything else stays at the permissive default.
        let compat = Compat::from_base_url("http://127.0.0.1:8000/v1", Api::OpenAiResponses);
        assert!(!compat.send_session_affinity);
        assert!(!compat.supports_strict_mode);
        assert!(!compat.supports_long_cache);
    }

    #[test]
    fn anthropic_gets_its_own_search_shape() {
        let compat = Compat::from_base_url("https://api.anthropic.com", Api::AnthropicMessages);
        assert_eq!(compat.search_format, Some(SearchFormat::Anthropic));
    }

    #[test]
    fn config_patch_overrides_only_what_it_names() {
        let mut compat = Compat::from_base_url("url", Api::OpenAiResponses);
        assert!(!compat.supports_strict_mode);
        assert_eq!(compat.search_format, Some(SearchFormat::WebSearch));
        let patch: CompatPatch = serde_json::from_str(r#"{"supports_strict_mode":true}"#).unwrap();
        compat.apply(&patch);
        assert!(compat.supports_strict_mode);
        // Untouched fields keep their detected values.
        assert!(compat.send_session_affinity);
        assert_eq!(compat.search_format, Some(SearchFormat::WebSearch));
    }

    #[test]
    fn hosted_search_follows_the_host_and_the_protocol() {
        use crate::llm::Api;
        assert_eq!(
            Compat::from_base_url("https://api.anthropic.com", Api::AnthropicMessages)
                .search_format,
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
        // DashScope and Zhipu only ever had a Chat Completions search flag. On the Responses
        // API there is nothing to send, so `search: true` fails at startup rather than
        // silently doing nothing.
        for url in [
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "https://open.bigmodel.cn/api/paas/v4",
            "https://api.z.ai/api/paas/v4",
        ] {
            assert_eq!(
                Compat::from_base_url(url, Api::OpenAiResponses).search_format,
                None,
                "{url}"
            );
        }
        // DeepSeek documents `web_search` as ignored on Responses. Treating that as "no
        // format" is what stops a silent no-op.
        assert_eq!(
            Compat::from_base_url("https://api.deepseek.com", Api::OpenAiResponses).search_format,
            None
        );
        assert_eq!(
            Compat::from_base_url("https://api.deepseek.com/v1", Api::OpenAiResponses)
                .search_format,
            None
        );
        // An unknown host gets the Responses default: the tool shape is the same everywhere.
        assert_eq!(
            Compat::from_base_url("https://example.test/v1", Api::OpenAiResponses).search_format,
            Some(SearchFormat::WebSearch)
        );
    }

    #[test]
    fn search_format_off_clears_a_detected_format() {
        let mut compat =
            Compat::from_base_url("https://api.x.ai/v1", crate::llm::Api::OpenAiResponses);
        let patch: CompatPatch = serde_json::from_str(r#"{"search_format":"off"}"#).unwrap();
        compat.apply(&patch);
        assert_eq!(compat.search_format, None);
    }
}
