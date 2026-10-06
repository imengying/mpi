//! Path judgements: where a path resolves to, and whether that is allowed.
//!
//! Everything here answers the same question from a different angle — `resolve_tool_path`
//! turns what the model wrote into an absolute path, `canonical_path` follows it through
//! symlinks, and `sensitive` decides whether the place it landed is one that needs asking
//! about. [`assess_path`] is the entry point; the rest are its parts.
//!
//! Two rules run through all of it. Resolution is bounded: a symlink chain longer than
//! [`MAX_DEPTH`] is refused rather than followed forever. And nothing is decided from the
//! text of a path alone — `cat id_rsa` and `cat .ssh/id_rsa` are the same file, so the
//! check happens after resolution, against the real place.

use std::path::{Component, Path, PathBuf};

use super::{Assessment, Operation};

/// Resolve a literal tool path, supporting only the home-directory shorthand `~`.
pub fn resolve_tool_path(input: &str, cwd: &Path) -> PathBuf {
    let path = input;
    let expanded = if path == "~" {
        home().to_string_lossy().to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home().join(rest).to_string_lossy().to_string()
    } else {
        path.to_string()
    };
    let candidate = PathBuf::from(expanded);
    if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
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
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    match std::fs::canonicalize(&absolute) {
        Ok(resolved) => Ok(resolved),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Resolve links before processing `..`, including when a write's leaf is new.
            let mut resolved = PathBuf::new();
            for component in absolute.components() {
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        resolved.pop();
                    }
                    Component::Normal(part) => {
                        let next = resolved.join(part);
                        match std::fs::symlink_metadata(&next) {
                            Ok(meta) if meta.file_type().is_symlink() => {
                                let target = std::fs::read_link(&next)?;
                                let target = if target.is_absolute() {
                                    target
                                } else {
                                    resolved.join(target)
                                };
                                resolved = canonical_path(&target, depth + 1)?;
                            }
                            Ok(_) => resolved = next,
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                                resolved = next
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    other => resolved.push(other.as_os_str()),
                }
            }
            Ok(resolved)
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

pub(super) fn inside(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

pub(super) fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Directories that are sensitive wherever they appear. A project may legitimately contain
/// a `.docker` or `.azure` directory, but reading one is still worth a question: the names
/// only mean credentials, and a false prompt is cheaper than a leaked key.
const SENSITIVE_DIRS: &[&str] = &[
    ".ssh", ".gnupg", ".aws", ".kube", ".docker", ".azure", ".gcloud",
];
/// Multi-segment credential locations, relative to the home directory. The agent harnesses
/// are listed too: pi keeps provider keys in `~/.pi/config.json` and its sessions next to
/// them, and codex/Claude/Gemini keep the same kind of live secret. `~/.pi` is this tool's
/// own store — the config holds the provider key in clear text, so a command that reads it
/// is worth a question.
const HOME_PATHS: &[&str] = &[
    ".config/gh",
    ".config/gcloud",
    ".config/glab-cli",
    ".config/hub",
    ".config/doctl",
    ".pi",
    ".codex",
    ".claude",
    ".gemini",
    ".continue",
    ".aider",
    ".local/share/keyrings",
];
/// Credential-like names, matched on the basename anywhere: a directory-only rule misses
/// `grep -r . .ssh`, which reads private keys without naming one.
const SENSITIVE_NAMES: &[&str] = &[
    ".env",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".dockercfg",
    ".gitconfig",
    ".bash_history",
    ".zsh_history",
    ".python_history",
    ".mysql_history",
    ".psql_history",
    ".wgetrc",
    ".curlrc",
    ".pgpass",
    ".authinfo",
    ".s3cfg",
    ".terraformrc",
    ".my.cnf",
    ".mylogin.cnf",
    ".kubeconfig",
    ".credentials.json",
    ".envrc",
    ".htpasswd",
    "application_default_credentials.json",
    "hosts.yml",
];
/// Private-key containers: the extension alone is enough to ask before touching.
const SENSITIVE_SUFFIXES: &[&str] = &[
    "pem", "key", "pfx", "p12", "jks", "keystore", "ppk", "kdbx", "ovpn",
];

pub(super) fn basename_is_sensitive(name: &str) -> bool {
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
    if lower.starts_with("service") && lower.contains("account") && lower.ends_with(".json") {
        return true;
    }
    false
}

/// Secret-bearing basenames that are only meaningful outside a project checkout: a
/// repository may legitimately contain a fixture called `auth.json`.
pub(super) fn home_only_name(name: &str) -> bool {
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

pub(super) fn extension_is_sensitive(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => SENSITIVE_SUFFIXES
            .iter()
            .any(|s| s.eq_ignore_ascii_case(ext)),
        None => false,
    }
}

pub(super) fn sensitive(path: &Path) -> bool {
    let segments: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    if segments
        .iter()
        .any(|part| SENSITIVE_DIRS.iter().any(|d| d.eq_ignore_ascii_case(part)))
    {
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
    if path.starts_with("/proc") && matches!(name.as_str(), "environ" | "mem") {
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

/// Exclusions for recursive content searches, shared by the native and shell tools.
/// These are appended after user globs so a broad include cannot restore secret files.
pub(crate) fn search_exclusions(ripgrep: bool) -> Vec<String> {
    let mut patterns: Vec<String> = SENSITIVE_NAMES.iter().map(|s| s.to_string()).collect();
    patterns.extend(
        [
            ".env.*",
            "id_*",
            "service*account*.json",
            "shadow",
            "gshadow",
            "environ",
            "mem",
        ]
        .map(str::to_string),
    );
    patterns.extend(SENSITIVE_SUFFIXES.iter().map(|s| format!("*.{s}")));
    patterns.extend(
        [
            ".claude.json",
            ".aider.conf.yml",
            "auth.json",
            "oauth_creds.json",
            "oauth-creds.json",
            "credentials",
            "credentials.json",
            "credentialsdb",
            "token.json",
            "login.keyring",
            "keyring.*",
        ]
        .map(str::to_string),
    );
    let mut dirs: Vec<String> = SENSITIVE_DIRS.iter().map(|s| s.to_string()).collect();
    dirs.extend(
        HOME_PATHS
            .iter()
            .map(|s| s.rsplit('/').next().unwrap().to_string()),
    );
    if ripgrep {
        patterns.extend(dirs);
        patterns
            .into_iter()
            .flat_map(|pattern| ["--iglob".into(), format!("!{pattern}")])
            .collect()
    } else {
        // GNU grep has no case-insensitive glob flag. Character classes give the same rule.
        let insensitive = |s: String| {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphabetic() {
                        format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
                    } else {
                        c.to_string()
                    }
                })
                .collect::<String>()
        };
        patterns
            .into_iter()
            .map(|p| format!("--exclude={}", insensitive(p)))
            .chain(
                dirs.into_iter()
                    .map(|p| format!("--exclude-dir={}", insensitive(p))),
            )
            .collect()
    }
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
