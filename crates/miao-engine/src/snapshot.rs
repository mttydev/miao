//! Session-owned file snapshots and the three-phase revert (ADR-0019).
//!
//! Capture is limited to paths the Session's mutating tools touched. A manifest
//! maps a workspace-relative path to a content hash, or `null` for a path the
//! last touch removed. Blobs live beside the engine database and are shared by
//! snapshots and staged pre-images.
use crate::{
    protocol::Error,
    store::{append, write_tx, Store},
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

const RETAINED_SNAPSHOTS: i64 = 20;
const MAX_BLOB_BYTES: u64 = 8 * 1024 * 1024;
/// Manifest values naming git state instead of a blob: `git:<sha>` says the
/// file's content equals the repo's committed content at that commit, which
/// restores through git at any size.
const GIT_PREFIX: &str = "git:";
/// The HEAD probe answers within this budget; status and show calls share the
/// budget a `vcs` status call spends, so a wedged filesystem cannot hold the
/// database worker forever either way.
const GIT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn blob_dir(database: &Path, session: &str) -> PathBuf {
    database.with_extension("snapshots").join(session)
}

pub(crate) fn valid_path(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if path.as_os_str().is_empty() || path.is_absolute() {
        return None;
    }
    path.components()
        .all(|part| matches!(part, Component::Normal(_)))
        .then(|| path.to_path_buf())
}

/// Windows canonical paths carry a verbatim prefix (`canonicalize` adds it)
/// while the path handed in may not. Comparing the de-prefixed spellings keeps
/// the containment check honest: without it `strip_prefix` failed and a
/// snapshot inside the workspace was refused as escaping it.
fn strip_verbatim(path: &Path) -> std::borrow::Cow<'_, Path> {
    match path
        .to_string_lossy()
        .strip_prefix(crate::runtime::VERBATIM)
    {
        Some(rest) => std::borrow::Cow::Owned(Path::new(rest).to_path_buf()),
        None => std::borrow::Cow::Borrowed(path),
    }
}

fn within(root: &Path, path: &Path) -> Result<PathBuf, Error> {
    let root = fs::canonicalize(root)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::Invalid("snapshot path has no parent".into()))?;
    let resolved = if parent.exists() {
        fs::canonicalize(parent)?.join(
            path.file_name()
                .ok_or_else(|| Error::Invalid("snapshot path has no name".into()))?,
        )
    } else {
        // New directories are created below the already checked parent chain.
        let mut existing = parent;
        while !existing.exists() {
            existing = existing
                .parent()
                .ok_or_else(|| Error::Invalid("snapshot path escapes the workspace".into()))?;
        }
        if !fs::canonicalize(existing)?.starts_with(&root) {
            return Err(Error::Invalid("snapshot path escapes the workspace".into()));
        }
        let relative = strip_verbatim(path)
            .strip_prefix(strip_verbatim(&root).as_ref())
            .map_err(|_| Error::Invalid("snapshot path escapes the workspace".into()))?
            .to_path_buf();
        root.join(relative)
    };
    if !resolved.starts_with(&root) {
        return Err(Error::Invalid("snapshot path escapes the workspace".into()));
    }
    Ok(resolved)
}

fn read_manifest(text: &str) -> Result<BTreeMap<String, Option<String>>, Error> {
    Ok(serde_json::from_str(text)?)
}

fn encode_manifest(manifest: &BTreeMap<String, Option<String>>) -> Result<String, Error> {
    Ok(serde_json::to_string(manifest)?)
}

pub(crate) fn write_blob(database: &Path, session: &str, bytes: &[u8]) -> Result<String, Error> {
    let hash = crate::permission::digest(bytes);
    let dir = blob_dir(database, session);
    fs::create_dir_all(&dir)?;
    let path = dir.join(&hash);
    if !path.exists() {
        let staging = dir.join(format!(".{hash}.{}", std::process::id()));
        fs::write(&staging, bytes)?;
        fs::rename(&staging, &path)?;
    }
    Ok(hash)
}

fn read_blob(database: &Path, session: &str, hash: &str) -> Result<Vec<u8>, Error> {
    fs::read(blob_dir(database, session).join(hash))
        .map_err(|_| Error::Invalid("file snapshot data is no longer available".into()))
}

fn is_git_target(value: &str) -> bool {
    value.starts_with(GIT_PREFIX)
}

