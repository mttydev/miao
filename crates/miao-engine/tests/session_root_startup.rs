//! Startup must not walk a workspace's whole Session history.
//!
//! Every historical Session recorded at the engine root used to be
//! re-registered one at a time, costing one or two `git rev-parse` processes
//! each. On a workspace with hundreds of Sessions that work ran long after the
//! window opened; a quit while it was still going dropped the tokio blocking
//! pool under the remaining rows, which reported
//! `file IO failed: background task failed` for Sessions that were perfectly
//! usable. An unregistered Session already resolves to the engine root, so
//! only worktree placements need an entry.

use async_trait::async_trait;
use miao_engine::{
    protocol::ModelRequest,
    provider::{Provider, ProviderError, Reply},
    runtime::Runtime,
    store::Store,
    tools::Tools,
};
use serde_json::Value;
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

/// Two hundred Sessions at the engine root must cost no per-Session git work.
///
/// This test lives in its own integration binary because it rewrites `PATH`:
/// every `git` invocation goes through a shim that logs it and then execs the
/// real tool, so the assertion sees the processes the runtime started without
/// changing what they do.
#[cfg(unix)]
#[tokio::test]
async fn a_long_session_history_costs_no_git_processes_at_startup() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    repository(dir.path());

    let state = tempfile::tempdir().unwrap();
    let store = Store::open(state.path().join("engine.db")).await.unwrap();
    let root = canonical(dir.path()).to_string_lossy().to_string();
    for index in 0..200 {
        store
            .initialize(&format!("ses_{index:04}"), "history", &root)
            .await
            .unwrap();
    }

    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("git.log");
    let shim = bin.path().join("git");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec /usr/bin/git \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let previous = std::env::var_os("PATH").unwrap_or_default();
    std::env::set_var(
        "PATH",
        format!("{}:{}", bin.path().display(), previous.to_string_lossy()),
    );

    let tools = Tools::new(dir.path()).await.unwrap();
    Runtime::new(store, Arc::new(Idle), tools).await.unwrap();

    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        calls.lines().count() < 20,
        "startup walked the Session history: {} git call(s)\n{calls}",
        calls.lines().count()
    );
}
