//! Session-persistent memory: facts the agent saves about the workspace,
//! injected into context so they survive compaction and restart. Stored in
//! `.miao/memory.json` inside the workspace — a separate store from
//! knowledge notes, which carry source-freshness contracts that auto-saved
//! memories cannot satisfy (there is no file to hash for "the user prefers
//! short answers").
//!
//! Entries the agent saves start unreviewed. The operator reviews them (via
//! the knowledge tool's toggle pattern) or they expire. The system prompt
//! tells the model the memory exists; the model decides what to save.

use crate::protocol::{Error, Message};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One saved memory: a durable fact the agent chose to remember about the
/// workspace, its tooling, or the operator's preferences.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub content: String,
    pub origin_session: String,
    pub created_at_ms: u64,
    /// The review switch: an unreviewed entry is advisory only and is
    /// listed for the operator to confirm or discard.
    #[serde(default)]
    pub reviewed: bool,
    #[serde(default)]
    pub disabled: bool,
    /// Machine-extracted at a turn boundary rather than saved through the
    /// tool. Defaulted so stores written before auto-extraction still parse.
    #[serde(default)]
    pub auto: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Document {
    pub version: u32,
    pub entries: Vec<Entry>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
}

/// The memory store path relative to the workspace root.
pub const MEMORY_PATH: &str = ".miao/memory.json";

/// 128 KiB: enough for hundreds of one-line facts, small enough that a
/// corrupted store cannot stall context assembly.
const STORE_LIMIT: usize = 128 * 1024;

/// Load the memory document. A missing file returns the default (empty).
/// A corrupted store also returns the default — memory is advisory and must
/// never block a session from starting.
pub fn load(root: &Path) -> Document {
    let path = root.join(MEMORY_PATH);
    let Ok(bytes) = std::fs::read(&path) else {
        return Document::default();
    };
    if bytes.len() > STORE_LIMIT {
        return Document::default();
    }
    serde_json::from_slice::<Document>(&bytes).unwrap_or_default()
}

/// Save the memory document, creating `.miao/` if needed.
pub fn save(root: &Path, document: &Document) -> std::io::Result<()> {
    let path = root.join(MEMORY_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_string_pretty(document)?;
    if bytes.len() > STORE_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "memory store exceeds 128 KiB",
        ));
    }
    std::fs::write(&path, bytes)
}

/// The memory block injected into the system context: the entries the model
/// saved, so they survive compaction and restart. Unreviewed and disabled
/// entries are included (the model should know what it saved even if the
/// operator has not confirmed it yet), but disabled entries are marked.
pub fn context_block(document: &Document) -> Option<String> {
    let active: Vec<&Entry> = document.entries.iter().filter(|e| !e.disabled).collect();
    if active.is_empty() {
        return None;
    }
    let mut block = String::from("<memory>\n");
    for entry in &active {
        let marker = if entry.reviewed { "" } else { " (unreviewed)" };
        block.push_str(&format!("- {}{}\n", entry.content, marker));
    }
    block.push_str("</memory>");
    Some(block)
}

