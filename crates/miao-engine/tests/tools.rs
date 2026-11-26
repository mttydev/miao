use miao_engine::permission::ReadScope;
use miao_engine::tools::{ToolError, Tools};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn read_is_bounded_and_cannot_escape_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file"), "hello").unwrap();
    std::fs::write(dir.path().join("outside"), "secret").unwrap();
    std::fs::write(root.join("large"), "line of padding\n".repeat(4000)).unwrap();
    std::fs::write(root.join("binary"), [0xff]).unwrap();
    let tools = Tools::new(&root)
        .await
        .unwrap()
        .with_file_reads(ReadScope::Workspace);
    assert_eq!(
        tools
            .execute(
                "read_file",
                json!({"path":"file"}),
                CancellationToken::new()
            )
            .await
            .unwrap()["text"],
        "hello"
    );
    assert!(matches!(
        tools
            .execute(
                "read_file",
                json!({"path":"../outside"}),
                CancellationToken::new()
            )
            .await,
        Err(ToolError::OutsideWorkspace)
    ));
    assert!(matches!(
        tools
            .execute(
                "read_file",
                json!({"path":"binary"}),
                CancellationToken::new()
            )
            .await,
        Err(ToolError::InvalidFile)
    ));
    // An oversized whole-file read returns the first window instead of an
    // error: the model has no size information before reading, so the refusal
    // only cost a retry turn, and the window's flag says how to continue.
    let first = tools
        .execute(
            "read_file",
            json!({"path":"large"}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first["truncated"], json!(true), "{first}");
    assert_eq!(first["total_lines"], json!(4000), "{first}");
    assert!(
        first["note"]
            .as_str()
            .is_some_and(|note| note.contains("first window")),
        "{first}"
    );
    assert!(matches!(
        tools
            .execute("bash", json!({}), CancellationToken::new())
            .await,
        Err(ToolError::Unsupported)
    ));
    assert!(matches!(
        tools
            .execute(
                "read_file",
                json!({"path":"file","extra":true}),
                CancellationToken::new()
            )
            .await,
        Err(ToolError::InvalidInput)
    ));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("outside"), root.join("link")).unwrap();
        assert!(matches!(
            tools
                .execute(
                    "read_file",
                    json!({"path":"link"}),
                    CancellationToken::new()
                )
                .await,
            Err(ToolError::OutsideWorkspace)
        ));
    }
}
