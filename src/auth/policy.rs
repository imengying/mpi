//! The authorization policy.
//!
//! Two modes of thinking live in this file:
//!
//! 1. **Parsing.** [`parse_literal_commands`] recognises a small literal-shell subset.
//!    Anything it does not understand — expansions, redirects, background jobs, globs,
//!    control characters — is refused, not guessed at. A command pi cannot read is a
//!    command it asks about.
//! 2. **Vetting.** Every word of every segment is resolved: to a trusted absolute
//!    executable, to a concrete path, or to a refusal. Path arguments are checked
//!    including the values hiding inside options (`--file=X`, `-fX`, `-nfX`).
//!
//! The shell dialect matters: zsh expands `=cmd` and `~+` where bash leaves them
//! literal, so a word whose meaning differs from its text is sent to the user rather
//! than rewritten.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::config::DEFAULT_SHELL;

/// Ask the user before running. `ask` carries the reason handed to the model on refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    Allow { safe_command: Option<String> },
    Ask { reason: String },
}

impl Assessment {
    pub fn ask(reason: impl Into<String>) -> Self {
        Assessment::Ask { reason: reason.into() }
    }

    pub fn allow() -> Self {
        Assessment::Allow { safe_command: None }
    }

    pub fn allows(&self) -> bool {
        matches!(self, Assessment::Allow { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Assessment::Ask { reason } => Some(reason),
            _ => None,
        }
    }
}

/// The refusal text the model receives. Without the reason the model can only guess
/// why it was blocked and re-issues the same call.
pub fn refusal(reason: Option<&str>) -> String {
    match reason {
        Some(reason) => format!("未获得用户授权，操作未执行（{reason}）"),
        None => "未获得用户授权，操作未执行".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Bash,
    Zsh,
}

impl Dialect {
    /// pi always runs zsh; the bash branch exists for tests and future shells.
    pub fn for_shell_path(path: &str) -> Dialect {
        let name = path.rsplit('/').next().unwrap_or(path);
        let name = name.strip_suffix(".exe").unwrap_or(name);
        if name.eq_ignore_ascii_case("zsh") {
            Dialect::Zsh
        } else {
            Dialect::Bash
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Dialect::Zsh => "zsh",
            Dialect::Bash => "bash",
        }
    }
}

/// The dialect pi will actually use, derived from the configured shell.
pub fn configured_dialect(shell_path: &str) -> Dialect {
    Dialect::for_shell_path(if shell_path.is_empty() { DEFAULT_SHELL } else { shell_path })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Read,
    Write,
}

// ---------------------------------------------------------------------------
// Path handling
// ---------------------------------------------------------------------------

/// Resolve a tool-supplied path the way the tools themselves do, including `~`,
/// `@` prefixes and `file:` URLs.
pub fn resolve_tool_path(input: &str, cwd: &Path) -> PathBuf {
    let cleaned: String = input
        .chars()
        .map(|c| match c {
            '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect();
    let mut path = cleaned.as_str();
    if let Some(rest) = path.strip_prefix('@') {
        path = rest;
    }
    let owned;
    if let Some(rest) = path.strip_prefix("file://") {
        owned = rest.to_string();
        path = &owned;
    }
    let expanded = if path == "~" {
        home().to_string_lossy().to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home().join(rest).to_string_lossy().to_string()
    } else {
        path.to_string()
    };
    let candidate = PathBuf::from(expanded);
    if candidate.is_absolute() {
        normalize(&candidate)
    } else {
        normalize(&cwd.join(candidate))
    }
}

/// Lexical normalisation: drop `.`, resolve `..`, keep the path absolute and without
/// a trailing separator. Symlinks are *not* followed here; [`canonical_path`] does that.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

/// Resolve a path through symlinks even when the leaf does not exist yet, so a write
/// through a symlinked directory is judged by where it really lands.
pub fn canonical_path(path: &Path, depth: usize) -> std::io::Result<PathBuf> {
    if depth > 80 {
        return Err(std::io::Error::other("路径或符号链接层级过深"));
    }
    let absolute = if path.is_absolute() { normalize(path) } else { normalize(&std::env::current_dir()?.join(path)) };
    match std::fs::canonicalize(&absolute) {
        Ok(resolved) => Ok(resolved),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if let Ok(meta) = std::fs::symlink_metadata(&absolute)
                && meta.file_type().is_symlink()
                && let Ok(target) = std::fs::read_link(&absolute)
            {
                let next = if target.is_absolute() {
                    target
                } else {
                    absolute.parent().unwrap_or(Path::new("/")).join(target)
                };
                return canonical_path(&next, depth + 1);
            }
            match absolute.parent() {
                Some(parent) if parent != absolute => {
                    Ok(canonical_path(parent, depth + 1)?.join(absolute.file_name().unwrap_or_default()))
                }
                _ => Ok(absolute),
            }
        }
        Err(err) => Err(err),
    }
}

/// The canonical home directory, resolved once: it is hit on nearly every check.
pub fn home() -> PathBuf {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let raw = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        canonical_path(&raw, 0).unwrap_or(raw)
    })
    .clone()
}

fn inside(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn basename(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Directories that are sensitive wherever they appear. A project may legitimately contain
/// a `.docker` or `.azure` directory, but reading one is still worth a question: the names
/// only mean credentials, and a false prompt is cheaper than a leaked key.
const SENSITIVE_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", ".kube", ".docker", ".azure", ".gcloud"];
/// Multi-segment credential locations, relative to the home directory. The agent harnesses
/// are listed too: pi keeps provider keys in `~/.pi/config.json` and its sessions next to
/// them, and codex/Claude/Gemini keep the same kind of live secret. `~/.pi` is this tool's
/// own store — the config holds the provider key in clear text, so a command that reads it
/// is worth a question.
const HOME_PATHS: &[&str] = &[
    ".config/gh", ".config/gcloud", ".config/glab-cli", ".config/hub", ".config/doctl",
    ".pi", ".codex", ".claude", ".gemini", ".continue", ".aider", ".local/share/keyrings",
];
/// Credential-like names, matched on the basename anywhere: a directory-only rule misses
/// `grep -r . .ssh`, which reads private keys without naming one.
const SENSITIVE_NAMES: &[&str] = &[
    ".env", ".netrc", ".git-credentials", ".npmrc", ".pypirc", ".dockercfg", ".gitconfig",
    ".bash_history", ".zsh_history", ".python_history", ".mysql_history", ".psql_history",
    ".wgetrc", ".curlrc", ".pgpass", ".authinfo", ".s3cfg", ".terraformrc", ".my.cnf",
    ".mylogin.cnf", ".kubeconfig", ".credentials.json", ".envrc", ".htpasswd",
    "application_default_credentials.json", "hosts.yml",
];
/// Private-key containers: the extension alone is enough to ask before touching.
const SENSITIVE_SUFFIXES: &[&str] = &["pem", "key", "pfx", "p12", "jks", "keystore", "ppk", "kdbx", "ovpn"];

fn basename_is_sensitive(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if SENSITIVE_NAMES.iter().any(|n| *n == lower) {
        return true;
    }
    // `.env` itself and the `.env.local` family.
    if lower.starts_with(".env.") {
        return true;
    }
    // SSH keys, whatever the algorithm: `id_rsa`, `id_ed25519`, …
    if lower.starts_with("id_") && lower.len() > 3 {
        return true;
    }
    // service-account.json, service_account_x.json, serviceaccount.json
    if lower.starts_with("service")
        && lower.contains("account")
        && lower.ends_with(".json")
    {
        return true;
    }
    false
}

/// Secret-bearing basenames that are only meaningful outside a project checkout: a
/// repository may legitimately contain a fixture called `auth.json`.
fn home_only_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        ".claude.json"
            | ".aider.conf.yml"
            | "auth.json"
            | "oauth_creds.json"
            | "oauth-creds.json"
            | "credentials"
            | "credentials.json"
            | "credentialsdb"
            | "token.json"
            | "login.keyring"
    ) || lower
        .strip_prefix("keyring.")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn extension_is_sensitive(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => SENSITIVE_SUFFIXES.iter().any(|s| s.eq_ignore_ascii_case(ext)),
        None => false,
    }
}

fn sensitive(path: &Path) -> bool {
    let segments: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    if segments.iter().any(|part| SENSITIVE_DIRS.iter().any(|d| d == part)) {
        return true;
    }
    let name = basename(path);
    if basename_is_sensitive(&name) {
        return true;
    }
    if extension_is_sensitive(path) {
        return true;
    }
    let text = path.to_string_lossy();
    if text.ends_with("/shadow") || text.ends_with("/gshadow") {
        return true;
    }
    if text.contains("/proc/self/environ") || text.contains("/proc/self/mem") {
        return true;
    }
    // The remaining rules only apply inside the user's own home directory.
    let home = home();
    if !inside(path, &home) {
        return false;
    }
    if home_only_name(&name) {
        return true;
    }
    let relative = path.strip_prefix(&home).unwrap_or(path);
    let parts: Vec<String> = relative
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    for end in 1..=parts.len() {
        let prefix = parts[..end].join("/");
        if HOME_PATHS.iter().any(|p| *p == prefix) {
            return true;
        }
    }
    false
}

/// Path check for `read`, `write` and `edit`.
pub fn assess_path(operation: Operation, input: &str, cwd: &Path) -> Assessment {
    if input.is_empty() || input.contains('\0') {
        return Assessment::ask("无法确认目标路径");
    }
    let original = resolve_tool_path(input, cwd);
    let target = match canonical_path(&original, 0) {
        Ok(target) => target,
        Err(_) => return Assessment::ask("目标路径无法可靠解析，需要人工确认"),
    };
    if sensitive(&original) || sensitive(&target) {
        return Assessment::ask("目标涉及凭据或敏感配置");
    }
    if operation == Operation::Read {
        return Assessment::allow();
    }
    let root = match canonical_path(cwd, 0) {
        Ok(root) => root,
        Err(_) => return Assessment::ask("无法确认当前工作目录"),
    };
    if !inside(&target, &root) {
        return Assessment::ask("目标位于当前工作目录之外（已解析符号链接）");
    }
    let touches_metadata = |path: &Path| {
        path.components().any(|c| match c {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                matches!(part.as_ref(), ".git" | ".pi" | ".codex" | ".agents")
            }
            _ => false,
        }) || basename(path) == "AGENTS.md"
    };
    if touches_metadata(&original) || touches_metadata(&target) {
        return Assessment::ask("目标涉及 Git 元数据、代理配置或权限规则");
    }
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if meta.nlink() > 1 {
                return Assessment::ask("目标存在硬链接，写入可能影响其他路径");
            }
        }
        #[cfg(not(unix))]
        let _ = meta;
    }
    Assessment::allow()
}

