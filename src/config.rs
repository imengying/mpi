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
    #[error("已写出示例配置：{0}\n编辑它，至少写出一个 provider 及其 models，然后重新运行。")]
    Created(PathBuf),
    #[error("配置文件读取失败：{0}")]
    Read(#[from] std::io::Error),
    #[error("无法写出示例配置 {path}：{source}")]
    Init { path: PathBuf, source: std::io::Error },
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

/// Where mpi keeps everything it owns: `~/.mpi`.
///
/// One directory rather than splitting config to `~/.config` and data to `~/.local/share`.
/// The file the user has to edit is then next to the sessions it produced, which is what
/// makes it findable — the two are always mentioned together.
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".mpi")
}

pub fn config_path() -> PathBuf {
    home_dir().join("config.json")
}

/// What a fresh install is given: every field is a placeholder to edit.
///
/// The values are deliberately unusable. mpi stops after writing it rather than running
/// against it, because a template that silently "works" would send requests to a host that
/// does not exist and report that as a network failure.
pub const TEMPLATE: &str = r#"{
  "shell": { "path": "/usr/bin/zsh" },
  "providers": [
    {
      "name": "provider-name",
      "api": "openai-completions",
      "base_url": "http://127.0.0.1:8000/v1",
      "api_key_env": "PROVIDER_NAME_API_KEY",
      "models": [
        {
          "id": "model-id",
          "name": "model-id",
          "context_window": 200000,
          "max_tokens": 32000,
          "reasoning": true,
          "thinking_levels": ["low", "medium", "high", "xhigh", "max"]
        }
      ]
    }
  ],
  "default_model": "provider-name/model-id"
}"#;

/// Write the template, creating `~/.mpi` if needed.
///
/// The file is created exclusively: two mpi processes starting at once must not have the
/// second one overwrite the first one's edits. Losing that race is reported as a plain IO
/// error, because from the caller's side the file it wanted to create is there now.
fn init_config(path: &Path) -> Result<(), ConfigError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|source| ConfigError::Init { path: path.to_path_buf(), source })?;
    }
    let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        // Another process wrote it between the `exists` check and here: use that one.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(source) => return Err(ConfigError::Init { path: path.to_path_buf(), source }),
    };
    use std::io::Write as _;
    file.write_all(TEMPLATE.as_bytes())
        .map_err(|source| ConfigError::Init { path: path.to_path_buf(), source })
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

/// The root of the session store: `~/.mpi/sessions`.
pub fn sessions_root() -> PathBuf {
    home_dir().join(Defaults::SESSIONS_DIR)
}

/// The file mapping a short directory id back to the path it stands for.
pub fn dirs_index_path() -> PathBuf {
    dirs_index_path_in(&sessions_root())
}

/// [`dirs_index_path`] under a given store, so tests do not touch the real table.
fn dirs_index_path_in(root: &Path) -> PathBuf {
    root.join("dirs.json")
}

/// The directory holding the sessions of one working directory.
///
/// Sessions are grouped by where the work happened: a session is about one project, and
/// listing every conversation the user ever had — in every other directory — buries the
/// ones that belong to the project in front of them. `/resume` therefore shows the current
/// directory's sessions and nothing else.
///
/// The directory is named by a short id rather than by the path, and the mapping lives in
/// `dirs.json`. Encoding the path into the name produced names like
/// `--home-user-文档-mpi--`: long enough to wrap in a listing, and still not the path it
/// stands for. An id is short and sortable, and the table is the one place that knows where
/// a session came from — which is also what makes a *rename* of the project a non-event.
pub fn sessions_dir(cwd: &Path) -> PathBuf {
    sessions_dir_in(&sessions_root(), cwd)
}

/// [`sessions_dir`] under a given store, so tests do not touch the real one.
pub fn sessions_dir_in(root: &Path, cwd: &Path) -> PathBuf {
    root.join(match dir_id_in(root, cwd) {
        Some(id) => id,
        // Nothing recorded: the answer is an id that names no directory, which is exactly
        // right — there is nothing to list. It is *not* written here, because merely asking
        // where a directory's sessions would live must not add it to the table.
        None => provisional_dir_id(),
    })
}

/// An id for a directory that has no sessions yet.
///
/// Never written: the table is a list of directories that *have* sessions, and registering
/// every directory mpi is merely run in would make it a log of where the user has been.
fn provisional_dir_id() -> String {
    short_id(&uuid::Uuid::now_v7().simple().to_string())
}

