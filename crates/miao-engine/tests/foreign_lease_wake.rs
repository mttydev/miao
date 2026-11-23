//! A cross-process wake must not kill the live owner's run.
//!
//! Regression for the interruption storm: a Session draining in one process
//! (fresh lease, run in flight) received a peer's wake, and the sender's drain
//! claimed the lease, reconciled the Session, and marked the owner's run
//! `interrupted` mid-tool. The drain now leaves the mail in the durable inbox
//! for the owner, and still takes over once the lease is no longer live.

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

/// Answers every turn with plain text, so a promoted input completes.
struct Text;

#[async_trait]
impl Provider for Text {
    async fn stream(
        &self,
        _request: ModelRequest,
        _progress: mpsc::Sender<Value>,
        _cancel: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        Ok(Reply {
            content: json!([{"type": "text", "text": "done"}]),
            usage: json!({}),
            needs_tools: false,
        })
    }
}

fn mail(id: &str) -> Input {
    Input {
        agent_references: Default::default(),
        command: None,
        session_id: "a".into(),
        input_id: id.into(),
        prompt: "Message from Session b:\ncoordinate".into(),
        delivery: Delivery::Steer,
    }
}

async fn wait_for_finished(store: &Store) {
    // A busy machine can stretch the takeover's store writes well past five
    // seconds; the drain itself is bounded by the lease, not by this wait.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let finished = store
                .events("a", 0, 1000)
                .await
                .unwrap()
                .iter()
                .any(|event| event.kind == "run.finished" && event.data["reason"] == "completed");
            if finished {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the takeover drain finished its run");
}

#[tokio::test]
async fn a_wake_leaves_the_mail_when_a_live_foreign_owner_drains() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.db");
    let store = Store::open(db.clone()).await.unwrap();
    let root = miao_engine::placement::canonical(dir.path())
        .to_string_lossy()
        .to_string();
    store.initialize("a", "Owner", &root).await.unwrap();

    // The owner's process: a fresh claim plus a run still in flight.
    store.claim_session("a", 30_000).await.unwrap();
    store.start_run("a", "owner-run").await.unwrap();

    // A live lease held by a different, real process, the way a second window
    // or a peer's engine would hold it.
    let mut foreign = std::process::Command::new("sleep")
        .arg("6")
        .spawn()
        .unwrap();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE engine_lease SET owner_pid=?1 WHERE session_id='a'",
            rusqlite::params![foreign.id()],
        )
        .unwrap();
    }

    let runtime = Runtime::new(
        store.clone(),
        Arc::new(Text),
        Tools::new(dir.path()).await.unwrap(),
    )
    .await
    .unwrap();

    // A peer delivers mail and wakes the Session. The drain must leave the
    // owner's run alone: the mail stays durable, and the owner promotes it at
    // its next provider-turn boundary.
    runtime.admit(mail("msg-1"), true).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let events = store.events("a", 0, 1000).await.unwrap();
    assert!(
        !events.iter().any(|event| event.kind == "run.finished"),
        "the owner's run was not interrupted: {events:?}"
    );
    assert_eq!(
        store.active_run("a").await.unwrap().as_deref(),
        Some("owner-run")
    );
    let pending = store.pending_inputs("a", None, 10).await.unwrap()["inputs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        pending.len(),
        1,
        "the mail waits for the owner: {pending:?}"
    );

    // Once the foreign lease is no longer live, the same wake takes over: the
    // stale run is reconciled as interrupted, and the waiting mail runs.
    foreign.kill().unwrap();
    let _ = foreign.wait().unwrap();
    runtime.admit(mail("msg-2"), true).await.unwrap();
    wait_for_finished(&store).await;

    let events = store.events("a", 0, 1000).await.unwrap();
    let interrupted = events
        .iter()
        .find(|event| {
            event.kind == "run.finished"
                && event.data["reason"] == "interrupted"
                && event.data["recovered"] == true
        })
        .expect("the stale run was reconciled");
    assert_eq!(interrupted.data["run_id"], "owner-run");
    assert!(
        events
            .iter()
            .any(|event| event.kind == "run.finished" && event.data["reason"] == "completed"),
        "the takeover ran the waiting mail: {events:?}"
    );
    assert!(store.pending_inputs("a", None, 10).await.unwrap()["inputs"]
        .as_array()
        .unwrap()
        .is_empty());
    runtime.shutdown().await;
}

/// The reconciliation itself — not only the wake path — must respect a live
/// foreign owner: `recover_session` runs after a claim on the drain and shell
/// paths, and unconditionally killing every running row is what turned a
/// peer's wake into an interruption storm.
#[tokio::test]
async fn reconciliation_spares_a_run_whose_lease_is_alive_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.db");
    let store = Store::open(db.clone()).await.unwrap();
    let root = miao_engine::placement::canonical(dir.path())
        .to_string_lossy()
        .to_string();
    store.initialize("a", "Owner", &root).await.unwrap();
    store.claim_session("a", 30_000).await.unwrap();
    store.start_run("a", "owner-run").await.unwrap();

    let mut foreign = std::process::Command::new("sleep")
        .arg("6")
        .spawn()
        .unwrap();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE engine_lease SET owner_pid=?1 WHERE session_id='a'",
            rusqlite::params![foreign.id()],
        )
        .unwrap();
    }

    store.recover_session(Some("a")).await.unwrap();
    assert_eq!(
        store.active_run("a").await.unwrap().as_deref(),
        Some("owner-run"),
        "a live foreign lease keeps its run"
    );

    foreign.kill().unwrap();
    let _ = foreign.wait().unwrap();
    store.recover_session(Some("a")).await.unwrap();
    assert_eq!(
        store.active_run("a").await.unwrap(),
        None,
        "a dead lease releases its run to reconciliation"
    );
}
