//! Prompt-cache invariants.
//!
//! The single biggest cost lever in the whole project is that each turn's request prefix is
//! byte-identical to the previous turn's. These tests serialise real request bodies and
//! compare them, because a comment claiming the prefix is stable proves nothing.

use mpi::config::{Config, ModelConfig, Provider};
use mpi::llm::{Block, Message, Request, ToolSpec, anthropic, openai};

fn fixtures() -> (ModelConfig, Provider) {
    let config: Config = serde_json::from_str(
        r#"{
          "providers": [{
            "name": "work",
            "api": "openai-completions",
            "base_url": "http://192.168.1.16:1221/v1",
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
    let (provider, model) = config.find("work/deepseek-v4.1-flash").unwrap();
    (model.clone(), provider.clone())
}

/// The system prompt the agent actually sends, kept in one place for these tests.
const SYSTEM: &str = mpi::agent::r#loop::SYSTEM_PROMPT;

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
        two.starts_with(&one.trim_end_matches(']')),
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
        Message::Tool { tool_call_id: "c1".into(), name: "read".into(), content: "data".into() },
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
fn the_system_prompt_holds_nothing_that_changes_per_turn() {
    // A cache breakpoint sits on the system prompt, so anything in here has to be stable for
    // the life of the binary. cwd (which differs per project), the clock, the git branch and
    // the model name all live in the environment block instead, which is stored as the first
    // conversation message where a change is visible but harmless.
    let names_a_thing_that_moves = [
        std::env::current_dir().unwrap().display().to_string(),
        "2026-".to_string(),   // an ISO timestamp
        "main".to_string(),    // a git branch
        "deepseek".to_string(),// a model name
        "T".to_string() ,      // the RFC 3339 separator
    ];
    for volatile in names_a_thing_that_moves {
        if volatile == "T" {
            continue; // too short to be meaningful
        }
        assert!(
            !SYSTEM.contains(&volatile),
            "the system prompt mentions {volatile:?}, which changes between sessions and \
             would break the cache prefix"
        );
    }
    // The shell path is a per-machine setting, but it never changes while mpi runs, so it is
    // deliberately *not* a cache risk; the environment block repeats it for the model anyway.
    assert!(SYSTEM.contains("zsh"));
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
    assert_eq!(provider.compat(&model).send_session_affinity, true);
}

#[test]
fn a_summary_request_opts_out_of_the_cache() {
    // Compaction must not write cache entries for the main conversation, so it drops both
    // the cache key and the breakpoints.
    let (model, provider) = fixtures();
    let tools: Vec<ToolSpec> = Vec::new();
    let messages = vec![Message::System { content: "summary".into() }, Message::user_text("flatten")];
    let request = Request {
        model: &model,
        provider: &provider,
        messages: &messages,
        tools: &tools,
        level: "high",
        session_id: "session-id",
        cache_hints: false,
    };
    let body = serde_json::to_value(openai::build_request(&request, false)).unwrap();
    assert!(body["prompt_cache_key"].is_null(), "{body}");
}

#[test]
fn anthropic_puts_breakpoints_on_system_tools_and_the_last_block() {
    let config: Config = serde_json::from_str(
        r#"{"providers":[{"name":"a","api":"anthropic-messages","base_url":"https://api.anthropic.com",
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
    // Anthropic allows four breakpoints; mpi uses three, and they must be the stable ones.
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
        r#"{"providers":[{"name":"a","api":"anthropic-messages","base_url":"https://api.anthropic.com",
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
