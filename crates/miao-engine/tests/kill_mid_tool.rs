use async_trait::async_trait;
use miao_engine::{
    permission::{Config, Decision, Mode, Policy, Rule},
    protocol::{Delivery, Input, ModelRequest},
    provider::{Provider, ProviderError, Reply},
    runtime::Runtime,
    store::Store,
    tools::Tools,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Each run replays one bash call that appends to the probe file. The crash
/// residue is seeded before the first run, so the guard — not timing — decides
/// whether the first re-issue executes.
struct Scripted {
    calls: Arc<AtomicUsize>,
    command: String,
}

#[async_trait]
impl Provider for Scripted {
    async fn stream(
        &self,
        _: ModelRequest,
        _: mpsc::Sender<Value>,
        _: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if matches!(call, 0 | 2) {
            return Ok(Reply {
                content: json!([
                    {"type": "tool_use", "id": format!("call-{call}"), "name": "bash", "input": {"command": self.command}}
                ]),
                usage: json!({}),
                needs_tools: true,
            });
        }
        Ok(Reply {
            content: json!([{"type": "text", "text": "done"}]),
            usage: json!({}),
            needs_tools: false,
        })
    }
}

fn policy() -> Policy {
    Policy::new(Config {
        mode: Mode::Workspace,
        allow_process: true,
        process_network: false,
        rules: vec![Rule {
            tool: "*".into(),
            path: "**".into(),
            decision: Decision::Allow,
        }],
        ..Config::default()
    })
    .unwrap()
}

fn input(id: &str) -> Input {
    Input {
        agent_references: Default::default(),
        command: None,
        session_id: "s".into(),
        input_id: id.into(),
        prompt: id.into(),
        delivery: Delivery::Queue,
    }
}

async fn wait_runs(store: &Store, minimum: usize, seconds: u64) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let finished = store
            .events("s", 0, 5000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "run.finished")
            .count();
        if finished >= minimum {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timeout waiting for {minimum} finished runs (got {finished})");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_events(store: &Store, kind: &str, minimum: usize, seconds: u64) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let found: Vec<Value> = store
            .events("s", 0, 5000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == kind)
            .map(|event| event.data)
            .collect();
        if found.len() >= minimum {
            return found;
        }
        if Instant::now() >= deadline {
            panic!("timeout waiting for {kind} (got {})", found.len());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Seed the durable residue a crashed process leaves behind: its run never
/// settled, so the dispatched tool row carries no result — the state the
/// guard consumes on the resumed run's first identical re-issue.
fn seed_crash_residue(db: &Path, fingerprint: &str) {
    // The rows are written through rusqlite rather than the `sqlite3` CLI: the
    // hosted Windows runner does not install that tool, and a fixture that
    // needs it fails with `program not found` instead of exercising the guard.
    let connection = rusqlite::Connection::open(db).unwrap();
    connection
        .execute_batch(&format!(
            "INSERT INTO engine_run(id,session_id,state,lease_epoch,shared_permission)
               VALUES('crashed-run','s','interrupted',0,0);
             INSERT INTO engine_tool(id,run_id,name,input,state,fingerprint)
               VALUES('crashed-run/call','crashed-run','bash','{{}}','unknown','{fingerprint}');"
        ))
        .unwrap();
}

#[tokio::test]
async fn kill_mid_tool_then_resume_consumes_one_unknown_guard() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.db");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    // The Session's root is compared by spelling against the location the
    // engine admits with, which is the canonical one. On Windows `tempdir`'s
    // spelling differs from `canonicalize`'s (verbatim prefix, folder case),
    // so the raw path recorded here made every later admission conflict
    // against a root naming the very same directory.
    let workspace = miao_engine::placement::canonical(&workspace);
    let marker = workspace.join("probe");
    // The call runs through bash on every platform (Git Bash on Windows), and
    // bash reads backslashes as escapes: an unquoted `C:\Users\…` target loses
    // its separators and the redirect lands on a mangled name instead of the
    // marker. Forward slashes keep the path literal for both shells.
    let literal = marker.to_string_lossy().replace('\\', "/");
    let command = format!("echo ran >> '{literal}'");

    let store = Store::open(&db).await.unwrap();
    store
        .initialize("s", "fixture", workspace.to_str().unwrap())
        .await
        .unwrap();

    // The exact fingerprint the run's tool projection will carry.
    let fingerprint = miao_engine::permission::digest(
        &serde_json::to_vec(&json!({"name": "bash", "input": {"command": command}})).unwrap(),
    );
    seed_crash_residue(&db, &fingerprint);

    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Runtime::with_policy(
        store.clone(),
        Arc::new(Scripted {
            calls: calls.clone(),
            command: command.clone(),
        }),
        Tools::new(&workspace)
            .await
            .unwrap()
            .with_process_runner(env!("CARGO_BIN_EXE_miao-engine").into()),
        policy(),
    )
    .await
    .unwrap();

    // Run 1: the identical call consumes the guard instead of executing —
    // the probe file stays absent, so no side effect ran.
    runtime.admit(input("one"), true).await.unwrap();
    let guards = wait_events(&store, "tool.idempotency_guard", 1, 10).await;
    assert_eq!(guards[0]["tool"], "bash");
    wait_runs(&store, 1, 10).await;
    assert!(
        !marker.exists(),
        "the guarded call must not execute its side effect"
    );

    // Run 2: the guard is consumed, so the identical call executes for real
    // and its side effect lands exactly once.
    runtime.admit(input("two"), true).await.unwrap();
    wait_runs(&store, 2, 10).await;
    runtime.shutdown().await;
    let written = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(written, "ran\n");

    // The guard fired exactly once and its notice reached the transcript once.
    let guards_after = wait_events(&store, "tool.idempotency_guard", 1, 5).await;
    assert_eq!(guards_after.len(), 1, "the guard must be one-time");
    let transcript = store
        .events("s", 0, 5000)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "message.committed")
        .map(|event| event.data.to_string())
        .filter(|text| text.contains("outcome is unknown"))
        .count();
    assert_eq!(transcript, 1, "one guard notice on the transcript");
}
