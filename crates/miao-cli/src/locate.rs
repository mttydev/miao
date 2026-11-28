//! Find a Session that lives in another workspace's ledger.
//!
//! Every workspace owns one engine database, named by the digest of its
//! directory (`miao_engine::placement::database_for`). That keeps projects
//! apart, but it also means a Session ID alone does not say which file holds
//! it: `miao -s ses_...` opened the current workspace's ledger, found nothing,
//! and the only way left was searching the disk. The mapping was never lost —
//! `engine_location` records `session_id → root` in the ledger that owns the
//! Session — it simply had no entry point.
//!
//! `locate` closes that gap by reading the storage root's ledgers directly. A
//! cache file makes the common case a single read; when it is missing, stale or
//! unreadable the ledgers are scanned instead, so no state on disk can make the
//! lookup fail. This never writes to a ledger and never changes where data
//! lives, so an existing installation needs no migration.

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// One ledger that holds at least one Session, with the directory it serves.
///
/// A ledger file's `engine_location` rows all name the same root in practice,
/// but the cache records the root per Session so a ledger that ever holds more
/// than one still resolves correctly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub session: String,
    pub root: String,
    /// The ledger's own path, so a caller can open it with `--db`.
    pub database: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    /// Bumped when the recorded shape changes, so an older file is rebuilt
    /// instead of misread.
    version: u32,
    /// Ledger path → the byte length and modification time it was read at.
    /// A ledger that changed since is scanned again rather than trusted.
    #[serde(default)]
    ledgers: Vec<LedgerStamp>,
    #[serde(default)]
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LedgerStamp {
    path: PathBuf,
    /// SQLite's change counter, from the database header. Every write bumps it,
    /// which makes it the authority on "has this ledger changed".
    ///
    /// A length and a modification time are not enough: an insert can leave the
    /// file exactly the same size, and two writes can land inside one
    /// millisecond, so a fresh index was accepted over a ledger that had gained
    /// a Session. Four bytes at offset 24 cost one short read and cannot be
    /// fooled that way.
    change_counter: u32,
}

/// Bumped when the recorded shape changes. Version 2 records SQLite's change
/// counter in place of a length and modification time, so a version-1 file is
/// rebuilt rather than misread as current.
const INDEX_VERSION: u32 = 2;
const INDEX_FILE: &str = "sessions-index.json";

/// A Session found by `locate`, plus whether a prefix matched several.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub entry: Entry,
    /// More than one Session matched; the caller should show the candidates
    /// instead of opening one at random.
    pub ambiguous: bool,
}

/// Resolve `needle` — a full Session ID or a unique prefix — to the ledger that
/// holds it.
///
/// `Ok(None)` means no ledger under the storage root holds it, which is a
/// different answer from "the current workspace has no such Session".
pub fn locate(needle: &str) -> Result<Option<Found>, String> {
    let Some(root) = miao_engine::placement::storage_root() else {
        return Ok(None);
    };
    locate_under(&root, needle)
}

/// The same lookup against an explicit storage root, so a caller can be tested
/// without touching the operator's home directory.
pub fn locate_under(storage: &Path, needle: &str) -> Result<Option<Found>, String> {
    if needle.is_empty() {
        return Ok(None);
    }
    // A cached hit is verified against the one ledger that claims it instead of
    // validating every ledger stamp first. That validation opened all 523
    // databases — and a busy ledger is always one write behind the cache — so
    // `miao -s` waited about a second before the UI could start.
    if let Some(found) = locate_cached(storage, needle) {
        return Ok(Some(found));
    }
    let entries = entries(storage)?;
    Ok(match_needle(&entries, needle))
}

/// How many cached entries a prefix may match before the lookup falls back to
/// the full list; verifying dozens of ledgers costs more than a scan.
const CACHED_CANDIDATES: usize = 16;

/// Resolve `needle` from the cache alone, verifying each candidate ledger.
///
/// `None` means the cache cannot answer (no index, a version it does not know,
/// no candidate, or a candidate that no longer holds the Session). The caller
/// then takes the scan path, so a stale cache costs time but never correctness.
fn locate_cached(storage: &Path, needle: &str) -> Option<Found> {
    let index = read_index(&storage.join(INDEX_FILE))?;
    if index.version != INDEX_VERSION {
        return None;
    }
    let matches: Vec<&Entry> = index
        .entries
        .iter()
        .filter(|entry| entry.session.starts_with(needle))
        .collect();
    if matches.is_empty() || matches.len() > CACHED_CANDIDATES {
        return None;
    }
    if let Some(entry) = matches.iter().find(|entry| entry.session == needle) {
        return Some(Found {
            entry: verify(entry)?,
            ambiguous: false,
        });
    }
    let mut verified = Vec::with_capacity(matches.len());
    for entry in matches {
        verified.push(verify(entry)?);
    }
    let first = verified.first()?.clone();
    Some(Found {
        entry: first,
        ambiguous: verified.len() > 1,
    })
}

