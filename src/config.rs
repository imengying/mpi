//! Configuration: a single JSON file read once at start-up.
//!
//! Everything except `providers` is optional and falls back to a default. There is no
//! runtime settings menu and no reload: what the user writes here *is* the model list.

use std::collections::BTreeMap;
use std::path::PathBuf;

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
        ShellConfig { path: DEFAULT_SHELL.into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Provider {
    pub name: String,
    /// `anthropic-messages` or `openai-completions`.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<CompatPatch>,
}

/// Every thinking level mpi knows about. `off` and `minimal` are deliberately absent:
/// `reasoning = false` already means "no reasoning".
pub const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub fn level_index(level: &str) -> Option<usize> {
    LEVELS.iter().position(|l| *l == level)
}

impl ModelConfig {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    pub fn max_tokens(&self) -> u64 {
        self.max_tokens.unwrap_or(8192)
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
            let distance = level_index(candidate).map(|i| i.abs_diff(target)).unwrap_or(usize::MAX);
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
    #[error("配置文件不存在：{0}\n请创建它，至少写出一个 provider 及其 models。")]
    Missing(PathBuf),
    #[error("配置文件读取失败：{0}")]
    Read(#[from] std::io::Error),
    #[error("配置文件解析失败：{0}")]
    Parse(#[from] serde_json::Error),
    #[error("配置缺少 providers：请在 {0} 里至少配置一个 provider")]
    NoProviders(PathBuf),
    #[error("provider「{provider}」的模型「{model}」思考级别「{level}」无效（可用：low、medium、high、xhigh、max）")]
    BadLevel { provider: String, model: String, level: String },
    #[error("provider「{0}」的 api 必须是 anthropic-messages 或 openai-completions")]
    BadApi(String),
    #[error("default_model「{0}」不是「<provider>/<model>」形式，或指向了未配置的模型")]
    BadDefaultModel(String),
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mpi")
        .join("config.json")
}

impl Config {
    /// Read and validate the config. Missing/invalid input is a hard error: mpi never
    /// falls back to a built-in model catalogue.
    pub fn load() -> Result<Config, ConfigError> {
        let path = config_path();
        if !path.exists() {
            return Err(ConfigError::Missing(path));
        }
        let raw = std::fs::read_to_string(&path)?;
        let mut config: Config = serde_json::from_str(&raw)?;
        config.validate(&path)?;
        config.fill_defaults();
        Ok(config)
    }

    fn validate(&self, path: &PathBuf) -> Result<(), ConfigError> {
        if self.providers.is_empty() {
            return Err(ConfigError::NoProviders(path.clone()));
        }
        for provider in &self.providers {
            if provider.api != "anthropic-messages" && provider.api != "openai-completions" {
                return Err(ConfigError::BadApi(provider.name.clone()));
            }
            if provider.models.is_empty() {
                return Err(ConfigError::NoProviders(path.clone()));
            }
            for model in &provider.models {
                for level in &model.thinking_levels {
                    if level_index(level).is_none() {
                        return Err(ConfigError::BadLevel {
                            provider: provider.name.clone(),
                            model: model.id.clone(),
                            level: level.clone(),
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
                provider.base_url = match provider.api.as_str() {
                    "anthropic-messages" => "https://api.anthropic.com".into(),
                    _ => "https://api.openai.com/v1".into(),
                };
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

    pub fn spec_of(&self, provider: &Provider, model: &ModelConfig) -> String {
        format!("{}/{}", provider.name, model.id)
    }

    /// Provider names to model specs, in configuration order. This list *is* the
    /// `/model` menu, so order matters and nothing is added or hidden.
    pub fn catalogue(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for provider in &self.providers {
            for model in &provider.models {
                out.push((provider.name.clone(), format!("{}/{}", provider.name, model.id)));
            }
        }
        out
    }
}

impl Provider {
    pub fn compat(&self, model: &ModelConfig) -> Compat {
        let mut compat = Compat::from_base_url(&self.base_url, &self.api);
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

/// Defaults written down in one place. None of these is configurable.
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

pub fn sessions_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mpi")
        .join(Defaults::SESSIONS_DIR)
}

/// Environment-variable names that must never be read. Kept for completeness: mpi
/// never reads process env values into the transcript on its own.
pub fn is_secret_env(name: &str) -> bool {
    let upper = name.to_uppercase();
    upper.contains("KEY") || upper.contains("TOKEN") || upper.contains("SECRET") || upper.contains("PASSWORD")
}

/// Placeholder so `BTreeMap` stays in use for deterministic JSON output elsewhere.
pub type OrderedMap = BTreeMap<String, serde_json::Value>;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        let raw = r#"{
          "providers": [
            {"name":"work","api":"openai-completions","base_url":"http://x/v1",
             "models":[{"id":"m1","reasoning":true,"thinking_levels":["low","high","max"]},
                       {"id":"m2"}]}
          ]
        }"#;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        cfg.fill_defaults();
        cfg
    }

    #[test]
    fn defaults_are_filled_in() {
        let cfg = sample();
        assert_eq!(cfg.shell.path, DEFAULT_SHELL);
        assert_eq!(cfg.default_model.as_deref(), Some("work/m1"));
        assert_eq!(cfg.providers[0].models[1].display_name(), "m2");
        assert_eq!(cfg.providers[0].api_key_env.as_deref(), Some("WORK_API_KEY"));
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
        let raw = r#"{"providers":[{"name":"p","api":"openai-completions","models":[{"id":"m","thinking_levels":["off"]}]}]}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        assert!(matches!(cfg.validate(&PathBuf::from("x")), Err(ConfigError::BadLevel { .. })));
    }

    #[test]
    fn empty_providers_is_an_error_not_a_builtin_list() {
        let cfg = Config::default();
        assert!(matches!(cfg.validate(&PathBuf::from("x")), Err(ConfigError::NoProviders(_))));
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

    #[test]
    fn token_formatting_is_shared_with_the_footer() {
        assert_eq!(crate::util::fmt_tokens(17_300, true), "17.3k");
    }
}
