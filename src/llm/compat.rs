//! OpenAI-兼容 providers differ in a handful of details. Rather than branching on
//! `if provider == "..."`, the differences live in this table: defaults are derived
//! from the base URL, and the config only writes the exceptions.

use serde::{Deserialize, Serialize};

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
        }
    }
}

impl Compat {
    pub fn from_base_url(base_url: &str, api: &str) -> Self {
        let mut compat = Compat::default();
        let url = base_url.to_lowercase();
        if api == "anthropic-messages" {
            compat.thinking_format = ThinkingFormat::Anthropic;
            compat.send_session_affinity = true;
            return compat;
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
        } else if host.contains("bigmodel") || host.contains("z.ai") || host.contains("zhipu") {
            compat.thinking_format = ThinkingFormat::Zai;
            compat.supports_reasoning_effort = false;
            compat.requires_reasoning_content_on_assistant = true;
        } else if host.contains("dashscope") || host.contains("aliyuncs") || host.contains("qwen") {
            compat.thinking_format = ThinkingFormat::Qwen;
            compat.supports_reasoning_effort = false;
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_uses_max_completion_tokens_and_developer_role() {
        let compat = Compat::from_base_url("https://api.openai.com/v1", "openai-completions");
        assert_eq!(compat.max_tokens_field, "max_completion_tokens");
        assert!(compat.supports_developer_role);
        assert!(compat.supports_strict_mode);
    }

    #[test]
    fn a_local_gateway_keeps_the_permissive_defaults() {
        let compat = Compat::from_base_url("http://192.168.1.16:1221/v1", "openai-completions");
        assert_eq!(compat.max_tokens_field, "max_tokens");
        assert!(!compat.supports_developer_role);
        assert_eq!(compat.thinking_format, ThinkingFormat::Openai);
    }

    #[test]
    fn anthropic_api_selects_the_budget_format() {
        let compat = Compat::from_base_url("https://api.anthropic.com", "anthropic-messages");
        assert_eq!(compat.thinking_format, ThinkingFormat::Anthropic);
    }

    #[test]
    fn config_patch_overrides_only_what_it_names() {
        let mut compat = Compat::from_base_url("http://192.168.1.16:1221/v1", "openai-completions");
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
}
