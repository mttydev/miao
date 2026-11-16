//! Import Claude Code sessions into native miao Sessions, and report its
//! settings. Sessions live under `~/.claude/projects/<encoded>/<uuid>.jsonl`,
//! one JSON record per line: user and assistant message records, plus mode
//! and summary notes. Import is an explicit operator action — nothing here
//! runs on its own — and credential files are never read: sessions and
//! settings report only, identities stay with Claude.

use crate::store::Store;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// One importable session as found on disk.
#[derive(Debug)]
pub struct ScanEntry {
    pub session_id: String,
    pub workspace: String,
    pub path: PathBuf,
    pub messages: u64,
    pub first_prompt: Option<String>,
    pub last_timestamp: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub session: String,
    pub workspace: String,
    pub imported: u64,
    pub skipped_sidechain: u64,
    pub skipped_non_text: u64,
}

/// Claude Code encodes a project's absolute path by replacing every `/` with
/// `-`, which is lossy for names containing dashes themselves; the decoded
/// form is the best guess and the caller decides whether it exists.
pub fn decode_project(name: &str) -> String {
    name.replace('-', "/")
}

/// Every importable session under `base`, newest activity first.
pub fn scan(base: &Path) -> Vec<ScanEntry> {
    let mut entries = Vec::new();
    let Ok(dirs) = std::fs::read_dir(base) else {
        return entries;
    };
    for dir in dirs.flatten() {
        let project_dir = dir.file_name().to_string_lossy().into_owned();
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            if let Some(entry) = scan_file(&path, &project_dir) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|a, b| b.last_timestamp.cmp(&a.last_timestamp));
    entries
}

fn scan_file(path: &Path, project_dir: &str) -> Option<ScanEntry> {
    let file = std::fs::File::open(path).ok()?;
    let mut entry = ScanEntry {
        session_id: path.file_stem()?.to_string_lossy().into_owned(),
        workspace: decode_project(project_dir),
        path: path.to_path_buf(),
        messages: 0,
        first_prompt: None,
        last_timestamp: None,
        size_bytes: std::fs::metadata(path)
            .ok()
            .map(|meta| meta.len())
            .unwrap_or(0),
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = record.get("type").and_then(Value::as_str);
        if matches!(kind, Some("user") | Some("assistant"))
            && !record
                .get("isSidechain")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            && message_text(record.get("message")).is_some()
        {
            entry.messages += 1;
            if entry.first_prompt.is_none() && kind == Some("user") {
                entry.first_prompt = message_text(record.get("message"))
                    .map(|text| text.chars().take(120).collect());
            }
        }
        if let Some(timestamp) = record.get("timestamp").and_then(Value::as_str) {
            entry.last_timestamp = Some(timestamp.to_owned());
        }
    }
    Some(entry)
}

/// The text content of a Claude message: a plain string, or the text blocks
/// of a content array. Tool results, thinking, and tool_use blocks
/// contribute nothing — they carry no conversation prose to import.
fn message_text(message: Option<&Value>) -> Option<String> {
    let message = message?;
    match message.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// A Session already exists with the import identity: a repeat import would
/// adopt it and duplicate history, so the caller skips instead.
pub async fn already_imported(store: &Store, session_id: &str) -> bool {
    store
        .location(session_id)
        .await
        .map(|location| location.is_some())
        .unwrap_or(false)
}

/// Import one Claude session file as a new native Session whose history is
/// the conversation prose, in file order. Ordering follows the file's append
/// order, which is the order Claude wrote the turns.
pub async fn import_session(
    store: &Store,
    path: &Path,
    session_id: &str,
    workspace: &str,
    title: Option<&str>,
) -> Result<ImportReport, crate::protocol::Error> {
    let file = std::fs::File::open(path).map_err(|error| {
        crate::protocol::Error::Invalid(format!("cannot read {}: {error}", path.display()))
    })?;
    let mut report = ImportReport {
        session: session_id.to_owned(),
        workspace: workspace.to_owned(),
        ..Default::default()
    };
    store
        .initialize(
            session_id,
            title.unwrap_or("imported claude session"),
            workspace,
        )
        .await?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let Ok(record) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let role = match record.get("type").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        if record
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            report.skipped_sidechain += 1;
            continue;
        }
        let Some(text) = message_text(record.get("message")) else {
            report.skipped_non_text += 1;
            continue;
        };
        store
            .message(session_id, role, json!([{"type": "text", "text": text}]))
            .await?;
        report.imported += 1;
    }
    Ok(report)
}