// ---------------------------------------------------------------------------
// Literal shell parsing
// ---------------------------------------------------------------------------

/// One parsed word. zsh decides some expansions from the *source* text rather than the
/// resulting value: a leading `=` or `~` is expanded only when it was not quoted or
/// escaped, so `quoted` records whether a quote or escape produced the first character.
/// `''=ls` stays unquoted because an empty quote does not start the word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    pub value: String,
    pub quoted: bool,
}

/// A command together with the operator that separated it from the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub words: Vec<Word>,
    pub operator: Option<String>,
}

/// Recognise a small literal-shell subset. Everything else returns `Err(reason)` so the
/// caller asks the user. Nothing unrecognised is ever assumed safe.
// The final `flush_segment!` resets the word state one last time; nothing reads it after
// that, which is correct but not something the compiler can see through a macro.
#[allow(unused_assignments)]
pub fn parse_literal_commands(command: &str) -> Result<Vec<Segment>, String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<Segment> = Vec::new();
    let mut words: Vec<Word> = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quoted_start = false;
    let mut quote: Option<char> = None;
    let mut index = 0usize;

    /// Push the pending word and reset the word state. The macro is only ever used
    /// through `flush_segment!`, whose return value is what marks the state as consumed.
    macro_rules! flush_word {
        () => {{
            if started {
                words.push(Word { value: std::mem::take(&mut word), quoted: quoted_start });
            }
            word.clear();
            started = false;
            quoted_start = false;
        }};
    }
    macro_rules! flush_segment {
        ($operator:expr) => {{
            flush_word!();
            if words.is_empty() {
                false
            } else {
                segments.push(Segment { words: std::mem::take(&mut words), operator: $operator });
                true
            }
        }};
    }

    while index < chars.len() {
        let c = chars[index];
        if c == '\0' || (c.is_control() && c != '\n' && c != '\t' && c != '\r') {
            return Err("命令含控制字符".into());
        }
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            } else {
                if word.is_empty() {
                    quoted_start = true;
                }
                word.push(c);
            }
            index += 1;
            continue;
        }
        if c == '$' || c == '`' {
            return Err("命令含变量、替换或动态 shell 表达式".into());
        }
        if quote == Some('"') {
            if c == '"' {
                quote = None;
                index += 1;
                continue;
            }
            if c == '\\' {
                index += 1;
                let Some(next) = chars.get(index).copied() else {
                    return Err("命令转义不完整".into());
                };
                if next != '\n' {
                    if word.is_empty() {
                        quoted_start = true;
                    }
                    if matches!(next, '"' | '\\' | '$' | '`') {
                        word.push(next);
                    } else {
                        word.push('\\');
                        word.push(next);
                    }
                }
                index += 1;
                continue;
            }
            if word.is_empty() {
                quoted_start = true;
            }
            word.push(c);
            index += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                // Opening a quote keeps an empty argument alive. The protection flag is
                // set later, once a character really lands in the word.
                quote = Some(c);
                started = true;
                index += 1;
            }
            '\\' => {
                index += 1;
                let Some(next) = chars.get(index).copied() else {
                    return Err("命令转义不完整".into());
                };
                if next != '\n' {
                    word.push(next);
                    started = true;
                    quoted_start = true;
                }
                index += 1;
            }
            '#' if !started => {
                while index + 1 < chars.len() && chars[index + 1] != '\n' {
                    index += 1;
                }
                index += 1;
            }
            ' ' | '\t' | '\r' => {
                flush_word!();
                index += 1;
            }
            '\n' | ';' | '|' | '&' => {
                let operator = match c {
                    '\n' | ';' => ";",
                    '|' => {
                        if chars.get(index + 1) == Some(&'|') {
                            index += 1;
                            "||"
                        } else {
                            "|"
                        }
                    }
                    // A single `&` backgrounds the command, which pi never auto-approves.
                    _ => {
                        if chars.get(index + 1) == Some(&'&') {
                            index += 1;
                            "&&"
                        } else {
                            return Err("后台命令需要确认".into());
                        }
                    }
                };
                let had = flush_segment!(Some(operator.to_string()));
                if !had && (c != '\n' || segments.is_empty()) {
                    return Err("无法可靠解析复合命令".into());
                }
                index += 1;
            }
            '<' | '>' | '(' | ')' | '{' | '}' | '*' | '?' | '[' | ']' => {
                return Err(if c == '<' || c == '>' {
                    "重定向可能写入文件或执行脚本".into()
                } else {
                    "通配符或复合 shell 语法需要确认".into()
                });
            }
            other => {
                started = true;
                word.push(other);
                index += 1;
            }
        }
    }
    if quote.is_some() {
        return Err("命令引号未闭合".into());
    }
    let had_final = flush_segment!(None);
    if !had_final
        && let Some(last) = segments.last()
        && last.operator.is_some()
    {
        return Err("复合命令不完整".into());
    }
    if let Some(last) = segments.last_mut() {
        last.operator = None;
    }
    Ok(segments)
}

