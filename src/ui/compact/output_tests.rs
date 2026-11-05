use crate::config::{ModelConfig, Provider};
use crate::llm::{Block, Message, Request, StopReason, ToolStatus, anthropic, responses};
use crate::tools::{Display, ToolOutput};
use crate::ui::{compact, screen};

fn render(mut blocks: Vec<screen::Block>, expanded: bool) -> String {
    blocks
        .iter_mut()
        .flat_map(|block| {
            if let screen::Block::Collapsible(content) = block {
                content.expanded = expanded;
            }
            block.render(120).into_iter().map(|line| line.text())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn summaries_expand_to_the_result_and_errors_keep_the_cause_and_exit_status() {
    let arguments = serde_json::json!({"path": "src/main.rs"});
    let mut output = ToolOutput::text("1\tfirst\n2\tlast\n");
    output.display = compact::arguments_display("read", &arguments);
    let block = compact::tool_block("read", &arguments, &output, ToolStatus::Success);
    assert_eq!(
        render(vec![block.clone()], false),
        "✓ 读取 src/main.rs · 2 行"
    );
    let expanded = render(vec![block], true);
    assert!(expanded.contains("first") && expanded.contains("last"));

    for (name, text) in [("grep", "没有找到匹配。"), ("find", "没有找到匹配的文件。")]
    {
        let mut output = ToolOutput::text(text);
        output.display = compact::arguments_display(name, &arguments);
        let summary = render(
            vec![compact::tool_block(
                name,
                &arguments,
                &output,
                ToolStatus::Success,
            )],
            false,
        );
        assert!(summary.ends_with(" · 无匹配"), "{summary}");
    }

    output.content = format!("无法访问目标目录\n{}[退出码 3]", "stack frame\n".repeat(30));
    output.display = Display::Command;
    let block = compact::tool_block(
        "bash",
        &serde_json::json!({"command": "run"}),
        &output,
        ToolStatus::Error,
    );
    let preview = render(vec![block.clone()], false);
    assert!(preview.contains("无法访问目标目录"));
    assert!(preview.contains("[退出码 3]"));
    assert!(preview.lines().count() <= 6);
    assert_eq!(render(vec![block], true).matches("stack frame").count(), 30);
}

#[test]
fn a_collapsed_summary_keeps_the_whole_path_on_narrow_terminals() {
    let path = "src/很长的目录/another-directory/main.rs";
    let arguments = serde_json::json!({"path": path});
    let mut output = ToolOutput::text("1\tfile content\n");
    output.display = compact::arguments_display("read", &arguments);
    let block = compact::tool_block("read", &arguments, &output, ToolStatus::Success);
    let text: String = block.render(12).iter().map(screen::Line::text).collect();
    assert_eq!(text, format!("✓ 读取 {path} · 1 行"));
}

fn requests(messages: &[Message]) -> (serde_json::Value, serde_json::Value) {
    let model = ModelConfig {
        id: "test".into(),
        ..Default::default()
    };
    let provider = Provider {
        api: "responses".into(),
        ..Default::default()
    };
    let request = Request {
        model: &model,
        provider: &provider,
        messages,
        tools: &[],
        level: "",
        session_id: "presentation",
        cache_hints: true,
    };
    (
        serde_json::to_value(responses::build_request(&request, true)).unwrap(),
        serde_json::to_value(anthropic::build_request(&request)).unwrap(),
    )
}

#[test]
fn output_annotations_reach_models_but_not_live_or_restored_transcripts() {
    for (name, content, status) in [
        (
            "bash",
            "应用错误：输出太长\n\n[输出过长，已截断。完整输出：/tmp/pi-test.log]\n[退出码 3]\n",
            ToolStatus::Error,
        ),
        (
            "grep",
            "file:1:matched\n[已截断，仅返回前 1 条匹配]",
            ToolStatus::Success,
        ),
        (
            "find",
            "file.rs\n[已截断，仅列出前 1 个]",
            ToolStatus::Success,
        ),
        (
            "read",
            "1\tfirst\n\n[工具结果中间已裁剪；原始内容保存在 /tmp/session.jsonl，tool_call_id=c]\n\n9\tlast",
            ToolStatus::Success,
        ),
        (
            "read",
            "1\tdata\n\n[完整输出写入临时文件失败：no space]",
            ToolStatus::Success,
        ),
        ("bash", "（无输出）", ToolStatus::Success),
    ] {
        let arguments = serde_json::json!({"command":"example", "path":"file"});
        let messages = vec![
            Message::Assistant {
                content: vec![Block::ToolCall {
                    id: "c".into(),
                    name: name.into(),
                    arguments: arguments.clone(),
                }],
                stop_reason: Some(StopReason::ToolUse),
            },
            Message::Tool {
                tool_call_id: "c".into(),
                name: name.into(),
                content: content.into(),
                status,
                images: Vec::new(),
            },
        ];
        let restored: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
        let (responses, anthropic) = requests(&restored);
        assert_eq!(responses["input"][1]["output"], messages[1].text());
        assert_eq!(
            anthropic["messages"][1]["content"][0]["content"],
            messages[1].text()
        );
        let mut output = ToolOutput::text(content);
        output.display = if name == "bash" {
            Display::Command
        } else {
            Display::File {
                verb: "读取",
                path: "file".into(),
            }
        };
        for expanded in [false, true] {
            let live = render(
                vec![compact::tool_block(name, &arguments, &output, status)],
                expanded,
            );
            let replay = render(compact::replay_blocks(&restored), expanded);
            for text in [live, replay] {
                for note in ["已截断", "完整输出", "中间已裁剪", "（无输出）", "/tmp/"]
                {
                    assert!(!text.contains(note), "{name}: {text}");
                }
                if status == ToolStatus::Error {
                    assert!(text.contains("应用错误：输出太长"), "{text}");
                    assert!(text.contains("退出码 3"), "{text}");
                } else if name != "bash" && expanded {
                    for word in content.lines().next().unwrap().split_whitespace() {
                        assert!(text.contains(word), "{text}");
                    }
                }
            }
        }
    }
}

#[test]
fn length_limits_reach_both_protocols_without_changing_the_visible_answer() {
    for blocks in [
        vec![Block::Text {
            text: "已生成的正文".into(),
        }],
        vec![Block::Thinking {
            thinking: "未完成的思考".into(),
            signature: None,
        }],
    ] {
        let message = Message::Assistant {
            content: blocks,
            stop_reason: Some(StopReason::Length),
        };
        let original = message.text();
        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        let messages = [restored, Message::user_text("继续")];
        let (responses, anthropic) = requests(&messages);
        assert!(
            responses["input"]
                .to_string()
                .contains("上次回复达到输出上限")
        );
        assert!(
            anthropic["messages"]
                .to_string()
                .contains("上次回复达到输出上限")
        );
        let transcript = render(compact::replay_blocks(&messages), true);
        assert!(!transcript.contains("输出上限"), "{transcript}");
        assert!(transcript.contains(&original), "{transcript}");
        assert_eq!(messages[0].text(), original);
    }
}