/// The id for a new memory entry: `mem_<timestamp_ms>_<short_random>`.
pub fn new_id(now_ms: u64) -> String {
    format!(
        "mem_{now_ms}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    )
}

/// The turn-boundary auto-extraction switch: off by default, and a stride of
/// zero would never fire, so both defaults survive any malformed flag.
#[derive(Clone, Copy, Debug)]
pub struct AutoExtract {
    pub enabled: bool,
    pub every_turns: u32,
}

impl Default for AutoExtract {
    fn default() -> Self {
        Self {
            enabled: false,
            every_turns: 10,
        }
    }
}

impl AutoExtract {
    /// Tolerant flag parse: missing, `off`, or a malformed value falls back to
    /// the default rather than refusing the start — extraction is an optional
    /// enrichment, not a configuration the engine may fail on.
    pub fn from_flags(enabled: Option<&str>, every_turns: Option<&str>) -> Self {
        let mut config = Self::default();
        if enabled.is_some_and(|flag| matches!(flag, "on" | "true")) {
            config.enabled = true;
        }
        if let Some(turns) = every_turns
            .and_then(|flag| flag.parse::<u32>().ok())
            .filter(|turns| *turns > 0)
        {
            config.every_turns = turns;
        }
        config
    }
}

/// How many transcript messages one extraction pass reads.
pub const EXTRACTION_WINDOW: usize = 20;

/// The longest one saved fact may be, matching the tool's own input bound.
const FACT_LIMIT: usize = 500;

/// Auto-extraction proposes at most this many facts per pass: one quiet call
/// must not flood a store the operator has not reviewed.
const AUTO_EXTRACT_MAX: usize = 5;

/// The stride gate: fire on settled assistant replies whose committed count
/// lands exactly on the configured stride.
pub fn extraction_due(assistant_messages: u64, every_turns: u32) -> bool {
    assistant_messages > 0 && assistant_messages.is_multiple_of(u64::from(every_turns.max(1)))
}

/// Lenient extraction of the model's answer: the first `[` through the last
/// `]` parsed as a JSON array of strings. A chatty answer, wrapped prose, or
/// a malformed array yields nothing rather than poisoning the store.
pub fn extract_candidates(text: &str) -> Vec<String> {
    let Some(start) = text.find('[') else {
        return Vec::new();
    };
    let Some(end) = text.rfind(']') else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(&text[start..=end])
        .unwrap_or_default()
        .into_iter()
        .map(|fact| fact.trim().to_owned())
        .filter(|fact| !fact.is_empty())
        .collect()
}

/// Merge one extraction pass into the document: each fact is truncated to the
/// content bound, the batch is capped, and a fact the store (or the pass)
/// already holds word for word is skipped. Returns how many entries landed.
pub fn merge_auto(
    document: &mut Document,
    session: &str,
    now_ms: u64,
    candidates: Vec<String>,
) -> usize {
    let mut appended = 0;
    for fact in candidates {
        if appended >= AUTO_EXTRACT_MAX {
            break;
        }
        let content: String = fact.chars().take(FACT_LIMIT).collect();
        if content.is_empty()
            || document
                .entries
                .iter()
                .any(|entry| entry.content == content)
        {
            continue;
        }
        document.entries.push(Entry {
            id: new_id(now_ms),
            content,
            origin_session: session.to_owned(),
            created_at_ms: now_ms,
            reviewed: false,
            disabled: false,
            auto: true,
        });
        appended += 1;
    }
    appended
}

/// The extraction prompt: the recent transcript plus instructions to return
/// only durable facts as a JSON string array. Transient task state is
/// explicitly forbidden — the store holds facts that outlive the task.
pub fn extraction_prompt(messages: &[Message]) -> String {
    let transcript = messages
        .iter()
        .filter_map(|message| {
            let text = message
                .content
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then(|| format!("{}: {}", message.role, text))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "Below is the recent transcript of a coding session. Extract only durable facts worth remembering across sessions: the operator's preferences, project constraints, tooling conventions, and standing decisions. Exclude transient task state, current work in progress, file contents, and anything already obvious from the repository. Reply with a JSON array of short fact strings, at most five, and [] when there is nothing durable to record. Reply with only the JSON array.\n\n{transcript}"
    )
}

impl crate::store::Store {
    /// The settled assistant replies this Session has committed, excluding
    /// reverted and quarantined rows: the count the stride gate compares.
    pub async fn assistant_message_count(&self, session: &str) -> Result<u64, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM engine_message WHERE session_id=?1 AND role='assistant' AND reverted=0 AND quarantined=0",
                [&session],
                |row| row.get(0),
            )?)
        })
        .await
    }

    /// The newest committed transcript slice in conversation order, for the
    /// extraction prompt. Tool calls and results ride the same rows; the
    /// prompt keeps only their text blocks.
    pub async fn recent_transcript(
        &self,
        session: &str,
        limit: usize,
    ) -> Result<Vec<Message>, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT role,content FROM engine_message WHERE session_id=?1 AND reverted=0 AND quarantined=0 ORDER BY seq DESC LIMIT ?2",
            )?;
            let mut rows = stmt.query(rusqlite::params![session, limit as i64])?;
            let mut messages = Vec::new();
            while let Some(row) = rows.next()? {
                let content: String = row.get(1)?;
                messages.push(Message {
                    role: row.get(0)?,
                    content: serde_json::from_str(&content)?,
                    checkpoint: None,
                    recorded_at_ms: None,
                });
            }
            messages.reverse();
            Ok(messages)
        })
        .await
    }
}