// ---------------------------------------------------------------------------
// Command vetting
// ---------------------------------------------------------------------------

/// Commands whose arguments are data rather than paths, so they are not path-checked.
const DATA_ARG_COMMANDS: &[&str] = &["echo", "printf", "true", "false", "uname", "df"];

/// Commands auto-approved when every argument checks out.
const READ_COMMANDS: &[&str] = &[
    "pwd", "ls", "cat", "head", "tail", "wc", "stat", "readlink", "realpath", "printf",
    "echo", "true", "false", "cut", "tr", "du", "df", "uname", "rg", "grep", "find",
    "sort", "file", "sed",
];

/// Where a trusted executable may live. Anything else (including a `./cat`) is not
/// auto-approved, which is what stops PATH hijacking.
const TRUSTED_DIRS: &[&str] = &["/usr/bin", "/bin"];

/// Trusted absolute path for a whitelisted command, or `None` when it is unavailable.
pub fn trusted_executable(command: &str) -> Option<PathBuf> {
    let name = command.rsplit('/').next().unwrap_or(command);
    let allowed = READ_COMMANDS.contains(&name) || name == "git";
    if !allowed {
        return None;
    }
    let candidates: Vec<PathBuf> = if command.contains('/') {
        vec![PathBuf::from(command)]
    } else {
        TRUSTED_DIRS.iter().map(|dir| Path::new(dir).join(name)).collect()
    };
    for candidate in candidates {
        if !candidate.is_absolute() {
            continue;
        }
        let Ok(resolved) = std::fs::canonicalize(&candidate) else { continue };
        let trusted = resolved
            .parent()
            .map(|parent| TRUSTED_DIRS.iter().any(|dir| parent == Path::new(dir)))
            .unwrap_or(false);
        if !trusted {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let Ok(meta) = std::fs::metadata(&resolved) else { continue };
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Some(resolved);
    }
    None
}

fn command_reason(name: &str) -> String {
    if ["rm", "rmdir", "unlink", "shred", "wipe"].contains(&name) {
        return "命令会删除文件，需要确认目标".into();
    }
    if ["sudo", "su", "doas", "pkexec", "runuser"].contains(&name) {
        return "命令会提升权限或切换用户".into();
    }
    if name.starts_with("mkfs") || name.starts_with("fsck") {
        return "命令可能修改磁盘、挂载或系统状态".into();
    }
    if ["dd", "fdisk", "parted", "mount", "umount", "reboot", "shutdown"].contains(&name) {
        return "命令可能修改磁盘、挂载或系统状态".into();
    }
    if name == "git" || name == "gh" {
        return "Git 写操作、发布或自定义配置需要确认".into();
    }
    if ["curl", "wget", "ssh", "scp", "rsync", "nc", "ncat", "telnet"].contains(&name) {
        return "网络传输或远程命令需要确认".into();
    }
    if [
        "bash", "sh", "zsh", "fish", "python", "python3", "node", "bun", "deno", "perl",
        "ruby", "awk", "eval", "source", "exec", "env", "xargs", "make", "cargo",
    ]
    .contains(&name)
    {
        return "脚本可执行任意操作，需先查看完整命令".into();
    }
    format!("命令「{name}」不在已验证的简单只读命令范围内")
}

/// `knownOptions`: every long option must be in the allow-list, and every short-option
/// cluster must satisfy `short`. Abbreviations are rejected, because `--out` can mean
/// `--output` to GNU getopt.
fn known_long_options(args: &[String], allowed: &[&str]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if let Some(name) = arg.split('=').next()
            && arg.starts_with("--")
            && !allowed.contains(&name)
        {
            // The allow-list is written with its dashes, and abbreviations are rejected
            // outright because `--out` can mean `--output` to GNU getopt.
            return false;
        }
    }
    true
}

