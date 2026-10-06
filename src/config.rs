//! Configuration: a single JSON file read once at start-up.
//!
//! Everything except `providers` is optional and falls back to a default. There is no
//! runtime settings menu and no reload: what the user writes here *is* the model list.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::llm::compat::{Compat, CompatPatch};

pub const DEFAULT_SHELL: &str = "/usr/bin/zsh";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub shell: ShellConfig,
    pub providers: Vec<Provider>,
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub path: String,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig {
            path: DEFAULT_SHELL.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Provider {
    pub name: String,
    /// `messages`, `completions` or `responses`.
    pub api: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    /// Presence of a literal key is honoured, but the env var is preferred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<CompatPatch>,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelConfig {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub reasoning: bool,
    /// Levels this model accepts. Absent + `reasoning` means all five.
    pub thinking_levels: Vec<String>,
    /// Use the provider's hosted search. Off unless asked: most models cannot search, and
    /// a tool that appears and disappears between turns breaks the cache prefix.
    pub search: bool,
    /// Sampling overrides keyed by the effective thinking level. These are sent on
    /// OpenAI-compatible chat-completions routes; other protocols keep their defaults.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sampling_params_by_thinking_level: BTreeMap<String, SamplingParams>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<CompatPatch>,
    #[serde(skip_serializing_if = "CompactionConfig::is_default")]
    pub compaction: CompactionConfig,
}

/// Provider-neutral sampling knobs that can be selected with a thinking level.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SamplingParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
}

impl SamplingParams {
    fn validate(&self) -> Result<(), &'static str> {
        if self
            .temperature
            .is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value))
        {
            return Err("temperature 必须是 0 到 2 之间的有限数字");
        }
        if self
            .top_p
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err("top_p 必须是 0 到 1 之间的有限数字");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CompactionConfig {
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
}

