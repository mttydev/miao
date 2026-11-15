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
        });
        save(dir.path(), &doc).unwrap();
        let loaded = load(dir.path());
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].content, "uses bun, not npm");
        assert!(!loaded.entries[0].reviewed);
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
        });
        let block = context_block(&doc).unwrap();
        assert!(block.contains("<memory>"));
        assert!(block.contains("uses bun, not npm"));
    }
}