/// Validate a short-option cluster: every character is a plain flag, or one takes a
/// value (which then absorbs the rest of the argument, or the next one).
fn known_short_options(args: &[String], simple: &str, value_taking: &str) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if !arg.starts_with('-') || arg.starts_with("--") || arg == "-" {
            continue;
        }
        let body = &arg[1..];
        if body.is_empty() {
            continue;
        }
        let mut chars = body.chars();
        let first = chars.next().unwrap();
        if value_taking.contains(first) {
            continue;
        }
        if !body.chars().all(|c| simple.contains(c)) {
            return false;
        }
    }
    true
}

/// Short options that absorb the rest of their own argument as a path (or take one from
/// the next argument). `-nfFILE` is `-n -f FILE`, because letters are consumed left to
/// right as flags until one takes a value.
fn short_path_options(command: &str) -> &'static [(char, Operation)] {
    match command {
        "grep" => &[('f', Operation::Read)],
        "rg" => &[('f', Operation::Read)],
        "file" => &[('f', Operation::Read), ('m', Operation::Read)],
        "du" => &[('X', Operation::Read)],
        "sort" => &[('o', Operation::Write), ('T', Operation::Write)],
        _ => &[],
    }
}

/// The literal value a glued short option would take, e.g. `FILE` from `-nfFILE`.
fn glued_short_value(command: &str, arg: &str) -> Option<(String, Operation)> {
    let options = short_path_options(command);
    if options.is_empty() || !arg.starts_with('-') || arg.starts_with("--") {
        return None;
    }
    let body = &arg[1..];
    for (index, c) in body.char_indices() {
        if let Some((_, operation)) = options.iter().find(|(flag, _)| *flag == c) {
            return Some((body[index + c.len_utf8()..].to_string(), *operation));
        }
    }
    None
}

/// A leading `~` survives the quoting rewrite only if it is expanded first.
fn expand_home(value: &str, dialect: Dialect) -> Option<String> {
    if value == "~" {
        return Some(home().to_string_lossy().to_string());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Some(home().join(rest).to_string_lossy().to_string());
    }
    if dialect == Dialect::Zsh
        && (value == "~+" || value == "~-" || value.starts_with("~+/") || value.starts_with("~-/"))
    {
        // zsh expands these to directory-stack entries, which are unknowable here.
        return None;
    }
    if value.starts_with('~') {
        // A `~user` form is left to a human; guessing a home directory would be wrong.
        return None;
    }
    Some(value.to_string())
}

