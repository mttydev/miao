use async_trait::async_trait;
use miao_engine::{
    protocol::ModelRequest,
    provider::{Identity, Provider, ProviderError, Reply},
    runtime::Runtime,
    state::Mutation,
    store::Store,
    tools::Tools,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Answer;

#[async_trait]
impl Provider for Answer {
    fn identity(&self) -> Option<Identity> {
        Some(Identity {
            protocol: "fixture".into(),
            model: "wire-shared".into(),
        })
    }
    async fn stream(
        &self,
        _: ModelRequest,
        _: mpsc::Sender<Value>,
        _: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: json!([{"type": "text", "text": "done"}]),
            usage: json!({}),
            needs_tools: false,
        })
    }
}

async fn setup(dir: &std::path::Path) -> (Store, Runtime) {
    let store = Store::open(dir.join("engine.db")).await.unwrap();
    let runtime = Runtime::new(
        store.clone(),
        Arc::new(Answer),
        Tools::new(dir).await.unwrap(),
    )
    .await
    .unwrap();
    store
        .initialize(
            "s",
            "fixture",
            std::fs::canonicalize(dir).unwrap().to_str().unwrap(),
        )
        .await
        .unwrap();
    (store, runtime)
}

/// A spent Goal budget stops every drain before its first provider turn, so the
/// `goal` tool that could rewrite the budget is unreachable. The operator's
/// raise writes a new revision — which is what reopens the accounting window —
/// and every other field stays the model's record.
#[tokio::test]
async fn raising_a_goal_budget_writes_a_new_revision_and_keeps_the_goal() {
    let dir = tempfile::tempdir().unwrap();
    let (store, runtime) = setup(dir.path()).await;
    store
        .update_state(
            "s",
            "goal",
            Mutation::parse(
                "goal",
                json!({
                    "objective": "ship the offset reporting",
                    "status": "active",
                    "budget": {"iterations": 5, "tokens": 1000},
                    "stop_when": ["the suite passes"],
                    "autonomous": true
                }),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let before = store.state("s").await.unwrap();
    let before_revision = before["goal"]["revision"].as_u64().unwrap();
    let raised = runtime.raise_goal_budget("s", 40).await.unwrap();
    assert!(
        raised["revision"].as_u64().unwrap() > before_revision,
        "a raise is a new revision: {raised}"
    );
    let state = store.state("s").await.unwrap();
    let goal = &state["goal"]["value"];
    assert_eq!(goal["budget"]["iterations"], 40);
    assert_eq!(
        goal["budget"]["tokens"], 1000,
        "the other budget axes stay: {goal}"
    );
    assert_eq!(goal["objective"], "ship the offset reporting");
    assert_eq!(goal["status"], "active");
    assert_eq!(goal["stop_when"], json!(["the suite passes"]));
    assert_eq!(goal["autonomous"], true);
    runtime.shutdown().await;
}

#[tokio::test]
async fn an_unusable_budget_or_a_missing_goal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (store, runtime) = setup(dir.path()).await;
    assert!(runtime.raise_goal_budget("s", 10).await.is_err());
    store
        .update_state(
            "s",
            "goal",
            Mutation::parse(
                "goal",
                json!({"objective": "ship it", "status": "active", "budget": {"iterations": 5}}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let before = store.state("s").await.unwrap();
    for turns in [0, 10_001] {
        assert!(runtime.raise_goal_budget("s", turns).await.is_err());
    }
    assert_eq!(store.state("s").await.unwrap(), before);
    runtime.shutdown().await;
}
