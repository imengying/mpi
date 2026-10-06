//! Policy behaviour, asserted on real commands and paths.

use std::path::{Path, PathBuf};

use super::command::*;
use super::path::*;
use super::{Assessment, Dialect, Operation, assess_command, assess_tool, refusal};

use super::*;

#[test]
fn process_environments_need_approval() {
    for command in [
        "ps eww -p 1",
        "ps auxe",
        "ps --environment",
        "ps -eo environ",
        "ps --format=pid,env",
        "cat /proc/1/environ",
        "grep --fi .env needle .",
    ] {
        assert!(!allows(command), "{command}");
    }
    for command in ["ps aux", "ps -eo pid,cmd", "ps -p 1 -o pid,comm"] {
        assert!(allows(command), "{command}: {}", reason(command));
    }
    assert!(!allows("rg --follow needle ."));
    assert!(!allows("grep -R needle ."));
}

#[cfg(unix)]
#[test]
fn parent_components_are_resolved_after_symlinks() {
    let dir = std::env::temp_dir().join(format!("pi-path-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(dir.join(".ssh/child")).unwrap();
    std::fs::write(dir.join(".ssh/fixture.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(dir.join(".ssh/child"), dir.join("alias")).unwrap();
    assert_eq!(
        canonical_path(&resolve_tool_path("alias/../fixture.txt", &dir), 0).unwrap(),
        dir.join(".ssh/fixture.txt")
    );
    for operation in [Operation::Read, Operation::Write] {
        assert!(!assess_path(operation, "alias/../fixture.txt", &dir).allows());
        assert!(!assess_path(operation, "alias/../new.txt", &dir).allows());
    }
    assert!(!assess_command("cat alias/../fixture.txt", &dir, Dialect::Zsh).allows());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn executable_targets_accept_only_known_system_aliases() {
    for target in [
        "/usr/bin/which",
        "/bin/which",
        "/usr/bin/which.debianutils",
        "/bin/which.debianutils",
    ] {
        assert!(
            trusted_executable_target("which", Path::new(target)),
            "{target}"
        );
    }
    for (name, target) in [
        ("which", "/tmp/which.debianutils"),
        ("which", "/usr/local/bin/which.debianutils"),
        ("which", "/usr/bin/rm"),
        ("which", "/usr/bin/which.unknown"),
        ("cat", "/usr/bin/which.debianutils"),
        ("cat", "/usr/bin/rm"),
    ] {
        assert!(
            !trusted_executable_target(name, Path::new(target)),
            "{name}: {target}"
        );
    }
}

#[test]
fn search_patterns_are_data_but_pattern_files_are_checked() {
    for command in [
        "rg '.env' src",
        "rg -ne '.ssh/id_rsa' src",
        "grep -e /etc/shadow src/main.rs",
        "git --no-pager diff --stat",
        "pwd;",
    ] {
        assert!(allows(command), "{command}: {}", reason(command));
    }
    for command in [
        "rg -nf.env src",
        "rg -n -f .env src",
        "grep --exclude-from=.env hello .",
        "rg --files -- .env",
        "date --se=20260101",
        "date 010100002026",
    ] {
        assert!(!allows(command), "{command}");
    }
}

#[test]
fn directory_changes_cannot_escape_pipeline_or_failure_scopes() {
    for command in [
        "cd /tmp | cat .env",
        "cd /tmp || cat .env",
        "echo ok | cd /tmp; cat .env",
    ] {
        assert!(
            !assess_command(command, Path::new("/"), Dialect::Zsh).allows(),
            "{command}"
        );
    }
    assert!(assess_command("cd /tmp && pwd", Path::new("/"), Dialect::Zsh).allows());
}

#[cfg(unix)]
#[test]
fn an_executable_alias_cannot_turn_a_reader_into_a_writer() {
    let dir = std::env::temp_dir().join(format!("pi-executable-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&dir).unwrap();
    let alias = dir.join("cat");
    std::os::unix::fs::symlink("/usr/bin/rm", &alias).unwrap();
    assert!(trusted_executable(alias.to_str().unwrap()).is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

fn cwd() -> PathBuf {
    std::env::temp_dir().join("pi-policy-cwd")
}

fn allows(command: &str) -> bool {
    assess_command(command, &cwd(), Dialect::Zsh).allows()
}

fn reason(command: &str) -> String {
    assess_command(command, &cwd(), Dialect::Zsh)
        .reason()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn simple_read_commands_are_rewritten_to_absolute_paths() {
    let decision = assess_command("cat notes.txt", &cwd(), Dialect::Zsh);
    match decision {
        Assessment::Allow {
            safe_command: Some(command),
        } => {
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
    // A heredoc redirects stdin, so it is a real redirection and asks.
    assert!(reason("python3 - <<EOF").contains("重定向"));
}

#[test]
fn descriptor_redirects_do_not_ask() {
    // `2>&1` and `2>/dev/null` move or discard a descriptor. They are in a large share
    // of the commands a model writes out of habit, and refusing them is what pushed a
    // vetted `cargo test 2>&1` into a `python3 - <<EOF`.
    assert!(allows("ls 2>&1"));
    assert!(allows("cat notes.txt 2>/dev/null"));
    assert!(allows("cat notes.txt > /dev/null"));
    assert!(allows("cat notes.txt >&2"));
    assert!(allows("cat notes.txt 2>&-"));
    assert!(allows("ls 2>/dev/null; ls"));
    // A redirect that names a file is still a write, and still asks.
    assert!(reason("cat a > b").contains("重定向"));
    assert!(reason("cat a >> b").contains("重定向"));
    assert!(reason("cat a 2> b").contains("重定向"));
    assert!(reason("cat a >& b").contains("重定向"));
    assert!(reason("cat < a").contains("重定向"));
    assert!(reason("echo a>f").contains("重定向"));
}

#[test]
fn the_allowed_rewrite_keeps_the_descriptor_redirect() {
    // The command that runs has to be the command that was checked, or the transcript
    // shows one thing and the shell does another.
    match assess_command("cat notes.txt 2>&1", &cwd(), Dialect::Zsh) {
        Assessment::Allow {
            safe_command: Some(command),
        } => {
            assert!(command.ends_with("2>&1"), "{command}");
        }
        other => panic!("expected allow with rewrite, got {other:?}"),
    }
}

#[test]
fn inspection_commands_do_not_ask_but_keep_their_dangerous_options() {
    // These cannot change anything, and asking about them every time was a question
    // with one answer.
    for command in [
        "which cargo",
        "date",
        "date +%Y-%m-%d",
        "nproc",
        "uptime",
        "free -h",
        "ps aux",
        "id",
        "whoami",
        "basename /a/b",
        "dirname /a/b",
        "column -t notes.txt",
        "lscpu",
        "seq 1 5",
    ] {
        assert!(
            allows(command),
            "`{command}` should not ask: {}",
            reason(command)
        );
    }
    // `basename` prints its argument rather than opening it, so a sensitive-looking
    // name is not a leak here.
    assert!(allows("basename /etc/shadow"));
    // The options that do more than read still ask.
    assert!(reason("date -s 2020-01-01").contains("系统时间"));
    assert!(reason("date --set=now").contains("系统时间"));
    assert!(reason("date -f /etc/passwd").contains("系统时间"));
    // And a path argument is still checked, so these are not a way to read a secret.
    assert!(reason("column -t /etc/shadow").contains("凭据"));
    assert!(reason("lscpu /etc/shadow").contains("凭据"));
    // `env` prints every variable, the provider key included, so it never joins this
    // group however convenient it would be.
    assert!(!allows("env"));
}

#[test]
fn cd_moves_what_later_segments_are_checked_against() {
    let cwd = cwd();
    std::fs::create_dir_all(cwd.join("sub")).unwrap();
    // Inside the tree it is an ordinary helper, and the path it is given is resolved
    // against the directory the shell will really be in.
    assert!(allows("cd sub && ls"));
    assert!(allows("cd . && ls"));
    // Anything that could mean a different directory to the shell than to this check
    // asks: `-` depends on earlier commands, two arguments is not a cd, a symlinked
    // target changes how `..` resolves, and a missing directory cannot be checked.
    assert!(reason("cd -").contains("更早的命令"));
    assert!(reason("cd a b").contains("单个目录"));
    assert!(reason("cd /nonexistent-pi-xyz").contains("不是目录"));
}

#[test]
fn cd_cannot_launder_a_sensitive_path() {
    // The hole this guards: `cd ~` then a name that is only sensitive inside the home
    // directory. Judged against the project it looks like any other relative name, so
    // a check that forgot to move with the `cd` would call this safe while the shell
    // read the file next to the API keys.
    let home = home();
    let sensitive = home.join(".pi/config.json");
    assert!(
        reason(&format!(
            "cd {} && cat .pi/config.json",
            shell_quote(&home.to_string_lossy())
        ))
        .contains("凭据"),
        "a cd was used to launder a path into a sensitive directory"
    );
    assert!(!allows("cd ~ && cat .ssh/id_rsa"));
    // And the same name outside home is still an ordinary file.
    assert!(allows("cat .pi/config.json"));
    let _ = sensitive;
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
        Assessment::Allow {
            safe_command: Some(command),
        } => {
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
    assert!(
        assess_path(Operation::Write, "/tmp/elsewhere.txt", &cwd)
            .reason()
            .is_some()
    );
    assert!(assess_path(Operation::Write, "notes.txt", &cwd).allows());
    assert!(
        assess_path(Operation::Write, ".git/config", &cwd)
            .reason()
            .is_some()
    );
    assert!(
        assess_path(Operation::Write, "AGENTS.md", &cwd)
            .reason()
            .is_some()
    );
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
    assert_eq!(
        text,
        "未获得用户授权，操作未执行（命令会删除文件，需要确认目标）"
    );
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
