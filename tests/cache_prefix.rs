//! Prompt-cache invariants.
//!
//! The single biggest cost lever in the whole project is that each turn's request prefix is
//! byte-identical to the previous turn's. These tests serialise real request bodies and
//! compare them, because a comment claiming the prefix is stable proves nothing.

use mpi::config::{Config, ModelConfig, Provider};
use mpi::llm::{Block, Message, Request, ToolSpec, ToolStatus, anthropic, openai};

fn fixtures() -> (ModelConfig, Provider) {
    let config: Config = serde_json::from_str(
        r#"{
          "providers": [{
            "name": "name",
            "api": "completions",
            "base_url": "url",
            "models": [{
              "id": "deepseek-v4.1-flash",
              "name": "deepseek-v4.1-flash",
              "context_window": 1000000,
              "max_tokens": 64000,
              "reasoning": true,
              "thinking_levels": ["low", "high", "max"]
            }]
          }]
        }"#,
    )
    .unwrap();
    let (provider, model) = config.find("name/deepseek-v4.1-flash").unwrap();
    (model.clone(), provider.clone())
}

/// Stands for the system prompt of a session whose `AGENTS.md` says one thing.
///
/// The real value is read from disk once per session; what the cache depends on is that it
/// is the same bytes on every request, which is what these tests exercise.
const SYSTEM: &str = "<!-- AGENTS.md -->\n用 cargo test 跑测试。";

fn tools() -> Vec<ToolSpec> {
    mpi::tools::specs()
}

fn body_of(messages: &[Message], model: &ModelConfig, provider: &Provider) -> serde_json::Value {
    let tools = tools();
    let request = Request {
        model,
        provider,
        messages,
        tools: &tools,
        level: "high",
        session_id: "session-id",
        cache_hints: true,
    };
    serde_json::to_value(openai::build_request(&request, true)).unwrap()
}

/// The JSON prefix a gateway would reuse: the system message and the tool definitions.
fn prefix(body: &serde_json::Value) -> String {
    let messages = body["messages"].as_array().unwrap();
    let system = messages
        .iter()
        .find(|message| message["role"] == "system" || message["role"] == "developer")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    format!("{system}\n{}", body["tools"])
}

#[test]
fn the_system_message_and_tools_are_identical_between_turns() {
    let (model, provider) = fixtures();
    let tools = tools();
    let first = vec![
        Message::System { content: SYSTEM.into() },
        Message::user_text("first question"),
    ];
    let second = vec![
        Message::System { content: SYSTEM.into() },
        Message::user_text("first question"),
        Message::assistant_text("first answer"),
        Message::user_text("second question"),
    ];
    let tools_ref: Vec<ToolSpec> = tools.clone();
    let body = |messages: &[Message]| {
        let request = Request {
            model: &model,
            provider: &provider,
            messages,
            tools: &tools_ref,
            level: "high",
            session_id: "session-id",
            cache_hints: true,
        };
        serde_json::to_value(openai::build_request(&request, true)).unwrap()
    };
    assert_eq!(
        prefix(&body(&first)),
        prefix(&body(&second)),
        "the system prompt or tool block changed between turns, which invalidates the cache"
    );
    // The whole of turn one is a literal prefix of turn two, which is what a cache needs.
    let one = serde_json::to_string(&body(&first)["messages"]).unwrap();
    let two = serde_json::to_string(&body(&second)["messages"]).unwrap();
    assert!(
        two.starts_with(one.trim_end_matches(']')),
        "turn one is not a prefix of turn two:\n{one}\n{two}"
    );
}

#[test]
fn message_serialisation_order_is_fixed() {
    let (model, provider) = fixtures();
    // The same message must serialise to the same bytes every time, whatever order the
    // caller assembled the fields in.
    let messages = vec![
        Message::System { content: SYSTEM.into() },
        Message::user_text("hi"),
        Message::Assistant {
            content: vec![
                Block::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a.rs", "limit": 10}),
                },
            ],
            stop_reason: Some(mpi::llm::StopReason::ToolUse),
        },
        Message::Tool { status: ToolStatus::Success, tool_call_id: "c1".into(), name: "read".into(), content: "data".into() },
    ];
    let first = serde_json::to_string(&body_of(&messages, &model, &provider)).unwrap();
    for _ in 0..5 {
        assert_eq!(
            first,
            serde_json::to_string(&body_of(&messages, &model, &provider)).unwrap(),
            "serialisation is not deterministic"
        );
    }
}