/// Register `cwd` and return the id its sessions go under, minting one if it is new.
///
/// Called when a session is created, so the table only ever gains directories that produced
/// something.
pub fn register_dir(cwd: &Path) -> String {
    register_dir_in(&sessions_root(), cwd)
}

/// [`register_dir`] under a given store, so tests do not touch the real one.
pub fn register_dir_in(root: &Path, cwd: &Path) -> String {
    if let Some(id) = dir_id_in(root, cwd) {
        return id;
    }
    // Not registered yet: take the lock, re-read (another process may have just added it),
    // and write only if it is still missing.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("dirs.lock"))
        .ok()
        .and_then(|handle| {
            // A failure to lock is not fatal: the write below is atomic, so the worst case is
            // a lost update, which the re-read on the next start repairs.
            handle.lock().ok().map(|()| handle)
        });
    let mut index = read_dirs_index(root);
    if let Some(id) = lookup_dir(&index, cwd) {
        return id;
    }
    let id = fresh_dir_id(&index);
    index.insert(id.clone(), cwd.to_string_lossy().to_string());
    let _ = write_dirs_index(root, &index);
    drop(lock);
    id
}

/// The id recorded for `cwd`, if this directory has sessions.
pub fn dir_id_in(root: &Path, cwd: &Path) -> Option<String> {
    lookup_dir(&read_dirs_index(root), cwd)
}

/// The id recorded for `cwd`, if any. Matching is on the absolute path, so a symlinked
/// route to the same directory is a different project — resolving symlinks here would make
/// the same directory reachable under names that disagree with what the user typed.
fn lookup_dir(index: &BTreeMap<String, String>, cwd: &Path) -> Option<String> {
    let wanted = cwd.to_string_lossy();
    index
        .iter()
        .find(|(_, path)| path.as_str() == wanted)
        .map(|(id, _)| id.clone())
}

/// A short id not already in `index`.
///
/// A uuid rather than a counter: it costs no extra dependency (session ids already use one),
/// and an id that is never reused matters more than being compact — an id dropped from the
/// table must not be handed to a different project later, which would silently point old
/// sessions at a new directory.
///
/// The **random tail** is what gets shortened, not the head. A v7 uuid leads with a
/// millisecond timestamp, so its first characters are identical for everything created
/// within the same ~65-second window; taking those would collide on every directory made in
/// a burst, and the retry loop below would never find a free one.
fn fresh_dir_id(index: &BTreeMap<String, String>) -> String {
    loop {
        let short = short_id(&uuid::Uuid::now_v7().simple().to_string());
        if !index.contains_key(&short) {
            return short;
        }
    }
}

/// The last 8 hex characters of an id: the random tail, never the timestamp head.
fn short_id(full: &str) -> String {
    full[full.len() - 8..].to_string()
}