/// zsh-only spelling that bash would leave alone. A word like this means the vetted
/// command and the executed command would differ, so it is sent to the user.
///
/// `^foo` is deliberately allowed: it is a glob or a plain literal, and `grep '^import'`
/// is far too common to refuse. A leading `=` inside a longer word (`a=b.txt`) is not an
/// expansion in zsh and is left untouched.
fn zsh_rewrite_risk(value: &str) -> bool {
    value.len() > 1 && value.starts_with('=')
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn vet_segment(words: &[Word], cwd: &Path, dialect: Dialect) -> Assessment {
    let Some((command, raw_args)) = words.split_first() else {
        return Assessment::ask("未找到可执行命令");
    };
    let name = command.value.rsplit('/').next().unwrap_or(&command.value).to_string();
    if name.is_empty() {
        return Assessment::ask("命令为空或格式不正确");
    }
    // `FOO=bar cmd` changes how the command runs, so it is never auto-approved.
    if let Some((head, _)) = command.value.split_once('=')
        && !head.is_empty()
        && head.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Assessment::ask("环境变量赋值可能改变命令的执行方式");
    }
    if dialect == Dialect::Zsh && !command.quoted && zsh_rewrite_risk(&command.value) {
        return Assessment::ask("zsh 会对此命令名做 =命令 展开，无法确认实际执行的文件");
    }
    let Some(executable) = trusted_executable(&command.value) else {
        return Assessment::ask(command_reason(&name));
    };

    let mut args: Vec<String> = Vec::with_capacity(raw_args.len());
    for raw in raw_args {
        // A quoted or escaped leading `~` is literal in both shells, so the rewrite is
        // already equivalent and nothing has to be expanded or guessed.
        let expanded = if raw.quoted {
            Some(raw.value.clone())
        } else {
            expand_home(&raw.value, dialect)
        };
        let Some(expanded) = expanded else {
            return Assessment::ask("参数中含无法可靠解析的 shell 展开（如 ~user 或 zsh 的 ~+）");
        };
        if dialect == Dialect::Zsh && !raw.quoted && zsh_rewrite_risk(&expanded) {
            return Assessment::ask("参数会被 zsh 的 =命令 展开改写，无法确认实际参数");
        }
        args.push(expanded);
    }

    // Vetted read operations must not sneak in subcommands or output-file flags.
    if name == "rg"
        && args
            .iter()
            .any(|arg| arg.starts_with("--pre=") || arg == "--pre" || arg.starts_with("--hostname-bin"))
    {
        return Assessment::ask("搜索参数会启动外部程序");
    }
    if name == "find"
        && args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fprint" | "-fprint0"
                    | "-fprintf" | "-fls"
            )
        })
    {
        return Assessment::ask("find 参数会执行命令、删除或写入文件");
    }
    if name == "sort"
        && args.iter().any(|arg| {
            matches!(arg.as_str(), "--output" | "--compress-program")
                || arg.starts_with("--output=")
                || arg.starts_with("--compress-program=")
                || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('o'))
        })
    {
        return Assessment::ask("sort 参数会写入文件或执行外部程序");
    }
    if name == "file"
        && args.iter().any(|arg| arg.starts_with("--uncompress") || (arg.starts_with('-') && !arg.starts_with("--") && (arg.contains('z') || arg.contains('Z'))))
    {
        return Assessment::ask("file 解压参数可能调用外部程序");
    }

    match name.as_str() {
        "sort" => {
            if !known_long_options(
                &args,
                &[
                    "--numeric-sort", "--general-numeric-sort", "--human-numeric-sort",
                    "--version-sort", "--reverse", "--unique", "--stable", "--ignore-case",
                    "--ignore-leading-blanks", "--field-separator", "--key", "--check",
                    "--help", "--version",
                ],
            ) || !known_short_options(&args, "nNgGhHrVuMsbfcdm", "kt")
            {
                return Assessment::ask("sort 参数未被确认为只读");
            }
        }
        "file" => {
            if !known_long_options(
                &args,
                &[
                    "--brief", "--mime", "--mime-type", "--mime-encoding", "--dereference",
                    "--separator", "--keep-going", "--version", "--help",
                ],
            ) || !known_short_options(&args, "bikLNprsv0", "fm")
            {
                return Assessment::ask("file 参数未被确认为只读");
            }
        }
        "rg" => {
            if !known_long_options(
                &args,
                &[
                    "--files", "--hidden", "--no-ignore", "--no-ignore-vcs", "--no-ignore-parent",
                    "--no-ignore-global", "--glob", "--iglob", "--type", "--type-not",
                    "--type-list", "--line-number", "--no-line-number", "--count",
                    "--count-matches", "--with-filename", "--no-filename", "--ignore-case",
                    "--smart-case", "--case-sensitive", "--fixed-strings", "--word-regexp",
                    "--line-regexp", "--invert-match", "--max-count", "--max-depth",
                    "--max-filesize", "--context", "--before-context", "--after-context",
                    "--color", "--colors", "--heading", "--no-heading", "--sort", "--sortr",
                    "--stats", "--json", "--only-matching", "--replace", "--trim", "--pcre2",
                    "--multiline", "--multiline-dotall", "--follow", "--files-without-match",
                    "--files-with-matches", "--null", "--null-data", "--text", "--regexp",
                    "--file", "--quiet", "--encoding", "--no-messages", "--version", "--help",
                    "--crlf",
                ],
            ) || !known_short_options(&args, "nHhIilLovswxUaFcqSPz0u", "egftTrABCm")
            {
                return Assessment::ask("rg 参数未被确认为只读");
            }
        }
        "find" => {
            let known: HashSet<&str> = [
                "-name", "-iname", "-path", "-ipath", "-regex", "-iregex", "-type", "-maxdepth",
                "-mindepth", "-print", "-print0", "-ls", "-empty", "-size", "-mtime", "-mmin",
                "-atime", "-amin", "-ctime", "-cmin", "-newer", "-anewer", "-cnewer", "-user",
                "-group", "-perm", "-a", "-and", "-o", "-or", "-not", "-true", "-false",
                "-readable", "-writable", "-executable", "-P", "-H", "-L",
            ]
            .into_iter()
            .collect();
            let looks_numeric = |arg: &str| {
                arg.strip_prefix('-')
                    .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
            };
            if args.iter().any(|arg| {
                arg.starts_with('-') && !known.contains(arg.as_str()) && !looks_numeric(arg)
            }) {
                return Assessment::ask("find 参数未被确认为只读");
            }
        }
        "sed" => {
            let rest: Vec<&String> = if args.first().map(String::as_str) == Some("-n") {
                args[1..].iter().collect()
            } else {
                args.iter().collect()
            };
            let script_ok = rest.first().is_some_and(|script| is_line_range_print(script));
            if !script_ok || rest[1..].iter().any(|arg| arg.starts_with('-')) {
                return Assessment::ask("只自动放行 sed 的行范围打印；编辑、脚本及其他参数需要确认");
            }
        }
        _ => {}
    }

    // Check every argument a literal path could hide in, including option values such as
    // `--file=...`. `cat id_rsa` matters as much as `cat .ssh/id_rsa`, and printing
    // commands carry data rather than paths, so they are exempt.
    if !DATA_ARG_COMMANDS.contains(&name.as_str()) {
        let mut pending: Option<Operation> = None;
        for arg in &args {
            if arg == "--" {
                pending = None;
                continue;
            }
            if let Some(operation) = pending
                && !arg.starts_with('-') {
                    let decision = assess_path(operation, arg, cwd);
                    pending = None;
                    if !decision.allows() {
                        return decision;
                    }
                    continue;
                }
            pending = None;
            if arg.starts_with('-') && !arg.starts_with("--")
                && let Some((value, operation)) = glued_short_value(&name, arg) {
                    if value.is_empty() {
                        pending = Some(operation);
                        continue;
                    }
                    let decision = assess_path(operation, &value, cwd);
                    if !decision.allows() {
                        return decision;
                    }
                    continue;
                }
            let values: Vec<&str> = if arg.starts_with('-') && !arg.starts_with("-/") && !arg.starts_with("-~") {
                match arg.split_once('=') {
                    Some((_, value)) => vec![value],
                    None => Vec::new(),
                }
            } else {
                vec![arg.as_str()]
            };
            for value in values {
                if value.is_empty() {
                    continue;
                }
                let decision = assess_path(Operation::Read, value, cwd);
                if !decision.allows() {
                    return decision;
                }
            }
        }
    }

    if name == "git" {
        return vet_git(&executable, &args, cwd);
    }
    let mut rewritten = vec![executable.to_string_lossy().to_string()];
    rewritten.extend(args);
    Assessment::Allow {
        safe_command: Some(rewritten.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ")),
    }
}

