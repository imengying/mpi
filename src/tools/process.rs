//! Own the process group as well as the child: dropping a tool cancels its descendants.

use std::process::{ExitStatus, Stdio};

use super::output::Capture;

pub(super) struct Child {
    child: tokio::process::Child,
    #[cfg(unix)]
    group: Option<rustix::process::Pid>,
}

impl Child {
    pub fn spawn(command: &mut tokio::process::Command) -> std::io::Result<Self> {
        command.stdin(Stdio::null()).kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn()?;
        Ok(Self {
            #[cfg(unix)]
            group: child
                .id()
                .and_then(|id| rustix::process::Pid::from_raw(id as i32)),
            child,
        })
    }

    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait().await?;
        // Normal completion may intentionally start an approved background service.
        // Only an interrupted/erroring wait owns cancellation of the process group.
        #[cfg(unix)]
        {
            self.group = None;
        }
        Ok(status)
    }

    fn stop_group(&mut self) {
        #[cfg(unix)]
        if let Some(group) = self.group.take() {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.stop_group();
        // Tokio's child drop arranges reaping; kill_on_drop also covers non-Unix systems.
    }
}

pub(super) struct Output {
    pub status: ExitStatus,
    pub stdout: Capture,
    pub stderr: Capture,
}

pub(super) async fn capture(command: &mut tokio::process::Command) -> std::io::Result<Output> {
    let stdout = Capture::new()?;
    let stderr = Capture::new()?;
    command
        .stdout(Stdio::from(stdout.file.try_clone()?))
        .stderr(Stdio::from(stderr.file.try_clone()?));
    let status = Child::spawn(command)?.wait().await?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_captured_commands_stops_their_descendants() {
        let dir = std::env::temp_dir().join(format!("pi-capture-cancel-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&dir).unwrap();
        let task_dir = dir.clone();
        let task = tokio::spawn(async move {
            let mut command = tokio::process::Command::new("/usr/bin/zsh");
            command
                .args([
                    "-c",
                    "sh -c 'echo ready > ready; sleep 0.5; echo late > marker' & wait",
                ])
                .current_dir(task_dir);
            capture(&mut command).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !dir.join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.is_err());
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        assert!(!dir.join("marker").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
