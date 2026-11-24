use miao_engine::{
    catalog,
    protocol::{Delivery, Input},
    store::Store,
};
use rusqlite::Connection;
use serde_json::json;
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[tokio::test]
async fn catalog_reads_live_storage_without_projecting_history_or_adopting_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.db");
    let store = Store::open(&path).await.unwrap();
    store
        .initialize("s", "Retained title", "/workspace")
        .await
        .unwrap();
    store
        .message(
            "s",
            "user",
            json!([{"type":"text","text":"x".repeat(4 * 1024 * 1024)}]),
        )
        .await
        .unwrap();
    store
        .admit(Input {
            session_id: "s".into(),
            input_id: "pending".into(),
            prompt: "stay pending".into(),
            delivery: Delivery::Queue,
            agent_references: Default::default(),
            command: None,
        })
        .await
        .unwrap();
    let before = store.events("s", 0, 100).await.unwrap().len();
    let result = catalog::read(&path, &["s".into(), "missing".into()], true)
        .await
        .unwrap();
    assert_eq!(result[0]["snapshot"]["title"], "Retained title");
    assert!(result[0]["snapshot"].get("messages").is_none());
    assert!(result[0]["snapshot"].get("pending").is_none());
    assert!(result[0]["usage"].is_object());
    // A Session this storage never held is absent, not unreadable: no snapshot
    // and no error, so a listing can skip it without warning about it.
    assert!(result[1].get("snapshot").is_none());
    assert!(result[1].get("error").is_none());
    assert!(serde_json::to_vec(&result).unwrap().len() < 10_000);
    assert_eq!(store.events("s", 0, 100).await.unwrap().len(), before);
    assert_eq!(
        store.pending_inputs("s", None, 10).await.unwrap()["inputs"][0]["id"],
        "pending"
    );

    // A reader coexists with the live writer but cannot create or mutate data.
    let reader = Store::open_reader(&path).await.unwrap();
    assert!(reader.ensure("forbidden").await.is_err());
    reader.close().await.unwrap();
    assert_eq!(
        store.snapshot("s").await.unwrap()["title"],
        "Retained title"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_miao-engine"))
        .args([
            "catalog",
            "--db",
            path.to_str().unwrap(),
            "--sessions",
            "[\"s\"]",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cli[0]["snapshot"]["title"], "Retained title");
    assert_eq!(store.events("s", 0, 100).await.unwrap().len(), before);
    store.close().await.unwrap();
}

#[tokio::test]
async fn catalog_never_creates_a_missing_database_or_migrates_an_unrelated_one() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.db");
    assert!(catalog::read(&missing, &["s".into()], false).await.is_err());
    assert!(!missing.exists());
    let unrelated = dir.path().join("unrelated.db");
    let conn = rusqlite::Connection::open(&unrelated).unwrap();
    conn.execute_batch("CREATE TABLE existing(id TEXT); PRAGMA user_version=9999;")
        .unwrap();
    assert!(catalog::read(&unrelated, &["s".into()], false)
        .await
        .is_err());
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        9999
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='engine_session'",
            [],
            |row| row.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn old_catalog_metadata_survives_unavailable_usage_without_a_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA application_id=1296646469;
        CREATE TABLE engine_session(id TEXT PRIMARY KEY,title TEXT);
        INSERT INTO engine_session VALUES('old','Preserved title');
        CREATE TABLE engine_event(session_id TEXT,seq INTEGER,kind TEXT,data TEXT);
        CREATE TABLE engine_lineage(session_id TEXT,parent_session_id TEXT);",
    )
    .unwrap();
    let result = catalog::read(&path, &["old".into()], true).await.unwrap();
    assert_eq!(result[0]["snapshot"]["title"], "Preserved title");
    assert_eq!(
        result[0]["snapshot"]["created_at_ms"],
        serde_json::Value::Null
    );
    assert!(result[0]["usage_error"].is_string());
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('engine_session')",
            [],
            |row| row.get::<_, u32>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn catalog_accepts_bounded_stdin_instead_of_a_long_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.db");
    let store = Store::open(&path).await.unwrap();
    let id = format!("ses_{}", "x".repeat(252));
    store
        .initialize(&id, "Long identity", "/workspace")
        .await
        .unwrap();
    let input = serde_json::to_vec(&vec![&id; 200]).unwrap();
    assert!(input.len() > 32_767);
    let mut child = Command::new(env!("CARGO_BIN_EXE_miao-engine"))
        .args(["catalog", "--db", path.to_str().unwrap(), "--sessions", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entries: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(entries.as_array().unwrap().len(), 200);
    assert!(entries
        .as_array()
        .unwrap()
        .iter()
        .all(|entry| entry["snapshot"]["title"] == "Long identity"));
    store.close().await.unwrap();
}

#[test]
fn catalog_rejects_stdin_beyond_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_miao-engine"))
        .args(["catalog", "--db", path.to_str().unwrap(), "--sessions", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&vec![b' '; 1024 * 1024 + 1])
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("catalog input exceeds 1 MiB"));
    assert!(!path.exists());
}