/// One background extraction pass: recent transcript in, one small tool-free
/// provider call, lenient parse, merge, save. Spawned off a settled turn; it
/// never blocks the turn and every failure is swallowed — the store keeps
/// whatever the last successful pass saved.
pub(crate) async fn auto_extract(
    store: crate::store::Store,
    provider: std::sync::Arc<dyn crate::provider::Provider>,
    stop: tokio_util::sync::CancellationToken,
    root: std::path::PathBuf,
    session: String,
) {
    let Ok(messages) = store.recent_transcript(&session, EXTRACTION_WINDOW).await else {
        return;
    };
    if messages.is_empty() {
        return;
    }
    let Some(text) = extract_text(
        extract_reply(&provider, &stop, extraction_prompt(&messages))
            .await
            .as_ref(),
    ) else {
        return;
    };
    let candidates = extract_candidates(&text);
    if candidates.is_empty() {
        return;
    }
    let now = crate::approval::now_ms();
    let session_for_merge = session.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let mut document = load(&root);
        let session = session_for_merge;
        if merge_auto(&mut document, &session, now, candidates) > 0 {
            save(&root, &document)
        } else {
            Ok(())
        }
    })
    .await;
}

async fn extract_reply(
    provider: &std::sync::Arc<dyn crate::provider::Provider>,
    stop: &tokio_util::sync::CancellationToken,
    prompt: String,
) -> Option<crate::provider::Reply> {
    let request = crate::protocol::ModelRequest {
        system: "You extract durable facts from a coding session transcript. Reply with only a JSON array of short fact strings.".into(),
        messages: vec![Message {
            role: "user".into(),
            content: serde_json::json!([{"type": "text", "text": prompt}]),
            checkpoint: None,
            recorded_at_ms: None,
        }],
        tools: vec![],
    };
    // Progress must be drained even though only the final reply is consumed.
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    provider.stream(request, tx, stop.child_token()).await.ok()
}