/// Run one bounded git query. `None` means git failed or ran long, which
/// callers treat as "no git answer" rather than an error: snapshots must keep
/// working in non-git workspaces and under a wedged git.
fn git_bytes(root: &Path, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    let root = root.to_owned();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    std::thread::spawn(move || {
        let answer = match std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(output) => (output.status.success(), output.stdout),
            Err(_) => (false, Vec::new()),
        };
        let _ = sender.send(answer);
    });
    let (success, stdout) = receiver.recv_timeout(timeout).ok()?;
    success.then_some(stdout)
}

/// Paths `git status --porcelain=v1 -z` reports as changed: exactly the paths
/// whose index or worktree bytes differ from HEAD. A record is `XY path` with
/// ASCII status columns; a staged rename or copy carries the original path as
/// the next NUL field, which differs from HEAD too.
fn porcelain_paths(bytes: &[u8]) -> std::collections::HashSet<String> {
    let mut changed = std::collections::HashSet::new();
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let Ok(record) = std::str::from_utf8(record) else {
            continue;
        };
        // `get` refuses a slice boundary that would cut a character in half.
        let (Some(kind), Some(path)) = (record.get(..2), record.get(3..)) else {
            continue;
        };
        changed.insert(path.to_owned());
        if kind.contains('R') || kind.contains('C') {
            if let Some(original) = records.next() {
                if let Ok(original) = std::str::from_utf8(original) {
                    changed.insert(original.to_owned());
                }
            }
        }
    }
    changed
}

/// Tracked paths whose working-tree content still equals HEAD: absent from
/// `status` and present in `ls-files`. Ignored files are absent from `status`
/// too but not tracked, so the `ls-files` intersection is what makes the
/// `git:` reference safe.
fn clean_tracked(root: &Path, paths: &[String]) -> std::collections::HashSet<String> {
    // A leading `:` would read as pathspec magic, so every touched name
    // travels as literal; the byte-exact match a glob-shaped name still gets
    // is then the only way it can match at all.
    let path_args: Vec<String> = paths
        .iter()
        .map(|path| format!(":(literal){path}"))
        .collect();
    let mut ls = vec!["ls-files", "-z", "--full-name", "--"];
    ls.extend(path_args.iter().map(String::as_str));
    let mut status = vec!["status", "--porcelain=v1", "-z", "--"];
    status.extend(path_args.iter().map(String::as_str));
    let (Some(tracked), Some(changed)) = (
        git_bytes(root, &ls, GIT_TIMEOUT),
        git_bytes(root, &status, GIT_TIMEOUT),
    ) else {
        return std::collections::HashSet::new();
    };
    // Both answers name paths from the repository root (status always does;
    // ls-files with --full-name), while the touched keys are
    // workspace-relative. The workspace's own prefix inside the repo moves
    // one frame onto the other, and a name from outside the workspace cannot
    // match a touched key.
    let prefix = git_bytes(root, &["rev-parse", "--show-prefix"], GIT_PROBE_TIMEOUT)
        .map(|bytes| String::from_utf8_lossy(bytes.trim_ascii_end()).into_owned())
        .unwrap_or_default();
    let changed: std::collections::HashSet<String> = porcelain_paths(&changed)
        .into_iter()
        .filter_map(|path| path.strip_prefix(prefix.as_str()).map(str::to_owned))
        .collect();
    String::from_utf8_lossy(&tracked)
        .split('\0')
        .filter_map(|path| path.strip_prefix(prefix.as_str()))
        .filter(|path| !path.is_empty() && !changed.contains(*path))
        .map(str::to_owned)
        .collect()
}

/// The committed content one manifest entry names. A malformed or unavailable
/// reference fails on the same level as a lost blob: the snapshot says restore
/// and the data must exist.
fn git_show(root: &Path, value: &str, path: &str) -> Result<Vec<u8>, Error> {
    // Callers may pass the manifest value or an already-stripped reference,
    // so stripping the prefix is idempotent.
    let sha = value.strip_prefix(GIT_PREFIX).unwrap_or(value);
    if !(7..=64).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Invalid("snapshot git reference is malformed".into()));
    }
    // The `./` form resolves against the workspace root, which may sit in a
    // subdirectory of the repository; a bare path would read from the root.
    let spec = format!("{sha}:./{path}");
    git_bytes(root, &["show", &spec], GIT_TIMEOUT)
        .ok_or_else(|| Error::Invalid("file snapshot data is no longer available".into()))
}

/// Which halves of a Session a revert may move. The phases are independent:
/// `stage` restores files and `commit` rewinds history, so a caller that wants
/// only one of them says so and the other is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Phases {
    pub files: bool,
    pub history: bool,
}