/// The batch row and the single-Session snapshot answer the same question, so
/// they have to agree: a client parses both with one parser, and a field that
/// drifts would make the list disagree with the Session it opens.
#[tokio::test]
async fn a_catalog_row_matches_the_snapshot_it_stands_for() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("engine.db")).await.unwrap();
    store.initialize("ses_a", "First", "/work/a").await.unwrap();
    store
        .initialize("ses_b", "Second", "/work/b")
        .await
        .unwrap();
    store
        .message(
            "ses_a",
            "user",
            json!([{"type":"text","text":"hello from the fixture"}]),
        )
        .await
        .unwrap();

    let catalog = store.session_catalog().await.unwrap();
    assert_eq!(catalog.len(), 2, "every Session is listed, in one reply");
    let row = catalog
        .iter()
        .find(|row| row["session_id"] == "ses_a")
        .expect("ses_a is listed");
    let snapshot = store.snapshot_with_metadata("ses_a", true).await.unwrap();
    for field in [
        "session_id",
        "title",
        "parent_session_id",
        "location",
        "updated_at_ms",
        "archived_at_ms",
        "pinned_at_ms",
        "preview",
        "model_selection",
        "account_selection",
    ] {
        assert_eq!(
            row.get(field),
            snapshot.get(field),
            "the batch row and the snapshot disagree on {field}"
        );
    }
    assert_eq!(row["title"], "First");
    assert_eq!(row["location"], "/work/a");
    // The batch is a listing, never a transcript.
    assert!(snapshot.get("messages").is_none());
    assert!(row.get("messages").is_none());
    store.close().await.unwrap();
}

/// A dated comparison of the two ways to build the same list, not a gate.
///
/// `cargo test --test catalog -- --ignored --nocapture` prints both numbers, so
/// a regression in the batch path shows up as a measurement instead of a slow
/// first frame somebody has to notice.
#[tokio::test]
#[ignore]
async fn one_catalog_reply_beats_a_snapshot_per_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("engine.db")).await.unwrap();
    for index in 0..200 {
        let id = format!("ses_{index:03}");
        store
            .initialize(&id, &format!("Session {index}"), "/work")
            .await
            .unwrap();
        store
            .message(
                &id,
                "user",
                json!([{"type":"text","text":format!("prompt {index}")}]),
            )
            .await
            .unwrap();
    }
    let ids = store.sessions().await.unwrap();
    let started = std::time::Instant::now();
    for id in &ids {
        store.snapshot_with_metadata(id, true).await.unwrap();
    }
    let per_session = started.elapsed();
    let started = std::time::Instant::now();
    let rows = store.session_catalog().await.unwrap();
    let batch = started.elapsed();
    println!(
        "{} sessions: {:?} as one snapshot each, {:?} as one catalog",
        ids.len(),
        per_session,
        batch
    );
    assert_eq!(rows.len(), ids.len());
    store.close().await.unwrap();
}

/// The Session dialog reads the catalog as a one-screen agent view: a run
/// still going marks the row alive, and the todo/goal state rides the row so
/// an operator sees a running Session without opening it.
#[tokio::test]
async fn a_catalog_row_carries_the_live_state_the_agent_view_reads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.db");
    let store = Store::open(&path).await.unwrap();
    store
        .initialize("ses_live", "Live", "/work/live")
        .await
        .unwrap();
    // A run is going: the durable marker the dialog's ● chip reads.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO engine_run(id,session_id,state,lease_epoch) VALUES('run-1','ses_live','running',0)",
            [],
        )
        .unwrap();
    }
    // Todo state: one entry left open, one already finished.
    store
        .record_at(
            "ses_live",
            "session.state.updated",
            json!({"operation_id":"t1","kind":"todos","value":{"todos":[
                {"content":"left","status":"in_progress","priority":"high"},
                {"content":"done","status":"completed","priority":"high"}
            ]}}),
            1,
        )
        .await
        .unwrap();
    store
        .record_at(
            "ses_live",
            "session.state.updated",
            json!({"operation_id":"g1","kind":"goal","value":{"objective":"ship","status":"active"}}),
            2,
        )
        .await
        .unwrap();

    let catalog = store.session_catalog().await.unwrap();
    let row = catalog
        .iter()
        .find(|row| row["session_id"] == "ses_live")
        .expect("ses_live is listed");
    assert_eq!(row["active_run"], "run-1", "the row is marked alive");
    assert_eq!(row["todos_open"], 1, "only the open todo counts");
    assert_eq!(row["goal_armed"], true, "the armed goal rides the row");
}

