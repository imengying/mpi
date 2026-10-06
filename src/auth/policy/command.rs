//! Command-line judgements: which commands may run without asking.
//!
//! The line is parsed first ([`parse_literal_commands`], from `auth::parse`) into segments
//! and words, and then every word of every segment is resolved: to a trusted absolute
//! executable, to a concrete path, or to a refusal. That order matters, because the
//! alternative — pattern-matching the command text — cannot see through `--file=/etc/shadow`
//! or a `cd` earlier in the same line.
//!
//! Most of this file is the per-command option tables. A command is auto-approved only if
//! every option it was given is one this file knows to be harmless *for that command*:
//! `rg --files` reads, `rg --pre` executes. Unknown options are sent to the user, which is
//! why the tables err toward listing what is understood rather than what is forbidden.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::auth::parse::is_line_range_print;
use crate::auth::parse::{Word, parse_literal_commands};

use super::path::{
    assess_path, canonical_path, home, normalize, resolve_tool_path, search_exclusions,
};
use super::{Assessment, Dialect, Operation};

/// Commands whose arguments are data rather than paths.
///
/// `basename` and `dirname` never open what they are given — `basename /etc/shadow` prints
/// `shadow` — so path-checking their arguments invents a leak that cannot happen. The rest
/// are the printing commands whose arguments are text.
const DATA_ARG_COMMANDS: &[&str] = &[
    "echo", "printf", "true", "false", "uname", "df", "basename", "dirname",
];

/// Commands auto-approved when every argument checks out.
///
/// The second group is the inspection tools a model reaches for constantly and that cannot
/// change anything: `which` finds a binary, `date` prints the clock, `ps` lists processes.
/// They were all asking before, and a question that always has the same answer is a tax on
/// every turn — the refusal is not protecting anything, it is being clicked through.
///
/// Membership is not a promise that a command is harmless with *any* argument. `date -s`
/// sets the system clock, so the ones with a dangerous option get a check in [`vet_segment`]
/// alongside the rest. `env` is deliberately absent: bare `env` prints every environment
/// variable, provider keys included, and `env cmd` runs an arbitrary command.
const READ_COMMANDS: &[&str] = &[
    "pwd", "ls", "cat", "head", "tail", "wc", "stat", "readlink", "realpath", "printf", "echo",
    "true", "false", "cut", "tr", "du", "df", "uname", "rg", "grep", "find", "sort", "file", "sed",
    // Inspection only: no argument writes, and nothing here reads a path as data.
    "which", "date", "nproc", "uptime", "free", "ps", "id", "whoami", "basename", "dirname",
    "column", "lscpu", "seq",
];

/// Where a trusted executable may live. Anything else (including a `./cat`) is not
/// auto-approved, which is what stops PATH hijacking.
const TRUSTED_DIRS: &[&str] = &["/usr/bin", "/bin"];

pub(super) fn trusted_executable_target(name: &str, resolved: &Path) -> bool {
    let target_name = resolved.file_name().and_then(|n| n.to_str());
    // Debian/Ubuntu's alternatives system resolves `which` to which.debianutils.
    // Keep aliases explicit: accepting any renamed target could turn a reader into rm.
    let same_command =
        target_name == Some(name) || (name == "which" && target_name == Some("which.debianutils"));
    same_command
        && resolved
            .parent()
            .is_some_and(|parent| TRUSTED_DIRS.iter().any(|dir| parent == Path::new(dir)))
}

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
        TRUSTED_DIRS
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .collect()
    };
    for candidate in candidates {
        if !candidate.is_absolute() {
            continue;
        }
        let Ok(resolved) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if !trusted_executable_target(name, &resolved) {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let Ok(meta) = std::fs::metadata(&resolved) else {
                continue;
            };
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Some(resolved);
    }
    None
}