impl Phases {
    /// Refuse a revert that would move nothing rather than recording an intent
    /// that can never change anything.
    pub(crate) fn parse(files: bool, history: bool) -> Result<Self, Error> {
        if !files && !history {
            return Err(Error::Invalid(
                "a revert must restore files, rewind history, or both".into(),
            ));
        }
        Ok(Self { files, history })
    }

    /// The vocabulary every event and reply uses, so a client never has to
    /// infer the boundary from two booleans.
    pub(crate) fn scope(&self) -> &'static str {
        match (self.files, self.history) {
            (true, true) => "both",
            (true, false) => "files",
            (false, true) => "history",
            (false, false) => "none",
        }
    }

    /// Read a scope back out of the durable stage it was recorded in.
    pub(crate) fn from_scope(scope: &str) -> Self {
        Self {
            files: scope != "history",
            history: scope != "files",
        }
    }
}

/// A staged snapshot identity as text. Native captures store a numeric row id
/// and assembled imports store a transfer identity, so both representations
/// are legal in one column; a history-only stage stores no identity at all.
pub(crate) fn id_text(value: rusqlite::types::Value) -> Result<String, rusqlite::Error> {
    match value {
        rusqlite::types::Value::Text(id) => Ok(id),
        rusqlite::types::Value::Integer(id) => Ok(id.to_string()),
        other => Err(rusqlite::Error::InvalidColumnType(
            0,
            "snapshot_id".into(),
            other.data_type(),
        )),
    }
}

