use miao_engine::store::Store;
use std::fs;

async fn fixture() -> (tempfile::TempDir, Store, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    store
        .initialize("s", "fixture", workspace.to_str().unwrap())
        .await
        .unwrap();
    (dir, store, workspace)
}

#[tokio::test]
async fn stage_restores_touched_files_and_clear_undoes_it() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "one").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    let captured = store.capture_file_snapshot("s", 1).await.unwrap();
    assert_eq!(captured["available"], true);

    fs::write(workspace.join("a.txt"), "two").unwrap();
    fs::write(workspace.join("later.txt"), "later").unwrap();
    // The Session's own later work is captured at its own settled boundary,
    // which is what tells the engine this write is not an outside edit.
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 2).await.unwrap();
    let staged = store.revert_stage("s", 1, true, true, false).await.unwrap();
    assert_eq!(staged["boundary_seq"], 1);
    assert_eq!(fs::read_to_string(workspace.join("a.txt")).unwrap(), "one");
    // A file that no snapshot recorded is not deleted.
    assert_eq!(
        fs::read_to_string(workspace.join("later.txt")).unwrap(),
        "later"
    );
    assert_eq!(store.staged_revert("s").await.unwrap()["boundary_seq"], 1);

    let cleared = store.revert_clear("s").await.unwrap();
    assert_eq!(cleared["boundary_seq"], 1);
    assert_eq!(fs::read_to_string(workspace.join("a.txt")).unwrap(), "two");
    assert!(store.staged_revert("s").await.unwrap().is_null());
    assert!(store.revert_clear("s").await.is_err());
}

#[tokio::test]
async fn commit_soft_reverts_and_leaves_no_stage() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "one").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    fs::write(workspace.join("a.txt"), "two").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 2).await.unwrap();
    store.revert_stage("s", 1, true, true, false).await.unwrap();
    let committed = store.revert_commit("s").await.unwrap();
    assert_eq!(committed["boundary_seq"], 1);
    assert!(store.staged_revert("s").await.unwrap().is_null());
    assert_eq!(fs::read_to_string(workspace.join("a.txt")).unwrap(), "one");
    assert!(store.revert_commit("s").await.is_err());
}

#[tokio::test]
async fn restore_refuses_escapes_and_missing_data() {
    let (_dir, store, workspace) = fixture().await;
    assert!(store.record_file_touch("s", "../outside").await.is_err());
    assert!(store.record_file_touch("s", "/absolute").await.is_err());

    fs::write(workspace.join("a.txt"), "one").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    let blobs = workspace
        .parent()
        .unwrap()
        .join("engine.snapshots")
        .join("s");
    fs::remove_dir_all(blobs).unwrap();
    fs::write(workspace.join("a.txt"), "two").unwrap();
    // The rewritten file reads as an outside edit to the gate, so the caller
    // forces it and the restore still refuses the data it no longer has.
    let error = store
        .revert_stage("s", 1, true, true, true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no longer available"));
    // A failed restore leaves no staged intent.
    assert!(store.staged_revert("s").await.unwrap().is_null());
}

#[tokio::test]
async fn stage_requires_a_captured_boundary() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "one").unwrap();
    assert!(store.revert_stage("s", 1, true, true, false).await.is_err());
}

#[tokio::test]
async fn a_history_only_stage_rewinds_a_session_that_never_touched_a_file() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "untouched").unwrap();
    // No capture exists, so a file revert has nothing to restore; the explicit
    // history-only stage still records the intent and commits.
    let staged = store
        .revert_stage("s", 1, false, true, false)
        .await
        .unwrap();
    assert_eq!(staged["restored"], serde_json::json!([]));
    assert_eq!(staged["removed"], serde_json::json!([]));
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "untouched"
    );
    let committed = store.revert_commit("s").await.unwrap();
    assert_eq!(committed["boundary_seq"], 1);
    assert_eq!(committed["scope"], "history");
    let snapshot = store.snapshot("s").await.unwrap();
    assert_eq!(snapshot["staged_revert"], serde_json::Value::Null);
    assert_eq!(snapshot["reverted_count"], 0);
}

