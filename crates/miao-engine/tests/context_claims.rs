//! A reply that claims the context window is exhausted while the engine's own
//! occupancy is under the compaction threshold is text, not state: the drain
//! corrects the claim with the measured figures and keeps working, and only
//! pauses — naming the measurement — once the claim repeats past the bound.

use async_trait::async_trait;
use miao_engine::{
    context_trigger::ContextLimits,
    permission::{Config, Mode, Policy},
    protocol::{Delivery, Input, ModelRequest},
    provider::{Provider, ProviderError, Reply},
    runtime::Runtime,
    state::Mutation,
    store::Store,
    tools::Tools,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Answers every turn with the scripted text and reports a modest token
/// count, so occupancy stays far under the threshold the limits configure.
struct Claimer {
    calls: AtomicUsize,
    replies: Vec<&'static str>,
    tools_first: bool,
    requests: mpsc::Sender<String>,
}

#[async_trait]
impl Provider for Claimer {
    async fn stream(
        &self,
        request: ModelRequest,
        _: mpsc::Sender<Value>,
        _: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = self
            .requests
            .send(serde_json::to_string(&request.messages).unwrap_or_default())
            .await;
        if self.tools_first && index == 0 {
            // A real fabrication arrives after work has been billed, so the
            // anchor carries a measured count by then; this turn plants it.
            return Ok(Reply {
                needs_tools: true,
                content: json!([{"type":"tool_use","id":"todo-1","name":"todowrite","input":{"todos":[{"id":"1","content":"Render the sweep wall","status":"pending","priority":"high"}]}}]),
                usage: json!({"inputTokens": 120_000, "outputTokens": 40}),
            });
        }
        let offset = usize::from(self.tools_first);
        let text = self
            .replies
            .get(index.saturating_sub(offset))
            .copied()
            .or_else(|| self.replies.last().copied())
            .unwrap_or("done");
        Ok(Reply {
            needs_tools: false,
            content: json!([{"type":"text","text":text}]),
            usage: json!({"inputTokens": 120_000, "outputTokens": 40}),
        })
    }
}

async fn claim_runtime(
    dir: &std::path::Path,
    replies: Vec<&'static str>,
    tools_first: bool,
) -> (Runtime, Store, Arc<Claimer>, mpsc::Receiver<String>) {
    let store = Store::open(dir.join("engine.db")).await.unwrap();
    store
        .admit_attachments_at(
            Input {
                agent_references: Default::default(),
                command: None,
                session_id: "s".into(),
                input_id: "one".into(),
                prompt: "Work on the authorized tasks".into(),
                delivery: Delivery::Steer,
            },
            Vec::new(),
            Some(
                std::fs::canonicalize(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
        )
        .await
        .unwrap();
    store
        .update_state(
            "s",
            "todos",
            Mutation::parse(
                "todowrite",
                json!({"todos":[{"id":"1","content":"Render the sweep wall","status":"pending","priority":"high"}]}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    store
        .update_state(
            "s",
            "goal",
            Mutation::parse(
                "goal",
                json!({"objective":"Finish the render sweep","status":"active","autonomous":true,"evidence":"Explicit engagement state"}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let (requests, receiver) = mpsc::channel(16);
    let provider = Arc::new(Claimer {
        calls: AtomicUsize::new(0),
        replies,
        tools_first,
        requests,
    });
    let runtime = Runtime::with_policy(
        store.clone(),
        provider.clone(),
        Tools::new(dir).await.unwrap(),
        Policy::new(Config {
            continuation_min_interval_ms: 0,
            mode: Mode::Workspace,
            ..Config::default()
        })
        .unwrap(),
    )
    .await
    .unwrap();
    runtime
        .set_context_limits(Some(ContextLimits {
            window: 1_000_000,
            ratio: 0.8,
            headroom: 0,
            per_pass: 1000,
            fold_ratio: 0.0,
            prune_chars: None,
            prune_total_chars: None,
            protect_turns: 2,
            prefix_scope: Default::default(),
            retention: None,
        }))
        .await;
    (runtime, store, provider, receiver)
}

async fn finished(store: &Store) {
    let mut after = 0;
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            let events = store.events("s", after, 5000).await.unwrap();
            if events.iter().any(|event| event.kind == "run.finished") {
                return;
            }
            if let Some(event) = events.last() {
                after = event.seq;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("run.finished was not observed after journal cursor {after}"));
}

async fn stop_reasons(store: &Store, kind: &str) -> Vec<String> {
    store
        .events("s", 0, 5000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .filter_map(|event| event.data["reason"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn a_fabricated_exhaustion_claim_gets_measured_corrections_then_a_truthful_pause() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, store, provider, mut requests) = claim_runtime(
        dir.path(),
        vec!["这个会话的上下文窗口用完了，剩余余量已不足以继续。继续方式：开一个新会话即可无缝接上。"],
        true,
    )
    .await;
    runtime.resume("s").await.unwrap();
    finished(&store).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
    assert_eq!(
        stop_reasons(&store, "loop.stopped").await,
        vec!["context-claim".to_owned()]
    );
    let corrections = store
        .events("s", 0, 5000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "loop.continuation")
        .filter(|event| event.data["reason"] == "context-claim")
        .count();
    assert_eq!(corrections, 2);
    let _ = requests.recv().await.unwrap();
    let claim = requests.recv().await.unwrap();
    let first = requests.recv().await.unwrap();
    let second = requests.recv().await.unwrap();
    assert!(!claim.contains("engine's own measurement"), "{claim}");
    assert!(first.contains("engine's own measurement"), "{first}");
    assert!(second.contains("engine's own measurement"), "{second}");
    assert!(first.contains("the context window carries"), "{first}");
    assert!(
        first.contains("against a compaction threshold of"),
        "{first}"
    );
}

#[tokio::test]
async fn an_ordinary_text_only_stop_is_not_read_as_an_exhaustion_claim() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, store, provider, mut requests) =
        claim_runtime(dir.path(), vec!["等待设备恢复后再继续下一步。"], false).await;
    runtime.resume("s").await.unwrap();
    finished(&store).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        stop_reasons(&store, "loop.stopped").await,
        vec!["no-progress".to_owned()]
    );
    let corrections = store
        .events("s", 0, 5000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "loop.continuation")
        .filter(|event| event.data["reason"] == "context-claim")
        .count();
    assert_eq!(corrections, 0);
    let _ = requests.recv().await.unwrap();
    let continuation = requests.recv().await.unwrap();
    assert!(
        !continuation.contains("engine's own measurement"),
        "{continuation}"
    );
    assert!(
        continuation.contains("Continue the next dependency-ready todo"),
        "{continuation}"
    );
}