#[test]
fn tool_outcomes_reach_all_protocols_without_rewriting_success_content() {
    let (model, provider) = fixtures();
    for status in [ToolStatus::Success, ToolStatus::Error, ToolStatus::Cancelled, ToolStatus::Skipped, ToolStatus::Unknown] {
        let original = "原始内容\n    code\n";
        let messages = vec![
            Message::Assistant { content:vec![Block::ToolCall { id:"c".into(), name:"read".into(), arguments:serde_json::json!({"path":"a.rs"}) }], stop_reason:Some(mpi::llm::StopReason::ToolUse) },
            Message::Tool { tool_call_id:"c".into(), name:"read".into(), content:original.into(), status },
        ];
        let request = Request { model:&model, provider:&provider, messages:&messages, tools:&[], level:"high", session_id:"s", cache_hints:false };
        let chat = serde_json::to_value(openai::build_request(&request, true)).unwrap();
        let responses = serde_json::to_value(mpi::llm::responses::build_request(&request, true)).unwrap();
        let anthropic = serde_json::to_value(anthropic::build_request(&request)).unwrap();
        let expected = if status == ToolStatus::Success { original.to_string() } else { format!("[工具状态：{}]\n{original}", status.label()) };
        assert_eq!(chat["messages"][1]["content"], expected);
        assert_eq!(responses["input"][1]["output"], expected);
        assert_eq!(anthropic["messages"][1]["content"][0]["content"], expected);
        assert_eq!(anthropic["messages"][1]["content"][0]["is_error"], status != ToolStatus::Success);
        assert_eq!(messages[1].text(), expected);
    }
    assert!(serde_json::from_value::<Message>(serde_json::json!({"role":"tool","tool_call_id":"c","name":"read","content":"old result"})).is_err(), "execution status is required in the current session format");
}

#[test]
fn the_tool_block_is_stable_and_ordered() {
    // Tool order is a cache boundary, so it must not depend on a hash map's iteration order.
    let names: Vec<String> = tools().iter().map(|tool| tool.name.clone()).collect();
    assert_eq!(names, vec!["read", "write", "edit", "bash", "grep", "find", "ls"]);
    for _ in 0..5 {
        let again: Vec<String> = tools().iter().map(|tool| tool.name.clone()).collect();
        assert_eq!(names, again);
    }
}