/// The third boundary the criterion names: the workspace goes back to the
/// boundary and the transcript is left exactly as it is.
#[tokio::test]
async fn a_code_only_stage_restores_files_and_leaves_the_transcript_alone() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "one").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    store
        .message("s", "user", serde_json::json!("first"))
        .await
        .unwrap();
    store
        .message("s", "assistant", serde_json::json!("answer"))
        .await
        .unwrap();
    fs::write(workspace.join("a.txt"), "two").unwrap();
    // Captured at the later boundary, so the engine reads "two" as this
    // Session's own work rather than an outside edit.
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 2).await.unwrap();

    let staged = store
        .revert_stage("s", 1, true, false, false)
        .await
        .unwrap();
    assert_eq!(staged["scope"], "files");
    assert_eq!(staged["restored"], serde_json::json!(["a.txt"]));
    assert_eq!(fs::read_to_string(workspace.join("a.txt")).unwrap(), "one");
    // The stage's own vocabulary is durable, so a client that reconnects reads
    // the boundary instead of guessing it.
    assert_eq!(store.staged_revert("s").await.unwrap()["scope"], "files");

    let committed = store.revert_commit("s").await.unwrap();
    assert_eq!(committed["scope"], "files");
    assert_eq!(committed["reverted"], 0, "no message row moves");
    let snapshot = store.snapshot("s").await.unwrap();
    assert_eq!(snapshot["message_count"], 2);
    assert_eq!(snapshot["reverted_count"], 0);
    // A code-only commit is the end of the road: nothing was hidden, so a redo
    // has nothing to restore and the restored file stays restored.
    assert_eq!(fs::read_to_string(workspace.join("a.txt")).unwrap(), "one");
    let events = store.events("s", 0, 500).await.unwrap();
    assert!(events
        .iter()
        .any(|event| event.kind == "revert.staged" && event.data["scope"] == "files"));
    assert!(events
        .iter()
        .any(|event| event.kind == "revert.committed" && event.data["scope"] == "files"));
}

#[tokio::test]
async fn a_revert_that_moves_nothing_is_refused() {
    let (_dir, store, _workspace) = fixture().await;
    let error = store
        .revert_stage("s", 1, false, false, false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("must restore files"), "{error}");
    assert!(store.staged_revert("s").await.unwrap().is_null());
}

/// An outside edit is the thing a revert overwrites, so the stage refuses to
/// overwrite one until the caller says it showed the conflict.
#[tokio::test]
async fn a_stage_refuses_an_outside_edit_until_the_caller_forces_it() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "one").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    fs::write(workspace.join("a.txt"), "edited by hand").unwrap();

    let error = store
        .revert_stage("s", 1, true, true, false)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("changed outside this Session")
            && error.to_string().contains("a.txt"),
        "{error}"
    );
    // A refused stage leaves the outside edit and no intent behind.
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "edited by hand"
    );
    assert!(store.staged_revert("s").await.unwrap().is_null());

    let forced = store.revert_stage("s", 1, true, true, true).await.unwrap();
    assert_eq!(
        forced["conflicts_overwritten"],
        serde_json::json!([{ "path": "a.txt", "action": "restore", "disk": "modified" }])
    );
    assert_eq!(fs::read_to_string(workspace.join("a.txt")).unwrap(), "one");
}

