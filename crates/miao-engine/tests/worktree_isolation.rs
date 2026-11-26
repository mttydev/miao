//! Two Sessions on one checkout must never share a mutable HEAD.
//!
//! Placement is decided by the engine, not by a model reading `git status` and
//! choosing to stop. These tests drive the real runtime against a real git
//! repository so the worktree, the recorded root and the scoped tool root are
//! all checked on the path a turn actually uses.

use async_trait::async_trait;
use miao_engine::{
    protocol::{Delivery, Error, Input, ModelRequest},
    provider::{Provider, ProviderError, Reply},
    runtime::Runtime,
    store::Store,
    tools::Tools,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Placement never asks a provider for a turn; a call here is a test bug.
struct Idle;

#[async_trait]
impl Provider for Idle {
    async fn stream(
        &self,
        _request: ModelRequest,
        _progress: mpsc::Sender<Value>,
        _cancel: CancellationToken,
    ) -> Result<Reply, ProviderError> {
        Err(ProviderError::Interrupted)
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn repository(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
    git(dir, &["add", "tracked.txt"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn canonical(path: &Path) -> PathBuf {
    miao_engine::placement::canonical(path)
}

#[tokio::test]
async fn a_second_session_is_placed_in_its_own_worktree() {
    let dir = tempfile::tempdir().unwrap();
    repository(dir.path());
    let before_head = git(dir.path(), &["rev-parse", "HEAD"]);

    let state = tempfile::tempdir().unwrap();
    let store = Store::open(state.path().join("engine.db")).await.unwrap();
    let tools = Tools::new(dir.path()).await.unwrap();
    let runtime = Runtime::new(store.clone(), Arc::new(Idle), tools.clone())
        .await
        .unwrap();

    runtime.initialize("ses_one", "one").await.unwrap();
    runtime.initialize("ses_two", "two").await.unwrap();

    let checkout = canonical(dir.path()).to_string_lossy().to_string();
    assert_eq!(
        store.session_root("ses_one").await.unwrap(),
        Some(checkout.clone())
    );

    let second = store.session_root("ses_two").await.unwrap().unwrap();
    assert_ne!(second, checkout);
    assert!(
        Path::new(&second).starts_with(canonical(dir.path()).join(".miao-worktrees")),
        "second Session must be under .miao-worktrees, got {second}"
    );
    assert!(Path::new(&second).is_dir());
    assert!(git(dir.path(), &["worktree", "list", "--porcelain"]).contains(&second));

    // The first Session's checkout is not disturbed by placing the second.
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), before_head);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");

    // Every later tool call resolves paths against the Session's own root.
    assert_eq!(
        tools.clone().with_read_scope("ses_two").location(),
        second.as_str()
    );

    // A write that names a file in the main checkout is outside the worktree.
    let prepared = tools
        .clone()
        .with_writes(true)
        .with_read_scope("ses_two")
        .prepare(
            "write_file",
            json!({
                "path": dir.path().join("tracked.txt").display().to_string(),
                "text": "overwritten\n",
                "expected_sha256": null
            }),
        )
        .await;
    assert!(
        prepared.is_err(),
        "writing the main checkout must be refused"
    );

    // A git command redirected at the main checkout is refused before it runs.
    let redirected = tools
        .clone()
        .with_read_scope("ses_two")
        .prepare(
            "bash",
            json!({
                "command": format!("git -C {} checkout -- tracked.txt", dir.path().display())
            }),
        )
        .await;
    assert!(
        redirected.is_err(),
        "git redirected at the main checkout must be refused"
    );
}

#[tokio::test]
async fn admitting_a_prompt_keeps_a_worktree_session_on_its_worktree() {
    let dir = tempfile::tempdir().unwrap();
    repository(dir.path());
    let state = tempfile::tempdir().unwrap();
    let store = Store::open(state.path().join("engine.db")).await.unwrap();
    let tools = Tools::new(dir.path()).await.unwrap();
    let runtime = Runtime::new(store.clone(), Arc::new(Idle), tools)
        .await
        .unwrap();

    runtime.initialize("ses_one", "one").await.unwrap();
    runtime.initialize("ses_two", "two").await.unwrap();
    let second = store.session_root("ses_two").await.unwrap().unwrap();

    let admitted = runtime
        .admit(
            Input {
                agent_references: Default::default(),
                command: None,
                session_id: "ses_two".into(),
                input_id: "two-1".into(),
                prompt: "work in the worktree".into(),
                delivery: Delivery::Steer,
            },
            false,
        )
        .await
        .expect("a placed Session admits at its own root");
    assert!(!admitted.duplicate);
    assert_eq!(
        store.session_root("ses_two").await.unwrap(),
        Some(second),
        "admission must not move the Session back to the checkout"
    );
}

#[tokio::test]
async fn a_worktree_session_can_change_its_model_and_account() {
    let dir = tempfile::tempdir().unwrap();
    repository(dir.path());
    let state = tempfile::tempdir().unwrap();
    let store = Store::open(state.path().join("engine.db")).await.unwrap();
    let tools = Tools::new(dir.path()).await.unwrap();
    let runtime = Runtime::new(store.clone(), Arc::new(Idle), tools)
        .await
        .unwrap();

    runtime.initialize("ses_one", "one").await.unwrap();
    runtime.initialize("ses_two", "two").await.unwrap();
    let second = store.session_root("ses_two").await.unwrap().unwrap();
    assert_ne!(
        second,
        canonical(dir.path()).to_string_lossy(),
        "the second Session must be the worktree one"
    );

    runtime
        .select_model("ses_two", None)
        .await
        .expect("a worktree Session selects a model at its own root");
    let account = runtime.select_account("ses_two", None).await;
    assert!(
        !matches!(account, Err(Error::Conflict)),
        "a worktree Session must reach its account selection, not be rejected by the engine root: {account:?}"
    );
}

/// A worktree removed outside miao is re-homed before its next turn. Startup
/// skips the rows recorded at the engine root (the fallback already resolves
/// them), but a worktree placement still gets an entry, so a missing one is
/// repaired instead of silently falling back to the shared checkout.
#[tokio::test]
async fn a_removed_worktree_is_rehomed_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    repository(dir.path());
    let state = tempfile::tempdir().unwrap();
    let store = Store::open(state.path().join("engine.db")).await.unwrap();
    let gone = canonical(dir.path())
        .join(".miao-worktrees")
        .join("gone")
        .to_string_lossy()
        .to_string();
    store
        .initialize("ses_lost", "lost worktree", &gone)
        .await
        .unwrap();
    assert!(!Path::new(&gone).exists());

    let tools = Tools::new(dir.path()).await.unwrap();
    let runtime = Runtime::new(store.clone(), Arc::new(Idle), tools.clone())
        .await
        .unwrap();

    let rehomed = store.session_root("ses_lost").await.unwrap().unwrap();
    assert_ne!(rehomed, gone, "the missing root must be replaced");
    assert!(
        Path::new(&rehomed).is_dir(),
        "the Session was not re-homed at a directory: {rehomed}"
    );
    assert_eq!(
        tools.with_read_scope("ses_lost").location(),
        rehomed,
        "the repaired root must also reach the tools"
    );
    drop(runtime);
}
