//! A host that wants to compare its own config reader with the engine's has no
//! way to ask unless the engine can answer: this pins the read-only command that
//! returns the normalized document for the same ordered sources.

use serde_json::{json, Value};
use std::{
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn the_engine_reports_the_document_it_normalized() {
    let dir = tempfile::tempdir().unwrap();
    // The process-enabled runtime refuses a database inside the workspace.
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("miao.json"),
        json!({"model": "fixture/model", "shell": "/bin/zsh"}).to_string(),
    )
    .unwrap();
    let sources = json!([{"dir": workspace.to_string_lossy()}]).to_string();
    let mut process = Command::new(env!("CARGO_BIN_EXE_miao-engine"))
        .args([
            "serve",
            "--db",
            dir.path().join("engine.db").to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "--model",
            "fixture",
            "--config-sources",
            &sources,
        ])
        .env("ANTHROPIC_API_KEY", "fixture-key")
        // The spawned engine must not see the operator's state: a stored
        // credential in HOME turns the fixture start into a different
        // derivation and aborts it ("invalid default model route"), which is
        // exactly the state a self-hosted runner carries.
        .env("HOME", dir.path().join("home"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("MIAO_DATA_HOME", dir.path().join("data"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    for request in [
        json!({"id":1,"method":"config_normalized"}),
        json!({"id":2,"method":"shutdown"}),
    ] {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let document = replies
        .iter()
        .find(|reply| reply["id"] == 1)
        .expect("config_normalized reply")["result"]
        .clone();
    // Exactly the merged document this source plan describes: one file, two
    // keys, nothing invented.
    assert_eq!(
        document,
        json!({"model": "fixture/model", "shell": "/bin/zsh"}),
        "{document:#}"
    );
}

#[test]
fn a_process_without_ordered_sources_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut process = Command::new(env!("CARGO_BIN_EXE_miao-engine"))
        .args([
            "serve",
            "--db",
            dir.path().join("engine.db").to_str().unwrap(),
            "--workspace",
            dir.path().to_str().unwrap(),
            "--model",
            "fixture",
        ])
        .env("ANTHROPIC_API_KEY", "fixture-key")
        // Same isolation as above: the refusal under test is about the
        // ordered sources, not about whatever the runner's HOME happens to
        // hold.
        .env("HOME", dir.path().join("home"))
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("MIAO_DATA_HOME", dir.path().join("data"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = process.stdin.take().unwrap();
    for request in [
        json!({"id":1,"method":"config_normalized"}),
        json!({"id":2,"method":"shutdown"}),
    ] {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = process.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let reply = replies
        .iter()
        .find(|reply| reply["id"] == 1)
        .expect("config_normalized reply");
    let rendered = reply.to_string();
    assert!(
        rendered.contains("without ordered config sources"),
        "the refusal did not explain itself: {rendered}"
    );
}