/// The cached entry as the ledger describes it now: the Session must still be
/// there, and its root is read fresh so a re-homed placement is not opened at
/// the old tree.
fn verify(entry: &Entry) -> Option<Entry> {
    let connection = rusqlite::Connection::open_with_flags(
        &entry.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let root = connection
        .query_row(
            "SELECT COALESCE(l.root, '') FROM engine_session s \
             LEFT JOIN engine_location l ON l.session_id = s.id WHERE s.id=?1",
            [&entry.session],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()??;
    Some(Entry {
        session: entry.session.clone(),
        root,
        database: entry.database.clone(),
    })
}

/// Every Session in every ledger, newest first by ID.
fn entries(storage: &Path) -> Result<Vec<Entry>, String> {
    let index_path = storage.join(INDEX_FILE);
    let ledgers = ledger_paths(storage);
    if let Some(index) = read_index(&index_path) {
        if index.version == INDEX_VERSION && fresh(&index, &ledgers) {
            return Ok(index.entries);
        }
    }
    let scanned = scan(&ledgers);
    write_index(&index_path, &scanned);
    Ok(scanned.entries)
}

/// Ledgers under the storage root. A missing root scans nothing rather than
/// erroring: a machine that never ran the engine has no Sessions to find.
fn ledger_paths(storage: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(storage) else {
        return Vec::new();
    };
    read.filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "db"))
        .filter(|path| {
            // The product ledger beside the storage root is not a workspace
            // ledger, and unreadable files are skipped instead of failing the
            // whole lookup.
            path.is_file()
        })
        .collect()
}

fn stamp(path: &Path) -> Option<LedgerStamp> {
    let mut header = [0u8; 100];
    let mut file = std::fs::File::open(path).ok()?;
    // A file without a complete header is not a database yet: report nothing,
    // so a half-written ledger is scanned instead of trusted.
    if std::io::Read::read_exact(&mut file, &mut header).is_err() {
        return None;
    }
    if &header[..16] != b"SQLite format 3\0" {
        return None;
    }
    Some(LedgerStamp {
        path: path.to_path_buf(),
        change_counter: u32::from_be_bytes([header[24], header[25], header[26], header[27]]),
    })
}

/// Whether the recorded stamps still describe the ledgers on disk, with no
/// ledger added or removed since.
fn fresh(index: &Index, ledgers: &[PathBuf]) -> bool {
    if index.ledgers.len() != ledgers.len() {
        return false;
    }
    ledgers.iter().all(|path| {
        let Some(current) = stamp(path) else {
            return false;
        };
        index.ledgers.contains(&current)
    })
}

fn read_index(path: &Path) -> Option<Index> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_index(path: &Path, scanned: &Index) {
    // Best effort: the cache is an optimization, and a read-only storage root
    // must still answer the lookup.
    let Ok(body) = serde_json::to_vec(scanned) else {
        return;
    };
    let staging = path.with_extension("json.tmp");
    if std::fs::write(&staging, body).is_ok() {
        let _ = std::fs::rename(&staging, path);
    }
}

/// Read every ledger's `engine_session`/`engine_location` pair.
///
/// A ledger is opened read-only and a failure is skipped: one truncated or
/// locked file must not hide the Session the operator is looking for.
fn scan(ledgers: &[PathBuf]) -> Index {
    let mut index = Index {
        version: INDEX_VERSION,
        ..Index::default()
    };
    for path in ledgers {
        let Some(stamp) = stamp(path) else {
            continue;
        };
        // Record every ledger, even one with no Sessions yet. Freshness
        // compares this list against the files on disk, and a ledger the engine
        // created but has not started a Session in would otherwise make the
        // index look stale on every run and force a full rescan.
        index.ledgers.push(stamp);
        let Ok(connection) =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        else {
            continue;
        };
        let Ok(mut statement) = connection.prepare(
            "SELECT s.id, COALESCE(l.root, '') FROM engine_session s \
             LEFT JOIN engine_location l ON l.session_id = s.id",
        ) else {
            continue;
        };
        let Ok(rows) = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) else {
            continue;
        };
        for row in rows.flatten() {
            // A ledger can carry a stray row whose id is not a Session id
            // (`null` from an early import). Offering it would name a Session
            // that cannot be opened, so only real ids are indexed.
            if !row.0.starts_with("ses_") {
                continue;
            }
            index.entries.push(Entry {
                session: row.0,
                root: row.1,
                database: path.clone(),
            });
        }
    }
    index.entries.sort_by(|a, b| a.session.cmp(&b.session));
    index
}

