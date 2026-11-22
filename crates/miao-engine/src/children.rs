//! The parent's view of its delegated Sessions (AGT-01, C1).
//!
//! Lineage already records who forked whom; what a parent UI still cannot ask
//! is "which children do I have, and is one of them still working". This read
//! answers that from Session catalog rows, the lineage edge and the child's
//! latest run, so no new event or status vocabulary is invented: `busy` is
//! literally "the child has a running run".

use crate::{protocol::Error, store::Store};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

/// Most children one answer carries. A delegation loop is bounded by the
/// subagent depth and concurrency limits, but a fork tree is not, so the read
/// states how many entries it left out instead of growing without bound.
pub const MAX_CHILDREN: usize = 200;

impl Store {
    /// The Sessions forked from or delegated to by `session`, oldest first.
    ///
    /// Archived children stay in the list: archiving is a catalog preference,
    /// not a statement that the delegation never happened. A Session that has
    /// no children answers an empty list; an unknown Session is an error,
    /// which is the difference between "nothing was delegated" and "you asked
    /// about an identity that does not exist".
    pub async fn children(&self, session: &str, limit: usize) -> Result<Value, Error> {
        // One answer is bounded whatever the caller asks for: `limit + 1`
        // detects truncation, so an unbounded value would overflow that read.
        let (session, limit) = (session.to_owned(), limit.clamp(1, MAX_CHILDREN) as i64);
        self.call(move |conn| {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM engine_session WHERE id=?1)",
                [&session],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(Error::Invalid("Session does not exist".into()));
            }
            let mut statement = conn.prepare(
                "SELECT s.id,s.title,l.message_seq,s.created_at_ms,s.updated_at_ms,
                        s.archived_at_ms,s.pinned_at_ms,
                        (SELECT COUNT(*) FROM engine_message m WHERE m.session_id=s.id AND m.reverted=0 AND m.quarantined=0)
                 FROM engine_lineage l JOIN engine_session s ON s.id=l.session_id
                 WHERE l.parent_session_id=?1
                 ORDER BY s.created_at_ms, s.id
                 LIMIT ?2",
            )?;
            let rows = statement.query_map(rusqlite::params![session, limit + 1], |row| {
                Ok(json!({
                    "session_id": row.get::<_, String>(0)?,
                    "title": row.get::<_, String>(1)?,
                    "parent_message_seq": row.get::<_, u64>(2)?,
                    "created_at_ms": row.get::<_, Option<i64>>(3)?,
                    "updated_at_ms": row.get::<_, Option<i64>>(4)?,
                    "archived_at_ms": row.get::<_, Option<i64>>(5)?,
                    "pinned_at_ms": row.get::<_, Option<i64>>(6)?,
                    "message_count": row.get::<_, u64>(7)?,
                }))
            })?;
            let mut children = rows.collect::<Result<Vec<_>, _>>()?;
            let truncated = children.len() as i64 > limit;
            children.truncate(limit as usize);
            // The latest run is the child's state: a running one means busy,
            // a finished one names how it ended. A child that never ran has no
            // run at all rather than a fabricated idle run.
            for child in children.iter_mut() {
                let session_id = child["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                child["run"] = latest_run(conn, &session_id)?;
                child["status"] = json!(if child["run"]["state"] == "running" {
                    "busy"
                } else {
                    "idle"
                });
                // The dashboard reads one snapshot per child: who it is, how
                // long its current run has taken, what it has billed, and the
                // tools it called most recently. All of it is already durable;
                // composing it here keeps the TUI from opening a second read
                // per block.
                let meta = delegation_meta(conn, &session, &session_id)?;
                child["description"] = meta["description"].clone();
                child["explore"] = meta["explore"].clone();
                child["background"] = meta["background"].clone();
                child["agent"] = meta["agent"].clone();
                let run = child["run"]["id"].as_str().unwrap_or_default().to_string();
                child["tokens"] = run_tokens(conn, &session_id, &run)?;
                child["tools"] = recent_tools(conn, &run)?;
                child["age_ms"] = child["run"]["started_at_ms"]
                    .as_i64()
                    .map(|started| json!(now_ms().saturating_sub(started.max(0) as u64)))
                    .unwrap_or(Value::Null);
            }
            Ok(json!({
                "session_id": session,
                "children": children,
                "truncated": truncated,
            }))
        })
        .await
    }
}