fn read_dirs_index(root: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(dirs_index_path_in(root))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Write the table through a temporary file and a rename.
///
/// `rename` replaces the target in one step, so a reader never sees a half-written table
/// and a crash mid-write cannot destroy the mapping for every directory at once.
fn write_dirs_index(root: &Path, index: &BTreeMap<String, String>) -> std::io::Result<()> {
    let path = dirs_index_path_in(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut text = serde_json::to_string_pretty(index)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    text.push('\n');
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &path)
}

/// Drop `cwd`'s entry if its directory holds no sessions, and remove the directory.
///
/// Called after a session is deleted. Without it the store keeps a directory for every
/// project that was ever used, and `ls ~/.mpi/sessions` stops being a list of the projects
/// that have history — which is the one thing the short-id layout is for.
///
/// Only an *empty* directory is dropped: the entry names a real store as long as one session
/// remains, and removing it then would orphan that session. The directory is removed before
/// the entry, so a crash between the two leaves an empty directory with no entry — harmless,
/// and the next session there registers a fresh id.
pub fn forget_dir_if_empty(cwd: &Path) -> bool {
    forget_dir_if_empty_in(&sessions_root(), cwd)
}

/// [`forget_dir_if_empty`] under a given store, so tests do not touch the real one.
pub fn forget_dir_if_empty_in(root: &Path, cwd: &Path) -> bool {
    let Some(id) = dir_id_in(root, cwd) else {
        return false;
    };
    let dir = root.join(&id);
    // A directory that cannot be read is treated as non-empty: forgetting an entry whose
    // sessions might still be there would make them unreachable, which is worse than a
    // stale row in a list nobody reads by hand.
    let Ok(mut entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    if entries.next().is_some() {
        return false;
    }
    let _ = std::fs::remove_dir(&dir);
    remove_dir_from_index(root, &id)
}

/// Remove one entry, under the same lock and atomic rewrite as registration.
fn remove_dir_from_index(root: &Path, id: &str) -> bool {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("dirs.lock"))
        .ok()
        .and_then(|handle| handle.lock().ok().map(|()| handle));
    let mut index = read_dirs_index(root);
    let removed = index.remove(id).is_some();
    if removed {
        let _ = write_dirs_index(root, &index);
    }
    drop(lock);
    removed
}

/// The table under a given store, for tests that assert on what was recorded.
pub fn read_dirs_index_for_test(root: &Path) -> BTreeMap<String, String> {
    read_dirs_index(root)
}

/// Every directory that has sessions, newest id first, as `(id, path)`.
///
/// Kept for diagnostics: nothing in the turn loop needs it, but a store whose table is
/// wrong is otherwise impossible to inspect.
pub fn known_dirs() -> Vec<(String, String)> {
    read_dirs_index(&sessions_root()).into_iter().collect()
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
    fn the_template_parses_and_is_not_usable_as_it_stands() {
        // The template has to parse, or the user opens a file that mpi then rejects for a
        // reason unrelated to what they are editing. It must not be *runnable* either: its
        // host and key are placeholders, and a request sent there would fail as a network
        // error rather than as "you have not configured this yet".
        let config: Config = serde_json::from_str(TEMPLATE).expect("the template parses");
        assert_eq!(config.providers.len(), 1);
        let provider = &config.providers[0];
        assert!(provider.base_url.contains("127.0.0.1"), "not a real host");
        assert_eq!(provider.name, "provider-name");
        assert!(
            config.default_model.as_deref().unwrap_or_default().contains("provider-name"),
            "the default model names the placeholder provider"
        );
    }

    #[test]
    fn a_burst_of_new_directories_still_gets_distinct_ids() {
        // The id is shortened from a v7 uuid, whose first characters are a timestamp shared
        // by everything created in the same window. Shortening the *head* would make every
        // directory registered in one burst collide, and the retry loop would spin forever
        // looking for a free id. A handful of registrations in a row has to stay distinct.
        let ids: std::collections::HashSet<String> = (0..50)
            .map(|_| {
                let index = std::collections::BTreeMap::new();
                fresh_dir_id(&index)
            })
            .collect();
        assert_eq!(ids.len(), 50, "ids collided within one burst");
        for id in &ids {
            assert_eq!(id.len(), 8, "{id} is not a short id");
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "{id} is not hex");
        }
    }

    #[test]
    fn a_directory_leaves_the_table_once_its_last_session_is_gone() {
        // The table is a list of where history *is*. Keeping a row for every project ever
        // used would turn it into a log of where the user has been, which is exactly what
        // the short-id layout exists to avoid.
        let root = std::env::temp_dir().join(format!("mpiforget{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = root.join("store");
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();

        let id = register_dir_in(&store, &project);
        let dir = store.join(&id);
        std::fs::create_dir_all(&dir).unwrap();

        // A session is still there: the entry and the directory both stay.
        std::fs::write(dir.join("one.jsonl"), "{}").unwrap();
        assert!(!forget_dir_if_empty_in(&store, &project));
        assert!(dir.is_dir());
        assert!(dir_id_in(&store, &project).is_some());

        // The last one is gone: both go.
        std::fs::remove_file(dir.join("one.jsonl")).unwrap();
        assert!(forget_dir_if_empty_in(&store, &project));
        assert!(!dir.exists());
        assert!(dir_id_in(&store, &project).is_none(), "the id must not linger");
        // Forgetting twice is not an error, just a no-op.
        assert!(!forget_dir_if_empty_in(&store, &project));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_path_lives_under_the_mpi_directory() {
        // One directory for everything mpi owns, so the file to edit sits next to the
        // sessions it produced. Asserted on the *names* rather than by calling the
        // resolvers: `sessions_dir` registers the directory it is asked about, and a test
        // that calls it writes into the real table — which is how a stray `/tmp/x` entry
        // ended up in a user's store.
        let home = home_dir();
        assert!(home.ends_with(".mpi"), "{}", home.display());
        assert_eq!(config_path(), home.join("config.json"));
        assert_eq!(sessions_root(), home.join("sessions"));
        assert_eq!(dirs_index_path(), home.join("sessions/dirs.json"));

        // Under a store of its own, the grouping still holds.
        let root = std::env::temp_dir().join(format!("mpipaths{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = sessions_dir_in(&root, Path::new("/tmp/somewhere"));
        assert!(dir.starts_with(&root), "{}", dir.display());
        assert_eq!(dir.parent(), Some(root.as_path()));
        let _ = std::fs::remove_dir_all(root);
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