impl CompactionConfig {
    fn is_default(&self) -> bool {
        self.reserve_tokens.is_none() && self.keep_recent_tokens.is_none()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CompactionBudget {
    pub threshold: u64,
    pub keep_recent: u64,
}

/// Every thinking level pi knows about. `off` and `minimal` are deliberately absent:
/// `reasoning = false` already means "no reasoning".
pub const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub fn level_index(level: &str) -> Option<usize> {
    LEVELS.iter().position(|l| *l == level)
}

impl ModelConfig {
    fn is_deepseek(&self) -> bool {
        self.id
            .rsplit('/')
            .next()
            .unwrap_or(&self.id)
            .to_ascii_lowercase()
            .starts_with("deepseek-")
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    pub fn max_tokens(&self) -> u64 {
        self.max_tokens.unwrap_or(8192)
    }

    pub fn compaction_budget(&self) -> Result<CompactionBudget, String> {
        if self.context_window.is_none() && !self.compaction.is_default() {
            return Err("配置 compaction 时必须指定 context_window".into());
        }
        let window = self.context_window.unwrap_or(128_000);
        let reserve = self
            .compaction
            .reserve_tokens
            .unwrap_or(Defaults::RESERVE_TOKENS.min(window / 4));
        // With an unknown window, only manual summaries use this fallback. Their output
        // is capped at 16k; do not reject a model based on a window it never declared.
        let output = if self.context_window.is_some() {
            self.max_tokens()
        } else {
            self.max_tokens().min(16_384)
        };
        let reserved = output
            .checked_add(reserve)
            .filter(|tokens| *tokens < window)
            .ok_or_else(|| {
                "max_tokens 与 reserve_tokens 之和必须小于 context_window".to_string()
            })?;
        let threshold = window - reserved;
        let keep_recent = self.compaction.keep_recent_tokens.unwrap_or(
            Defaults::KEEP_RECENT_TOKENS
                .min(window / 3)
                .min(threshold / 2),
        );
        if keep_recent >= threshold {
            return Err("keep_recent_tokens 必须小于压缩触发阈值".into());
        }
        Ok(CompactionBudget {
            threshold,
            keep_recent,
        })
    }

    /// The levels this model actually supports, defaulting to all of them.
    pub fn levels(&self) -> Vec<String> {
        if !self.reasoning {
            return Vec::new();
        }
        if self.thinking_levels.is_empty() {
            LEVELS.iter().map(|s| s.to_string()).collect()
        } else {
            self.thinking_levels.clone()
        }
    }

    pub fn sampling_params(&self, level: &str) -> Option<&SamplingParams> {
        let level = if level.is_empty() { "off" } else { level };
        self.sampling_params_by_thinking_level.get(level)
    }

    /// Clamp a level into this model's supported set, preferring the nearest one.
    pub fn clamp_level(&self, level: &str) -> String {
        let levels = self.levels();
        if levels.is_empty() {
            return String::new();
        }
        if levels.iter().any(|l| l == level) {
            return level.to_string();
        }
        let target = level_index(level).unwrap_or(0);
        // Walk the supported levels from weakest to strongest and keep the closest one,
        // preferring the stronger level on a tie: the user asked for more thinking than
        // the model's nearest step, so rounding up loses less.
        let mut candidates: Vec<&String> = levels.iter().collect();
        candidates.sort_by_key(|level| level_index(level).unwrap_or(usize::MAX));
        let mut best = candidates[0].clone();
        let mut best_distance = usize::MAX;
        for candidate in candidates {
            let distance = level_index(candidate)
                .map(|i| i.abs_diff(target))
                .unwrap_or(usize::MAX);
            if distance <= best_distance {
                best_distance = distance;
                best = candidate.clone();
            }
        }
        best
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("已写出示例配置：{0}\n编辑它，至少写出一个 provider 及其 models，然后重新运行。")]
    Created(PathBuf),
    #[error("配置文件读取失败：{0}")]
    Read(#[from] std::io::Error),
    #[error("无法写出示例配置 {path}：{source}")]
    Init {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("配置文件解析失败：{0}")]
    Parse(#[from] serde_json::Error),
    #[error("配置缺少 providers：请在 {0} 里至少配置一个 provider")]
    NoProviders(PathBuf),
    #[error(
        "provider「{provider}」的模型「{model}」思考级别「{level}」无效（可用：low、medium、high、xhigh、max）"
    )]
    BadLevel {
        provider: String,
        model: String,
        level: String,
    },
    #[error("provider「{0}」的 api 必须是 messages、completions 或 responses")]
    BadApi(String),
    #[error("{0}")]
    UnknownField(String),
    #[error("default_model「{0}」不是「<provider>/<model>」形式，或指向了未配置的模型")]
    BadDefaultModel(String),
    #[error("provider「{provider}」的模型「{model}」上下文预算无效：{reason}")]
    BadCompaction {
        provider: String,
        model: String,
        reason: String,
    },
    #[error("provider「{provider}」的模型「{model}」思考级别「{level}」的采样参数无效：{reason}")]
    BadSampling {
        provider: String,
        model: String,
        level: String,
        reason: String,
    },
    #[error(
        "provider「{provider}」的模型「{model}」打开了 search，但这个接口没有可用的原生搜索写法。在 compat.search_format 里指定：responses 用 web_search 或 web_and_x，messages 用 anthropic，completions 用 xai、qwen 或 zhipu。DeepSeek 官方接口不能原生搜索，请把 search 设为 false"
    )]
    NoNativeSearch { provider: String, model: String },
}

/// Every field the config understands, per level.
///
/// This exists because serde ignores what it does not recognise: a provider written with
/// `baseUrl` instead of `base_url` parses fine, and the request then goes to the protocol's
/// default host. That is not a cosmetic mistake — it sends the conversation and the API key
/// to a host the user never named. So the raw JSON is checked against these lists before it
/// is trusted, and a misspelling is a start-up error instead.
mod fields {
    pub const ROOT: [&str; 3] = ["shell", "providers", "default_model"];
    pub const SHELL: [&str; 1] = ["path"];
    pub const PROVIDER: [&str; 7] = [
        "name",
        "api",
        "base_url",
        "api_key_env",
        "api_key",
        "compat",
        "models",
    ];
    pub const MODEL: [&str; 10] = [
        "id",
        "name",
        "context_window",
        "max_tokens",
        "reasoning",
        "thinking_levels",
        "search",
        "sampling_params_by_thinking_level",
        "compat",
        "compaction",
    ];
    pub const COMPACTION: [&str; 2] = ["reserve_tokens", "keep_recent_tokens"];
    pub const SAMPLING: [&str; 2] = ["temperature", "top_p"];
    pub const COMPAT: [&str; 13] = [
        "max_tokens_field",
        "supports_developer_role",
        "supports_reasoning_effort",
        "thinking_format",
        "requires_thinking_as_text",
        "requires_reasoning_content_on_assistant",
        "requires_assistant_after_tool_result",
        "supports_usage_in_streaming",
        "supports_strict_mode",
        "supports_cache_control",
        "send_session_affinity",
        "supports_long_cache",
        "search_format",
    ];
}

/// Report the first key in `object` that is not in `known`.
fn unknown_key(
    object: &serde_json::Map<String, serde_json::Value>,
    known: &[&str],
) -> Option<String> {
    object
        .keys()
        .find(|key| !known.contains(&key.as_str()))
        .map(|key| key.to_string())
}

/// Suggest the snake_case spelling of a camelCase key, when that is what it looks like.
fn suggestion(key: &str) -> Option<String> {
    if !key.chars().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let mut out = String::new();
    for (index, c) in key.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn unknown_field_error(location: &str, key: &str) -> ConfigError {
    let hint = match suggestion(key) {
        Some(snake) => format!("是不是想写「{snake}」？"),
        None => "检查拼写。".to_string(),
    };
    ConfigError::UnknownField(format!(
        "{location}里的「{key}」不是配置项，也不会生效（{hint}）\
         配置字段一律 snake_case；写错的键不能静默忽略，程序会在这里停下，\
         避免请求发到别处。"
    ))
}

/// Walk the raw JSON and reject keys the structs would silently drop.
fn check_unknown_fields(raw: &serde_json::Value) -> Result<(), ConfigError> {
    let Some(root) = raw.as_object() else {
        return Ok(());
    };
    if let Some(key) = unknown_key(root, &fields::ROOT) {
        return Err(unknown_field_error("配置", &key));
    }
    if let Some(shell) = root.get("shell").and_then(|value| value.as_object())
        && let Some(key) = unknown_key(shell, &fields::SHELL)
    {
        return Err(unknown_field_error("shell", &key));
    }
    let Some(providers) = root.get("providers").and_then(|value| value.as_array()) else {
        return Ok(());
    };
    for provider in providers {
        let Some(provider) = provider.as_object() else {
            continue;
        };
        let name = provider
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("?");
        if let Some(key) = unknown_key(provider, &fields::PROVIDER) {
            return Err(unknown_field_error(&format!("provider「{name}」"), &key));
        }
        if let Some(compat) = provider.get("compat").and_then(|value| value.as_object())
            && let Some(key) = unknown_key(compat, &fields::COMPAT)
        {
            return Err(unknown_field_error(
                &format!("provider「{name}」的 compat"),
                &key,
            ));
        }
        let Some(models) = provider.get("models").and_then(|value| value.as_array()) else {
            continue;
        };
        for model in models {
            let Some(model) = model.as_object() else {
                continue;
            };
            let id = model
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            if let Some(key) = unknown_key(model, &fields::MODEL) {
                return Err(unknown_field_error(
                    &format!("provider「{name}」的模型「{id}」"),
                    &key,
                ));
            }
            if let Some(compat) = model.get("compat").and_then(|value| value.as_object())
                && let Some(key) = unknown_key(compat, &fields::COMPAT)
            {
                return Err(unknown_field_error(
                    &format!("provider「{name}」的模型「{id}」的 compat"),
                    &key,
                ));
            }
            if let Some(compaction) = model.get("compaction").and_then(|value| value.as_object())
                && let Some(key) = unknown_key(compaction, &fields::COMPACTION)
            {
                return Err(unknown_field_error(
                    &format!("provider「{name}」的模型「{id}」的 compaction"),
                    &key,
                ));
            }
            if let Some(sampling) = model
                .get("sampling_params_by_thinking_level")
                .and_then(|value| value.as_object())
            {
                for (level, params) in sampling {
                    if let Some(params) = params.as_object()
                        && let Some(key) = unknown_key(params, &fields::SAMPLING)
                    {
                        return Err(unknown_field_error(
                            &format!("provider「{name}」的模型「{id}」的采样级别「{level}」"),
                            &key,
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Where pi keeps everything it owns: `~/.pi`.
///
/// One directory rather than splitting config to `~/.config` and data to `~/.local/share`.
/// The file the user has to edit is then next to the sessions it produced, which is what
/// makes it findable — the two are always mentioned together.
///
/// The project is `mpi`, but the store is named after the binary: what the user runs is
/// `pi`, and a directory they have to find by hand is far more likely to be looked for
/// under the name of the command than under the name of the repository.
pub fn home_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(NEW_HOME)
}

/// The directory name the store lives under.
pub const NEW_HOME: &str = ".pi";

pub fn config_path() -> PathBuf {
    home_dir().join("config.json")
}

/// What a fresh install is given: every field is a placeholder to edit.
///
/// The values are deliberately unusable. pi stops after writing it rather than running
/// against it, because a template that silently "works" would send requests to a host that
/// does not exist and report that as a network failure.
pub const TEMPLATE: &str = r#"{
  "shell": { "path": "/usr/bin/zsh" },
  "providers": [
    {
      "name": "provider-name",
      "api": "completions",
      "base_url": "http://127.0.0.1:8000/v1",
      "api_key_env": "PROVIDER_NAME_API_KEY",
      "models": [
        {
          "id": "model-id",
          "name": "model-id",
          "context_window": 200000,
          "max_tokens": 32000,
          "reasoning": true,
          "search": false,
          "thinking_levels": ["low", "medium", "high", "xhigh", "max"]
        }
      ]
    }
  ],
  "default_model": "provider-name/model-id"
}"#;

/// Write the template, creating `~/.pi` if needed.
///
/// The file is created exclusively: two pi processes starting at once must not have the
/// second one overwrite the first one's edits. Losing that race is reported as a plain IO
/// error, because from the caller's side the file it wanted to create is there now.
fn init_config(path: &Path) -> Result<(), ConfigError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| ConfigError::Init {
            path: path.to_path_buf(),
            source,
        })?;
    }
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        // Another process wrote it between the `exists` check and here: use that one.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(source) => {
            return Err(ConfigError::Init {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    use std::io::Write as _;
    file.write_all(TEMPLATE.as_bytes())
        .map_err(|source| ConfigError::Init {
            path: path.to_path_buf(),
            source,
        })
}

impl Config {
    /// Read and validate the config, creating a template first if there is none.
    ///
    /// Writing the file rather than printing a sample to copy is the difference between a
    /// tool that explains itself and one that makes the user transcribe JSON out of an
    /// error message: the path is almost always where they expected, and the file is what
    /// they were going to create anyway.
    pub fn load() -> Result<Config, ConfigError> {
        let path = config_path();
        if !path.exists() {
            init_config(&path)?;
            // Stop here rather than carrying on with the placeholders. The template has to
            // parse to be editable, so running it would send a request to a host that does
            // not exist and report that as a network failure — a confusing way to say
            // "you have not configured this yet".
            return Err(ConfigError::Created(path));
        }
        let raw = std::fs::read_to_string(&path)?;
        let parsed: serde_json::Value = serde_json::from_str(&raw)?;
        // Before the typed parse: a key serde does not know is dropped without a word, and
        // the drop is invisible in the parsed value.
        check_unknown_fields(&parsed)?;
        let mut config: Config = serde_json::from_str(&raw)?;
        config.validate(&path)?;
        config.fill_defaults();
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        if self.providers.is_empty() {
            return Err(ConfigError::NoProviders(path.to_path_buf()));
        }
        for provider in &self.providers {
            if provider.api().is_none() {
                return Err(ConfigError::BadApi(provider.name.clone()));
            }
            if provider.models.is_empty() {
                return Err(ConfigError::NoProviders(path.to_path_buf()));
            }
            for model in &provider.models {
                model
                    .compaction_budget()
                    .map_err(|reason| ConfigError::BadCompaction {
                        provider: provider.name.clone(),
                        model: model.id.clone(),
                        reason,
                    })?;
                for level in &model.thinking_levels {
                    if level_index(level).is_none() {
                        return Err(ConfigError::BadLevel {
                            provider: provider.name.clone(),
                            model: model.id.clone(),
                            level: level.clone(),
                        });
                    }
                }
                let supported_levels = model.levels();
                for (level, sampling) in &model.sampling_params_by_thinking_level {
                    if level != "off"
                        && (level_index(level).is_none()
                            || !supported_levels.iter().any(|supported| supported == level))
                    {
                        return Err(ConfigError::BadSampling {
                            provider: provider.name.clone(),
                            model: model.id.clone(),
                            level: level.clone(),
                            reason: "级别必须是该模型支持的 thinking_levels，或 off".into(),
                        });
                    }
                    if let Err(reason) = sampling.validate() {
                        return Err(ConfigError::BadSampling {
                            provider: provider.name.clone(),
                            model: model.id.clone(),
                            level: level.clone(),
                            reason: reason.into(),
                        });
                    }
                }
                if model.search {
                    let format = provider.compat(model).search_format;
                    let api = provider.api().unwrap_or(crate::llm::Api::OpenAiCompletions);
                    if format.is_none_or(|format| !format.fits(api)) {
                        return Err(ConfigError::NoNativeSearch {
                            provider: provider.name.clone(),
                            model: model.id.clone(),
                        });
                    }
                }
            }
        }
        if let Some(key) = &self.default_model
            && self.find(key).is_none()
        {
            return Err(ConfigError::BadDefaultModel(key.clone()));
        }
        Ok(())
    }

    fn fill_defaults(&mut self) {
        for provider in &mut self.providers {
            if provider.base_url.is_empty() {
                provider.base_url = crate::llm::Api::from_name(&provider.api)
                    .map(crate::llm::Api::default_base_url)
                    .unwrap_or_default()
                    .to_string();
            }
            if provider.api_key_env.is_none() && provider.api_key.is_none() {
                provider.api_key_env = Some(format!(
                    "{}_API_KEY",
                    provider.name.to_uppercase().replace(['-', '.'], "_")
                ));
            }
            for model in &mut provider.models {
                if model.name.is_none() {
                    model.name = Some(model.id.clone());
                }
            }
        }
        if self.default_model.is_none()
            && let Some(first) = self.providers.first()
            && let Some(model) = first.models.first()
        {
            self.default_model = Some(format!("{}/{}", first.name, model.id));
        }
        // `shell.path` may be left empty in the file; restore the default.
        if self.shell.path.is_empty() {
            self.shell.path = DEFAULT_SHELL.into();
        }
    }

    /// Look up `provider/model`, or a bare model id if it is unambiguous.
    pub fn find(&self, spec: &str) -> Option<(&Provider, &ModelConfig)> {
        let (provider_name, model_id) = match spec.split_once('/') {
            Some((p, m)) => (Some(p), m),
            None => (None, spec),
        };
        let mut found = None;
        for provider in &self.providers {
            if let Some(wanted) = provider_name
                && provider.name != wanted
            {
                continue;
            }
            for model in &provider.models {
                if model.id == model_id {
                    if found.is_some() {
                        // Ambiguous bare id: refuse rather than guess.
                        return None;
                    }
                    found = Some((provider, model));
                }
            }
        }
        found
    }

    /// Provider names to model specs, in configuration order. This list *is* the
    /// `/model` menu, so order matters and nothing is added or hidden.
    pub fn catalogue(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in &self.providers {
            for model in &provider.models {
                out.push((
                    provider.name.clone(),
                    format!("{}/{}", provider.name, model.id),
                ));
            }
        }
        out
    }
}

impl Provider {
    /// The protocol this provider speaks, or `None` when the config names one that does not
    /// exist.
    ///
    /// The one place that turns the config's `api` string into a protocol: start-up
    /// validation, the request shape and the compatibility defaults all ask here, so they
    /// cannot disagree about what a provider is.
    pub fn api(&self) -> Option<crate::llm::Api> {
        crate::llm::Api::from_name(&self.api)
    }

    pub fn compat(&self, model: &ModelConfig) -> Compat {
        // An unrecognised api has already been rejected at start-up; falling back to the
        // completions shape here keeps this total without inventing a protocol.
        let api = self.api().unwrap_or(crate::llm::Api::OpenAiCompletions);
        let mut compat = Compat::from_base_url(&self.base_url, api);
        // Gateways host several model families on one URL. DeepSeek's reasoning protocol
        // belongs to the model, so a neutral gateway must not drop its reasoning history.
        if api == crate::llm::Api::OpenAiCompletions && model.is_deepseek() {
            compat.thinking_format = crate::llm::compat::ThinkingFormat::Deepseek;
            compat.requires_reasoning_content_on_assistant = true;
        }
        if let Some(overrides) = &self.compat {
            compat.apply(overrides);
        }
        if let Some(overrides) = &model.compat {
            compat.apply(overrides);
        }
        compat
    }

    pub fn api_key(&self) -> Option<String> {
        if let Some(env) = &self.api_key_env
            && let Ok(value) = std::env::var(env)
            && !value.trim().is_empty()
        {
            return Some(value);
        }
        self.api_key.clone()
    }
}

/// A session-level tally for the footer's `↑` / `↓` fields.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn is_meaningful(&self) -> bool {
        self.input > 0 || self.output > 0 || self.cache_read > 0 || self.cache_write > 0
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    /// Cache hit rate over the prompt tokens of the most recent request.
    pub fn hit_rate(&self) -> Option<f64> {
        let prompt = self.input + self.cache_read + self.cache_write;
        (prompt > 0).then(|| self.cache_read as f64 / prompt as f64 * 100.0)
    }
}

/// The seven-section summary skeleton, reused verbatim by every compaction path.
pub const SUMMARY_SECTIONS: &str = "## Goal
[用户想完成什么？一个会话覆盖多个任务时都可以列出]

## Constraints & Preferences
- [用户提出的约束、偏好或要求]
- [没有就写 (none)]

## Progress
### Done
- [x] [已完成的任务或改动]

### In Progress
- [ ] [正在进行的工作]

### Blocked
- [阻塞项，没有就省略]

## Key Decisions
- **[决策]**：[简要理由]

## Next Steps
1. [接下来应该按顺序做的事]

## Critical Context
- [继续工作所需的数据、示例或引用]
- [没有就写 (none)]

每节保持简短。必须原样保留文件路径、函数名与报错信息。";

/// Shared defaults; model compaction budgets may override the token values.
pub struct Defaults;

impl Defaults {
    pub const COMMAND_PREVIEW_LINES: usize = 5;
    pub const DIFF_PREVIEW_LINES: usize = 14;
    pub const THINKING_PREVIEW_LINES: usize = 2;
    pub const AUTH_PANEL_MAX_HEIGHT: u16 = 22;
    pub const RESERVE_TOKENS: u64 = 16_384;
    pub const KEEP_RECENT_TOKENS: u64 = 20_000;
    pub const SESSION_NAME_WIDTH: usize = 40;
    pub const SESSIONS_DIR: &'static str = "sessions";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pi_path_lives_under_one_directory() {
        // One directory for everything pi owns, so the file to edit sits next to the sessions
        // it produced. The session half of this lives with the store, in `session::dirs`.
        let home = home_dir();
        assert!(home.ends_with(".pi"), "{}", home.display());
        assert_eq!(config_path(), home.join("config.json"));
    }

    #[test]
    fn model_budgets_reserve_output_and_scale_recent_context() {
        let model = ModelConfig {
            context_window: Some(6000),
            max_tokens: Some(1000),
            ..Default::default()
        };
        let budget = model.compaction_budget().unwrap();
        assert_eq!(budget.threshold, 3500);
        assert_eq!(budget.keep_recent, 1750);
        let large = ModelConfig {
            context_window: Some(1_000_000),
            max_tokens: Some(64_000),
            ..Default::default()
        };
        let budget = large.compaction_budget().unwrap();
        assert_eq!(budget.threshold, 919_616);
        assert_eq!(budget.keep_recent, 20_000);
        let tuned = ModelConfig {
            compaction: CompactionConfig {
                reserve_tokens: Some(512),
                keep_recent_tokens: Some(1000),
            },
            ..model.clone()
        };
        assert_eq!(tuned.compaction_budget().unwrap().threshold, 4488);
        assert_eq!(tuned.compaction_budget().unwrap().keep_recent, 1000);
        for compaction in [
            CompactionConfig {
                reserve_tokens: Some(5000),
                keep_recent_tokens: None,
            },
            CompactionConfig {
                reserve_tokens: Some(u64::MAX),
                keep_recent_tokens: None,
            },
            CompactionConfig {
                reserve_tokens: None,
                keep_recent_tokens: Some(3500),
            },
        ] {
            assert!(
                ModelConfig {
                    compaction,
                    ..model.clone()
                }
                .compaction_budget()
                .is_err()
            );
        }
        assert!(
            ModelConfig {
                context_window: None,
                ..tuned
            }
            .compaction_budget()
            .is_err()
        );
        let unknown = ModelConfig {
            max_tokens: Some(200_000),
            ..Default::default()
        };
        assert_eq!(unknown.compaction_budget().unwrap().keep_recent, 20_000);
        let raw = serde_json::json!({"providers":[{"name":"p","models":[{"id":"m","compaction":{"keepRecentTokens":100}}]}]});
        assert!(check_unknown_fields(&raw).is_err());
    }

    #[test]
    fn gateway_models_select_their_own_reasoning_protocol_and_keep_overrides() {
        use crate::llm::compat::ThinkingFormat;
        let provider: Provider = serde_json::from_value(serde_json::json!({
            "name":"gateway", "api":"completions", "base_url":"https://gateway.example/v1"
        }))
        .unwrap();
        for id in ["deepseek-v4.1-flash", "deepseek-ai/DeepSeek-V4.1-Flash"] {
            let model = ModelConfig {
                id: id.into(),
                ..Default::default()
            };
            let compat = provider.compat(&model);
            assert_eq!(compat.thinking_format, ThinkingFormat::Deepseek);
            assert!(compat.requires_reasoning_content_on_assistant);
        }
        let mut model = ModelConfig {
            id: "deepseek-v4.1-flash".into(),
            ..Default::default()
        };
        model.compat = Some(
            serde_json::from_value(serde_json::json!({
                "thinking_format":"none", "requires_reasoning_content_on_assistant":false
            }))
            .unwrap(),
        );
        let compat = provider.compat(&model);
        assert_eq!(compat.thinking_format, ThinkingFormat::None);
        assert!(!compat.requires_reasoning_content_on_assistant);
        assert_eq!(
            provider
                .compat(&ModelConfig {
                    id: "another-model".into(),
                    ..Default::default()
                })
                .thinking_format,
            ThinkingFormat::Openai
        );
        let provider = Provider {
            api: "responses".into(),
            ..provider
        };
        assert!(
            !provider
                .compat(&ModelConfig {
                    id: "deepseek-v4.1-flash".into(),
                    ..Default::default()
                })
                .requires_reasoning_content_on_assistant
        );
    }

    #[test]
    fn sampling_params_follow_the_effective_level_and_validate_ranges() {
        let raw = serde_json::json!({
            "providers": [{"name":"p", "api":"completions", "models": [{
                "id":"m",
                "sampling_params_by_thinking_level": {
                    "off": {"temperature": 0.2},
                    "high": {"temperature": 0.7, "top_p": 0.9}
                }
            }]}]
        });
        let config: Config = serde_json::from_value(raw).unwrap();
        let model = &config.providers[0].models[0];
        assert_eq!(model.sampling_params("").unwrap().temperature, Some(0.2));
        assert_eq!(model.sampling_params("high").unwrap().top_p, Some(0.9));

        let invalid = serde_json::json!({
            "providers": [{"name":"p", "api":"completions", "models": [{
                "id":"m", "sampling_params_by_thinking_level": {"high": {"top_p": 2.0}}
            }]}]
        });
        let config: Config = serde_json::from_value(invalid).unwrap();
        assert!(config.validate(Path::new("config.json")).is_err());
    }

    fn sample() -> Config {
        let raw = r#"{
          "providers": [
            {"name":"work","api":"completions","base_url":"http://x/v1",
             "models":[{"id":"m1","reasoning":true,"thinking_levels":["low","high","max"]},
                       {"id":"m2"}]}
          ]
        }"#;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        cfg.fill_defaults();
        cfg
    }

    #[test]
    fn the_template_parses_and_is_not_usable_as_it_stands() {
        // The template has to parse, or the user opens a file that pi then rejects for a
        // reason unrelated to what they are editing. It must not be *runnable* either: its
        // host and key are placeholders, and a request sent there would fail as a network
        // error rather than as "you have not configured this yet".
        let config: Config = serde_json::from_str(TEMPLATE).expect("the template parses");
        assert_eq!(config.providers.len(), 1);
        let provider = &config.providers[0];
        assert!(provider.base_url.contains("127.0.0.1"), "not a real host");
        assert_eq!(provider.name, "provider-name");
        assert!(
            config
                .default_model
                .as_deref()
                .unwrap_or_default()
                .contains("provider-name"),
            "the default model names the placeholder provider"
        );
    }

    #[test]
    fn defaults_are_filled_in() {
        let cfg = sample();
        assert_eq!(cfg.shell.path, DEFAULT_SHELL);
        assert_eq!(cfg.default_model.as_deref(), Some("work/m1"));
        assert_eq!(cfg.providers[0].models[1].display_name(), "m2");
        assert_eq!(
            cfg.providers[0].api_key_env.as_deref(),
            Some("WORK_API_KEY")
        );
    }

    #[test]
    fn levels_default_to_all_five_and_clamp() {
        let cfg = sample();
        assert_eq!(cfg.providers[0].models[1].levels().len(), 0);
        assert_eq!(cfg.providers[0].models[1].clamp_level("high"), "");
        let m1 = &cfg.providers[0].models[0];
        assert_eq!(m1.levels(), vec!["low", "high", "max"]);
        // `medium` sits exactly between `low` and `high`; a tie rounds up, because the
        // user asked for more thinking than either neighbour provides.
        assert_eq!(m1.clamp_level("medium"), "high");
        assert_eq!(m1.clamp_level("xhigh"), "max");
        assert_eq!(m1.clamp_level("high"), "high");
    }

    #[test]
    fn unknown_level_is_rejected() {
        let raw = r#"{"providers":[{"name":"p","api":"completions","models":[{"id":"m","thinking_levels":["off"]}]}]}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            cfg.validate(&PathBuf::from("x")),
            Err(ConfigError::BadLevel { .. })
        ));
    }

    #[test]
    fn the_third_protocol_is_accepted_and_gets_its_own_default_host() {
        let raw = r#"{"providers":[{"name":"p","api":"responses","models":[{"id":"m"}]}]}"#;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        cfg.validate(&PathBuf::from("x")).unwrap();
        cfg.fill_defaults();
        assert_eq!(cfg.providers[0].base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn an_api_that_is_not_a_protocol_is_rejected() {
        let raw = r#"{"providers":[{"name":"p","api":"openai-respones","models":[{"id":"m"}]}]}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        assert!(matches!(
            cfg.validate(&PathBuf::from("x")),
            Err(ConfigError::BadApi(_))
        ));
    }

    #[test]
    fn the_long_protocol_names_are_gone_not_aliased() {
        // The config names a protocol with one word. The names these replaced are not
        // accepted as a second spelling: two ways to write the same thing means the next
        // reader has to know both, and the error says the same thing the docs do.
        for raw in [
            r#"{"providers":[{"name":"p","api":"openai-completions","models":[{"id":"m"}]}]}"#,
            r#"{"providers":[{"name":"p","api":"anthropic-messages","models":[{"id":"m"}]}]}"#,
            r#"{"providers":[{"name":"p","api":"openai-responses","models":[{"id":"m"}]}]}"#,
        ] {
            let cfg: Config = serde_json::from_str(raw).unwrap();
            let err = cfg.validate(&PathBuf::from("x")).unwrap_err().to_string();
            assert!(err.contains("messages、completions 或 responses"), "{err}");
        }
    }

    #[test]
    fn a_misspelled_key_is_an_error_not_a_silent_default() {
        // The bug this guards: `baseUrl` parses, is dropped, and the request goes to the
        // protocol's default host with the user's key attached — which is how a working
        // gateway turns into a 403 from a company the user never mentioned.
        let raw = serde_json::json!({
            "providers": [{
                "name": "grok",
                "api": "completions",
                "baseUrl": "https://api.example.org/v1",
                "models": [{"id": "grok-4.7"}]
            }]
        });
        let err = check_unknown_fields(&raw).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("baseUrl"), "{text}");
        assert!(
            text.contains("base_url"),
            "the suggestion must name the right key: {text}"
        );
        assert!(text.contains("grok"), "{text}");
    }

    #[test]
    fn misspellings_are_caught_at_every_level() {
        let cases = [
            // A provider field.
            serde_json::json!({"providers":[{"name":"p","api":"completions",
                "key":"x","models":[{"id":"m"}]}]}),
            // A model field.
            serde_json::json!({"providers":[{"name":"p","api":"completions",
                "models":[{"id":"m","maxTokens":100}]}]}),
            // A compat switch.
            serde_json::json!({"providers":[{"name":"p","api":"completions",
                "compat":{"sendSessionAffinityHeaders":true},"models":[{"id":"m"}]}]}),
            // The shell block.
            serde_json::json!({"shell":{"shell":"/bin/zsh"},"providers":[{"name":"p",
                "api":"completions","models":[{"id":"m"}]}]}),
            // The root itself.
            serde_json::json!({"provider":[],"providers":[{"name":"p",
                "api":"completions","models":[{"id":"m"}]}]}),
        ];
        for case in cases {
            let err =
                check_unknown_fields(&case).expect_err(&format!("this must be rejected: {case}"));
            assert!(
                err.to_string().contains("不是配置项"),
                "the message must say what is wrong: {err}"
            );
        }
    }

    #[test]
    fn a_correct_config_passes_the_field_check() {
        // Every documented field, so the check cannot be passing by rejecting the docs.
        let raw = serde_json::json!({
            "shell": {"path": "/usr/bin/zsh"},
            "providers": [{
                "name": "p",
                "api": "responses",
                "base_url": "https://api.example.org/v1",
                "api_key_env": "P_API_KEY",
                "api_key": "sk-x",
                "compat": {
                    "max_tokens_field": "max_completion_tokens",
                    "supports_developer_role": false,
                    "supports_reasoning_effort": true,
                    "thinking_format": "none",
                    "requires_thinking_as_text": false,
                    "requires_reasoning_content_on_assistant": false,
                    "requires_assistant_after_tool_result": false,
                    "supports_usage_in_streaming": true,
                    "supports_strict_mode": false,
                    "supports_cache_control": false,
                    "send_session_affinity": true,
                    "supports_long_cache": false
                },
                "models": [{
                    "id": "m",
                    "name": "m",
                    "context_window": 400000,
                    "max_tokens": 32000,
                    "reasoning": true,
                    "thinking_levels": ["low", "high"],
                    "compat": {"supports_strict_mode": true}
                }]
            }],
            "default_model": "p/m"
        });
        check_unknown_fields(&raw).expect("the documented shape must be accepted");
    }

    #[test]
    fn a_key_with_no_obvious_snake_case_form_is_still_reported() {
        let raw = serde_json::json!({"providers":[{"name":"p","api":"completions",
            "models":[{"id":"m","contextwindo":1}]}]});
        let text = check_unknown_fields(&raw).unwrap_err().to_string();
        assert!(text.contains("contextwindo"), "{text}");
        // Nothing to suggest, so the message must not invent one.
        assert!(!text.contains("是不是想写"), "{text}");
    }

    #[test]
    fn empty_providers_is_an_error_not_a_builtin_list() {
        let cfg = Config::default();
        assert!(matches!(
            cfg.validate(&PathBuf::from("x")),
            Err(ConfigError::NoProviders(_))
        ));
    }

    #[test]
    fn catalogue_lists_exactly_what_is_configured() {
        let cfg = sample();
        assert_eq!(
            cfg.catalogue(),
            vec![
                ("work".to_string(), "work/m1".to_string()),
                ("work".to_string(), "work/m2".to_string())
            ]
        );
    }
}