/// One child's most recently opened run, with the local timestamp its
/// `run.started` event recorded, so a UI can show elapsed time without
/// replaying events itself.
fn latest_run(conn: &rusqlite::Connection, session: &str) -> Result<Value, Error> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT id,state FROM engine_run WHERE session_id=?1 ORDER BY rowid DESC LIMIT 1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((run, state)) = row else {
        return Ok(Value::Null);
    };
    let started_at_ms: Option<i64> = conn
        .query_row(
            "SELECT recorded_at_ms FROM engine_event
             WHERE session_id=?1 AND kind='run.started' AND json_extract(data,'$.run_id')=?2
             ORDER BY seq LIMIT 1",
            rusqlite::params![session, run],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    // The run's own terminal event is where a child states how it ended — the
    // difference between a child that delivered and one whose provider turn
    // failed is exactly `reason`, and a parent that cannot read it has to find
    // out by polling the worktree (issue #2862).
    let ended: Option<(Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT json_extract(data,'$.reason'),json_extract(data,'$.message') FROM engine_event
             WHERE session_id=?1 AND kind='run.finished' AND json_extract(data,'$.run_id')=?2
             ORDER BY seq DESC LIMIT 1",
            rusqlite::params![session, run],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (reason, message) = match ended {
        Some((reason, message)) => (json!(reason), json!(message)),
        None => (Value::Null, Value::Null),
    };
    Ok(json!({
        "id": run,
        "state": state,
        "reason": reason,
        "message": message,
        "started_at_ms": started_at_ms,
    }))
}

/// What the parent recorded when it delegated this child: the description the
/// dashboard titles the block with, whether the child was an exploration
/// branch, and whether the parent still holds it as a background task.
fn delegation_meta(conn: &rusqlite::Connection, parent: &str, child: &str) -> Result<Value, Error> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT data FROM engine_event
             WHERE session_id=?1 AND kind='subagent_start'
               AND json_extract(data,'$.subagent_id')=?2
             ORDER BY seq DESC LIMIT 1",
            rusqlite::params![parent, child],
            |row| row.get(0),
        )
        .optional()?;
    let data = raw
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    Ok(json!({
        "description": data.get("description").cloned().unwrap_or(Value::Null),
        "explore": data.get("explore").cloned().unwrap_or(json!(false)),
        "background": data.get("background").cloned().unwrap_or(json!(false)),
        "agent": data.get("agent").cloned().unwrap_or(Value::Null),
    }))
}

/// Billed tokens for one child run, summed from the durable `usage` rows the
/// provider attempts recorded. A run that has not settled a turn yet reports
/// zeros rather than a missing field, so the dashboard can always print it.
fn run_tokens(conn: &rusqlite::Connection, session: &str, run: &str) -> Result<Value, Error> {
    if run.is_empty() {
        return Ok(json!({"input": 0, "output": 0, "cacheRead": 0, "total": 0}));
    }
    let (input, output, cache_read, total) = conn.query_row(
        "SELECT COALESCE(SUM(COALESCE(json_extract(data,'$.usage.inputTokens'),0)),0),
                COALESCE(SUM(COALESCE(json_extract(data,'$.usage.outputTokens'),0)),0),
                COALESCE(SUM(COALESCE(json_extract(data,'$.usage.cacheReadInputTokens'),0)),0),
                COALESCE(SUM(COALESCE(json_extract(data,'$.usage.totalTokens'),0)),0)
         FROM engine_event
         WHERE session_id=?1 AND kind='usage' AND json_extract(data,'$.run_id')=?2",
        rusqlite::params![session, run],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    Ok(json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "total": if total > 0 { total } else { input + output },
    }))
}

/// The tail of one child run's tool calls, oldest first. Four entries are
/// enough for the dashboard to show a current call plus the two or three that
/// led to it; a longer log belongs in the child's own transcript.
fn recent_tools(conn: &rusqlite::Connection, run: &str) -> Result<Value, Error> {
    if run.is_empty() {
        return Ok(json!([]));
    }
    let mut statement = conn.prepare(
        "SELECT name,input,state FROM engine_tool WHERE run_id=?1 ORDER BY rowid DESC LIMIT 4",
    )?;
    let rows = statement.query_map([run], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut tools = rows.collect::<Result<Vec<_>, _>>()?;
    tools.reverse();
    Ok(json!(tools
        .iter()
        .map(|(name, input, state)| json!({
            "name": name,
            "detail": tool_detail(name, input),
            "state": state,
        }))
        .collect::<Vec<_>>()))
}

/// One readable argument for a tool row: a path, a search pattern, or the
/// first line of a shell command. Unknown tools carry no detail rather than a
/// JSON blob.
fn tool_detail(name: &str, raw: &str) -> String {
    let value: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let input = value.get("input").unwrap_or(&value);
    let detail = match name {
        "read_file" | "read_image" | "list_files" => input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string),
        "grep" => input.get("pattern").and_then(Value::as_str).map(|pattern| {
            match input.get("path").and_then(Value::as_str) {
                Some(path) if !path.is_empty() => format!("{pattern} in {path}"),
                _ => pattern.to_string(),
            }
        }),
        "glob" => input
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::to_string),
        "bash" | "run_command" => input
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
    .unwrap_or_default();
    let collapsed = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.chars().count() > 96 {
        true => collapsed.chars().take(95).collect::<String>() + "…",
        false => collapsed,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
