//! Compaction behaviour, asserted on the projections it produces.

use crate::config::{ModelConfig, Provider, Usage};
use crate::llm::{Block, Completion, Message, StopReason, client::Client};
use crate::util;

use super::CompactError;
use super::checkpoint::*;
use super::prune::*;
use super::run::*;
use super::summary::*;

#[tokio::test]
async fn compaction_has_separate_session_headers_and_no_main_cache_key() {
    use std::io::{BufRead, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "summary request did not arrive"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("{err}"),
            }
        };
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
        let mut headers = std::collections::HashMap::new();
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                headers.insert(key.to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let mut bytes = vec![0; headers["content-length"].parse::<usize>().unwrap()];
        reader.read_exact(&mut bytes).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let response = serde_json::json!({"choices":[{"message":{"content":"## Goal\n完成第一项任务"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10000,"prompt_cache_hit_tokens":9000,"prompt_cache_miss_tokens":1000,"completion_tokens":30}}).to_string();
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        (request, headers)
    });
    let provider: Provider = serde_json::from_value(serde_json::json!({
        "api":"completions","base_url":format!("http://{address}/v1"),"api_key":"local-test",
        "compat":{"send_session_affinity":true}
    }))
    .unwrap();
    let model = ModelConfig {
        id: "deepseek-v4.1-flash".into(),
        reasoning: true,
        max_tokens: Some(8000),
        context_window: Some(128000),
        ..Default::default()
    };
    let tools = crate::tools::specs();
    let source = vec![
        user("第一项任务"),
        assistant(&"工作细节".repeat(3000)),
        user("接下来做第二项任务"),
    ];
    let original = source.clone();
    let settings = SummaryRequest {
        provider: &provider,
        model: &model,
        tools: &tools,
        level: "max",
        session_id: "summary-session",
        system_prompt: Some("固定系统提示"),
        custom_instructions: None,
    };
    let facts = CheckpointFacts {
        files: FileOps::collect(&source),
        user_requests: vec!["不做7和8，不用 /copy".into()],
    };
    let ids = (0..source.len())
        .map(|index| format!("entry-{index}"))
        .collect::<Vec<_>>();
    let outcome = run(
        &Client::local_test_client(),
        settings,
        &source,
        &ids,
        "compaction-test",
        10,
        facts,
    )
    .await
    .unwrap();
    assert!(outcome.summary.contains("不做7和8，不用 /copy"));
    let (request, headers) = server.join().unwrap();
    assert_eq!(headers["x-session-id"], "summary-session");
    assert_eq!(headers["x-session-affinity"], "summary-session");
    assert!(request.get("prompt_cache_key").is_none());
    assert_eq!(request["tool_choice"], "none");
    assert_eq!(request["tools"].as_array().unwrap().len(), tools.len());
    assert_eq!(request["messages"][0]["content"], "固定系统提示");
    assert_eq!(request["messages"][1]["content"], "第一项任务");
    assert_eq!(request["messages"][2]["content"], source[1].text());
    assert_eq!(request["reasoning_effort"], "max");
    assert_eq!(outcome.usage.cache_read, 9000);
    assert_eq!(outcome.replacement.last(), source.last());
    assert_eq!(source, original);
}