/// `sed -n '1,10p' file`, `sed -n '5p' file`, `sed '1,$p' file`. Nothing else.
fn is_line_range_print(script: &str) -> bool {
    let Some(body) = script.strip_suffix('p') else { return false };
    if body.is_empty() {
        return true; // bare `p`
    }
    let (start, end) = match body.split_once(',') {
        Some((start, end)) => (start, Some(end)),
        None => (body, None),
    };
    let valid_address = |address: &str| {
        !address.is_empty() && (address == "$" || address.chars().all(|c| c.is_ascii_digit()))
    };
    if !valid_address(start) {
        return false;
    }
    match end {
        Some(end) => valid_address(end),
        None => true,
    }
}

fn vet_git(executable: &Path, args: &[String], cwd: &Path) -> Assessment {
    // Scan for the options that make git execute something else *before* choosing a
    // subcommand, because `git -c core.pager=evil log` would otherwise be reported as an
    // unknown subcommand and the real hazard would go unnamed.
    let dangerous = args.iter().any(|arg| {
        let long_bad = ["--ext-diff", "--textconv", "--output", "--config-env", "--exec-path"]
            .iter()
            .any(|bad| arg == bad || arg.starts_with(&format!("{bad}=")));
        let sets_config = arg == "-c" || (arg.starts_with("-c") && !arg.starts_with("--"));
        let order_file = arg.starts_with("-O") && !arg.starts_with("--");
        long_bad || sets_config || order_file || arg.contains("%G")
    });
    if dangerous {
        return Assessment::ask("Git 参数可能执行外部程序或写入文件");
    }
    let Some((subcommand, options)) = args.split_first() else {
        return Assessment::ask(command_reason("git"));
    };
    if subcommand.starts_with('-') {
        return Assessment::ask("Git 全局参数未被确认为只读");
    }
    // `<rev>:<path>` reads a blob, so the path after the colon needs the same checks.
    for arg in options {
        if !arg.starts_with('-')
            && let Some((_, path)) = arg.split_once(':')
            && !path.is_empty()
        {
            let decision = assess_path(Operation::Read, path, cwd);
            if !decision.allows() {
                return decision;
            }
        }
    }
    if !["status", "diff", "log", "show", "rev-parse", "ls-files", "ls-tree"]
        .contains(&subcommand.as_str())
    {
        return Assessment::ask(command_reason("git"));
    }
    if !known_long_options(
        options,
        &[
            "--stat", "--shortstat", "--numstat", "--name-only", "--name-status", "--check",
            "--summary", "--cached", "--staged", "--no-index", "--patch", "--no-patch",
            "--color", "--no-color", "--word-diff", "--word-diff-regex",
            "--ignore-space-at-eol", "--ignore-space-change", "--ignore-all-space",
            "--ignore-blank-lines", "--exit-code", "--quiet", "--no-ext-diff",
            "--no-textconv", "--unified", "--oneline", "--decorate", "--graph", "--all",
            "--branches", "--tags", "--remotes", "--max-count", "--pretty", "--format",
            "--since", "--until", "--author", "--committer", "--grep", "--date",
            "--abbrev-commit", "--reverse", "--first-parent", "--follow", "--no-merges",
            "--merges", "--short", "--porcelain", "--branch", "--untracked-files", "--ignored",
            "--ignore-submodules", "--show-toplevel", "--git-dir", "--absolute-git-dir",
            "--show-prefix", "--is-inside-work-tree", "--verify", "--abbrev-ref",
            "--symbolic-full-name", "--sq", "--end-of-options", "--git-path", "--stage",
            "--deleted", "--modified", "--others", "--exclude-standard", "--error-unmatch",
            "--full-name", "--eol", "--long", "--full-tree", "--object-only",
        ],
    ) || !git_short_options_ok(options)
    {
        return Assessment::ask("Git 参数未被确认为只读");
    }
    let mut rewritten = vec![
        executable.to_string_lossy().to_string(),
        "--no-pager".into(),
        "--no-optional-locks".into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
    ];
    // `diff`, `log` and `show` are the ones that can be told to call out to an external
    // program, so the switches are forced on for them.
    if ["diff", "log", "show"].contains(&subcommand.as_str()) {
        rewritten.push(subcommand.clone());
        rewritten.push("--no-ext-diff".into());
        rewritten.push("--no-textconv".into());
    } else {
        rewritten.push(subcommand.clone());
    }
    rewritten.extend(options.iter().cloned());
    Assessment::Allow {
        safe_command: Some(rewritten.iter().map(|w| shell_quote(w)).collect::<Vec<_>>().join(" ")),
    }
}

