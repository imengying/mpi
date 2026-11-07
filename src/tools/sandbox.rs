//! Execution isolation, separate from policy and the UI's approval decisions.
//! Linux workers get a fresh OS thread: restrictions must never reach a Tokio pool thread.
use super::{ToolContext, ToolOutput};
use crate::auth::guard::ApprovedCall;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    ReadOnly,
    Workspace,
    Unrestricted,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "只读文件 · 无网络",
            Self::Workspace => "工作目录可写 · 无网络",
            Self::Unrestricted => "不限制文件或网络访问",
        }
    }
}

thread_local! {
    static OUTPUT_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn output_dir() -> PathBuf {
    OUTPUT_DIR
        .with(|dir| dir.borrow().clone())
        .unwrap_or_else(std::env::temp_dir)
}

pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        *std::sync::OnceLock::get_or_init(&AVAILABLE, || linux::ruleset().is_ok())
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}
#[cfg(target_os = "linux")]
static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub async fn execute(name: &str, call: ApprovedCall, context: ToolContext) -> ToolOutput {
    if call.scope == Scope::Unrestricted {
        return super::execute(name, &call.arguments, &context).await;
    }
    let temp = std::env::temp_dir().join(format!("pi-sandbox-{}", uuid::Uuid::now_v7()));
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if let Err(err) = builder.create(&temp) {
        return ToolOutput::error_for(
            name,
            &call.arguments,
            format!("无法创建隔离输出目录：{err}"),
        );
    }
    let cleanup_dir = temp.clone();
    let name = name.to_string();
    let failure_name = name.clone();
    let failure_arguments = call.arguments.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let worker = std::thread::Builder::new()
        .name("pi-tool".into())
        .spawn(move || {
            let result = (|| -> anyhow::Result<ToolOutput> {
                let thread_temp = temp.clone();
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .on_thread_start(move || {
                        OUTPUT_DIR.with(|dir| *dir.borrow_mut() = Some(thread_temp.clone()))
                    })
                    .build()?;
                OUTPUT_DIR.with(|dir| *dir.borrow_mut() = Some(temp.clone()));
                restrict(call.scope, &context.cwd, &temp)?;
                Ok(runtime.block_on(async {
                    tokio::select! {
                        biased;
                        _ = cancel_rx => ToolOutput::error("操作已停止"),
                        output = super::execute(&name, &call.arguments, &context) => output,
                    }
                }))
            })();
            let output = result.unwrap_or_else(|err| {
                ToolOutput::error_for(
                    &name,
                    &call.arguments,
                    format!(
                        "无法启用执行隔离：{err}。操作未执行；如需在隔离之外运行，请重新申请授权。"
                    ),
                )
            });
            let _ = result_tx.send(output);
        });
    // Cleanup stays outside the restricted worker, including when its caller is cancelled.
    let completion = tokio::spawn(async move {
        let result = result_rx.await;
        let _ = std::fs::remove_dir(cleanup_dir);
        result
    });
    let result = match worker {
        Ok(_) => completion
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(|| {
                ToolOutput::error_for(&failure_name, &failure_arguments, "工具执行线程退出")
            }),
        Err(err) => ToolOutput::error_for(
            &failure_name,
            &failure_arguments,
            format!("无法创建工具线程：{err}"),
        ),
    };
    drop(cancel_tx);
    result
}