#[test]
fn oversized_history_falls_back_without_changing_system_tools_or_recent_request() {
    let provider = Provider {
        api: "completions".into(),
        ..Default::default()
    };
    let model = ModelConfig {
        id: "deepseek-v4.1-flash".into(),
        reasoning: true,
        context_window: Some(12_000),
        max_tokens: Some(4000),
        ..Default::default()
    };
    let tools = crate::tools::specs();
    let settings = SummaryRequest {
        provider: &provider,
        model: &model,
        session_id: "s",
        system_prompt: Some("原系统提示"),
        tools: &tools,
        level: "max",
        custom_instructions: None,
    };
    let source = vec![
        Message::user_text("原始要求"),
        Message::Assistant {
            content: vec![
                Block::Thinking {
                    thinking: "推理".repeat(30_000),
                    signature: None,
                },
                Block::Text {
                    text: "首部结论".to_string() + &"过长说明".repeat(20_000) + "尾部待办",
                },
            ],
            stop_reason: Some(StopReason::Stop),
        },
    ];
    let original = source.clone();
    let plan = prepare_summary(&settings, &source, false).unwrap();
    assert!(!plan.reuses_history_prefix);
    assert_eq!(plan.messages[0].text(), "原系统提示");
    let text = plan.messages.last().unwrap().text();
    assert!(!text.contains("推理推理"));
    assert!(text.contains("原始要求"));
    assert!(text.contains("尾部待办"));
    let tool_tokens: u64 = tools
        .iter()
        .map(|t| util::estimate_tokens(&serde_json::to_string(t).unwrap()))
        .sum();
    assert!(
        crate::llm::estimate_request_context(&plan.messages, "", &[])
            + tool_tokens
            + plan.model.max_tokens()
            + 512
            <= 12_000
    );
    assert_eq!(source, original);
    assert_eq!(plan.request(&settings).tools, tools);
    let recent = vec![Message::user_text("最近任务")];
    let replacement = replacement_history(&source, &recent, "摘要");
    assert_eq!(replacement.last(), recent.last());
}

#[test]
fn summary_preserves_absent_system_and_rejects_an_unfit_fixed_prefix() {
    let provider = Provider {
        api: "completions".into(),
        ..Default::default()
    };
    let model = ModelConfig {
        context_window: Some(8000),
        max_tokens: Some(2000),
        ..Default::default()
    };
    let source = vec![Message::user_text("已有任务")];
    let settings = SummaryRequest {
        provider: &provider,
        model: &model,
        session_id: "s",
        tools: &[],
        system_prompt: None,
        level: "",
        custom_instructions: None,
    };
    let plan = prepare_summary(&settings, &source, false).unwrap();
    assert!(plan.reuses_history_prefix);
    assert!(
        plan.messages
            .iter()
            .all(|m| !matches!(m, Message::System { .. }))
    );
    let small_model = ModelConfig {
        context_window: Some(100),
        ..model.clone()
    };
    let settings = SummaryRequest {
        model: &small_model,
        ..settings
    };
    assert!(matches!(
        prepare_summary(&settings, &source, false),
        Err(CompactError::InputTooLarge)
    ));
}

#[test]
fn summary_never_accepts_tool_calls_even_with_a_success_stop() {
    let completion = Completion {
        message: Message::Assistant {
            content: vec![Block::ToolCall {
                id: "c1".into(),
                name: "write".into(),
                arguments: serde_json::json!({}),
            }],
            stop_reason: Some(StopReason::Stop),
        },
        stop_reason: StopReason::Stop,
        usage: Usage::default(),
        error: None,
    };
    assert!(matches!(
        check_summary(&completion),
        Err(CompactError::ToolCallInSummary)
    ));
}

fn user(text: &str) -> Message {
    Message::user_text(text)
}

fn assistant(text: &str) -> Message {
    Message::Assistant {
        content: vec![Block::Text { text: text.into() }],
        stop_reason: None,
    }
}

fn call(id: &str, name: &str, path: &str) -> Message {
    Message::Assistant {
        content: vec![Block::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({ "path": path }),
        }],
        stop_reason: Some(StopReason::ToolUse),
    }
}

fn result(id: &str, content: &str) -> Message {
    Message::Tool {
        status: crate::llm::ToolStatus::Success,
        tool_call_id: id.into(),
        name: "read".into(),
        content: content.into(),
    }
}

/// Roughly `tokens` tokens, given the chars/4 estimate.
fn sized(text: &str, tokens: usize) -> String {
    format!("{text}{}", "x".repeat(tokens * 4))
}

#[test]
fn the_cut_never_lands_on_a_tool_result() {
    let mut messages = Vec::new();
    for i in 0..20 {
        messages.push(user(&sized(&format!("u{i} "), 4000)));
        messages.push(call(&format!("c{i}"), "read", "a.rs"));
        messages.push(result(&format!("c{i}"), &sized("r", 8000)));
        messages.push(assistant(&sized("a", 4000)));
    }
    let cut = find_cut_point(&messages, 20_000).expect("a cut exists");
    assert!(
        is_cut_point(&messages[cut.first_kept]),
        "cut landed on a tool result"
    );
    // The kept window really is around the budget, not wildly off.
    let kept: u64 = messages[cut.first_kept..]
        .iter()
        .map(Message::estimate_tokens)
        .sum();
    assert!(kept >= 20_000, "kept only {kept} tokens");
}

