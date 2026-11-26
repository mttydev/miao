use miao_engine::{permission::ReadScope, tools::Tools};
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// The product default: reads follow the OS user, so a workspace Session reads
/// a sibling tree without a reference grant. `readScope: "workspace"` is the
/// explicit jail, and it keeps the old behaviour.
#[tokio::test]
async fn file_reads_follow_the_user_unless_the_workspace_jail_is_selected() {
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let document = external.path().join("notes.md");
    std::fs::write(&document, "outside the workspace").unwrap();

    let tools = Tools::new(workspace.path()).await.unwrap();
    let read = tools
        .execute(
            "read_file",
            json!({"path": document.to_string_lossy()}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(read["text"], "outside the workspace");

    let listed = tools
        .execute(
            "list_files",
            json!({"path": external.path().to_string_lossy()}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["name"] == "notes.md"));

    let jailed = tools.with_file_reads(ReadScope::Workspace);
    assert!(jailed
        .execute(
            "read_file",
            json!({"path": document.to_string_lossy()}),
            CancellationToken::new(),
        )
        .await
        .is_err());
}