pub(super) fn command_reason(name: &str) -> String {
    if ["rm", "rmdir", "unlink", "shred", "wipe"].contains(&name) {
        return "命令会删除文件，需要确认目标".into();
    }
    if ["sudo", "su", "doas", "pkexec", "runuser"].contains(&name) {
        return "命令会提升权限或切换用户".into();
    }
    if name.starts_with("mkfs") || name.starts_with("fsck") {
        return "命令可能修改磁盘、挂载或系统状态".into();
    }
    if [
        "dd", "fdisk", "parted", "mount", "umount", "reboot", "shutdown",
    ]
    .contains(&name)
    {
        return "命令可能修改磁盘、挂载或系统状态".into();
    }
    if name == "git" || name == "gh" {
        return "Git 写操作、发布或自定义配置需要确认".into();
    }
    if [
        "curl", "wget", "ssh", "scp", "rsync", "nc", "ncat", "telnet",
    ]
    .contains(&name)
    {
        return "网络传输或远程命令需要确认".into();
    }
    if [
        "bash", "sh", "zsh", "fish", "python", "python3", "node", "bun", "deno", "perl", "ruby",
        "awk", "eval", "source", "exec", "env", "xargs", "make", "cargo",
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
pub(super) fn known_long_options(args: &[String], allowed: &[&str]) -> bool {
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
pub(super) fn known_short_options(args: &[String], simple: &str, value_taking: &str) -> bool {
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
        for c in body.chars() {
            if value_taking.contains(c) {
                break;
            }
            if !simple.contains(c) {
                return false;
            }
        }
    }
    true
}

/// Short options that absorb the rest of their own argument as a path (or take one from
/// the next argument). `-nfFILE` is `-n -f FILE`, because letters are consumed left to
/// right as flags until one takes a value.
pub(super) fn short_path_options(command: &str) -> &'static [(char, Operation)] {
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
pub(super) fn glued_short_value(command: &str, arg: &str) -> Option<(String, Operation)> {
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
pub(super) fn expand_home(value: &str, dialect: Dialect) -> Option<String> {
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
pub(super) fn zsh_rewrite_risk(value: &str) -> bool {
    value.len() > 1 && value.starts_with('=')
}

pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The per-command option rules.
///
/// Every command in [`READ_COMMANDS`] has its own idea of which arguments are safe, and a
/// few have an option that turns a read into something else — `sort -o` writes, `find
/// -exec` runs, `file -z` unpacks. They live together, away from the parsing and path
/// checking that every command shares, so a rule can be read next to the command it
/// belongs to instead of inside a 200-line function.
///
/// `args` has already had `~` expanded and dialect rewrites resolved. `Some(reason)` asks
/// the user; `None` means the command's options were all recognised.
pub(super) fn option_problem(name: &str, args: &[String]) -> Option<String> {
    if matches!(name, "rg" | "grep")
        && args.iter().any(|arg| {
            matches!(arg.as_str(), "--follow" | "--dereference-recursive")
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg[1..].contains(if name == "rg" { 'L' } else { 'R' }))
        })
    {
        return Some("搜索会跟随符号链接，可能读取敏感文件".into());
    }
    if name == "grep" && std::env::var_os("POSIXLY_CORRECT").is_some() {
        return Some("当前环境会改变 grep 参数解析，需要确认".into());
    }
    if name == "ps" {
        let mut value = false;
        for arg in args {
            if value {
                if arg.to_ascii_lowercase().contains("env") {
                    return Some("ps 参数会显示进程环境变量".into());
                }
                value = false;
                continue;
            }
            if arg.starts_with("--") {
                let (flag, data) = arg
                    .split_once('=')
                    .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
                if ![
                    "--pid",
                    "--ppid",
                    "--user",
                    "--User",
                    "--group",
                    "--Group",
                    "--format",
                    "--sort",
                    "--no-headers",
                    "--headers",
                    "--forest",
                    "--help",
                    "--version",
                ]
                .contains(&flag)
                {
                    return Some("ps 参数未被确认为不含环境变量".into());
                }
                if let Some(data) = data {
                    if data.to_ascii_lowercase().contains("env") {
                        return Some("ps 参数会显示进程环境变量".into());
                    }
                } else {
                    value = matches!(
                        flag,
                        "--pid"
                            | "--ppid"
                            | "--user"
                            | "--User"
                            | "--group"
                            | "--Group"
                            | "--format"
                            | "--sort"
                    );
                }
            } else if let Some(body) = arg.strip_prefix('-') {
                for (index, flag) in body.char_indices() {
                    if "opPuUgGt".contains(flag) {
                        let data = &body[index + flag.len_utf8()..];
                        if data.to_ascii_lowercase().contains("env") {
                            return Some("ps 参数会显示进程环境变量".into());
                        }
                        value = data.is_empty();
                        break;
                    }
                    if !"aAdefHlLNwxy".contains(flag) {
                        return Some("ps 参数未被确认为不含环境变量".into());
                    }
                }
            } else if arg.chars().all(|c| "auxwflhT".contains(c)) && !arg.is_empty() {
                // BSD's bare `e` prints the environment; GNU `-e` only selects all processes.
            } else if !arg.chars().all(|c| c.is_ascii_digit() || c == ',') {
                return Some("ps 参数未被确认为不含环境变量".into());
            }
        }
        if value {
            return Some("ps 选项缺少参数".into());
        }
    }
    if name == "rg"
        && args.iter().any(|arg| {
            arg.starts_with("--pre=") || arg == "--pre" || arg.starts_with("--hostname-bin")
        })
    {
        return Some("搜索参数会启动外部程序".to_string());
    }
    if name == "find"
        && args.iter().any(|arg| {
            matches!(
                arg.as_str(),
                "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-delete"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        })
    {
        return Some("find 参数会执行命令、删除或写入文件".to_string());
    }
    if name == "sort"
        && args.iter().any(|arg| {
            matches!(arg.as_str(), "--output" | "--compress-program")
                || arg.starts_with("--output=")
                || arg.starts_with("--compress-program=")
                || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('o'))
        })
    {
        return Some("sort 参数会写入文件或执行外部程序".to_string());
    }
    if name == "file"
        && args.iter().any(|arg| {
            arg.starts_with("--uncompress")
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && (arg.contains('z') || arg.contains('Z')))
        })
    {
        return Some("file 解压参数可能调用外部程序".to_string());
    }

    match name {
        "grep" => {
            if !known_long_options(
                args,
                &[
                    "--extended-regexp",
                    "--fixed-strings",
                    "--basic-regexp",
                    "--perl-regexp",
                    "--regexp",
                    "--file",
                    "--ignore-case",
                    "--no-ignore-case",
                    "--word-regexp",
                    "--line-regexp",
                    "--invert-match",
                    "--no-messages",
                    "--binary-files",
                    "--text",
                    "--directories",
                    "--devices",
                    "--recursive",
                    "--include",
                    "--exclude",
                    "--exclude-from",
                    "--exclude-dir",
                    "--files-without-match",
                    "--files-with-matches",
                    "--count",
                    "--max-count",
                    "--byte-offset",
                    "--line-number",
                    "--with-filename",
                    "--no-filename",
                    "--label",
                    "--only-matching",
                    "--quiet",
                    "--silent",
                    "--line-buffered",
                    "--null",
                    "--null-data",
                    "--before-context",
                    "--after-context",
                    "--context",
                    "--group-separator",
                    "--no-group-separator",
                    "--color",
                    "--colour",
                    "--help",
                    "--version",
                ],
            ) || !known_short_options(args, "EFGPiwyxvnsbHhZoclLqrsIazUV", "efmABCdD")
            {
                return Some("grep 参数未被确认为只读".into());
            }
        }
        "sort" => {
            if !known_long_options(
                args,
                &[
                    "--numeric-sort",
                    "--general-numeric-sort",
                    "--human-numeric-sort",
                    "--version-sort",
                    "--reverse",
                    "--unique",
                    "--stable",
                    "--ignore-case",
                    "--ignore-leading-blanks",
                    "--field-separator",
                    "--key",
                    "--check",
                    "--help",
                    "--version",
                ],
            ) || !known_short_options(args, "nNgGhHrVuMsbfcdm", "kt")
            {
                return Some("sort 参数未被确认为只读".to_string());
            }
        }
        "file" => {
            if !known_long_options(
                args,
                &[
                    "--brief",
                    "--mime",
                    "--mime-type",
                    "--mime-encoding",
                    "--dereference",
                    "--separator",
                    "--keep-going",
                    "--version",
                    "--help",
                ],
            ) || !known_short_options(args, "bikLNprsv0", "fm")
            {
                return Some("file 参数未被确认为只读".to_string());
            }
        }
        "rg" => {
            if !known_long_options(
                args,
                &[
                    "--files",
                    "--hidden",
                    "--no-ignore",
                    "--no-ignore-vcs",
                    "--no-ignore-parent",
                    "--no-ignore-global",
                    "--glob",
                    "--iglob",
                    "--type",
                    "--type-not",
                    "--type-list",
                    "--line-number",
                    "--no-line-number",
                    "--count",
                    "--count-matches",
                    "--with-filename",
                    "--no-filename",
                    "--ignore-case",
                    "--smart-case",
                    "--case-sensitive",
                    "--fixed-strings",
                    "--word-regexp",
                    "--line-regexp",
                    "--invert-match",
                    "--max-count",
                    "--max-depth",
                    "--max-filesize",
                    "--context",
                    "--before-context",
                    "--after-context",
                    "--color",
                    "--colors",
                    "--heading",
                    "--no-heading",
                    "--sort",
                    "--sortr",
                    "--stats",
                    "--json",
                    "--only-matching",
                    "--replace",
                    "--trim",
                    "--pcre2",
                    "--multiline",
                    "--multiline-dotall",
                    "--follow",
                    "--files-without-match",
                    "--files-with-matches",
                    "--null",
                    "--null-data",
                    "--text",
                    "--regexp",
                    "--file",
                    "--quiet",
                    "--encoding",
                    "--no-messages",
                    "--version",
                    "--help",
                    "--crlf",
                ],
            ) || !known_short_options(args, "nHhIilLovswxUaFcqSPz0u", "egftTrABCm")
            {
                return Some("rg 参数未被确认为只读".to_string());
            }
        }
        "find" => {
            let known: HashSet<&str> = [
                "-name",
                "-iname",
                "-path",
                "-ipath",
                "-regex",
                "-iregex",
                "-type",
                "-maxdepth",
                "-mindepth",
                "-print",
                "-print0",
                "-ls",
                "-empty",
                "-size",
                "-mtime",
                "-mmin",
                "-atime",
                "-amin",
                "-ctime",
                "-cmin",
                "-newer",
                "-anewer",
                "-cnewer",
                "-user",
                "-group",
                "-perm",
                "-a",
                "-and",
                "-o",
                "-or",
                "-not",
                "-true",
                "-false",
                "-readable",
                "-writable",
                "-executable",
                "-P",
                "-H",
                "-L",
            ]
            .into_iter()
            .collect();
            let looks_numeric = |arg: &str| {
                arg.strip_prefix('-').is_some_and(|rest| {
                    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
                })
            };
            if args.iter().any(|arg| {
                arg.starts_with('-') && !known.contains(arg.as_str()) && !looks_numeric(arg)
            }) {
                return Some("find 参数未被确认为只读".to_string());
            }
        }
        "sed" => {
            let rest: Vec<&String> = if args.first().map(String::as_str) == Some("-n") {
                args[1..].iter().collect()
            } else {
                args.iter().collect()
            };
            let script_ok = rest
                .first()
                .is_some_and(|script| is_line_range_print(script));
            if !script_ok || rest[1..].iter().any(|arg| arg.starts_with('-')) {
                return Some(
                    "只自动放行 sed 的行范围打印；编辑、脚本及其他参数需要确认".to_string(),
                );
            }
        }
        "date" => {
            // `date -s` sets the system clock and `date -f` reads a file as input; every
            // other option only formats the current time, which cannot change anything.
            // This one is a block-list rather than an allow-list because `date` takes a
            // format string as a bare argument (`date +%Y-%m-%d`), so an allow-list would
            // have to enumerate every date format a caller might want.
            let sets_or_reads = args.iter().any(|arg| {
                matches!(arg.as_str(), "-s" | "--set" | "-f" | "--file")
                    || arg.starts_with("--set=")
                    || arg.starts_with("--file=")
                    || (arg.starts_with("-s") && arg.len() > 2 && !arg.starts_with("--"))
            });
            if sets_or_reads {
                return Some("date 参数会设置系统时间或读取文件".to_string());
            }
            if !known_long_options(
                args,
                &[
                    "--utc",
                    "--universal",
                    "--iso-8601",
                    "--rfc-3339",
                    "--rfc-email",
                    "--help",
                    "--version",
                ],
            ) || !known_short_options(args, "uR", "I")
                || args
                    .iter()
                    .any(|arg| !arg.starts_with('-') && !arg.starts_with('+'))
            {
                return Some("date 参数未被确认为仅显示时间".to_string());
            }
        }
        _ => {}
    }
    None
}

pub(super) fn vet_segment(words: &[Word], cwd: &Path, dialect: Dialect) -> Assessment {
    let Some((command, raw_args)) = words.split_first() else {
        return Assessment::ask("未找到可执行命令");
    };
    let name = command
        .value
        .rsplit('/')
        .next()
        .unwrap_or(&command.value)
        .to_string();
    if name.is_empty() {
        return Assessment::ask("命令为空或格式不正确");
    }
    // `FOO=bar cmd` changes how the command runs, so it is never auto-approved.
    if let Some((head, _)) = command.value.split_once('=')
        && !head.is_empty()
        && head
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
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

    if let Some(reason) = option_problem(&name, &args) {
        return Assessment::ask(reason);
    }

    if let Some(decision) = path_problem(&name, &args, cwd) {
        return decision;
    }

    if name == "git" {
        return vet_git(&executable, &args, cwd);
    }
    if matches!(name.as_str(), "rg" | "grep") {
        let mut pending = false;
        let mut boundary = args.len();
        for (index, arg) in args.iter().enumerate() {
            if pending {
                pending = false;
                continue;
            }
            if arg == "--" {
                boundary = index;
                break;
            }
            if arg.starts_with("--") {
                pending = !arg.contains('=')
                    && matches!(
                        arg.as_str(),
                        "--regexp"
                            | "--file"
                            | "--glob"
                            | "--iglob"
                            | "--type"
                            | "--type-not"
                            | "--replace"
                            | "--max-count"
                            | "--max-columns"
                            | "--max-depth"
                            | "--max-filesize"
                            | "--context"
                            | "--before-context"
                            | "--after-context"
                            | "--colors"
                            | "--sort"
                            | "--sortr"
                            | "--encoding"
                            | "--include"
                            | "--exclude"
                            | "--exclude-dir"
                            | "--exclude-from"
                            | "--label"
                            | "--binary-files"
                            | "--directories"
                            | "--devices"
                            | "--group-separator"
                    )
                    || (name == "rg" && arg == "--color");
            } else if let Some(body) = arg.strip_prefix('-') {
                for (offset, flag) in body.char_indices() {
                    if (if name == "rg" {
                        "efgtTrABCm"
                    } else {
                        "efABCmdD"
                    })
                    .contains(flag)
                    {
                        pending = offset + flag.len_utf8() == body.len();
                        break;
                    }
                }
            }
        }
        args.splice(boundary..boundary, search_exclusions(name == "rg"));
        if name == "rg" {
            args.insert(0, "--no-config".into());
        }
    }
    let mut rewritten = vec![executable.to_string_lossy().to_string()];
    rewritten.extend(args);
    Assessment::Allow {
        safe_command: Some(
            rewritten
                .iter()
                .map(|w| shell_quote(w))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

/// Check every argument a literal path could hide in, including option values such as
/// `--file=...`.
///
/// `cat id_rsa` matters as much as `cat .ssh/id_rsa`, and a name that is only sensitive
/// inside the home directory is judged against `cwd` — which for a `cd`-prefixed line is
/// where that segment will really run. Printing commands
/// ([`DATA_ARG_COMMANDS`]) carry data rather than paths and are exempt.
///
/// `Some(decision)` means an argument was refused; `None` means they all checked out.
pub(super) fn path_problem(name: &str, args: &[String], cwd: &Path) -> Option<Assessment> {
    if matches!(name, "rg" | "grep") {
        return search_path_problem(name, args, cwd);
    }
    // Check every argument a literal path could hide in, including option values such as
    // `--file=...`. `cat id_rsa` matters as much as `cat .ssh/id_rsa`, and printing
    // commands carry data rather than paths, so they are exempt.
    if !DATA_ARG_COMMANDS.contains(&name) {
        let mut pending: Option<Operation> = None;
        for arg in args {
            if arg == "--" {
                pending = None;
                continue;
            }
            if let Some(operation) = pending
                && !arg.starts_with('-')
            {
                let decision = assess_path(operation, arg, cwd);
                pending = None;
                if !decision.allows() {
                    return Some(decision);
                }
                continue;
            }
            pending = None;
            if arg.starts_with('-')
                && !arg.starts_with("--")
                && let Some((value, operation)) = glued_short_value(name, arg)
            {
                if value.is_empty() {
                    pending = Some(operation);
                    continue;
                }
                let decision = assess_path(operation, &value, cwd);
                if !decision.allows() {
                    return Some(decision);
                }
                continue;
            }
            let values: Vec<&str> =
                if arg.starts_with('-') && !arg.starts_with("-/") && !arg.starts_with("-~") {
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
                    return Some(decision);
                }
            }
        }
    }

    /// Search patterns and formatting options are data; pattern files and operands are paths.
    fn search_path_problem(name: &str, args: &[String], cwd: &Path) -> Option<Assessment> {
        let default_root = assess_path(Operation::Read, &cwd.to_string_lossy(), cwd);
        if !default_root.allows() {
            return Some(default_root);
        }
        let mut pattern = false;
        let mut files_only = false;
        let mut positional = false;
        let mut pending: Option<bool> = None; // true: path, false: literal data
        for arg in args {
            if let Some(path) = pending.take() {
                if path {
                    let decision = assess_path(Operation::Read, arg, cwd);
                    if !decision.allows() {
                        return Some(decision);
                    }
                }
                continue;
            }
            if !positional && arg == "--" {
                positional = true;
                continue;
            }
            if !positional && arg.starts_with("--") {
                let (flag, value) = arg
                    .split_once('=')
                    .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
                if flag == "--files" {
                    files_only = true;
                }
                if matches!(flag, "--regexp" | "--file") {
                    pattern = true;
                }
                let path = matches!(flag, "--file" | "--exclude-from");
                let data = matches!(
                    flag,
                    "--regexp"
                        | "--glob"
                        | "--iglob"
                        | "--type"
                        | "--type-not"
                        | "--replace"
                        | "--max-count"
                        | "--max-columns"
                        | "--max-depth"
                        | "--max-filesize"
                        | "--encoding"
                        | "--color"
                        | "--colour"
                        | "--colors"
                        | "--sort"
                        | "--sortr"
                        | "--binary-files"
                        | "--directories"
                        | "--devices"
                        | "--group-separator"
                        | "--before-context"
                        | "--after-context"
                        | "--context"
                        | "--include"
                        | "--exclude"
                        | "--exclude-dir"
                        | "--label"
                );
                if let Some(value) = value {
                    if !data {
                        let decision = assess_path(Operation::Read, value, cwd);
                        if !decision.allows() {
                            return Some(decision);
                        }
                    }
                } else if (path || data)
                    && (name != "grep" || !matches!(flag, "--color" | "--colour"))
                {
                    pending = Some(path);
                }
                continue;
            }
            if !positional && arg.starts_with('-') && arg != "-" {
                let value_flags = if name == "rg" {
                    "efgtrTABCm"
                } else {
                    "efABCmdD"
                };
                for (index, flag) in arg[1..].char_indices() {
                    if !value_flags.contains(flag) {
                        continue;
                    }
                    if matches!(flag, 'e' | 'f') {
                        pattern = true;
                    }
                    let value = &arg[1 + index + flag.len_utf8()..];
                    if value.is_empty() {
                        pending = Some(flag == 'f');
                    } else if flag == 'f' {
                        let decision = assess_path(Operation::Read, value, cwd);
                        if !decision.allows() {
                            return Some(decision);
                        }
                    }
                    break;
                }
                continue;
            }
            if !pattern && !files_only {
                pattern = true;
                continue;
            }
            let decision = assess_path(Operation::Read, arg, cwd);
            if !decision.allows() {
                return Some(decision);
            }
        }
        pending.map(|_| Assessment::ask("搜索选项缺少参数"))
    }
    None
}

/// `sed -n '1,10p' file`, `sed -n '5p' file`, `sed '1,$p' file`. Nothing else.
pub(super) fn vet_git(executable: &Path, args: &[String], cwd: &Path) -> Assessment {
    let args = args
        .strip_prefix(&["--no-pager".to_string()])
        .unwrap_or(args);
    // Scan for the options that make git execute something else *before* choosing a
    // subcommand, because `git -c core.pager=evil log` would otherwise be reported as an
    // unknown subcommand and the real hazard would go unnamed.
    let dangerous = args.iter().any(|arg| {
        let long_bad = [
            "--ext-diff",
            "--textconv",
            "--output",
            "--config-env",
            "--exec-path",
        ]
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
    if ![
        "status",
        "diff",
        "log",
        "show",
        "rev-parse",
        "ls-files",
        "ls-tree",
    ]
    .contains(&subcommand.as_str())
    {
        return Assessment::ask(command_reason("git"));
    }
    if !known_long_options(
        options,
        &[
            "--stat",
            "--shortstat",
            "--numstat",
            "--name-only",
            "--name-status",
            "--check",
            "--summary",
            "--cached",
            "--staged",
            "--no-index",
            "--patch",
            "--no-patch",
            "--color",
            "--no-color",
            "--word-diff",
            "--word-diff-regex",
            "--ignore-space-at-eol",
            "--ignore-space-change",
            "--ignore-all-space",
            "--ignore-blank-lines",
            "--exit-code",
            "--quiet",
            "--no-ext-diff",
            "--no-textconv",
            "--unified",
            "--oneline",
            "--decorate",
            "--graph",
            "--all",
            "--branches",
            "--tags",
            "--remotes",
            "--max-count",
            "--pretty",
            "--format",
            "--since",
            "--until",
            "--author",
            "--committer",
            "--grep",
            "--date",
            "--abbrev-commit",
            "--reverse",
            "--first-parent",
            "--follow",
            "--no-merges",
            "--merges",
            "--short",
            "--porcelain",
            "--branch",
            "--untracked-files",
            "--ignored",
            "--ignore-submodules",
            "--show-toplevel",
            "--git-dir",
            "--absolute-git-dir",
            "--show-prefix",
            "--is-inside-work-tree",
            "--verify",
            "--abbrev-ref",
            "--symbolic-full-name",
            "--sq",
            "--end-of-options",
            "--git-path",
            "--stage",
            "--deleted",
            "--modified",
            "--others",
            "--exclude-standard",
            "--error-unmatch",
            "--full-name",
            "--eol",
            "--long",
            "--full-tree",
            "--object-only",
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
        safe_command: Some(
            rewritten
                .iter()
                .map(|w| shell_quote(w))
                .collect::<Vec<_>>()
                .join(" "),
        ),
    }
}

/// `-[0-9]+`, `-n[0-9]*`, `-[pswbrzR]+`, `-U[0-9]*`, `-M[0-9]*`, `-C[0-9]*`, `-S...`,
/// `-G...` — the short flags git shortens but never uses for writes.
pub(super) fn git_short_options_ok(options: &[String]) -> bool {
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

/// Recognise `cd`, and resolve the directory the *rest* of the line will run in.
///
/// `cd` itself does nothing — it cannot read or write anything. What makes it worth
/// handling is the segments after it: `cd /etc && cat shadow` reads `/etc/shadow`, and a
/// check that resolved `shadow` against the project directory would call it safe while the
/// shell read something else. So the tracked directory moves with the `cd`, and every later
/// segment is vetted against where it will really run.
///
/// `None` means this is not a `cd`. `Some(Err)` asks the user.
pub(super) fn cd_target(
    words: &[Word],
    cwd: &Path,
    dialect: Dialect,
) -> Option<Result<PathBuf, String>> {
    let (command, args) = words.split_first()?;
    if command.value != "cd" || command.quoted {
        return None;
    }
    let raw = match args {
        // A bare `cd` goes home, which is knowable.
        [] => "~".to_string(),
        [only] => {
            // `cd -` is the previous directory: it depends on what ran before, which this
            // check cannot see.
            if only.value == "-" {
                return Some(Err("cd - 的目标取决于更早的命令，无法确认".into()));
            }
            only.value.clone()
        }
        _ => return Some(Err("cd 的参数未被确认为单个目录".into())),
    };
    let Some(expanded) = expand_home(&raw, dialect) else {
        return Some(Err("cd 的参数含无法可靠解析的 shell 展开".into()));
    };
    let lexical = resolve_tool_path(&expanded, cwd);
    let Ok(target) = canonical_path(&lexical, 0) else {
        return Some(Err("cd 的目标路径无法可靠解析".into()));
    };
    if !target.is_dir() {
        return Some(Err("cd 的目标不是目录".into()));
    }
    if normalize(&lexical) != target {
        // The shell keeps the path it was handed while the files resolve through the link,
        // so `cd link` followed by `cat ..` means one directory to zsh and another to this
        // check. Refusing keeps the checked path and the executed path the same one.
        return Some(Err("cd 的目标经过符号链接，其后的相对路径无法确认".into()));
    }
    Some(Ok(target))
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
    // Where the segments after this one will run. It only moves for a `cd`, and it is what
    // keeps a relative path meaning the same thing here as it does to the shell.
    let mut here = cwd.to_path_buf();
    for (index, segment) in segments.iter().enumerate() {
        if let Some(target) = cd_target(&segment.words, &here, dialect) {
            // Pipelines and failure branches do not share one predictable working directory.
            if index != 0
                || segment.operator.as_deref().is_some_and(|op| op != "&&")
                || segments.iter().any(|s| s.operator.as_deref() == Some("||"))
            {
                return Assessment::ask("只能自动确认命令开头通过 && 连接的目录切换");
            }
            let target = match target {
                Ok(target) => target,
                Err(reason) => return Assessment::ask(reason),
            };
            // Rewritten to the resolved absolute path, so the directory the shell enters is
            // the one that was checked rather than a relative name that could mean another.
            let mut rewritten = format!("cd {}", shell_quote(&target.to_string_lossy()));
            for redirect in &segment.redirects {
                rewritten.push(' ');
                rewritten.push_str(redirect);
            }
            normalized.push(rewritten);
            if let Some(operator) = &segment.operator {
                normalized.push(operator.clone());
            }
            here = target;
            continue;
        }
        let decision = vet_segment(&segment.words, &here, dialect);
        match decision {
            Assessment::Ask { reason } => return Assessment::ask(reason),
            Assessment::Allow { safe_command } => {
                let mut rewritten = safe_command.unwrap_or_else(|| {
                    segment
                        .words
                        .iter()
                        .map(|word| shell_quote(&word.value))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                // The descriptor redirects travel with the segment. They are part of the
                // command the user sees in the transcript, and dropping them would change
                // what runs; they were checked by the parser, which refuses any redirection
                // that names a file.
                for redirect in &segment.redirects {
                    rewritten.push(' ');
                    rewritten.push_str(redirect);
                }
                normalized.push(rewritten);
                if let Some(operator) = &segment.operator {
                    normalized.push(operator.clone());
                }
            }
        }
    }
    Assessment::Allow {
        safe_command: Some(normalized.join(" ")),
    }
}