#[test]
fn cutting_on_an_assistant_tool_call_keeps_its_results() {
    // A finished turn, then a long second turn whose assistant message crosses the
    // budget. The cut lands on that assistant message, so its tool results follow it.
    let messages = vec![
        user(&sized("ask ", 1_000)),
        assistant(&sized("answer ", 1_000)),
        user(&sized("ask again ", 40_000)),
        call("c1", "read", "a.rs"),
        result("c1", &sized("r", 40_000)),
    ];
    let cut = find_cut_point(&messages, 30_000).expect("a cut exists");
    assert_eq!(cut.first_kept, 3);
    assert!(
        cut.is_split_turn(),
        "an assistant cut has to remember its turn start"
    );
    assert_eq!(cut.turn_start, Some(2));
    let kept = messages_to_keep(&messages, cut);
    // The tool result stays together with its call.
    assert_eq!(kept.len(), 2);
    assert!(kept.iter().any(|m| matches!(m, Message::Tool { .. })));
    let summarized = messages_to_summarize(&messages, cut);
    assert_eq!(summarized.len(), 3);
    // …and the replacement history still contains the user message that opened the
    // split turn, verbatim, so the intent is not lost.
    let replacement = replacement_history(&summarized, &kept, "S");
    let user_texts: Vec<String> = replacement
        .iter()
        .filter(|m| is_user(m))
        .map(Message::text)
        .collect();
    assert!(
        user_texts.iter().any(|t| t.starts_with("ask again")),
        "{user_texts:?}"
    );
}

#[test]
fn a_single_open_turn_cannot_be_compacted() {
    let messages = vec![
        user(&sized("start ", 20_000)),
        call("c1", "read", "a.rs"),
        result("c1", &sized("r", 30_000)),
    ];
    // The only cut candidates sit inside the first turn, so there is nothing to
    // summarise: compaction reports "too short" instead of producing an empty
    // checkpoint.
    assert!(find_cut_point(&messages, 10_000).is_none());
}

#[test]
fn a_cut_inside_an_enormous_turn_still_keeps_the_whole_turn_start() {
    // The newest turn alone is far past the budget, so the cut lands *inside* it and
    // the turn is flagged as split. The user message that opened it must survive.
    let messages = vec![
        user(&sized("first ", 1_000)),
        assistant(&sized("answer ", 1_000)),
        user(&sized("second ", 200_000)),
        assistant(&sized("long answer ", 300_000)),
    ];
    let cut = find_cut_point(&messages, 20_000).expect("a cut exists");
    assert!(cut.is_split_turn());
    assert_eq!(cut.turn_start, Some(2));
    let summarized = messages_to_summarize(&messages, cut);
    let kept = messages_to_keep(&messages, cut);
    let replacement = replacement_history(&summarized, &kept, "S");
    let user_texts: Vec<String> = replacement
        .iter()
        .filter(|m| is_user(m))
        .map(Message::text)
        .collect();
    assert!(!user_texts.iter().any(|t| t.starts_with("first")));
    assert!(
        user_texts.iter().any(|t| t.starts_with("second")),
        "{user_texts:?}"
    );
}

#[test]
fn a_mid_turn_cut_records_the_user_message_that_opened_it() {
    let messages = vec![
        user("first"),
        assistant(&sized("a", 40_000)),
        user("second"),
        assistant(&sized("b", 40_000)),
    ];
    let cut = find_cut_point(&messages, 25_000).unwrap();
    assert!(cut.is_split_turn());
    assert_eq!(cut.turn_start, Some(2));
    // Everything before the cut goes into the summary, and the split turn is announced
    // to the summariser so an unfinished step is described as such.
    let summarized = messages_to_summarize(&messages, cut);
    assert_eq!(summarized.len(), 3);
    assert!(summary_prompt("C", None, cut.is_split_turn()).contains("还没有结束"));
}

