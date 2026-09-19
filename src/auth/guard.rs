//! The gate in front of tool execution.
//!
//! Every tool call passes through [`PermissionGate::check`]. A call the policy allows
//! runs immediately; anything else pops the authorization panel. With no terminal the
//! gate refuses — silently approving a dangerous command in a headless run would defeat
//! the whole point of the policy.
//!
//! Approval is bound to one call: the fingerprint of the arguments that were shown is
//! remembered, and anything that changes after the user has looked is asked again.

use std::path::Path;

use sha2_shim::fingerprint;

use crate::auth::policy::{self, Assessment, Dialect};
use crate::ui::auth_panel::{self, PanelRequest};

/// Why a call was refused, in the words the policy used, plus the instruction that stops
/// the model from re-issuing the same command.
pub struct Refusal {
    pub reason: String,
}

impl Refusal {
    pub fn message(&self) -> String {
        format!(
            "{}。请勿改写命令绕过授权，也不要重试同一条命令。",
            policy::refusal(Some(&self.reason))
        )
    }
}

pub struct PermissionGate {
    /// Arguments the user has already approved, keyed by tool-call id.
    approvals: std::collections::HashMap<String, String>,
    /// When false the panel is never shown and everything risky is refused (headless).
    interactive: bool,
    dialect: Dialect,
}

impl PermissionGate {
    pub fn new(interactive: bool, dialect: Dialect) -> Self {
        PermissionGate { approvals: std::collections::HashMap::new(), interactive, dialect }
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn reset(&mut self) {
        self.approvals.clear();
    }

    /// Decide whether `tool` may run with `input`. `Ok(())` means run; `Err(refusal)`
    /// means the model gets the refusal text back as the tool result.
    pub async fn check(
        &mut self,
        call_id: &str,
        tool: &str,
        input: &serde_json::Value,
        cwd: &Path,
    ) -> Result<(), Refusal> {
        let decision = policy::assess_tool(tool, input, cwd, self.dialect);
        let Assessment::Ask { reason } = decision else {
            return Ok(());
        };
        if !self.interactive {
            return Err(Refusal { reason });
        }
        let fingerprint = fingerprint(tool, input, cwd);
        if self.approvals.get(call_id) == Some(&fingerprint) {
            return Ok(());
        }
        let body = panel_body(tool, input);
        let decision = auth_panel::ask(PanelRequest { body });
        if decision == auth_panel::Decision::Allow {
            self.approvals.insert(call_id.to_string(), fingerprint);
            Ok(())
        } else {
            self.approvals.remove(call_id);
            Err(Refusal { reason })
        }
    }

    /// Called once a tool call has finished, so an approval cannot be replayed.
    pub fn finish(&mut self, call_id: &str) {
        self.approvals.remove(call_id);
    }
}

/// What the panel shows: the command itself for `bash`, the resolved arguments for the
/// file tools. The policy's own reason is deliberately *not* shown — it goes to the
/// model through the refusal message, where it can be acted on.
fn panel_body(tool: &str, input: &serde_json::Value) -> String {
    if tool == "bash"
        && let Some(command) = input.get("command").and_then(|v| v.as_str())
    {
        return command.to_string();
    }
    serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string())
}

/// A small content hash. Implemented here rather than pulling in a digest crate: the
/// value is only ever compared with itself inside one process.
mod sha2_shim {
    // FNV-1a over the canonical JSON, plus the length, is enough to notice that the
    // arguments changed between the panel and the execution.
    pub fn fingerprint(tool: &str, input: &serde_json::Value, cwd: &std::path::Path) -> String {
        let mut hasher = Fnv::default();
        hasher.write(tool.as_bytes());
        hasher.write(cwd.to_string_lossy().as_bytes());
        hasher.write(ordered(input).as_bytes());
        format!("{:016x}", hasher.finish())
    }

    /// Serialise with sorted object keys so field order in the model's JSON does not
    /// change the fingerprint.
    fn ordered(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let inner: Vec<String> =
                    keys.into_iter().map(|k| format!("{k:?}:{}", ordered(&map[k]))).collect();
                format!("{{{}}}", inner.join(","))
            }
            serde_json::Value::Array(items) => {
                let inner: Vec<String> = items.iter().map(ordered).collect();
                format!("[{}]", inner.join(","))
            }
            other => other.to_string(),
        }
    }

    #[derive(Default)]
    struct Fnv {
        state: u64,
    }

    impl Fnv {
        fn write(&mut self, bytes: &[u8]) {
            if self.state == 0 {
                self.state = 0xcbf2_9ce4_8422_2325;
            }
            for byte in bytes {
                self.state ^= *byte as u64;
                self.state = self.state.wrapping_mul(0x1000_0000_01b3);
            }
        }

        fn finish(self) -> u64 {
            self.state
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_calls_pass_without_asking() {
        let mut gate = PermissionGate::new(true, Dialect::Zsh);
        let cwd = std::env::temp_dir();
        let input = serde_json::json!({"command": "ls"});
        let result = futures_lite_block(gate.check("1", "bash", &input, &cwd));
        assert!(result.is_ok());
    }

    #[test]
    fn headless_refuses_what_needs_approval() {
        let mut gate = PermissionGate::new(false, Dialect::Zsh);
        let cwd = std::env::temp_dir();
        let input = serde_json::json!({"command": "rm -rf /"});
        let error = futures_lite_block(gate.check("1", "bash", &input, &cwd)).unwrap_err();
        assert!(error.message().starts_with("未获得用户授权，操作未执行（"));
        assert!(error.message().contains("请勿改写命令绕过授权"));
    }

    #[test]
    fn the_fingerprint_ignores_key_order() {
        let cwd = std::env::temp_dir();
        let a = serde_json::json!({"path": "x", "content": "y"});
        let b: serde_json::Value = serde_json::from_str(r#"{"content":"y","path":"x"}"#).unwrap();
        assert_eq!(fingerprint("write", &a, &cwd), fingerprint("write", &b, &cwd));
        let c: serde_json::Value = serde_json::from_str(r#"{"content":"z","path":"x"}"#).unwrap();
        assert_ne!(fingerprint("write", &a, &cwd), fingerprint("write", &c, &cwd));
    }

    #[test]
    fn the_panel_shows_the_command_for_bash_and_json_for_files() {
        let bash = serde_json::json!({"command": "rm -rf build"});
        assert_eq!(panel_body("bash", &bash), "rm -rf build");
        let write = serde_json::json!({"path": "a.txt", "content": "hi"});
        let body = panel_body("write", &write);
        assert!(body.contains("\"path\": \"a.txt\""));
    }

    /// The gate is async only because the rest of the loop is; in tests it never awaits.
    fn futures_lite_block<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut future = Box::pin(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the policy check must not await anything"),
        }
    }
}
