//! The settle row carries how long a step took, not when it happened: the
//! status dialog renders the value as seconds, so an epoch stamp reads as
//! hundreds of thousands of seconds.

use async_trait::async_trait;
use miao_engine::{
    protocol::{Delivery, Input, ModelRequest},
    provider::{Provider, ProviderError, Reply},
    runtime::Runtime,
    store::Store,
    tools::Tools,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Instant;

#[async_trait]
impl Provider for Instant {
    async fn stream(
        &self,
        _: ModelRequest,
        _: mpsc::Sender<Value>,
        _: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: json!([{"type":"text","text":"done"}]),
            usage: json!({}),
            needs_tools: false,
        })
    }
}

#[tokio::test]
async fn a_settled_step_reports_a_duration_not_an_epoch_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    let runtime = Runtime::new(
        store.clone(),
        Arc::new(Instant),
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();
    runtime
        .admit(
            Input {
                agent_references: Default::default(),
                command: None,
                session_id: "s".into(),
                input_id: "one".into(),
                prompt: "go".into(),
                delivery: Delivery::Steer,
            },
            true,
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if store
                .events("s", 0, 500)
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
    .expect("the turn settled");
    let report = store.usage_report("s").await.unwrap();
    let detail = report["providerDetail"]
        .as_array()
        .expect("provider detail");
    let turn_ms = detail
        .last()
        .and_then(|row| row["turnMs"].as_u64())
        .expect("a settled turn carries its duration");
    assert!(
        turn_ms < 60_000,
        "a duration, not a wall-clock stamp: {turn_ms}"
    );
    runtime.shutdown().await;
}
