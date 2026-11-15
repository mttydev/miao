//! File-state checkpoints for the edit pipeline. Before `write_file` or
//! `edit_file` touches a file, the current content is saved to
//! `.miao/checkpoints/` inside the workspace. The agent can then rewind to
//! the last checkpoint if the edit turned out wrong.
//!
//! Checkpoints are per-file (not per-repo): one edit produces one
//! checkpoint. The store is capped — the oldest checkpoints are evicted
//! when the count or size limit is reached, so long sessions cannot fill
//! the workspace.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The checkpoints directory relative to the workspace root.
pub const CHECKPOINTS_DIR: &str = ".miao/checkpoints";

/// How many checkpoints to keep per file. Older ones are evicted.
const MAX_PER_FILE: usize = 5;

/// One checkpoint: the file content before an edit touched it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Checkpoint {
    pub path: String,
    pub content: String,
    pub created_at_ms: u64,
}

/// Save the current content of `path` as a checkpoint. Creates the
/// checkpoints directory if needed. A missing file produces no checkpoint
/// (there is nothing to rewind to before the first write).
pub fn create(root: &Path, path: &str, now_ms: u64) -> std::io::Result<()> {
    let absolute = root.join(path);
    let Ok(content) = std::fs::read_to_string(&absolute) else {
        return Ok(()); // File doesn't exist yet — nothing to checkpoint.
    };
    let dir = root.join(CHECKPOINTS_DIR);
    std::fs::create_dir_all(&dir)?;

    let checkpoint = Checkpoint {
        path: path.to_owned(),
        content,
        created_at_ms: now_ms,
    };
    let json = serde_json::to_string(&checkpoint)?;
    let file_hash = format!("{:x}", md5_hash(path.as_bytes()));
    let checkpoint_path = dir.join(format!("{now_ms}_{file_hash}.json"));
    std::fs::write(&checkpoint_path, json)?;

    // Evict old checkpoints for this file beyond the cap.
    evict_old(&dir, path, MAX_PER_FILE)?;
    Ok(())
}

/// Restore the most recent checkpoint for `path`, returning the restored
/// content. Returns `None` if no checkpoint exists.
pub fn rewind_last(root: &Path, path: &str) -> std::io::Result<Option<String>> {
    let dir = root.join(CHECKPOINTS_DIR);
    if !dir.exists() {
        return Ok(None);
    }
    let file_hash = format!("{:x}", md5_hash(path.as_bytes()));
    let mut checkpoints: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(&file_hash) && n.ends_with(".json"))
        })
        .collect();
    checkpoints.sort();
    let Some(latest) = checkpoints.pop() else {
        return Ok(None);
    };
    let bytes = std::fs::read(&latest)?;
    let checkpoint: Checkpoint = serde_json::from_slice(&bytes)?;
    let absolute = root.join(path);
    std::fs::write(&absolute, &checkpoint.content)?;
    // Consume the checkpoint: rewinding to it removes it.
    std::fs::remove_file(&latest)?;
    Ok(Some(checkpoint.content))
}

/// Evict the oldest checkpoints for `path` beyond `keep`.
fn evict_old(dir: &Path, path: &str, keep: usize) -> std::io::Result<()> {
    let file_hash = format!("{:x}", md5_hash(path.as_bytes()));
    let mut checkpoints: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(&file_hash) && n.ends_with(".json"))
        })
        .collect();
    checkpoints.sort();
    while checkpoints.len() > keep {
        let oldest = checkpoints.remove(0);
        std::fs::remove_file(&oldest)?;
    }
    Ok(())
}

/// A tiny non-crypto hash for file name disambiguation (FNV-1a).
fn md5_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_rewind_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "original content").unwrap();

        // Create a checkpoint (pre-edit snapshot).
        create(dir.path(), "src/main.rs", 1000).unwrap();

        // Simulate an edit.
        std::fs::write(&file, "edited content").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited content");

        // Rewind restores the pre-edit content.
        let restored = rewind_last(dir.path(), "src/main.rs").unwrap();
        assert_eq!(restored.as_deref(), Some("original content"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original content");
    }

    #[test]
    fn missing_file_creates_no_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        create(dir.path(), "nonexistent.rs", 1000).unwrap();
        assert!(rewind_last(dir.path(), "nonexistent.rs").unwrap().is_none());
    }

    #[test]
    fn evict_keeps_only_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "v0").unwrap();
        for ms in [1000, 2000, 3000, 4000, 5000, 6000, 7000] {
            std::fs::write(&file, format!("v{ms}")).unwrap();
            create(dir.path(), "test.rs", ms).unwrap();
        }
        // Only MAX_PER_FILE checkpoints remain.
        let dir_entries = std::fs::read_dir(dir.path().join(CHECKPOINTS_DIR))
            .unwrap()
            .count();
        assert!(dir_entries <= MAX_PER_FILE + 1, "got {dir_entries}");
    }
}