/// Report the Claude settings that exist, without reading any credential
/// file: settings import maps facts, not identities, and credentials stay
/// with Claude.
pub fn report_settings(home: &Path) -> String {
    let path = home.join(".claude").join("settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return format!("no Claude settings at {}", path.display());
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        return format!("unreadable Claude settings at {}", path.display());
    };
    let mut report = format!("settings  {}\n", path.display());
    report.push_str(&format!("keys      {:?}\n", map.keys().collect::<Vec<_>>()));
    if let Some(model) = map.get("model").and_then(Value::as_str) {
        report.push_str(&format!("model     {model}\n"));
    }
    if let Some(permissions) = map.get("permissions").and_then(Value::as_object) {
        report.push_str(&format!(
            "allow     {} rule(s)\n",
            permissions
                .get("allow")
                .and_then(Value::as_array)
                .map(|rules| rules.len())
                .unwrap_or(0)
        ));
        report.push_str(&format!(
            "deny      {} rule(s)\n",
            permissions
                .get("deny")
                .and_then(Value::as_array)
                .map(|rules| rules.len())
                .unwrap_or(0)
        ));
    }
    report.push_str("note      sessions and settings report only; credentials are never read\n");
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path) -> PathBuf {
        let project = dir.join("-tmp-demo");
        std::fs::create_dir_all(&project).unwrap();
        let lines = [
            r#"{"type":"mode","mode":"normal","sessionId":"1a553f04"}"#,
            r#"{"parentUuid":null,"type":"user","message":{"role":"user","content":"按照embrix替换nos功能"},"uuid":"u1","timestamp":"2026-10-25T14:38:12.197Z"}"#,
            r#"{"parentUuid":"u1","isSidechain":true,"type":"user","message":{"role":"user","content":"sidechain"},"uuid":"u2","timestamp":"2026-10-25T14:38:13.000Z"}"#,
            r#"{"parentUuid":"u1","type":"assistant","message":{"id":"m1","type":"message","role":"assistant","model":"deepseek-flash","content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"答案一"}],"stop_reason":"end_turn"},"uuid":"a1","timestamp":"2026-10-25T14:38:20.000Z"}"#,
            r#"{"parentUuid":"a1","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"uuid":"u3","timestamp":"2026-10-25T14:38:21.000Z"}"#,
            r#"{"type":"summary","summary":"embrix nos 替换","leafUuid":"a1"}"#,
        ];
        let path = project.join("1a553f04-087a-47c0-a29f-e2d6d10af3cf.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    #[test]
    fn decode_project_restores_the_path_guess() {
        assert_eq!(
            decode_project("-Users-dingzg-workspace"),
            "/Users/dingzg/workspace"
        );
    }

    #[test]
    fn scan_counts_prose_turns_and_skips_sidechain() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let entries = scan(dir.path());
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.path, path);
        assert_eq!(entry.messages, 2);
        assert_eq!(entry.first_prompt.as_deref(), Some("按照embrix替换nos功能"));
        assert_eq!(
            entry.last_timestamp.as_deref(),
            Some("2026-10-25T14:38:21.000Z")
        );
        assert_eq!(entry.workspace, "/tmp/demo");
    }

    #[test]
    fn message_text_joins_only_text_blocks() {
        let message = json!({"content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"a"},{"type":"text","text":"b"}]});
        assert_eq!(message_text(Some(&message)).as_deref(), Some("a\nb"));
        let tool_only =
            json!({"content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]});
        assert_eq!(message_text(Some(&tool_only)), None);
    }

    #[tokio::test]
    async fn import_writes_prose_turns_as_a_native_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let store = Store::open(dir.path().join("engine.db")).await.unwrap();
        let report = import_session(
            &store,
            &path,
            "claude-1a553f04",
            "/tmp/demo",
            Some("embrix nos 替换"),
        )
        .await
        .unwrap();
        assert_eq!(report.imported, 2);
        assert_eq!(report.skipped_sidechain, 1);
        assert_eq!(report.skipped_non_text, 1);
        let snapshot = store.snapshot("claude-1a553f04").await.unwrap();
        assert_eq!(snapshot["message_count"], 2);
    }

    #[tokio::test]
    async fn an_imported_session_reports_as_already_imported() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path());
        let store = Store::open(dir.path().join("engine.db")).await.unwrap();
        import_session(&store, &path, "claude-1a553f04", "/tmp/demo", None)
            .await
            .unwrap();
        assert!(already_imported(&store, "claude-1a553f04").await);
        assert!(!already_imported(&store, "claude-other").await);
    }
}