#[tokio::test]
async fn preview_reports_the_file_plan_without_touching_the_workspace() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "one").unwrap();
    fs::write(workspace.join("gone.txt"), "here").unwrap();
    fs::write(workspace.join("old.txt"), "stale").unwrap();
    fs::remove_file(workspace.join("old.txt")).unwrap();
    for path in ["a.txt", "gone.txt", "old.txt"] {
        store.record_file_touch("s", path).await.unwrap();
    }
    store.capture_file_snapshot("s", 1).await.unwrap();

    // This Session's own later work: a.txt rewritten, gone.txt deleted,
    // old.txt recreated.
    fs::write(workspace.join("a.txt"), "two").unwrap();
    fs::remove_file(workspace.join("gone.txt")).unwrap();
    fs::write(workspace.join("old.txt"), "fresh").unwrap();
    for path in ["a.txt", "gone.txt", "old.txt"] {
        store.record_file_touch("s", path).await.unwrap();
    }
    store.capture_file_snapshot("s", 4).await.unwrap();

    let preview = store.revert_preview("s", 1).await.unwrap();
    assert_eq!(preview["available"], true);
    assert_eq!(preview["boundary_seq"], 1);
    assert_eq!(preview["restore"], serde_json::json!(["a.txt", "gone.txt"]));
    assert_eq!(preview["remove"], serde_json::json!(["old.txt"]));
    // The current content is exactly what the Session last recorded, so no
    // path reads as an outside edit.
    assert_eq!(preview["conflicts"], serde_json::json!([]));
    // The plan itself says what no boundary reverses, so a client does not have
    // to own the sentence about shell work.
    assert_eq!(preview["external_side_effects"], "not_reversible");
    // A preview is a read: the workspace and the stage are untouched.
    assert_eq!(fs::read_to_string(workspace.join("a.txt")).unwrap(), "two");
    assert!(store.staged_revert("s").await.unwrap().is_null());
}

#[tokio::test]
async fn preview_flags_a_path_that_changed_outside_the_session() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "one").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    // Something other than this Session rewrites the file after the capture.
    fs::write(workspace.join("a.txt"), "edited by hand").unwrap();
    let preview = store.revert_preview("s", 1).await.unwrap();
    assert_eq!(preview["restore"], serde_json::json!(["a.txt"]));
    assert_eq!(
        preview["conflicts"],
        serde_json::json!([{ "path": "a.txt", "action": "restore", "disk": "modified" }])
    );
    // A deletion outside the Session is a conflict too: restoring would
    // recreate a file the operator removed on purpose.
    fs::remove_file(workspace.join("a.txt")).unwrap();
    let preview = store.revert_preview("s", 1).await.unwrap();
    assert_eq!(
        preview["conflicts"],
        serde_json::json!([{ "path": "a.txt", "action": "restore", "disk": "missing" }])
    );
}

#[tokio::test]
async fn preview_says_when_a_boundary_has_no_file_record() {
    let (_dir, store, _workspace) = fixture().await;
    let preview = store.revert_preview("s", 1).await.unwrap();
    assert_eq!(preview["available"], false);
    assert_eq!(preview["reason"], "no-file-snapshot");
}

#[tokio::test]
async fn commit_reports_the_reverted_count_a_redo_can_rely_on() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), "one").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    store
        .message("s", "user", serde_json::json!("first"))
        .await
        .unwrap();
    store
        .message("s", "assistant", serde_json::json!("answer"))
        .await
        .unwrap();
    store.revert_stage("s", 1, true, true, false).await.unwrap();
    let committed = store.revert_commit("s").await.unwrap();
    assert_eq!(committed["reverted"], 2);
    // The durable count is what a redo affordance reads: the soft-reverted
    // rows leave the projection but stay in the ledger until `unrevert`.
    assert_eq!(store.snapshot("s").await.unwrap()["reverted_count"], 2);
    store.unrevert("s").await.unwrap();
    assert_eq!(store.snapshot("s").await.unwrap()["reverted_count"], 0);
}