/// Resolve `needle` against `entries`: an exact ID wins, otherwise a unique
/// prefix does. A prefix matching several Sessions reports the ambiguity rather
/// than picking one, because opening the wrong Session is worse than asking.
fn match_needle(entries: &[Entry], needle: &str) -> Option<Found> {
    if let Some(entry) = entries.iter().find(|entry| entry.session == needle) {
        return Some(Found {
            entry: entry.clone(),
            ambiguous: false,
        });
    }
    let mut matches = entries
        .iter()
        .filter(|entry| entry.session.starts_with(needle));
    let first = matches.next()?.clone();
    let ambiguous = matches.next().is_some();
    Some(Found {
        entry: first,
        ambiguous,
    })
}

/// Every Session matching a prefix, for the message that asks the operator to
/// pick one. Exact matches come back alone.
pub fn candidates(needle: &str) -> Result<Vec<Entry>, String> {
    let Some(root) = miao_engine::placement::storage_root() else {
        return Ok(Vec::new());
    };
    Ok(candidates_under(&root, needle))
}

pub fn candidates_under(storage: &Path, needle: &str) -> Vec<Entry> {
    let Ok(entries) = entries(storage) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|entry| entry.session.starts_with(needle))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(path: &Path, rows: &[(&str, &str)]) {
        let connection = rusqlite::Connection::open(path).unwrap();
        // Called twice in the staleness test, so creating the schema must be
        // idempotent — the point is a ledger that gained a row, not a rebuild.
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS engine_session (id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '');
                 CREATE TABLE IF NOT EXISTS engine_location (session_id TEXT PRIMARY KEY, root TEXT NOT NULL);",
            )
            .unwrap();
        for (session, root) in rows {
            connection
                .execute("INSERT INTO engine_session(id) VALUES(?1)", [session])
                .unwrap();
            connection
                .execute(
                    "INSERT INTO engine_location(session_id, root) VALUES(?1, ?2)",
                    [session, root],
                )
                .unwrap();
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("miao-locate-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_exact_id_resolves_to_the_ledger_that_holds_it() {
        let dir = scratch("exact");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        ledger(&dir.join("b.db"), &[("ses_bbb", "/work/two")]);
        let found = locate_under(&dir, "ses_bbb").unwrap().unwrap();
        assert_eq!(found.entry.root, "/work/two");
        assert_eq!(found.entry.database, dir.join("b.db"));
        assert!(!found.ambiguous);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_ledger_changed_behind_the_cache_still_resolves_the_exact_id() {
        let dir = scratch("cached-ledger-changed");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        ledger(&dir.join("b.db"), &[("ses_bbb", "/work/two")]);
        // Prime the cache, then write a ledger so its change counter no longer
        // matches the stamp the cache recorded.
        locate_under(&dir, "ses_aaa").unwrap();
        ledger(&dir.join("b.db"), &[("ses_ccc", "/work/three")]);
        let found = locate_under(&dir, "ses_bbb").unwrap().unwrap();
        assert_eq!(found.entry.root, "/work/two");
        assert_eq!(found.entry.database, dir.join("b.db"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_rehomed_session_answers_with_its_current_root() {
        let dir = scratch("rehome");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/old")]);
        locate_under(&dir, "ses_aaa").unwrap();
        let connection = rusqlite::Connection::open(dir.join("a.db")).unwrap();
        connection
            .execute(
                "UPDATE engine_location SET root='/work/new' WHERE session_id='ses_aaa'",
                [],
            )
            .unwrap();
        drop(connection);
        let found = locate_under(&dir, "ses_aaa").unwrap().unwrap();
        assert_eq!(found.entry.root, "/work/new");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_session_deleted_behind_the_cache_falls_back_to_the_scan() {
        let dir = scratch("deleted");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        locate_under(&dir, "ses_aaa").expect("prime the cache");
        let connection = rusqlite::Connection::open(dir.join("a.db")).unwrap();
        connection
            .execute("DELETE FROM engine_session WHERE id='ses_aaa'", [])
            .unwrap();
        drop(connection);
        assert!(locate_under(&dir, "ses_aaa").unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_shared_prefix_is_reported_as_ambiguous() {
        let dir = scratch("prefix");
        ledger(
            &dir.join("a.db"),
            &[("ses_18ea1", "/work/one"), ("ses_18ea2", "/work/one")],
        );
        let found = locate_under(&dir, "ses_18ea").unwrap().unwrap();
        assert!(found.ambiguous);
        assert_eq!(candidates_under(&dir, "ses_18ea").len(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_unknown_id_reports_nothing_rather_than_the_nearest_ledger() {
        let dir = scratch("missing");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        assert!(locate_under(&dir, "ses_zzz").unwrap().is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_ledger_added_after_the_index_was_written_is_still_found() {
        let dir = scratch("reindex");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        // First lookup writes the index; the second ledger arrives afterwards.
        assert!(locate_under(&dir, "ses_bbb").unwrap().is_none());
        ledger(&dir.join("b.db"), &[("ses_bbb", "/work/two")]);
        assert_eq!(
            locate_under(&dir, "ses_bbb").unwrap().unwrap().entry.root,
            "/work/two"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_ledger_that_gained_a_session_invalidates_the_index() {
        let dir = scratch("stale");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        assert!(locate_under(&dir, "ses_bbb").unwrap().is_none());
        ledger(&dir.join("a.db"), &[("ses_bbb", "/work/two")]);
        assert_eq!(
            locate_under(&dir, "ses_bbb").unwrap().unwrap().entry.root,
            "/work/two"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// The stamp must notice a write that leaves the file the same size.
    ///
    /// Measured on a real ledger: every insert here leaves the file at 20480
    /// bytes, and two writes inside one millisecond carry the same modification
    /// time, so a length-plus-mtime stamp judged the index fresh over a ledger
    /// that had gained a Session. SQLite's change counter is bumped by the write
    /// itself, so it cannot be fooled this way.
    #[test]
    fn a_same_size_write_still_changes_the_stamp() {
        let dir = scratch("same-size");
        let path = dir.join("a.db");
        ledger(&path, &[("ses_aaa", "/work/one")]);
        let size_before = std::fs::metadata(&path).unwrap().len();
        let before = super::stamp(&path).expect("a readable ledger has a stamp");

        ledger(&path, &[("ses_bbb", "/work/one")]);
        let size_after = std::fs::metadata(&path).unwrap().len();
        let after = super::stamp(&path).expect("a readable ledger has a stamp");

        assert_eq!(
            size_before, size_after,
            "the size is unchanged, which is exactly why size alone is not enough"
        );
        assert_ne!(
            before.change_counter, after.change_counter,
            "the change counter must move even when the size does not"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_corrupt_index_is_rebuilt_instead_of_failing() {
        let dir = scratch("corrupt");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        std::fs::write(dir.join(INDEX_FILE), b"not json").unwrap();
        assert_eq!(
            locate_under(&dir, "ses_aaa").unwrap().unwrap().entry.root,
            "/work/one"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_non_ledger_file_in_the_storage_root_is_ignored() {
        let dir = scratch("noise");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        std::fs::write(dir.join("notes.txt"), b"hello").unwrap();
        std::fs::write(dir.join("empty.db"), b"").unwrap();
        assert_eq!(
            locate_under(&dir, "ses_aaa").unwrap().unwrap().entry.root,
            "/work/one"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// A ledger with no Sessions still counts toward freshness. Recording only
    /// non-empty ledgers made the index disagree with the directory whenever the
    /// engine created a ledger it had not started a Session in, so every lookup
    /// rescanned all of them.
    #[test]
    fn an_empty_ledger_does_not_invalidate_the_index() {
        let dir = scratch("empty-ledger");
        ledger(&dir.join("a.db"), &[("ses_aaa", "/work/one")]);
        // Same schema, no rows: what a freshly placed workspace ledger looks like.
        ledger(&dir.join("empty.db"), &[]);
        assert!(locate_under(&dir, "ses_aaa").unwrap().is_some());
        let first = std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap();
        // A second lookup must trust the index rather than rescan and rewrite it.
        assert!(locate_under(&dir, "ses_aaa").unwrap().is_some());
        let second = std::fs::read_to_string(dir.join(INDEX_FILE)).unwrap();
        assert_eq!(first, second, "the index must be reusable as written");
        std::fs::remove_dir_all(dir).ok();
    }

    /// An early import left rows whose id is `null`. They name no Session, so
    /// they must not be offered as one.
    #[test]
    fn a_row_that_is_not_a_session_id_is_not_indexed() {
        let dir = scratch("null-id");
        ledger(
            &dir.join("a.db"),
            &[("ses_aaa", "/work/one"), ("null", "/work/one")],
        );
        let entries = super::entries(&dir).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session, "ses_aaa");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_missing_storage_root_is_not_an_error() {
        let dir = scratch("absent").join("never-created");
        assert!(locate_under(&dir, "ses_aaa").unwrap().is_none());
    }
}