/// `-[0-9]+`, `-n[0-9]*`, `-[pswbrzR]+`, `-U[0-9]*`, `-M[0-9]*`, `-C[0-9]*`, `-S...`,
/// `-G...` — the short flags git shortens but never uses for writes.
fn git_short_options_ok(options: &[String]) -> bool {
    for arg in options {
        if arg == "--" {
            break;
        }
        if !arg.starts_with('-') || arg.starts_with("--") || arg == "-" {
            continue;
        }
        let body = &arg[1..];
        let mut chars = body.chars();
        let Some(first) = chars.next() else { continue };
        let rest = chars.as_str();
        let ok = if first.is_ascii_digit() {
            body.chars().all(|c| c.is_ascii_digit())
        } else {
            match first {
                'n' => rest.chars().all(|c| c.is_ascii_digit()),
                'U' | 'M' | 'C' => rest.chars().all(|c| c.is_ascii_digit()),
                'S' | 'G' => true,
                c if "pswbrzR".contains(c) => body.chars().all(|c| "pswbrzR".contains(c)),
                _ => false,
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Assess a whole command line, returning a rewritten equivalent when it is safe.
pub fn assess_command(command: &str, cwd: &Path, dialect: Dialect) -> Assessment {
    if command.trim().is_empty() {
        return Assessment::ask("命令为空或格式不正确");
    }
    if command.len() > 128 * 1024 {
        return Assessment::ask("命令过长，无法自动判断");
    }
    let segments = match parse_literal_commands(command) {
        Ok(segments) => segments,
        Err(reason) => return Assessment::ask(reason),
    };
    if segments.is_empty() {
        return Assessment::ask("未找到可执行命令");
    }
    let mut normalized: Vec<String> = Vec::new();
    for segment in &segments {
        let decision = vet_segment(&segment.words, cwd, dialect);
        match decision {
            Assessment::Ask { reason } => return Assessment::ask(reason),
            Assessment::Allow { safe_command } => {
                let rewritten = safe_command.unwrap_or_else(|| {
                    segment
                        .words
                        .iter()
                        .map(|word| shell_quote(&word.value))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                normalized.push(rewritten);
                if let Some(operator) = &segment.operator {
                    normalized.push(operator.clone());
                }
            }
        }
    }
    Assessment::Allow { safe_command: Some(normalized.join(" ")) }
}

/// Tools that only ever read.
pub fn assess_tool(name: &str, input: &serde_json::Value, cwd: &Path, dialect: Dialect) -> Assessment {
    let string = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "bash" => assess_command(string("command"), cwd, dialect),
        "write" | "edit" => {
            let path = if string("path").is_empty() { string("file_path") } else { string("path") };
            assess_path(Operation::Write, path, cwd)
        }
        "read" | "grep" | "find" | "ls" => {
            let path = {
                let explicit = if string("path").is_empty() { string("file_path") } else { string("path") };
                if explicit.is_empty() {
                    cwd.to_string_lossy().to_string()
                } else {
                    explicit.to_string()
                }
            };
            assess_path(Operation::Read, &path, cwd)
        }
        _ => Assessment::ask("自定义工具尚未归类，需要确认其操作"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        std::env::temp_dir().join("pi-policy-cwd")
    }

    fn allows(command: &str) -> bool {
        assess_command(command, &cwd(), Dialect::Zsh).allows()
    }

    fn reason(command: &str) -> String {
        assess_command(command, &cwd(), Dialect::Zsh).reason().unwrap_or_default().to_string()
    }

    #[test]
    fn simple_read_commands_are_rewritten_to_absolute_paths() {
        let decision = assess_command("cat notes.txt", &cwd(), Dialect::Zsh);
        match decision {
            Assessment::Allow { safe_command: Some(command) } => {
                assert_eq!(command, "'/usr/bin/cat' 'notes.txt'");
            }
            other => panic!("expected allow with rewrite, got {other:?}"),
        }
    }

    #[test]
    fn a_hijacked_command_name_is_not_auto_approved() {
        assert!(!allows("./cat x"));
        assert!(!allows("/tmp/cat x"));
    }

    #[test]
    fn secret_paths_ask() {
        assert!(reason("cat ~/.ssh/id_rsa").contains("凭据"));
        assert!(reason("cat /etc/shadow").contains("凭据"));
        assert!(reason("cat .env").contains("凭据"));
        assert!(reason("cat server.key").contains("凭据"));
        // `~/.pi` is this tool's own store, and its config holds the provider key.
        assert!(!allows("cat ~/.pi/config.json"));
        assert!(!allows("cat ~/.codex/config.toml"));
        assert!(!allows("grep -r . .ssh"));
    }

    #[test]
    fn a_project_fixture_called_auth_json_is_still_checked_but_allowed_outside_home() {
        // Outside the home directory the generic names do not apply.
        let decision = assess_path(Operation::Read, "/srv/app/tests/auth.json", &cwd());
        assert!(decision.allows());
    }

    #[test]
    fn redirection_and_substitution_ask() {
        assert!(reason("echo hi > file").contains("重定向"));
        assert!(reason("cat $(ls)").contains("变量"));
        assert!(reason("cat `ls`").contains("变量"));
        assert!(reason("ls *.rs").contains("通配符"));
        assert!(reason("ls; rm -rf /").contains("删除"));
        assert!(reason("sleep 1 &").contains("后台"));
    }

    #[test]
    fn data_argument_commands_are_not_path_checked() {
        assert!(allows("echo hello"));
        assert!(allows("printf %s x"));
        assert!(allows("uname -a"));
        // …but they still cannot smuggle options that write.
        assert!(allows("echo --file=/etc/passwd"));
    }

    #[test]
    fn glued_option_values_are_checked() {
        assert!(reason("grep -f~/.ssh/id_rsa x").contains("凭据"));
        assert!(reason("grep --file=~/.ssh/id_rsa x").contains("凭据"));
        assert!(reason("grep -f ~/.ssh/id_rsa x").contains("凭据"));
        // -nfFILE must be read as -n -f FILE, not as the flag `fFILE`.
        assert!(reason("grep -nf~/.ssh/id_rsa x").contains("凭据"));
        assert!(reason("file -f~/.ssh/id_rsa x").contains("凭据"));
        assert!(reason("du -X~/.ssh/id_rsa").contains("凭据"));
        assert!(reason("sort -o/tmp/out x").contains("写入文件"));
    }

    #[test]
    fn zsh_only_expansions_ask_but_quoted_forms_do_not() {
        assert!(reason("cat =ls").contains("=命令"));
        assert!(reason("cat ~+/x").contains("展开"));
        assert!(reason("cat ~root/x").contains("展开"));
        // A quoted or escaped leading = is a literal in zsh.
        assert!(allows("grep '=x' file"));
        assert!(allows("grep \"=x\" file"));
        assert!(allows("grep \\=x file"));
        // An empty quote does not protect the expansion.
        assert!(reason("cat ''=ls").contains("=命令"));
        // And in bash the same word is just a literal.
        assert!(assess_command("cat =ls", &cwd(), Dialect::Bash).allows());
    }

    #[test]
    fn git_is_limited_to_read_only_subcommands() {
        assert!(allows("git status"));
        assert!(allows("git log --oneline -5"));
        assert!(allows("git diff --stat"));
        assert!(allows("git rev-parse HEAD"));
        assert!(allows("git ls-files"));
        assert!(reason("git commit -m x").contains("Git 写操作"));
        assert!(reason("git push").contains("Git 写操作"));
        assert!(reason("git -c core.pager=evil log").contains("外部程序"));
        assert!(reason("git diff --ext-diff").contains("外部程序"));
        assert!(reason("git show HEAD:%G").contains("外部程序"));
    }

    #[test]
    fn git_rewrites_disable_pager_hooks_and_external_diff() {
        match assess_command("git log --oneline", &cwd(), Dialect::Zsh) {
            Assessment::Allow { safe_command: Some(command) } => {
                assert!(command.contains("--no-pager"));
                assert!(command.contains("core.fsmonitor=false"));
                assert!(command.contains("core.hooksPath=/dev/null"));
                assert!(command.contains("--no-ext-diff"));
                assert!(command.contains("--no-textconv"));
            }
            other => panic!("expected rewrite, got {other:?}"),
        }
    }

    #[test]
    fn git_rev_path_forms_are_path_checked() {
        assert!(reason("git show HEAD:~/.ssh/id_rsa").contains("凭据"));
        assert!(reason("git show HEAD:.env").contains("凭据"));
    }

    #[test]
    fn sed_only_allows_line_range_printing() {
        assert!(allows("sed -n '1,10p' file.txt"));
        assert!(allows("sed -n '5p' file.txt"));
        assert!(allows(r"sed '1,$p' file.txt"));
        assert!(reason("sed -i 's/a/b/' file.txt").contains("sed"));
        assert!(reason("sed -n '1,10p' -e 'w /tmp/x' file.txt").contains("sed"));
        assert!(reason("sed -n '1,10w /tmp/x' file.txt").contains("sed"));
    }

    #[test]
    fn unknown_options_ask() {
        assert!(reason("rg --pre 'evil' pattern").contains("外部程序"));
        assert!(reason("rg --unknown-flag x").contains("未被确认"));
        assert!(reason("find . -delete").contains("find"));
        assert!(reason("find . -frobnicate").contains("未被确认"));
        assert!(reason("sort --output=/tmp/x f").contains("写入文件"));
        assert!(reason("file --uncompress x").contains("外部程序"));
        assert!(allows("rg --hidden pattern"));
        assert!(allows("find . -name '*.rs' -type f"));
        assert!(allows("sort -u file"));
        assert!(allows("file -b x"));
    }

    #[test]
    fn writes_outside_the_project_ask() {
        let cwd = cwd();
        assert!(assess_path(Operation::Write, "/tmp/elsewhere.txt", &cwd).reason().is_some());
        assert!(assess_path(Operation::Write, "notes.txt", &cwd).allows());
        assert!(assess_path(Operation::Write, ".git/config", &cwd).reason().is_some());
        assert!(assess_path(Operation::Write, "AGENTS.md", &cwd).reason().is_some());
    }

    #[test]
    fn compound_commands_are_vetted_segment_by_segment() {
        assert!(allows("cat a.txt | head -3"));
        assert!(reason("cat a.txt && rm a.txt").contains("删除"));
        assert!(reason("ls || sudo rm -rf /").contains("提升权限"));
    }

    #[test]
    fn script_and_network_commands_ask() {
        assert!(reason("python3 -c 'print(1)'").contains("脚本"));
        assert!(reason("curl http://x").contains("网络"));
        assert!(reason("sudo ls").contains("提升权限"));
        assert!(reason("msg=hi env").contains("环境变量"));
    }

    #[test]
    fn empty_or_broken_input_asks() {
        assert!(!allows(""));
        assert!(!allows("   "));
        assert!(reason("cat 'unterminated").contains("引号未闭合"));
        assert!(reason("cat a.txt |").contains("不完整"));
    }

    #[test]
    fn headless_denial_text_names_the_rule() {
        let text = refusal(Some("命令会删除文件，需要确认目标"));
        assert_eq!(text, "未获得用户授权，操作未执行（命令会删除文件，需要确认目标）");
        assert_eq!(refusal(None), "未获得用户授权，操作未执行");
    }

    #[test]
    fn tools_route_to_the_right_check() {
        let cwd = cwd();
        let read = serde_json::json!({"path": "src/main.rs"});
        assert!(assess_tool("read", &read, &cwd, Dialect::Zsh).allows());
        let write = serde_json::json!({"path": "src/main.rs", "content": "x"});
        assert!(assess_tool("write", &write, &cwd, Dialect::Zsh).allows());
        let outside = serde_json::json!({"path": "/etc/hosts", "content": "x"});
        assert!(!assess_tool("write", &outside, &cwd, Dialect::Zsh).allows());
        let grep = serde_json::json!({"pattern": "x", "path": "~/.ssh"});
        assert!(!assess_tool("grep", &grep, &cwd, Dialect::Zsh).allows());
        let bash = serde_json::json!({"command": "ls"});
        assert!(assess_tool("bash", &bash, &cwd, Dialect::Zsh).allows());
    }
}