#[test]
fn pi_adds_nothing_of_its_own_to_the_system_prompt() {
    // The prompt is the user's file and nothing else, so nothing pi knows — cwd, the clock,
    // the branch, the model name — can leak into the cache prefix. Anything volatile belongs
    // in the environment block, which is a conversation message instead.
    let dir = std::env::temp_dir().join(format!("pi-prefix-agents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let body = "# 项目约定\n- 注释写英文。\n";
    std::fs::write(dir.join("AGENTS.md"), body).unwrap();

    let (path, text) = mpi::agent::r#loop::load_agents_md(&dir).expect("the file is found");
    assert_eq!(path, dir.join("AGENTS.md"));
    // The file's own text is reproduced verbatim; the only thing added is a marker naming
    // where it came from, which is stable for as long as the file does not move.
    assert!(text.contains(body.trim_end()), "{text:?}");
    for volatile in ["2026-", "main", "deepseek"] {
        assert!(!text.contains(volatile), "pi injected {volatile:?}: {text:?}");
    }
    // Reading twice gives the same bytes, which is what makes it a usable prefix.
    let (_, again) = mpi::agent::r#loop::load_agents_md(&dir).unwrap();
    assert_eq!(text, again);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_environment_block_is_a_message_not_the_system_prompt() {
    // The split is the mechanism that keeps the cache prefix stable, so it is worth asserting
    // that the volatile facts really do land in the conversation.
    let cwd = std::path::Path::new("/tmp/some-project");
    let block = mpi::agent::r#loop::environment_block(cwd, "session-1", "/usr/bin/zsh");
    let messages = vec![
        Message::System { content: SYSTEM.into() },
        Message::user_text(block.clone()),
    ];
    let (model, provider) = fixtures();
    let body = body_of(&messages, &model, &provider);
    let array = body["messages"].as_array().unwrap();
    assert_eq!(array[0]["role"], "system");
    assert!(array[0]["content"].as_str().unwrap() == SYSTEM);
    assert!(
        array[1]["content"].as_str().unwrap().contains("/tmp/some-project"),
        "the cwd must travel in the conversation, not the system prompt"
    );
    // And the environment block is present exactly once, as the first user turn.
    assert_eq!(array.len(), 2);
}

#[test]
fn history_is_never_rewritten_by_a_later_turn() {
    let (model, provider) = fixtures();
    // A short conversation, then several more turns. The earlier messages must serialise to
    // exactly the same bytes as they did at the start — no "已执行" annotations, no
    // reordering, no reformatting.
    let mut messages = vec![
        Message::System { content: SYSTEM.into() },
        Message::user_text("read a.rs"),
    ];
    let baseline = serde_json::to_string(&body_of(&messages, &model, &provider)["messages"]).unwrap();
    for turn in 0..4 {
        messages.push(mpi::llm::Message::assistant_text(format!("answer {turn}")));
        messages.push(mpi::llm::Message::user_text(format!("question {turn}")));
        let now = serde_json::to_string(&body_of(&messages, &model, &provider)["messages"]).unwrap();
        assert!(
            now.starts_with(baseline.trim_end_matches(']')),
            "turn {turn} rewrote the earlier history"
        );
    }
}

#[test]
fn openai_requests_carry_the_prompt_cache_key() {
    let (model, provider) = fixtures();
    let messages = vec![Message::System { content: SYSTEM.into() }, Message::user_text("hi")];
    let body = body_of(&messages, &model, &provider);
    assert_eq!(body["prompt_cache_key"], "session-id");
    // The session id is also what pins a gateway's load balancer to one backend.
    assert!(provider.compat(&model).send_session_affinity);
}

#[test]
fn summary_requests_reuse_system_tools_and_the_selected_history_prefix() {
    use mpi::agent::compact::{SummaryRequest, prepare_summary};
    use mpi::llm::{StopReason, responses, client::summary_request_body};
    let (model, provider) = fixtures();
    let tools = tools();
    let prefix = vec![
        Message::user_text("第一项任务"),
        Message::Assistant { content:vec![
            Block::Thinking { thinking:"保留这段原始推理".into(), signature:None },
            Block::ToolCall { id:"c1".into(), name:"read".into(), arguments:serde_json::json!({"path":"a.rs"}) },
        ], stop_reason:Some(StopReason::ToolUse) },
        Message::Tool { status: ToolStatus::Success, tool_call_id:"c1".into(), name:"read".into(), content:"原始文件内容".into() },
    ];
    let original = prefix.clone();
    let mut full_history = vec![Message::System { content:SYSTEM.into() }];
    full_history.extend(prefix.clone());
    full_history.push(Message::assistant_text("已完成"));
    full_history.push(Message::user_text("最近任务"));
    for api in ["completions", "messages", "responses"] {
        let provider = Provider { api:api.into(), ..provider.clone() };
        let settings = SummaryRequest {
            provider:&provider, model:&model, tools:&tools, level:"max", session_id:"session-id",
            system_prompt:Some(SYSTEM), custom_instructions:Some("保留待办"),
        };
        let prepared = prepare_summary(&settings, &prefix, true).unwrap();
        assert!(prepared.reuses_history_prefix);
        assert_eq!(&prepared.messages[1..1 + prefix.len()], prefix.as_slice());
        assert!(prepared.messages.last().unwrap().text().contains("保留待办"));
        let request = prepared.request(&settings);
        let summary = serde_json::to_value(summary_request_body(&request).unwrap()).unwrap();
        let normal = Request { model:&model, messages:&full_history, ..request };
        let (normal, messages_key, system_key) = match api {
            "completions" => (serde_json::to_value(openai::build_request(&normal, true)).unwrap(), "messages", "unused"),
            "messages" => (serde_json::to_value(anthropic::build_request(&normal)).unwrap(), "messages", "system"),
            _ => (serde_json::to_value(responses::build_request(&normal, true)).unwrap(), "input", "instructions"),
        };
        assert_eq!(normal["tools"], summary["tools"], "{api}: tools changed");
        assert_eq!(normal[system_key], summary[system_key], "{api}: system changed");
        let summary_messages = summary[messages_key].as_array().unwrap();
        let main_messages = normal[messages_key].as_array().unwrap();
        assert_eq!(&summary_messages[..summary_messages.len() - 1], &main_messages[..summary_messages.len() - 1], "{api}: history prefix changed");
        assert_eq!(summary["prompt_cache_key"], normal["prompt_cache_key"]);
        assert_eq!(summary["tool_choice"], if api == "messages" { serde_json::json!({"type":"none"}) } else { serde_json::json!("none") });
        if api == "completions" {
            assert_eq!(normal["thinking"], summary["thinking"]);
            assert_eq!(normal["reasoning_effort"], summary["reasoning_effort"]);
        }
        assert_eq!(summary["stream"], false);
    }
    assert_eq!(prefix, original, "summary planning rewrote the source history");
}

#[test]
fn anthropic_puts_breakpoints_on_system_tools_and_the_last_block() {
    let config: Config = serde_json::from_str(
        r#"{"providers":[{"name":"a","api":"messages","base_url":"https://api.anthropic.com",
             "models":[{"id":"claude","max_tokens":8000,"reasoning":true,"thinking_levels":["high"]}]}]}"#,
    )
    .unwrap();
    let (provider, model) = config.find("a/claude").unwrap();
    let tools = tools();
    let messages = vec![
        Message::System { content: SYSTEM.into() },
        Message::user_text("hi"),
    ];
    let request = Request {
        model,
        provider,
        messages: &messages,
        tools: &tools,
        level: "high",
        session_id: "s",
        cache_hints: true,
    };
    let body = serde_json::to_value(anthropic::build_request(&request)).unwrap();
    // Anthropic allows four breakpoints; pi uses three, and they must be the stable ones.
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    let tool_array = body["tools"].as_array().unwrap();
    assert!(tool_array[0]["cache_control"].is_null());
    assert_eq!(
        tool_array[tool_array.len() - 1]["cache_control"]["type"],
        "ephemeral"
    );
    let last_message = body["messages"].as_array().unwrap().last().unwrap();
    let last_block = last_message["content"].as_array().unwrap().last().unwrap();
    assert_eq!(last_block["cache_control"]["type"], "ephemeral");
}

#[test]
fn the_tool_block_is_identical_across_anthropic_turns() {
    let config: Config = serde_json::from_str(
        r#"{"providers":[{"name":"a","api":"messages","base_url":"https://api.anthropic.com",
             "models":[{"id":"claude","max_tokens":8000,"reasoning":true,"thinking_levels":["high"]}]}]}"#,
    )
    .unwrap();
    let (provider, model) = config.find("a/claude").unwrap();
    let tools = tools();
    let render = |messages: &[Message]| {
        let request = Request {
            model,
            provider,
            messages,
            tools: &tools,
            level: "high",
            session_id: "s",
            cache_hints: true,
        };
        serde_json::to_string(&serde_json::to_value(anthropic::build_request(&request)).unwrap()["tools"])
            .unwrap()
    };
    let short = vec![Message::user_text("one")];
    let longer = vec![Message::user_text("one"), Message::assistant_text("two"), Message::user_text("three")];
    assert_eq!(render(&short), render(&longer));
}