async fn stored_preview(store: &Store, session: &str) -> serde_json::Value {
    store
        .session_catalog()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row["session_id"] == session)
        .expect("the Session is listed")["preview"]
        .clone()
}

/// The list line is kept when its text commits: the catalog reads the row
/// instead of scanning message content, so a cold store never pays for the
/// history it does not show.
#[tokio::test]
async fn a_catalog_preview_follows_the_newest_visible_text() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("engine.db")).await.unwrap();
    store.initialize("s", "Preview", "/work").await.unwrap();
    store
        .message("s", "user", json!([{"type":"text","text":"first prompt"}]))
        .await
        .unwrap();
    assert_eq!(
        stored_preview(&store, "s").await,
        json!({"text":"first prompt","truncated":false})
    );
    // A tool-only turn is not what the conversation last said.
    store
        .message(
            "s",
            "assistant",
            json!([{"type":"tool_use","id":"c1","name":"bash","input":{}}]),
        )
        .await
        .unwrap();
    assert_eq!(
        stored_preview(&store, "s").await,
        json!({"text":"first prompt","truncated":false})
    );
    // Whitespace collapses so the row stays one line.
    store
        .message(
            "s",
            "assistant",
            json!([{"type":"text","text":"  a\n b   c "}]),
        )
        .await
        .unwrap();
    assert_eq!(
        stored_preview(&store, "s").await,
        json!({"text":"a b c","truncated":false})
    );
    // The line caps at the inbox preview's 200 characters.
    store
        .message("s", "user", json!([{"type":"text","text":"x".repeat(240)}]))
        .await
        .unwrap();
    assert_eq!(
        stored_preview(&store, "s").await,
        json!({"text":"x".repeat(200),"truncated":true})
    );
    store.close().await.unwrap();
}

/// A revert hides messages without deleting them, so the kept line has to be
/// rewritten from what is still visible; a restore puts it back.
#[tokio::test]
async fn a_revert_rewrites_the_preview_and_unrevert_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("engine.db")).await.unwrap();
    store.initialize("s", "Revert", "/work").await.unwrap();
    store
        .admit_at(
            Input {
                agent_references: Default::default(),
                command: None,
                session_id: "s".into(),
                input_id: "one".into(),
                prompt: "first".into(),
                delivery: Delivery::Queue,
            },
            Some("workspace".into()),
        )
        .await
        .unwrap();
    let promoted = store.promote("s", true).await.unwrap();
    let checkpoint = promoted[0].data["checkpoint"]
        .as_str()
        .expect("checkpoint on the promoted event")
        .to_string();
    assert_eq!(
        stored_preview(&store, "s").await,
        json!({"text":"first","truncated":false})
    );
    store.revert("s", &checkpoint).await.unwrap();
    assert_eq!(
        stored_preview(&store, "s").await,
        json!(null),
        "a reverted turn leaves nothing readable"
    );
    store.unrevert("s").await.unwrap();
    assert_eq!(
        stored_preview(&store, "s").await,
        json!({"text":"first","truncated":false})
    );
    store.close().await.unwrap();
}

/// A store written before the preview table still lists its lines: the upgrade
/// fills them once instead of leaving the catalog to scan content forever.
#[tokio::test]
async fn an_upgrade_fills_previews_for_a_store_written_before_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("engine.db");
    let store = Store::open(&path).await.unwrap();
    store.initialize("s", "Old", "/work").await.unwrap();
    store
        .message(
            "s",
            "user",
            json!([{"type":"text","text":"written before the table"}]),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute("DROP TABLE engine_preview", [])
        .expect("drop the preview table");
    connection
        .pragma_update(None, "user_version", miao_engine::store::SCHEMA_VERSION - 1)
        .expect("rewind the schema version");
    drop(connection);
    let store = Store::open(&path).await.unwrap();
    assert_eq!(
        stored_preview(&store, "s").await,
        json!({"text":"written before the table","truncated":false})
    );
    store.close().await.unwrap();
}