fn git(workspace: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["-c", "user.email=test@test", "-c", "user.name=test"])
        .args(args)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn tracked_and_clean_file_restores_from_git() {
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("a.txt"), "committed").unwrap();
    git(&workspace, &["add", "a.txt"]);
    git(&workspace, &["commit", "-q", "-m", "one"]);
    store.record_file_touch("s", "a.txt").await.unwrap();
    let captured = store.capture_file_snapshot("s", 1).await.unwrap();
    assert_eq!(captured["available"], true);
    assert!(captured["head"].is_string(), "{captured}");
    // A clean tracked file records no blob: the git reference is the data.
    assert!(!_dir.path().join("engine.snapshots").join("s").exists());
    fs::write(workspace.join("a.txt"), "dirty").unwrap();
    let staged = store.revert_stage("s", 1, true, true, true).await.unwrap();
    assert_eq!(staged["restored"], serde_json::json!(["a.txt"]));
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "committed"
    );
    store.revert_clear("s").await.unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "dirty"
    );
}

#[tokio::test]
async fn oversized_tracked_file_restores_from_git() {
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    let big = "x".repeat(9 * 1024 * 1024);
    fs::write(workspace.join("big.bin"), &big).unwrap();
    git(&workspace, &["add", "big.bin"]);
    git(&workspace, &["commit", "-q", "-m", "big"]);
    store.record_file_touch("s", "big.bin").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    fs::write(workspace.join("big.bin"), "truncated").unwrap();
    store.revert_stage("s", 1, true, true, true).await.unwrap();
    assert_eq!(fs::read_to_string(workspace.join("big.bin")).unwrap(), big);
    store.revert_clear("s").await.unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("big.bin")).unwrap(),
        "truncated"
    );
}

#[tokio::test]
async fn dirty_tracked_file_captures_content_not_git() {
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("a.txt"), "committed").unwrap();
    git(&workspace, &["add", "a.txt"]);
    git(&workspace, &["commit", "-q", "-m", "one"]);
    fs::write(workspace.join("a.txt"), "edited").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    fs::write(workspace.join("a.txt"), "wiped").unwrap();
    store.revert_stage("s", 1, true, true, true).await.unwrap();
    // Restoring the pre-edit state proves the content was recorded, where a
    // git reference would have brought back the committed text.
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "edited"
    );
}

#[tokio::test]
async fn ignored_file_captures_content_not_git() {
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join(".gitignore"), "ignored.txt\n").unwrap();
    git(&workspace, &["add", ".gitignore"]);
    git(&workspace, &["commit", "-q", "-m", "ignore"]);
    fs::write(workspace.join("ignored.txt"), "secret").unwrap();
    store.record_file_touch("s", "ignored.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    fs::remove_file(workspace.join("ignored.txt")).unwrap();
    store.revert_stage("s", 1, true, true, true).await.unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("ignored.txt")).unwrap(),
        "secret"
    );
}

/// A clean tracked path the Session touched but did not change: its newest
/// record is the git reference itself and the disk holds exactly the
/// committed bytes, so the plan reads the path as this Session's own state,
/// not as an outside edit — no force, nothing to restore.
#[tokio::test]
async fn reverting_an_unmodified_clean_tracked_file_needs_no_force() {
    let big = "y".repeat(8 * 1024 * 1024 + 1);
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("big.bin"), &big).unwrap();
    git(&workspace, &["add", "big.bin"]);
    git(&workspace, &["commit", "-q", "-m", "big"]);
    store.record_file_touch("s", "big.bin").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();

    let preview = store.revert_preview("s", 1).await.unwrap();
    assert_eq!(preview["restore"], serde_json::json!([]));
    assert_eq!(preview["remove"], serde_json::json!([]));
    assert_eq!(preview["conflicts"], serde_json::json!([]));
    let staged = store
        .revert_stage("s", 1, true, false, false)
        .await
        .unwrap();
    assert_eq!(staged["restored"], serde_json::json!([]));
    assert_eq!(fs::read(workspace.join("big.bin")).unwrap(), big.as_bytes());
}

