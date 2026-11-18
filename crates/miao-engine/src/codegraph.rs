//! Built-in code index tools, backed by `miao-codegraph`.
//!
//! The index lives beside the engine store rather than in the workspace: it is
//! a cache of the source tree, not a project artifact, so indexing never shows
//! up as a working-tree change and never needs a permission prompt to write.

use crate::protocol::ToolDefinition;
use crate::tools::ToolError;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Subdirectory of the engine store that holds one index per workspace.
pub const DIRECTORY: &str = "codegraph";
pub const NAMES: &[&str] = &[
    "codegraph_explore",
    "codegraph_node",
    "codegraph_callers",
    "codegraph_status",
];

/// How long a cold tool call may spend finishing the index before it reports
/// `Pending` instead of answering from a subset of the workspace.
const WAIT: Duration = Duration::from_secs(120);
const PREWARM_FILES: usize = 4_000;
const PREWARM_MILLIS: u128 = 10_000;
const PREWARM_DEADLINE: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExploreInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallersInput {
    symbol: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusInput {}

pub fn is_codegraph(name: &str) -> bool {
    NAMES.contains(&name)
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "codegraph_explore".into(),
            description: "Answer a code question from the local symbol index instead of grepping. One call returns the verbatim, line-numbered source of the symbols the query names plus the call paths between them, so treat what it returns as already read. Name symbols, or a file (`path` or `path:12-40`), in the query. Prefer it over grep/read when you need to understand or locate code across files; follow up with another explore naming what it says it left out rather than reading those files. The index is built automatically and a cold call waits until it is complete; it covers only the current workspace, and a path outside it is reported as outside scope.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1, "maxLength": 4096},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 24, "default": 6},
                    "max_chars": {"type": "integer", "minimum": 2000, "maximum": 200000}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "codegraph_node".into(),
            description: "Read one file or one symbol from the index. Without `symbol` it returns a workspace file's current source with line numbers plus the files that import it — the shape read_file gives, with the blast radius attached. With `symbol` it returns that definition's source, its callers and what it calls, so an edit decision is made in one call.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "maxLength": 4096},
                    "symbol": {"type": "string", "maxLength": 256},
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "codegraph_callers".into(),
            description: "List the call sites of a symbol, with the file and line of each call. Use it to size the impact of changing that symbol before editing it.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "minLength": 1, "maxLength": 256},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}
                },
                "required": ["symbol"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "codegraph_status".into(),
            description: "Report index coverage for the current workspace: files, symbols, call edges, unresolved references, languages, store size and what the last refresh did.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
    ]
}

/// Validate an input before it reaches the permission layer and return the
/// workspace resource the call touches. `node` names a path, so it is evaluated
/// as a read of that path; the others read the workspace as a whole.
pub fn parse(name: &str, input: &Value) -> Result<String, ToolError> {
    Ok(match name {
        "codegraph_explore" => {
            let parsed: ExploreInput = decode(input.clone())?;
            if parsed.query.trim().is_empty() {
                return Err(ToolError::InvalidInput);
            }
            ".".to_owned()
        }
        "codegraph_node" => {
            let parsed: NodeInput = decode(input.clone())?;
            match (parsed.path.as_deref(), parsed.symbol.as_deref()) {
                (None, None) => return Err(ToolError::InvalidInput),
                (path, _) => path.unwrap_or(".").to_owned(),
            }
        }
        "codegraph_callers" => {
            let parsed: CallersInput = decode(input.clone())?;
            if parsed.symbol.trim().is_empty() {
                return Err(ToolError::InvalidInput);
            }
            ".".to_owned()
        }
        "codegraph_status" => {
            let _: StatusInput = decode(input.clone())?;
            ".".to_owned()
        }
        _ => return Err(ToolError::Unsupported),
    })
}

pub async fn execute(
    name: &str,
    input: Value,
    root: PathBuf,
    database: PathBuf,
) -> Result<Value, ToolError> {
    let name = name.to_owned();
    let scope_input = input.clone();
    tokio::task::spawn_blocking(move || {
        let _ = seed_worktree(&root, &database);
        let mut index = miao_codegraph::Index::open(&root, &database).map_err(external)?;
        index = index.with_budget(PREWARM_FILES, PREWARM_MILLIS);
        if name != "codegraph_status" {
            index.wait_until_ready(wait_deadline()).map_err(external)?;
        }
        let answer = match name.as_str() {
            "codegraph_explore" => {
                let parsed: ExploreInput = decode(input)?;
                index.explore(&parsed.query, parsed.limit, parsed.max_chars)
            }
            "codegraph_node" => {
                let parsed: NodeInput = decode(input)?;
                index.node(
                    parsed.path.as_deref(),
                    parsed.symbol.as_deref(),
                    parsed.offset,
                    parsed.limit,
                )
            }
            "codegraph_callers" => {
                let parsed: CallersInput = decode(input)?;
                index.callers(&parsed.symbol, parsed.limit)
            }
            "codegraph_status" => {
                let _: StatusInput = decode(input)?;
                index.status()
            }
            _ => return Err(ToolError::Unsupported),
        }
        .map_err(external)?;
        let mut content = answer.content;
        append_scope(&mut content, &root, answer.files, &name, &scope_input);
        Ok(json!({
            "content": content,
            "index": {
                "files": answer.files,
                "symbols": answer.nodes,
                "refreshed": answer.refreshed,
                "pending": answer.pending,
                "millis": answer.millis
            }
        }))
    })
    .await
    .map_err(|_| ToolError::Io(std::io::Error::other("codegraph worker failed")))?
}