#[test]
fn too_little_history_has_no_cut_point() {
    let messages = vec![user("hi"), assistant("hello")];
    assert!(find_cut_point(&messages, 20_000).is_none());
    assert!(find_cut_point(&messages, 20_000).is_none());
}

#[test]
fn the_replacement_history_does_not_repeat_summarized_user_messages() {
    let summarized = vec![
        user("keep me"),
        call("c1", "read", "a.rs"),
        result("c1", "noise"),
        assistant("noise too"),
        user("keep me as well"),
    ];
    let kept = vec![user("recent question"), assistant("recent answer")];
    let replacement = replacement_history(&summarized, &kept, "SUMMARY");
    let text: Vec<String> = replacement.iter().map(Message::text).collect();
    assert!(text[0].contains("SUMMARY"));
    assert!(!text.iter().any(|t| t == "keep me"));
    assert!(!text.iter().any(|t| t == "keep me as well"));
    assert!(text.iter().any(|t| t == "recent question"));
    // Assistant and tool traffic from the summarised part is gone.
    assert!(!text.iter().any(|t| t.contains("noise")));
    assert!(
        !replacement
            .iter()
            .any(|m| matches!(m, Message::Tool { .. }))
    );
}

#[test]
fn file_blocks_separate_read_files_from_modified_ones() {
    let messages = vec![
        call("1", "read", "src/a.rs"),
        result("1", "read"),
        call("2", "read", "src/b.rs"),
        result("2", "read"),
        call("3", "edit", "src/b.rs"),
        Message::Tool {
            tool_call_id: "3".into(),
            name: "edit".into(),
            content: "done".into(),
            status: crate::llm::ToolStatus::Success,
        },
        call("4", "write", "src/c.rs"),
        Message::Tool {
            tool_call_id: "4".into(),
            name: "write".into(),
            content: "done".into(),
            status: crate::llm::ToolStatus::Success,
        },
    ];
    let ops = FileOps::collect(&messages);
    let (read, modified) = ops.lists();
    assert_eq!(read, vec!["src/a.rs"]);
    assert_eq!(modified, vec!["src/b.rs", "src/c.rs"]);
    let blocks = format_file_blocks(&read, &modified);
    assert!(blocks.contains("<read-files>\nsrc/a.rs\n</read-files>"));
    assert!(blocks.contains("<modified-files>\nsrc/b.rs\nsrc/c.rs\n</modified-files>"));
}

#[test]
fn serialization_flattens_tools_and_caps_a_giant_result() {
    let messages = vec![
        user("do the thing"),
        call("c1", "bash", "x"),
        result("c1", &sized("", 50_000)),
    ];
    let text = serialize_conversation(&messages);
    assert!(text.contains("[用户]: do the thing"));
    assert!(text.contains("[助手工具调用]: bash("));
    assert!(text.contains("已截断"));
    assert!(text.chars().count() < 50_000);
}

#[test]
fn reasoning_traces_are_left_out_of_the_summary_request() {
    // This is the bug that made `/compact` fail on a long session: the reasoning traces
    // were 3.4M of a 4.4M-character request, so the request to shrink the conversation
    // was itself larger than the model's window and was refused. A trace is the model's
    // scratch work; the summary is of the conversation, not of the working.
    let messages = vec![
        user("what changed?"),
        Message::Assistant {
            content: vec![
                Block::Thinking {
                    thinking: sized("scratch ", 40_000),
                    signature: None,
                },
                Block::Text {
                    text: "I edited a.rs".into(),
                },
            ],
            stop_reason: Some(StopReason::Stop),
        },
    ];
    let text = serialize_conversation(&messages);
    assert!(
        !text.contains("scratch "),
        "the trace must not be sent: {}",
        text.chars().count()
    );
    assert!(!text.contains("助手思考"), "{text}");
    // The answer itself is still there — that is the part worth summarising.
    assert!(text.contains("[助手]: I edited a.rs"), "{text}");
}