/// The core fix end to end and without force: the oversized tracked file was
/// recorded from HEAD, and the revert brings those bytes back.
#[tokio::test]
async fn an_oversized_tracked_file_restores_without_force() {
    let big = "x".repeat(8 * 1024 * 1024 + 1);
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("big.bin"), &big).unwrap();
    git(&workspace, &["add", "big.bin"]);
    git(&workspace, &["commit", "-q", "-m", "big"]);
    store.record_file_touch("s", "big.bin").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    fs::write(workspace.join("big.bin"), "changed").unwrap();
    store.record_file_touch("s", "big.bin").await.unwrap();
    store.capture_file_snapshot("s", 2).await.unwrap();

    let preview = store.revert_preview("s", 1).await.unwrap();
    assert_eq!(preview["restore"], serde_json::json!(["big.bin"]));
    assert_eq!(preview["conflicts"], serde_json::json!([]));
    let staged = store
        .revert_stage("s", 1, true, false, false)
        .await
        .unwrap();
    assert_eq!(staged["restored"], serde_json::json!(["big.bin"]));
    assert_eq!(fs::read_to_string(workspace.join("big.bin")).unwrap(), big);
}

/// The realistic small-file flow: the Session's own later write is captured
/// at its own boundary, so reverting to the earlier one is this Session's
/// undo — preview and stage agree, and clear brings the rewrite back.
#[tokio::test]
async fn reverting_a_tracked_file_after_its_own_write_needs_no_force() {
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("a.txt"), "committed").unwrap();
    git(&workspace, &["add", "a.txt"]);
    git(&workspace, &["commit", "-q", "-m", "one"]);
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    fs::write(workspace.join("a.txt"), "rewritten").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 2).await.unwrap();

    let preview = store.revert_preview("s", 1).await.unwrap();
    assert_eq!(preview["restore"], serde_json::json!(["a.txt"]));
    assert_eq!(preview["conflicts"], serde_json::json!([]));
    let staged = store
        .revert_stage("s", 1, true, false, false)
        .await
        .unwrap();
    assert_eq!(staged["restored"], serde_json::json!(["a.txt"]));
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "committed"
    );
    store.revert_clear("s").await.unwrap();
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "rewritten"
    );
}

/// Untracked paths are invisible to git tracking, so a fresh file stores its
/// own bytes exactly as it would outside a repository.
#[tokio::test]
async fn an_untracked_file_still_stores_its_own_bytes() {
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("a.txt"), "committed").unwrap();
    git(&workspace, &["add", "a.txt"]);
    git(&workspace, &["commit", "-q", "-m", "one"]);
    fs::write(workspace.join("fresh.txt"), "untracked").unwrap();
    store.record_file_touch("s", "fresh.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    // The blob, not a git reference, carries the content.
    let blobs = _dir.path().join("engine.snapshots").join("s");
    assert_eq!(fs::read_dir(&blobs).unwrap().count(), 1);
    fs::write(workspace.join("fresh.txt"), "rewritten").unwrap();
    store.record_file_touch("s", "fresh.txt").await.unwrap();
    store.capture_file_snapshot("s", 2).await.unwrap();
    let staged = store
        .revert_stage("s", 1, true, false, false)
        .await
        .unwrap();
    assert_eq!(staged["restored"], serde_json::json!(["fresh.txt"]));
    assert_eq!(
        fs::read_to_string(workspace.join("fresh.txt")).unwrap(),
        "untracked"
    );
}

/// Outside a repository nothing changed: an oversized file is recorded as
/// unrecoverable, no blob is stored, and the revert removes what it cannot
/// bring back.
#[tokio::test]
async fn outside_a_repository_an_oversized_file_stays_unrecoverable() {
    let (_dir, store, workspace) = fixture().await;
    fs::write(workspace.join("a.txt"), vec![3u8; 8 * 1024 * 1024 + 1]).unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    let captured = store.capture_file_snapshot("s", 1).await.unwrap();
    assert_eq!(captured["available"], true);
    assert!(captured["head"].is_null(), "{captured}");
    assert!(!_dir.path().join("engine.snapshots").join("s").exists());
    let staged = store
        .revert_stage("s", 1, true, false, false)
        .await
        .unwrap();
    assert_eq!(staged["removed"], serde_json::json!(["a.txt"]));
    assert!(!workspace.join("a.txt").exists());
}