/// Build this workspace's index in the background once the engine knows where
/// its private store lives. A cold `codegraph_explore` otherwise pays the
/// first-build cost inside the model's turn; the tool still waits for this
/// work to finish, so a prewarm that loses the race is late, never wrong.
pub fn prewarm(root: PathBuf, database: PathBuf) {
    static STARTED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let started = STARTED.get_or_init(|| Mutex::new(HashSet::new()));
    {
        let mut started = started.lock().unwrap_or_else(|held| held.into_inner());
        if !started.insert(database.clone()) {
            return;
        }
    }
    let _ = std::thread::spawn(move || {
        let _ = seed_worktree(&root, &database);
        let Ok(mut index) = miao_codegraph::Index::open(&root, &database) else {
            return;
        };
        index = index.with_budget(PREWARM_FILES, PREWARM_MILLIS);
        let _ = index.wait_until_ready(PREWARM_DEADLINE);
    });
}

/// Seed a linked worktree's index from its main checkout, the first time that
/// worktree is opened. The source layout is the same, so unchanged files keep
/// their rows and only the branch's edits are extracted.
fn seed_worktree(root: &Path, database: &Path) -> bool {
    if database.exists() {
        return false;
    }
    let Some(directory) = database.parent() else {
        return false;
    };
    let Some(main) = crate::worktree::main_checkout(root) else {
        return false;
    };
    let Some(source) = crate::placement::database_for(directory, &main) else {
        return false;
    };
    if source == database {
        return false;
    }
    miao_codegraph::seed(database, &source).unwrap_or(false)
}

fn wait_deadline() -> Duration {
    std::env::var("MIAO_CODEGRAPH_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(WAIT)
}

/// Make the index boundary visible in every answer. A symbol query that names
/// another checkout cannot be answered from this workspace, and a bare symbol
/// list can otherwise look as if it covered the whole filesystem.
fn append_scope(content: &mut String, root: &Path, files: usize, name: &str, input: &Value) {
    if name == "codegraph_status" {
        content.push_str(&format!("\n> index root: `{}`\n", root.display()));
        if files == 0 {
            content.push_str(&format!(
                "> warning: no source files are indexed for `{}`; codegraph has no index for this workspace.\n",
                root.display()
            ));
        }
        return;
    }
    for path in outside_paths(root, name, input) {
        content.push_str(&format!(
            "\n> warning: `{path}` is outside the indexed workspace `{}`; codegraph indexes only that workspace, so this answer does not cover it.\n",
            root.display()
        ));
    }
    if files == 0 {
        content.push_str(&format!(
            "\n> warning: no source files are indexed for `{}`; codegraph has no index for this workspace.\n",
            root.display()
        ));
        return;
    }
    content.push_str(&format!(
        "\n> index scope: `{}` ({} files indexed; other checkouts are not indexed).\n",
        root.display(),
        files
    ));
}

fn outside_paths(root: &Path, name: &str, input: &Value) -> Vec<String> {
    let text = match name {
        "codegraph_node" => input.get("path").and_then(Value::as_str).unwrap_or(""),
        _ => input.get("query").and_then(Value::as_str).unwrap_or(""),
    };
    text.split(|character: char| {
        character.is_whitespace()
            || matches!(
                character,
                '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}'
            )
    })
    .filter(|token| token.contains('/') || token.contains('\\') || token.starts_with('~'))
    .filter_map(|token| {
        if token.starts_with('~') {
            return Some(token.to_owned());
        }
        let path = Path::new(token);
        let resolved = crate::placement::canonical(&if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        });
        (!resolved.starts_with(root)).then_some(token.to_owned())
    })
    .collect()
}

fn decode<T: DeserializeOwned>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|_| ToolError::InvalidInput)
}

fn external(error: impl std::fmt::Display) -> ToolError {
    ToolError::External(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_outside_the_workspace_is_reported_and_an_inside_one_is_not() {
        let root = std::env::temp_dir().join("miao-codegraph-scope");
        let inside = root.join("src/ok.rs");
        let outside = std::env::temp_dir()
            .join("miao-codegraph-other")
            .join("x.rs");
        let input = json!({
            "query": format!("src/lib.rs {} {}", inside.display(), outside.display())
        });
        assert_eq!(
            outside_paths(&root, "codegraph_explore", &input),
            vec![outside.display().to_string()]
        );
    }

    #[test]
    fn an_answer_names_the_workspace_it_covered() {
        let root = std::env::temp_dir().join("miao-codegraph-scope");
        let mut content = "Found 1 symbol(s)".to_owned();
        append_scope(
            &mut content,
            &root,
            3,
            "codegraph_explore",
            &json!({ "query": "evaluate" }),
        );
        assert!(
            content.contains(&format!(
                "index scope: `{}` (3 files indexed",
                root.display()
            )),
            "{content}"
        );
    }

    #[test]
    fn an_unindexed_workspace_is_called_out() {
        let root = std::env::temp_dir().join("miao-codegraph-scope");
        let mut content = "codegraph index: 0 files".to_owned();
        append_scope(
            &mut content,
            &root,
            0,
            "codegraph_explore",
            &json!({ "query": "evaluate" }),
        );
        assert!(content.contains("no source files are indexed"), "{content}");
    }
}