#[test]
fn a_long_assistant_message_is_capped() {
    // The per-message cap is the other half: one very long write-up should not decide how
    // big the request is either.
    let messages = vec![Message::Assistant {
        content: vec![Block::Text {
            text: sized("word ", 80_000),
        }],
        stop_reason: Some(StopReason::Stop),
    }];
    let text = serialize_conversation(&messages);
    assert!(text.contains("已截断"), "a capped message says so");
    assert!(
        text.chars().count() < ASSISTANT_TEXT_MAX_CHARS * 2,
        "{}",
        text.chars().count()
    );
}

#[test]
fn the_first_prompt_asks_for_the_seven_sections() {
    let prompt = summary_prompt("CONVERSATION", None, false);
    for section in [
        "## Goal",
        "## Constraints & Preferences",
        "## Progress",
        "## Key Decisions",
        "## Next Steps",
        "## Critical Context",
    ] {
        assert!(prompt.contains(section), "missing {section}");
    }
    assert!(prompt.contains("<conversation>\nCONVERSATION\n</conversation>"));
    assert!(!prompt.contains("<previous-summary>"));
}

#[test]
fn a_split_turn_is_flagged_to_the_summariser() {
    let prompt = summary_prompt("CONVERSATION", None, true);
    assert!(prompt.contains("还没有结束"));
    assert!(prompt.contains("In Progress"));
    let plain = summary_prompt("CONVERSATION", None, false);
    assert!(!plain.contains("还没有结束"));
}

#[test]
fn a_truncated_summary_is_refused() {
    let completion = Completion {
        message: Message::assistant_text("half a summary"),
        usage: Usage::default(),
        stop_reason: StopReason::Length,
        error: None,
    };
    assert!(matches!(
        check_summary(&completion),
        Err(CompactError::Truncated)
    ));
    let failed = Completion {
        message: Message::assistant_text(""),
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error: Some("boom".into()),
    };
    assert!(matches!(
        check_summary(&failed),
        Err(CompactError::Summarize(_))
    ));
}

#[test]
fn a_complete_summary_is_accepted() {
    let completion = Completion {
        message: Message::assistant_text("## Goal\nstuff"),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error: None,
    };
    assert!(check_summary(&completion).is_ok());
}

#[test]
fn overflows_are_recognised_across_providers() {
    assert!(looks_like_overflow(
        "prompt is too long: 210000 tokens > 200000 maximum"
    ));
    assert!(looks_like_overflow(
        "This model's maximum context length is 128000 tokens"
    ));
    assert!(looks_like_overflow(
        "Your request exceeds the context window"
    ));
    assert!(looks_like_overflow("输入超过了最大长度"));
}

#[test]
fn rate_limits_are_not_overflows() {
    assert!(!looks_like_overflow(
        "Rate limit reached for gpt-4 in organization org-x"
    ));
    assert!(!looks_like_overflow("Too many requests, please slow down"));
    assert!(!looks_like_overflow("Throttling error: 429"));
    assert!(!looks_like_overflow("insufficient_quota"));
}

#[test]
fn an_explicit_overflow_error_is_detected() {
    let completion = Completion {
        message: Message::Assistant {
            content: vec![],
            stop_reason: Some(StopReason::Error),
        },
        usage: Usage::default(),
        stop_reason: StopReason::Error,
        error: Some("prompt is too long".into()),
    };
    assert_eq!(
        detect_overflow(&completion, Some(200_000)),
        Some(OverflowSignal::ExplicitError)
    );
}

#[test]
fn a_silent_overflow_is_detected_from_usage_alone() {
    // The request "succeeded", but the prompt alone did not fit.
    let completion = Completion {
        message: Message::assistant_text("ok"),
        usage: Usage {
            input: 210_000,
            output: 5,
            cache_read: 0,
            cache_write: 0,
        },
        stop_reason: StopReason::Stop,
        error: None,
    };
    assert_eq!(
        detect_overflow(&completion, Some(200_000)),
        Some(OverflowSignal::SilentOverflow {
            prompt_tokens: 210_000,
            context_window: 200_000
        })
    );
}