/// A workspace may sit inside a repository rather than at its root; manifest
/// paths are workspace-relative, and `git show` must resolve them the same
/// way.
#[tokio::test]
async fn a_workspace_nested_in_a_repository_still_restores() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let workspace = repo.join("sub");
    fs::create_dir_all(&workspace).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(workspace.join("a.txt"), "committed").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "one"]);
    let store = Store::open(dir.path().join("engine.db")).await.unwrap();
    store
        .initialize("s", "fixture", workspace.to_str().unwrap())
        .await
        .unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    let captured = store.capture_file_snapshot("s", 1).await.unwrap();
    assert!(captured["head"].is_string(), "{captured}");
    fs::write(workspace.join("a.txt"), "rewritten").unwrap();
    store.record_file_touch("s", "a.txt").await.unwrap();
    store.capture_file_snapshot("s", 2).await.unwrap();
    let staged = store
        .revert_stage("s", 1, true, false, false)
        .await
        .unwrap();
    assert_eq!(staged["restored"], serde_json::json!(["a.txt"]));
    assert_eq!(
        fs::read_to_string(workspace.join("a.txt")).unwrap(),
        "committed"
    );
}

/// A touched name that reads like a glob pattern must still match itself and
/// take the git reference rather than a blob.
#[tokio::test]
async fn a_glob_shaped_name_still_captures_as_git() {
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("weird[1].txt"), "committed").unwrap();
    git(&workspace, &["add", "--", ":(literal)weird[1].txt"]);
    git(&workspace, &["commit", "-q", "-m", "one"]);
    store.record_file_touch("s", "weird[1].txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    // No blob: the literal path matched itself and took the git reference.
    assert!(!_dir.path().join("engine.snapshots").join("s").exists());
    fs::write(workspace.join("weird[1].txt"), "rewritten").unwrap();
    store.record_file_touch("s", "weird[1].txt").await.unwrap();
    store.capture_file_snapshot("s", 2).await.unwrap();
    let staged = store
        .revert_stage("s", 1, true, false, false)
        .await
        .unwrap();
    assert_eq!(staged["restored"], serde_json::json!(["weird[1].txt"]));
    assert_eq!(
        fs::read_to_string(workspace.join("weird[1].txt")).unwrap(),
        "committed"
    );
}

/// A clean tracked symlink stores through its target like the blob branch
/// always has: `git show` would answer the link string, not the content.
#[cfg(unix)]
#[tokio::test]
async fn a_tracked_symlink_still_stores_its_target_bytes() {
    let (_dir, store, workspace) = fixture().await;
    git(&workspace, &["init", "-q"]);
    fs::write(workspace.join("target.txt"), "pointed at").unwrap();
    std::os::unix::fs::symlink("target.txt", workspace.join("link.txt")).unwrap();
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-q", "-m", "one"]);
    store.record_file_touch("s", "link.txt").await.unwrap();
    store.capture_file_snapshot("s", 1).await.unwrap();
    // The blob carries the target's bytes, not the link string.
    let blobs = _dir.path().join("engine.snapshots").join("s");
    assert_eq!(fs::read_dir(&blobs).unwrap().count(), 1);
    fs::remove_file(workspace.join("link.txt")).unwrap();
    // Removing the recorded path outside a capture reads as an outside edit,
    // which this test forces past: it pins the content branch, not the gate.
    let staged = store.revert_stage("s", 1, true, false, true).await.unwrap();
    assert_eq!(staged["restored"], serde_json::json!(["link.txt"]));
    assert_eq!(
        fs::read_to_string(workspace.join("link.txt")).unwrap(),
        "pointed at"
    );
}
