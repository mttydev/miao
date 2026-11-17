//! A foreground `task` call must not pin the parent turn for the whole child.
//! This file is its own process, so the threshold env var cannot leak into
//! other writers of the same engine test binary.

use async_trait::async_trait;
use miao_engine::{
    permission::{Config, Decision, Mode, Policy, Rule},
    protocol::{Delivery, Input, Message, ModelRequest},
    provider::{Provider, ProviderError, Reply},
    runtime::Runtime,
    store::Store,
    tools::Tools,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct SlowChild;

#[async_trait]
impl Provider for SlowChild {
    async fn stream(
        &self,
        request: ModelRequest,
        _: mpsc::Sender<Value>,
        _: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        let text = request
            .messages
            .iter()
            .filter_map(|message| message.content.as_array())
            .flatten()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains("slow work") {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            return Ok(Reply {
                content: json!([{"type":"text","text":"child done"}]),
                usage: json!({}),
                needs_tools: false,
            });
        }
        let has_result = request.messages.iter().any(|message| {
            message
                .content
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["type"] == "tool_result"))
        });
        if !has_result {
            return Ok(Reply {
                content: json!([{"type":"tool_use","id":"d1","name":"task","input":{"prompt":"slow work"}}]),
                usage: json!({}),
                needs_tools: true,
            });
        }
        Ok(Reply {
            content: json!([{"type":"text","text":"parent done"}]),
            usage: json!({}),
            needs_tools: false,
        })
    }
}

fn policy() -> Policy {
    Policy::new(Config {
        mode: Mode::Workspace,
        allow_subagents: true,
        max_provider_turns: Some(10),
        rules: vec![Rule {
            tool: "*".into(),
            path: "**".into(),
            decision: Decision::Allow,
        }],
        ..Config::default()
    })
    .unwrap()
}

fn tool_result(history: &[Message]) -> Value {
    for message in history {
        for part in message.content.as_array().into_iter().flatten() {
            if part["type"] != "tool_result" {
                continue;
            }
            if let Some(text) = part["content"].as_str() {
                if let Ok(value) = serde_json::from_str::<Value>(text) {
                    return value;
                }
            }
        }
    }
    Value::Null
}

#[tokio::test]
async fn a_foreground_task_hands_off_to_the_background_after_the_threshold() {
    std::env::set_var("MIAO_TASK_BACKGROUND_MS", "10");
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let runtime = Runtime::with_policy(
        store.clone(),
        Arc::new(SlowChild),
        Tools::new(dir.path()).await.unwrap(),
        policy(),
    )
    .await
    .unwrap();
    runtime
        .admit(
            Input {
                agent_references: Default::default(),
                command: None,
                session_id: "parent".into(),
                input_id: "one".into(),
                prompt: "delegate this".into(),
                delivery: Delivery::Steer,
            },
            true,
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if store
                .events("parent", 0, 500)
                .await
                .unwrap()
                .iter()
                .any(|event| event.kind == "run.finished")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the parent turn settled");

    let result = tool_result(&store.history("parent").await.unwrap());
    assert_eq!(result["auto_background"], true, "{result}");
    assert_eq!(result["background"], true, "{result}");
    let child = result["session_id"].as_str().expect("child id").to_owned();

    // The child keeps running without the parent: it settles into its own
    // report, and a completed child admits no input into the parent.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let settled = store
                .events(&child, 0, 500)
                .await
                .unwrap()
                .iter()
                .any(|event| event.kind == "run.finished");
            let listed = store
                .subagents("parent")
                .await
                .unwrap()
                .iter()
                .any(|task| task["id"] == json!(child) && task["status"] != json!("running"));
            if settled && listed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the auto-backgrounded child settled");
    let history = store.history(&child).await.unwrap();
    assert_eq!(history.last().unwrap().content[0]["text"], "child done");
    // The handoff is durable and discoverable: a parent that returns before
    // its child must still find it through task_list/task_result.
    let tasks = store.subagents("parent").await.unwrap();
    let row = tasks
        .iter()
        .find(|task| task["id"] == json!(child))
        .unwrap_or_else(|| panic!("the handoff is not listed: {tasks:?}"));
    assert_eq!(row["auto_background"], true, "{row}");
    assert_eq!(row["status"], "completed", "{row}");
    runtime.shutdown().await;
}