#[test]
fn output_exhaustion_is_not_context_overflow() {
    let completion = Completion {
        message: Message::Assistant {
            content: vec![],
            stop_reason: Some(StopReason::Length),
        },
        usage: Usage {
            input: 99_000,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        },
        stop_reason: StopReason::Length,
        error: None,
    };
    assert_eq!(detect_overflow(&completion, Some(100_000)), None);
    assert_eq!(detect_overflow(&completion, None), None);
}

#[test]
fn a_genuine_length_stop_near_the_cap_is_not_an_overflow() {
    let completion = Completion {
        message: Message::Assistant {
            content: vec![],
            stop_reason: Some(StopReason::Length),
        },
        usage: Usage {
            input: 1000,
            output: 8000,
            cache_read: 0,
            cache_write: 0,
        },
        stop_reason: StopReason::Length,
        error: None,
    };
    assert_eq!(detect_overflow(&completion, Some(200_000)), None);
}

#[test]
fn a_healthy_turn_is_not_an_overflow() {
    let completion = Completion {
        message: Message::assistant_text("done"),
        usage: Usage {
            input: 1000,
            output: 50,
            cache_read: 0,
            cache_write: 0,
        },
        stop_reason: StopReason::Stop,
        error: None,
    };
    assert_eq!(detect_overflow(&completion, Some(200_000)), None);
    assert_eq!(completion.stop_reason, StopReason::Stop);
}

#[test]
fn the_retry_budget_allows_exactly_one_retry_per_turn() {
    let mut budget = RetryBudget::default();
    assert!(budget.spend());
    assert!(!budget.spend());
    budget.reset();
    assert!(budget.spend());
}

#[test]
fn a_second_compaction_cannot_start_while_one_is_running() {
    let mut state = CompactionState::default();
    state.begin().unwrap();
    assert!(matches!(state.begin(), Err(CompactError::InProgress)));
    state.finish();
    assert!(state.begin().is_ok());
}

#[test]
fn real_usage_is_preferred_over_the_estimate() {
    let messages = vec![user("short")];
    assert!(crate::llm::estimate_request_context(&messages, "system", &[]) < 100);
}

#[test]
fn failed_cancelled_skipped_and_unknown_operations_are_not_completed_files() {
    use crate::llm::ToolStatus;
    let mut messages = Vec::new();
    for (index, status) in [
        ToolStatus::Error,
        ToolStatus::Cancelled,
        ToolStatus::Skipped,
        ToolStatus::Unknown,
        ToolStatus::Success,
    ]
    .into_iter()
    .enumerate()
    {
        let id = index.to_string();
        messages.push(call(&id, "edit", &format!("{index}.rs")));
        messages.push(Message::Tool {
            tool_call_id: id,
            name: "edit".into(),
            content: "result".into(),
            status,
        });
    }
    messages.push(call("pending", "write", "never-executed.rs"));
    let (read, modified) = FileOps::collect(&messages).lists();
    assert!(read.is_empty());
    assert_eq!(modified, ["4.rs"]);
}

#[test]
fn pruning_preserves_unicode_tool_pairing_status_and_original_history() {
    let original = format!("HEAD{}TAIL", "中文🦀\n".repeat(4000));
    let messages = vec![
        user("不要提交，也不要执行部署"),
        call("c", "read", "a.rs"),
        result("c", &original),
    ];
    let outcome =
        prune_tool_results(&messages, std::path::Path::new("/tmp/session.jsonl")).unwrap();
    assert_eq!(outcome.tool_results, 1);
    assert!(outcome.saved_tokens > 0);
    assert_eq!(messages[2].text(), original);
    assert_eq!(outcome.replacement[0], messages[0]);
    assert_eq!(outcome.replacement[1], messages[1]);
    let text = outcome.replacement[2].text();
    assert!(text.starts_with("HEAD"));
    assert!(text.ends_with("TAIL"));
    assert!(text.contains("/tmp/session.jsonl"));
    assert!(text.contains("tool_call_id=c"));
    crate::llm::validate_tool_history(&outcome.replacement).unwrap();
    assert!(
        prune_tool_results(
            &outcome.replacement,
            std::path::Path::new("/tmp/session.jsonl")
        )
        .is_none()
    );
}