fn extract_text(reply: Option<&crate::provider::Reply>) -> Option<String> {
    let text = reply?
        .content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let doc = load(dir.path());
        assert!(doc.entries.is_empty());
    }

    #[test]
    fn save_and_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut doc = Document::default();
        doc.entries.push(Entry {
            id: new_id(1000),
            content: "uses bun, not npm".into(),
            origin_session: "ses_test".into(),
            created_at_ms: 1000,
            reviewed: false,
            disabled: false,
            auto: true,
        });
        save(dir.path(), &doc).unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].content, "uses bun, not npm");
        assert!(!loaded.entries[0].reviewed);
        assert!(loaded.entries[0].auto);
    }

    #[test]
    fn a_document_written_before_auto_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".miao")).unwrap();
        let legacy = serde_json::json!({
            "version": 1,
            "entries": [{
                "id": "mem_1_abcdef01",
                "content": "uses bun",
                "origin_session": "s",
                "created_at_ms": 1,
            }],
        });
        std::fs::write(dir.path().join(MEMORY_PATH), legacy.to_string()).unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.entries.len(), 1);
        assert!(!loaded.entries[0].auto);
    }

    #[test]
    fn an_oversized_store_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".miao")).unwrap();
        let path = dir.path().join(MEMORY_PATH);
        std::fs::write(path, vec![b'x'; 128 * 1024 + 1]).unwrap();
        assert!(load(dir.path()).entries.is_empty());
    }

    #[test]
    fn the_stride_gate_fires_only_on_multiples() {
        assert!(!extraction_due(0, 10));
        assert!(!extraction_due(9, 10));
        assert!(extraction_due(10, 10));
        assert!(!extraction_due(11, 10));
        assert!(extraction_due(20, 10));
    }

    #[test]
    fn candidates_come_from_the_json_array_only() {
        assert_eq!(extract_candidates("[]"), Vec::<String>::new());
        assert_eq!(
            extract_candidates("[\"uses bun\",\"prefers short answers\"]"),
            vec!["uses bun", "prefers short answers"]
        );
        assert_eq!(
            extract_candidates("Sure: [\"uses bun\"] — done."),
            vec!["uses bun"]
        );
        assert!(extract_candidates("no array here").is_empty());
        assert!(extract_candidates("[\"oops\",").is_empty());
        assert!(extract_candidates("[1,2,3]").is_empty());
        assert_eq!(extract_candidates("  [\"  padded  \"]  "), vec!["padded"]);
    }

    #[test]
    fn merge_auto_truncates_caps_and_deduplicates() {
        let mut doc = Document::default();
        doc.entries.push(Entry {
            id: "m0".into(),
            content: "uses bun".into(),
            origin_session: "s".into(),
            created_at_ms: 0,
            reviewed: true,
            disabled: false,
            auto: false,
        });
        let long = "x".repeat(600);
        let added = merge_auto(
            &mut doc,
            "ses_now",
            42,
            vec![
                "uses bun".into(),
                long,
                "  ".into(),
                "deploys with bun run build".into(),
                "a".repeat(600),
                "b".repeat(600),
                "c".repeat(600),
                "d".repeat(600),
            ],
        );
        assert_eq!(added, 5, "duplicate, blank, and the batch cap apply");
        assert!(doc
            .entries
            .iter()
            .all(|entry| entry.content.chars().count() <= 500));
        assert!(doc.entries[1..].iter().all(|entry| entry.auto));
        assert_eq!(doc.entries[1].origin_session, "ses_now");
    }

    #[test]
    fn the_extraction_prompt_carries_the_transcript_and_the_contract() {
        let messages = vec![
            Message {
                role: "user".into(),
                content: serde_json::json!([{"type":"text","text":"deploy with bun"}]),
                checkpoint: None,
                recorded_at_ms: None,
            },
            Message {
                role: "assistant".into(),
                content: serde_json::json!([
                    {"type":"tool_use","id":"t","name":"bash","input":{}},
                    {"type":"text","text":"understood, always bun"}
                ]),
                checkpoint: None,
                recorded_at_ms: None,
            },
        ];
        let prompt = extraction_prompt(&messages);
        assert!(prompt.contains("user: deploy with bun"));
        assert!(prompt.contains("assistant: understood, always bun"));
        assert!(
            !prompt.contains("bash"),
            "tool calls stay out of the prompt"
        );
        assert!(prompt.contains("JSON array"));
    }

    #[test]
    fn context_block_skips_empty_and_disabled() {
        assert!(context_block(&Document::default()).is_none());
        let mut doc = Document::default();
        doc.entries.push(Entry {
            id: "m1".into(),
            content: "uses bun".into(),
            origin_session: "s".into(),
            created_at_ms: 0,
            reviewed: false,
            disabled: true,
            auto: false,
        });
        assert!(context_block(&doc).is_none(), "disabled entries are hidden");
    }

    #[test]
    fn context_block_lists_active_entries() {
        let mut doc = Document::default();
        doc.entries.push(Entry {
            id: "m1".into(),
            content: "uses bun, not npm".into(),
            origin_session: "s".into(),
            created_at_ms: 0,
            reviewed: true,
            disabled: false,
            auto: false,
        });
        let block = context_block(&doc).unwrap();
        assert!(block.contains("<memory>"));
        assert!(block.contains("uses bun, not npm"));
        assert!(!block.contains("unreviewed"));
    }
}