fn restrict(scope: Scope, cwd: &Path, temp: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::restrict(scope, cwd, temp)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (scope, cwd, temp);
        anyhow::bail!("此平台尚无可用隔离后端")
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreated, RulesetCreatedAttr, RulesetStatus,
    };

    pub(super) fn ruleset() -> anyhow::Result<RulesetCreated> {
        Ok(Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V5))?
            .scope(landlock::Scope::Signal)?
            .scope(landlock::Scope::AbstractUnixSocket)?
            .create()?)
    }

    pub(super) fn restrict(scope: Scope, cwd: &Path, temp: &Path) -> anyhow::Result<()> {
        let read = AccessFs::from_read(ABI::V5);
        let write = AccessFs::from_all(ABI::V5);
        let mut rules = ruleset()?
            .add_rule(PathBeneath::new(PathFd::new("/")?, read))?
            .add_rule(PathBeneath::new(PathFd::new(temp)?, write))?
            .add_rule(PathBeneath::new(
                PathFd::new("/dev/null")?,
                AccessFs::ReadFile | AccessFs::WriteFile,
            ))?;
        if scope == Scope::Workspace {
            rules = rules.add_rule(PathBeneath::new(PathFd::new(cwd)?, write))?;
        }
        // Prevent networking, cross-process access, and bypasses through mount/handle/io_uring.
        let mut denied = vec![
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_ptrace,
            libc::SYS_process_vm_writev,
            libc::SYS_process_vm_readv,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_open_by_handle_at,
            libc::SYS_bpf,
            libc::SYS_io_uring_setup,
            libc::SYS_setns,
            libc::SYS_unshare,
            libc::SYS_fchmod,
            libc::SYS_fchmodat,
            libc::SYS_fchown,
            libc::SYS_fchownat,
            libc::SYS_setxattr,
            libc::SYS_lsetxattr,
            libc::SYS_fsetxattr,
            libc::SYS_removexattr,
            libc::SYS_lremovexattr,
            libc::SYS_fremovexattr,
        ];
        #[cfg(target_arch = "x86_64")]
        denied.extend([libc::SYS_chmod, libc::SYS_chown, libc::SYS_lchown]);
        // fchmodat2 is syscall 452 on all supported Linux architectures.
        denied.push(452);
        let filter: seccompiler::BpfProgram = seccompiler::SeccompFilter::new(
            denied
                .into_iter()
                .map(|number| (number, Vec::new()))
                .collect(),
            seccompiler::SeccompAction::Allow,
            seccompiler::SeccompAction::Errno(libc::EPERM as u32),
            std::env::consts::ARCH.try_into()?,
        )?
        .try_into()?;
        let status = rules.restrict_self()?;
        anyhow::ensure!(
            status.ruleset == RulesetStatus::FullyEnforced,
            "文件隔离未完整生效"
        );
        seccompiler::apply_filter(&filter)?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn kernel_restricts_writes_symlink_escapes_and_network() {
        if !available() {
            eprintln!("kernel has no supported sandbox");
            return;
        }
        let root = super::super::temp_dir("sandbox");
        let workspace = root.join("workspace");
        let temp = root.join("temp");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&temp).unwrap();
        std::fs::write(root.join("outside"), "original").unwrap();
        std::os::unix::fs::symlink(root.join("outside"), workspace.join("link")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        for scope in [Scope::ReadOnly, Scope::Workspace] {
            let workspace = workspace.clone();
            let temp = temp.clone();
            let outside = root.join("outside");
            std::thread::spawn(move || {
                restrict(scope, &workspace, &temp).unwrap();
                assert_eq!(std::fs::read_to_string(&outside).unwrap(), "original");
                assert!(std::fs::write(&outside, "bad").is_err());
                use std::os::unix::fs::PermissionsExt;
                assert!(
                    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o777))
                        .is_err()
                );
                assert!(std::fs::write(workspace.join("link"), "bad").is_err());
                assert_eq!(
                    std::fs::write(workspace.join("inside"), "ok").is_ok(),
                    scope == Scope::Workspace
                );
                assert!(std::fs::write(temp.join("output"), "ok").is_ok());
                assert!(std::net::TcpStream::connect(address).is_err());
            })
            .join()
            .unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.join("outside")).unwrap(),
            "original"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancellation_reaches_sandboxed_shell_descendants() {
        if !available() {
            return;
        }
        let cwd = super::super::temp_dir("sandbox-cancel");
        let context = ToolContext {
            cwd: cwd.clone(),
            shell: "/bin/sh".into(),
            observed: Default::default(),
        };
        let task = tokio::spawn(async move {
            execute("bash", ApprovedCall { scope: Scope::Workspace,
                arguments: serde_json::json!({"command":"sh -c 'echo ready > ready; sleep 0.5; echo late > marker' & wait"})
            }, context).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !cwd.join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(!cwd.join("marker").exists());
        std::fs::remove_dir_all(cwd).unwrap();
    }
}
