//! The seven tools. Each is a small module exposing a spec plus an `execute`.
//!
//! Tool order is fixed in [`specs`] and must not change between turns: the tool block is
//! part of the prompt-cache prefix.

pub mod bash;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

use std::path::Path;

use crate::llm::ToolSpec;
use crate::util;

/// What a tool hands back to the agent loop.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Text for the model, already truncated to the 2000-line / 50KB budget.
    pub content: String,
    /// Presentation-only extras (diffs, exit codes). Never sent to the model.
    pub display: Display,
    /// Set when the model should be told the call failed.
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        ToolOutput { content: content.into(), display: Display::None, is_error: false }
    }

    pub fn error(content: impl Into<String>) -> Self {
        ToolOutput { content: content.into(), display: Display::None, is_error: true }
    }

    /// An error that still echoes what was attempted, so a refusal in the transcript shows
    /// the command or path the user is being asked about.
    pub fn error_for(name: &str, arguments: &serde_json::Value, content: impl Into<String>) -> Self {
        let display = match name {
            "bash" => Display::Command {
                expanded: false,
                footer: Vec::new(),
            },
            "write" | "edit" | "read" | "grep" | "find" | "ls" => Display::File {
                verb: verb_for(name),
                path: arguments
                    .get("path")
                    .and_then(|value| value.as_str())
                    .map(|path| crate::util::one_line(path))
                    .unwrap_or_else(|| "…".into()),
            },
            _ => Display::None,
        };
        ToolOutput { content: content.into(), display, is_error: true }
    }

    /// Apply the shared output budget, parking the full text in a temp file when it
    /// does not fit.
    pub fn budget(self) -> Self {
        let truncated = util::truncate_output(&self.content);
        let display = match (self.display, truncated.full_path.clone()) {
            (Display::Command { expanded, footer }, path) => Display::Command {
                expanded,
                footer: {
                    let mut parts = footer;
                    if let Some(path) = path {
                        parts.push(format!("完整输出：{}", path.display()));
                    }
                    parts
                },
            },
            (other, _) => other,
        };
        ToolOutput { content: truncated.text, display, is_error: self.is_error }
    }
}

/// Presentation-only payload attached to a tool result.
#[derive(Debug, Clone, Default)]
pub enum Display {
    #[default]
    None,
    /// A bash result: exit status plus the note lines shown under the output.
    Command {
        expanded: bool,
        footer: Vec<String>,
    },
    /// A diff for `edit` / `write`.
    Diff {
        diff: String,
        added: usize,
        removed: usize,
        omitted: bool,
    },
    /// A plain file header line.
    File {
        verb: &'static str,
        path: String,
    },
}

fn verb_for(name: &str) -> &'static str {
    match name {
        "read" => "读取",
        "write" => "写入",
        "edit" => "修改",
        "grep" => "搜索",
        "find" => "查找",
        "ls" => "列出",
        _ => "处理",
    }
}

/// Tool names in the order they are advertised. **Do not reorder.**
pub const TOOL_ORDER: [&str; 7] =
    ["read", "write", "edit", "bash", "grep", "find", "ls"];

pub fn specs() -> Vec<ToolSpec> {
    vec![
        read::spec(),
        write::spec(),
        edit::spec(),
        bash::spec(),
        grep::spec(),
        find::spec(),
        ls::spec(),
    ]
}

/// Dispatch one tool call. The agent loop has already passed it through the
/// authorization gate.
pub async fn execute(
    name: &str,
    arguments: &serde_json::Value,
    cwd: &Path,
) -> ToolOutput {
    let result = match name {
        "read" => read::execute(arguments, cwd).await,
        "write" => write::execute(arguments, cwd).await,
        "edit" => edit::execute(arguments, cwd).await,
        "bash" => bash::execute(arguments, cwd).await,
        "grep" => grep::execute(arguments, cwd).await,
        "find" => find::execute(arguments, cwd).await,
        "ls" => ls::execute(arguments, cwd).await,
        other => return ToolOutput::error(format!("未知工具：{other}")),
    };
    result.unwrap_or_else(|err| ToolOutput::error(err.to_string())).budget()
}

/// Every tool needs a string argument; this keeps the error text uniform.
pub(crate) fn required_str<'a>(
    arguments: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, String> {
    arguments
        .get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("缺少必需参数「{key}」"))
}

pub(crate) fn optional_u64(arguments: &serde_json::Value, key: &str) -> Option<u64> {
    arguments.get(key).and_then(|value| value.as_u64())
}

/// Test-only helper: the tools read files and spawn processes, so their futures need a
/// reactor. A current-thread runtime is all any of them require.
#[cfg(test)]
pub(crate) fn block<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a test runtime")
        .block_on(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_order_is_pinned_for_cache_stability() {
        let specs = specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, TOOL_ORDER);
    }

    #[test]
    fn every_spec_has_a_description_and_an_object_schema() {
        for spec in specs() {
            assert!(!spec.description.is_empty(), "{} has no description", spec.name);
            assert_eq!(spec.parameters["type"], "object", "{} schema is not an object", spec.name);
            assert!(spec.parameters.get("properties").is_some(), "{} has no properties", spec.name);
        }
    }
}