fn retain(conn: &rusqlite::Connection, database: &Path, session: &str) -> Result<(), Error> {
    let staged_id: Option<String> = conn
        .query_row(
            "SELECT snapshot_id FROM engine_staged_revert WHERE session_id=?1",
            [session],
            |row| id_text(row.get(0)?),
        )
        .optional()?;
    let staged_id = staged_id.and_then(|id| id.parse::<i64>().ok());
    let mut statement = conn.prepare(
        "SELECT id,manifest FROM engine_file_snapshot WHERE session_id=?1 ORDER BY boundary_seq DESC, id DESC",
    )?;
    let rows = statement
        .query_map([session], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut keep = std::collections::BTreeSet::new();
    for (index, (id, manifest)) in rows.iter().enumerate() {
        let referenced = index < RETAINED_SNAPSHOTS as usize || staged_id == Some(*id);
        if referenced {
            if let Ok(manifest) = read_manifest(manifest) {
                keep.extend(manifest.into_values().flatten());
            }
        } else {
            conn.execute("DELETE FROM engine_file_snapshot WHERE id=?1", [id])?;
        }
    }
    if let Some(staged) = staged(conn, session)? {
        if let Ok(previous) =
            serde_json::from_str::<BTreeMap<String, Option<String>>>(&staged.previous)
        {
            keep.extend(previous.into_values().flatten());
        }
    }
    if let Ok(entries) = fs::read_dir(blob_dir(database, session)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !keep.contains(name.to_string_lossy().as_ref()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

impl Store {
    pub async fn record_file_touch(&self, session: &str, path: &str) -> Result<(), Error> {
        if valid_path(path).is_none() {
            return Err(Error::Invalid("invalid snapshot path".into()));
        }
        let (session, path) = (session.to_owned(), path.to_owned());
        self.call(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO engine_file_touch(session_id,path) VALUES(?1,?2)",
                params![session, path],
            )?;
            Ok(())
        })
        .await
    }

    /// Capture the paths touched since the previous boundary, merging them into
    /// the previous manifest so a later restore also recovers untouched files.
    pub async fn capture_file_snapshot(
        &self,
        session: &str,
        boundary_seq: u64,
    ) -> Result<Value, Error> {
        let (session, database) = (session.to_owned(), self.path().to_owned());
        self.call(move |conn| {
            let root: Option<String> = conn
                .query_row(
                    "SELECT root FROM engine_location WHERE session_id=?1",
                    [&session],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(root) = root else {
                return Ok(json!({"available":false,"reason":"no-location"}));
            };
            let touched = {
                let mut statement =
                    conn.prepare("SELECT path FROM engine_file_touch WHERE session_id=?1 ORDER BY path")?;
                let rows = statement
                    .query_map([&session], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            if touched.is_empty() {
                return Ok(json!({"available":false,"reason":"no-touches"}));
            }
            let head = git_bytes(
                Path::new(&root),
                &["rev-parse", "--verify", "HEAD"],
                GIT_PROBE_TIMEOUT,
            )
                .map(|sha| String::from_utf8_lossy(sha.trim_ascii_end()).into_owned());
            let clean = match &head {
                Some(_) => clean_tracked(Path::new(&root), &touched),
                None => std::collections::HashSet::new(),
            };
            let previous: Option<String> = conn
                .query_row(
                    "SELECT manifest FROM engine_file_snapshot WHERE session_id=?1 ORDER BY boundary_seq DESC, id DESC LIMIT 1",
                    [&session],
                    |row| row.get(0),
                )
                .optional()?;
            let mut manifest = previous
                .as_deref()
                .map(read_manifest)
                .transpose()?
                .unwrap_or_default();
            for path in touched {
                let Some(relative) = valid_path(&path) else {
                    continue;
                };
                let absolute = Path::new(&root).join(&relative);
                // A symlink stores through its target in the blob branch,
                // while `git show` would answer the link itself, so a link
                // keeps the byte-for-byte branch even when git calls it clean.
                if clean.contains(&path)
                    && fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.is_file())
                {
                    manifest.insert(
                        path,
                        Some(format!(
                            "{GIT_PREFIX}{}",
                            head.as_deref().unwrap_or_default()
                        )),
                    );
                    continue;
                }
                match fs::metadata(&absolute) {
                    Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_BLOB_BYTES => {
                        let bytes = fs::read(&absolute)?;
                        manifest.insert(path, Some(write_blob(&database, &session, &bytes)?));
                    }
                    Ok(metadata) if metadata.is_file() => {
                        // Oversized files cannot be restored; the snapshot says so
                        // instead of silently keeping a stale copy.
                        manifest.insert(path, None);
                    }
                    _ => {
                        manifest.insert(path, None);
                    }
                }
            }
            let tx = write_tx(conn)?;
            tx.execute("DELETE FROM engine_file_touch WHERE session_id=?1", [&session])?;
            tx.execute(
                "INSERT INTO engine_file_snapshot(session_id,boundary_seq,kind,manifest) VALUES(?1,?2,'paths',?3)",
                params![session, boundary_seq as i64, encode_manifest(&manifest)?],
            )?;
            let id = tx.last_insert_rowid();
            retain(&tx, &database, &session)?;
            tx.commit()?;
            Ok(json!({"available":true,"id":id,"boundary_seq":boundary_seq,"paths":manifest.len(),"head":head}))
        })
        .await
    }

    /// Assemble a provider-facing manifest from retained imported snapshots.
    /// Imported rows have no native boundary sequence; their order is the
    /// restored sequence of the referencing assistant, with unreferenced
    /// manifests last. First touch per path wins, including deletions.
    /// Symlink entries cannot be represented by native manifests and are
    /// skipped (counted) instead of being written as regular files.
    fn imported_manifest(
        conn: &rusqlite::Connection,
        session: &str,
        boundary_seq: u64,
    ) -> Result<Option<AssembledImport>, Error> {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(t.seq, -1), s.snapshot_id, s.manifest
             FROM engine_transfer_snapshot s
             JOIN engine_transfer_import i ON i.id = s.import_id AND i.session_id = ?1 AND i.state = 'active'
             LEFT JOIN engine_transfer_message t ON t.import_id = s.import_id AND t.source_message_id = s.boundary_message_id
             WHERE s.order_index IS NOT NULL OR t.seq IS NOT NULL
             ORDER BY COALESCE(t.seq, 9223372036854775807), s.order_index",
        )?;
        let rows = stmt.query_map(params![session], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut manifest = BTreeMap::new();
        let mut identity: Option<String> = None;
        for row in rows {
            let (seq, snapshot_id, raw) = row?;
            if seq >= 0 && seq as u64 > boundary_seq {
                continue;
            }
            let files: Vec<Value> = serde_json::from_str(&raw)?;
            for file in files {
                let path = file["path"]
                    .as_str()
                    .ok_or_else(|| Error::Invalid("retained snapshot path is missing".into()))?
                    .to_owned();
                if file["mode"].as_str() == Some("120000") {
                    continue;
                }
                let hash = file["hash"].as_str().map(str::to_owned);
                manifest.entry(path).or_insert(hash);
            }
            if identity.is_none() {
                identity = Some(format!("transfer:{snapshot_id}"));
            }
        }
        Ok(identity.map(|id| (id, manifest)))
    }

    pub async fn snapshot_at(&self, session: &str, boundary_seq: u64) -> Result<Value, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            let found: Option<(i64, i64, String)> = conn
                .query_row(
                    "SELECT id,boundary_seq,manifest FROM engine_file_snapshot WHERE session_id=?1 AND boundary_seq<=?2 ORDER BY boundary_seq DESC, id DESC LIMIT 1",
                    params![session, boundary_seq as i64],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            match found {
                Some((id, boundary, manifest)) => {
                    let manifest: BTreeMap<String, Option<String>> = read_manifest(&manifest)?;
                    let available = manifest.values().filter_map(|hash| hash.as_deref()).all(|hash| is_git_target(hash) || blob_dir(Path::new(conn.path().unwrap_or_default()), &session).join(hash).exists());
                    Ok(json!({"id":id,"boundary_seq":boundary,"available":available}))
                }
                None => match Self::imported_manifest(conn, &session, boundary_seq)? {
                    Some((id, manifest)) => {
                        let available = manifest.values().filter_map(|hash| hash.as_deref()).all(|hash| is_git_target(hash) || blob_dir(Path::new(conn.path().unwrap_or_default()), &session).join(hash).exists());
                        Ok(json!({"id":id,"boundary_seq":boundary_seq,"available":available,"assembled":true}))
                    }
                    None => Ok(Value::Null),
                },
            }
        })
        .await
    }
}

type Restored = (Vec<String>, Vec<String>, Vec<String>);

fn restore_manifest(
    database: &Path,
    session: &str,
    root: &Path,
    manifest: &BTreeMap<String, Option<String>>,
) -> Result<Restored, Error> {
    let mut restored = Vec::new();
    let mut created = Vec::new();
    let mut removed = Vec::new();
    for (path, hash) in manifest {
        let Some(relative) = valid_path(path) else {
            continue;
        };
        let absolute = root.join(&relative);
        let expected: Vec<u8> = match hash {
            Some(value) if value.starts_with(GIT_PREFIX) => git_show(root, value, path)?,
            Some(hash) => read_blob(database, session, hash)?,
            None => {
                if absolute.exists() {
                    let target = within(root, &absolute)?;
                    fs::remove_file(target)?;
                    removed.push(path.clone());
                }
                continue;
            }
        };
        if fs::read(&absolute).ok().as_deref() == Some(expected.as_slice()) {
            continue;
        }
        let existed = absolute.exists();
        let target = within(root, &absolute)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let staging = target.with_extension(format!("snapshot-{}", std::process::id()));
        fs::write(&staging, expected)?;
        fs::rename(&staging, &target)?;
        restored.push(path.clone());
        if !existed {
            created.push(path.clone());
        }
    }
    Ok((restored, created, removed))
}

fn preimage(
    database: &Path,
    session: &str,
    root: &Path,
    manifest: &BTreeMap<String, Option<String>>,
) -> Result<BTreeMap<String, Option<String>>, Error> {
    let mut previous = BTreeMap::new();
    for (path, target) in manifest {
        let Some(relative) = valid_path(path) else {
            continue;
        };
        let absolute = root.join(relative);
        let current = fs::read(&absolute).ok();
        let value = match target {
            // A pre-image stays self-contained wherever a blob can hold it.
            // Only content that still equals the committed state and is too
            // large to blob keeps the git reference: recording `None` there
            // would make a later clear delete the file.
            Some(value)
                if value.starts_with(GIT_PREFIX)
                    && current
                        .as_deref()
                        .is_some_and(|bytes| bytes.len() as u64 > MAX_BLOB_BYTES)
                    && current.as_deref() == git_show(root, value, path).ok().as_deref() =>
            {
                Some(value.clone())
            }
            _ => match current {
                Some(bytes) if bytes.len() as u64 <= MAX_BLOB_BYTES => {
                    Some(write_blob(database, session, &bytes)?)
                }
                _ => None,
            },
        };
        previous.insert(path.clone(), value);
    }
    Ok(previous)
}

/// The file set a boundary restores, and the identity of the snapshot it came
/// from. `None` means the Session has no recorded file state at or before the
/// boundary, which is a history-only revert, not a failure.
type Resolved = (BTreeMap<String, Option<String>>, i64, String);

fn resolve(
    conn: &rusqlite::Connection,
    session: &str,
    boundary_seq: u64,
) -> Result<Option<Resolved>, Error> {
    let found: Option<(i64, i64, String)> = conn
        .query_row(
            "SELECT id,boundary_seq,manifest FROM engine_file_snapshot WHERE session_id=?1 AND boundary_seq<=?2 ORDER BY boundary_seq DESC, id DESC LIMIT 1",
            params![session, boundary_seq as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((snapshot_id, boundary, manifest)) = found else {
        let Some((id, manifest)) = Store::imported_manifest(conn, session, boundary_seq)? else {
            return Ok(None);
        };
        // An assembled import has no native row id; its identity is the
        // transfer snapshot it was projected from.
        return Ok(Some((manifest, boundary_seq as i64, id)));
    };
    Ok(Some((
        read_manifest(&manifest)?,
        boundary,
        snapshot_id.to_string(),
    )))
}

impl Store {
    /// Stage a revert: restore the file set recorded at `boundary_seq` and
    /// remember the intent durably. `phases` decides which halves the commit
    /// that follows is allowed to move, so a code-only stage restores files and
    /// leaves the transcript alone. An outside edit is refused unless `force`
    /// says the caller already showed it to the operator.
    pub async fn revert_stage(
        &self,
        session: &str,
        boundary_seq: u64,
        files: bool,
        history: bool,
        force: bool,
    ) -> Result<Value, Error> {
        let (session, database) = (session.to_owned(), self.path().to_owned());
        self.call(move |conn| {
            let phases = Phases::parse(files, history)?;
            require_idle(conn, &session)?;
            let root: String = conn
                .query_row(
                    "SELECT root FROM engine_location WHERE session_id=?1",
                    [&session],
                    |row| row.get(0),
                )
                .map_err(|_| Error::Invalid("Session has no workspace location".into()))?;
            // A history-only stage deliberately skips the file record: the
            // caller asked to rewind the conversation and leave the workspace
            // alone, so a Session that never touched a file can still undo.
            let (manifest, boundary, snapshot_id) = match (
                phases.files,
                resolve(conn, &session, boundary_seq)?,
            ) {
                (false, _) => (BTreeMap::new(), boundary_seq as i64, String::new()),
                (true, Some(resolved)) => resolved,
                (true, None) => {
                    return Err(Error::Invalid("no file snapshot at this boundary".into()))
                }
            };
            let root = Path::new(&root);
            let conflicts = workspace_plan(conn, &session, root, &manifest)?.2;
            if !conflicts.is_empty() && !force {
                let paths = conflicts
                    .iter()
                    .filter_map(|conflict| conflict["path"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let count = conflicts.len();
                return Err(Error::Invalid(format!(
                    "refusing to overwrite {count} path(s) that changed outside this Session: {paths}; preview the revert and re-stage with force to overwrite them"
                )));
            }
            // One intent at a time: undo the previous staged restore first.
            if let Some(previous) = staged(conn, &session)? {
                let previous: BTreeMap<String, Option<String>> =
                    serde_json::from_str(&previous.previous)?;
                restore_manifest(&database, &session, root, &previous)?;
                conn.execute("DELETE FROM engine_staged_revert WHERE session_id=?1", [&session])?;
            }
            let previous = preimage(&database, &session, root, &manifest)?;
            let (restored, created, removed) = restore_manifest(&database, &session, root, &manifest)?;
            let tx = write_tx(conn)?;
            tx.execute(
                "INSERT INTO engine_staged_revert(session_id,boundary_seq,snapshot_id,scope,restored,created,removed,previous) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    session,
                    boundary,
                    snapshot_id,
                    phases.scope(),
                    serde_json::to_string(&restored)?,
                    serde_json::to_string(&created)?,
                    serde_json::to_string(&removed)?,
                    serde_json::to_string(&previous)?
                ],
            )?;
            append(
                &tx,
                &session,
                "revert.staged",
                json!({"boundary_seq":boundary,"snapshot_id":snapshot_id,"scope":phases.scope(),"restored":restored,"created":created,"removed":removed}),
            )?;
            tx.commit()?;
            Ok(json!({"boundary_seq":boundary,"snapshot_id":snapshot_id,"scope":phases.scope(),"restored":restored,"created":created,"removed":removed,"conflicts_overwritten":conflicts}))
        })
        .await
    }

    pub async fn revert_clear(&self, session: &str) -> Result<Value, Error> {
        let (session, database) = (session.to_owned(), self.path().to_owned());
        self.call(move |conn| {
            let staged = staged(conn, &session)?
                .ok_or_else(|| Error::Invalid("no staged revert".into()))?;
            let root: String = conn.query_row(
                "SELECT root FROM engine_location WHERE session_id=?1",
                [&session],
                |row| row.get(0),
            )?;
            let previous: BTreeMap<String, Option<String>> = serde_json::from_str(&staged.previous)?;
            let (restored, created, removed) =
                restore_manifest(&database, &session, Path::new(&root), &previous)?;
            let tx = write_tx(conn)?;
            tx.execute("DELETE FROM engine_staged_revert WHERE session_id=?1", [&session])?;
            append(
                &tx,
                &session,
                "revert.cleared",
                json!({"boundary_seq":staged.boundary,"restored":restored,"created":created,"removed":removed}),
            )?;
            tx.commit()?;
            Ok(json!({"boundary_seq":staged.boundary,"restored":restored,"created":created,"removed":removed}))
        })
        .await
    }

    /// Make a stage permanent. A combined revert rewinds the conversation
    /// through the existing soft revert (rows are marked, not deleted, so
    /// `unrevert` stays available); a code-only revert keeps the files the
    /// stage restored and touches no message row.
    pub async fn revert_commit(&self, session: &str) -> Result<Value, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            let staged = staged(conn, &session)?
                .ok_or_else(|| Error::Invalid("no staged revert".into()))?;
            require_idle(conn, &session)?;
            let tx = write_tx(conn)?;
            let reverted = match Phases::from_scope(&staged.scope).history {
                true => tx.execute(
                    "UPDATE engine_message SET reverted=1 WHERE session_id=?1 AND seq>=?2 AND reverted=0",
                    params![session, staged.boundary],
                )?,
                false => 0,
            };
            if reverted > 0 {
                tx.execute(
                    "DELETE FROM engine_compaction WHERE session_id=?1 AND through_seq>=?2",
                    params![session, staged.boundary],
                )?;
            }
            let event = append(
                &tx,
                &session,
                "revert.committed",
                json!({"boundary_seq":staged.boundary,"snapshot_id":staged.snapshot_id,"scope":staged.scope,"reverted":reverted}),
            )?;
            tx.execute("DELETE FROM engine_staged_revert WHERE session_id=?1", [&session])?;
            tx.commit()?;
            Ok(json!({"boundary_seq":staged.boundary,"snapshot_id":staged.snapshot_id,"scope":staged.scope,"reverted":reverted,"seq":event.seq}))
        })
        .await
    }

    pub async fn staged_revert(&self, session: &str) -> Result<Value, Error> {
        let session = session.to_owned();
        self.call(move |conn| match staged(conn, &session)? {
            Some(staged) => Ok(json!({
                "boundary_seq":staged.boundary,
                "snapshot_id":staged.snapshot_id,
                "scope":staged.scope,
                "paths":read_manifest(&staged.previous)?.len(),
            })),
            None => Ok(Value::Null),
        })
        .await
    }

    /// What a revert to `boundary_seq` would do to the workspace, without doing
    /// it. The engine decides every path, so the preview a client shows is the
    /// same plan the stage enforces.
    pub async fn revert_preview(&self, session: &str, boundary_seq: u64) -> Result<Value, Error> {
        let session = session.to_owned();
        self.call(move |conn| {
            require_idle(conn, &session)?;
            let Some(root) = conn
                .query_row(
                    "SELECT root FROM engine_location WHERE session_id=?1",
                    [&session],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            else {
                return Ok(json!({"available":false,"reason":"no-location"}));
            };
            let Some((manifest, boundary, snapshot_id)) = resolve(conn, &session, boundary_seq)?
            else {
                return Ok(json!({"available":false,"reason":"no-file-snapshot"}));
            };
            let root = Path::new(&root);
            let (restore, remove, conflicts) = workspace_plan(conn, &session, root, &manifest)?;
            Ok(json!({
                "available": true,
                "boundary_seq": boundary,
                "snapshot_id": snapshot_id,
                "restore": restore,
                "remove": remove,
                "conflicts": conflicts,
                // Every scope rewinds files and/or history; none of them
                // reverses what a turn did outside the workspace.
                "external_side_effects": "not_reversible",
            }))
        })
        .await
    }
}

fn require_idle(conn: &rusqlite::Connection, session: &str) -> Result<(), Error> {
    match conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM engine_run WHERE session_id=?1 AND state='running')",
        [session],
        |row| row.get::<_, bool>(0),
    )? {
        true => Err(Error::Invalid("revert requires an idle Session".into())),
        false => Ok(()),
    }
}

/// The paths a revert restores, the paths it removes, and the paths whose
/// current content is neither the restore target nor what the Session last
/// recorded. A path the Session never recorded has no entry, so only real
/// writes and removals are named.
type Workspace = (Vec<String>, Vec<String>, Vec<Value>);

fn workspace_plan(
    conn: &rusqlite::Connection,
    session: &str,
    root: &Path,
    manifest: &BTreeMap<String, Option<String>>,
) -> Result<Workspace, Error> {
    let recorded = recorded_hashes(conn, session)?;
    let (mut restore, mut remove, mut conflicts) = (Vec::new(), Vec::new(), Vec::new());
    for (path, target) in manifest {
        let Some(relative) = valid_path(path) else {
            continue;
        };
        // An unreadable or oversized file has no comparable digest, so it
        // reads as missing, which can only make the plan more cautious.
        let bytes = fs::read(root.join(relative)).ok();
        let disk = bytes
            .as_deref()
            .filter(|bytes| bytes.len() as u64 <= MAX_BLOB_BYTES)
            .map(crate::permission::digest);
        let committed = |value: Option<&str>| -> Result<Option<Vec<u8>>, Error> {
            value
                .and_then(|value| value.strip_prefix(GIT_PREFIX))
                .map(|sha| git_show(root, sha, path))
                .transpose()
        };
        let matches_target = match committed(target.as_deref())? {
            // A git reference compares against the committed content, so a
            // clean tracked file needs neither restore nor conflict.
            Some(expected) => bytes.as_deref() == Some(expected.as_slice()),
            None => disk.as_deref() == target.as_deref(),
        };
        if matches_target {
            continue;
        }
        let action = match target.is_some() {
            true => "restore",
            false => "remove",
        };
        if action == "restore" {
            restore.push(path.clone());
        } else {
            remove.push(path.clone());
        }
        // A path the engine still believes holds the content it last recorded
        // changed by this Session's own work, which a revert is meant to undo.
        // Anything else is an outside edit. A path with no record at all has no
        // baseline to compare against, so it is not evidence of an outside
        // edit: an imported manifest is the case that reaches this.
        // The conflict test is the newest record against the disk bytes, not
        // against the restore target: a git reference holds what the Session
        // last recorded exactly when the committed content is on disk.
        let outside = match recorded.get(path) {
            None => false,
            Some(newest) => match committed(newest.as_deref())? {
                Some(expected) => bytes.as_deref() != Some(expected.as_slice()),
                None => newest != &disk,
            },
        };
        if outside {
            conflicts.push(json!({
                "path": path,
                "action": action,
                "disk": if disk.is_some() { "modified" } else { "missing" },
            }));
        }
    }
    Ok((restore, remove, conflicts))
}

/// The newest content each path is recorded to have, across the Session's
/// retained snapshots. Manifests are cumulative, so the newest record is the
/// state the engine believes the workspace is in.
fn recorded_hashes(
    conn: &rusqlite::Connection,
    session: &str,
) -> Result<BTreeMap<String, Option<String>>, Error> {
    let mut statement = conn.prepare(
        "SELECT manifest FROM engine_file_snapshot WHERE session_id=?1 ORDER BY boundary_seq DESC, id DESC",
    )?;
    let rows = statement
        .query_map([session], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut newest: BTreeMap<String, Option<String>> = BTreeMap::new();
    for row in rows {
        for (path, hash) in read_manifest(&row)? {
            newest.entry(path).or_insert(hash);
        }
    }
    Ok(newest)
}

type AssembledImport = (String, BTreeMap<String, Option<String>>);

/// The one staged revert intent a Session may hold: the workspace state to
/// reverse a `clear` to, the boundary it targets, the snapshot it came from,
/// and which phases its commit may move.
struct Staged {
    previous: String,
    boundary: i64,
    snapshot_id: String,
    scope: String,
}

fn staged(conn: &rusqlite::Connection, session: &str) -> Result<Option<Staged>, Error> {
    Ok(conn
        .query_row(
            "SELECT previous,boundary_seq,snapshot_id,scope FROM engine_staged_revert WHERE session_id=?1",
            [session],
            // Native captures store a numeric row id; assembled imports store
            // the transfer identity, so accept either stored representation.
            |row| {
                Ok(Staged {
                    previous: row.get(0)?,
                    boundary: row.get(1)?,
                    snapshot_id: id_text(row.get(2)?)?,
                    scope: row.get(3)?,
                })
            },
        )
        .optional()?)
}
