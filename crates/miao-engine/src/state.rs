use crate::{
    protocol::Error,
    store::{append, write_tx, Store},
    tools::ToolError,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Waiting,
    Blocked,
    Completed,
    Cancelled,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    High,
    Medium,
    Low,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Todo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
    pub content: String,
    pub status: TodoStatus,
    pub priority: Priority,
    /// Verification is a receipt, not a claim: the todo surface never accepts
    /// this field, so it stays false until a verification path fills it.
    #[serde(default)]
    pub verified: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Todos {
    pub todos: Vec<TodoInput>,
    #[serde(default)]
    pub expected_revision: Option<u64>,
}
/// The model's write surface for one checklist row: verification is a
/// receipt, not a claim, so this face carries no `verified` field at all.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoInput {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    pub content: String,
    pub status: TodoStatus,
    pub priority: Priority,
}
impl From<TodoInput> for Todo {
    fn from(input: TodoInput) -> Self {
        Todo {
            id: input.id,
            depends_on: input.depends_on,
            content: input.content,
            status: input.status,
            priority: input.priority,
            verified: false,
        }
    }
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    Done,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomous: Option<bool>,
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub evidence: Option<String>,
    pub budget: Option<Budget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_when: Option<Vec<String>>,
    #[serde(default)]
    pub expected_revision: Option<u64>,
}
/// A structured boundary the run loop can evaluate without parsing prose.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallclock_ms: Option<u64>,
    /// Money the Goal may spend, in the ledger's USD unit. COST-02 enforces it
    /// against the charges a provider reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    Approved,
    Superseded,
}
/// The method awaiting approval, kept in its own durable row apart from the
/// checklist and the goal: a displayed plan is never itself proof of
/// implementation, so approval requires the steps to already live in the
/// checklist. It feeds no loop; only the checklist does.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub summary: String,
    #[serde(default)]
    pub steps: Vec<String>,
    pub status: PlanStatus,
    #[serde(default)]
    pub transcribed: bool,
    #[serde(default)]
    pub expected_revision: Option<u64>,
}

// Task numbers/order are not prerequisites. Validate the declared graph before admission.
fn valid_dependencies(todos: &[TodoInput]) -> bool {
    let mut by_id = HashMap::new();
    for (index, todo) in todos.iter().enumerate() {
        if let Some(id) = &todo.id {
            if id.trim().is_empty() || by_id.insert(id.as_str(), index).is_some() {
                return false;
            }
        }
        let dependencies = todo.depends_on.as_deref().unwrap_or_default();
        if !dependencies.is_empty() && todo.id.is_none() {
            return false;
        }
        if dependencies.iter().collect::<HashSet<_>>().len() != dependencies.len() {
            return false;
        }
    }
    let mut degree = vec![0; todos.len()];
    let mut children = vec![Vec::new(); todos.len()];
    for (index, todo) in todos.iter().enumerate() {
        for id in todo.depends_on.as_deref().unwrap_or_default() {
            let Some(&parent) = by_id.get(id.as_str()) else {
                return false;
            };
            if matches!(&todo.status, TodoStatus::InProgress | TodoStatus::Completed)
                && !matches!(&todos[parent].status, TodoStatus::Completed)
            {
                return false;
            }
            degree[index] += 1;
            children[parent].push(index);
        }
    }
    let mut ready: VecDeque<_> = degree
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    let mut visited = 0;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for child in &children[index] {
            degree[*child] -= 1;
            if degree[*child] == 0 {
                ready.push_back(*child);
            }
        }
    }
    visited == todos.len()
}

/// Readiness is derived from durable state. Cancelled prerequisites do not unlock dependents.
pub fn runnable(todos: &[Todo]) -> Vec<&Todo> {
    let by_id: HashMap<_, _> = todos
        .iter()
        .filter_map(|todo| todo.id.as_deref().map(|id| (id, todo)))
        .collect();
    todos
        .iter()
        .filter(|todo| {
            matches!(&todo.status, TodoStatus::Pending | TodoStatus::InProgress)
                && todo
                    .depends_on
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .all(|id| {
                        by_id
                            .get(id.as_str())
                            .is_some_and(|parent| matches!(&parent.status, TodoStatus::Completed))
                    })
        })
        .collect()
}

pub fn runnable_state(state: &Value) -> Result<Vec<String>, Error> {
    if state["todos"].is_null() {
        return Ok(Vec::new());
    }
    let todos: Vec<Todo> = serde_json::from_value(state["todos"]["value"]["todos"].clone())
        .map_err(|_| Error::Invalid("invalid durable todo state".into()))?;
    Ok(runnable(&todos)
        .iter()
        .map(|todo| todo.id.clone().unwrap_or_else(|| todo.content.clone()))
        .collect())
}

/// Work the Session cannot advance on its own: items marked blocked, or items
/// still waiting on a dependency that is not completed. Each entry names the
/// item and the dependencies it is waiting for.
///
/// This is what makes a stop honest. An empty [`runnable_state`] is ambiguous
/// on its own — the list may be finished, empty, or blocked — and a continuation
/// loop that treats all three the same parks an active goal with nothing moving
/// it. A caller that finds nothing runnable reads this to say which step would
/// unblock the rest, or that the Session genuinely has nothing left.
/// Whether a block names something the model cannot change.
///
/// "Too large for one reply", "too complex", "needs many steps" are not
/// blocks — they are a refusal to decompose, and honouring them is what parks a
/// Session the operator already approved. Only an external dependency the model
/// cannot satisfy — hardware down, power off, a credential or approval a human
/// holds, a third party that is unavailable — justifies a stop.
pub fn blocked_on_external_work(evidence: &str) -> bool {
    const SCOPE: &[&str] = &[
        "too large",
        "too big",
        "too complex",
        "too many",
        "at once",
        "one reply",
        "in one go",
        "太大",
        "太多",
        "太复杂",
        "一次性",
        "无法一次",
    ];
    const EXTERNAL: &[&str] = &[
        "hardware",
        "device",
        "power",
        "offline",
        "unreachable",
        "credential",
        "token",
        "permission",
        "approval",
        "third-party",
        "third party",
        "service down",
        "network",
        "operator",
        "硬件",
        "设备",
        "断电",
        "离线",
        "凭据",
        "权限",
        "授权",
        "网络",
        "第三方",
        "现场",
        "人工",
    ];
    let lower = evidence.to_lowercase();
    if SCOPE.iter().any(|needle| lower.contains(needle)) {
        return false;
    }
    EXTERNAL.iter().any(|needle| lower.contains(needle))
}

/// What the checklist says the Session should do next, in the same vocabulary
/// the composer paints. The engine owns this answer because the engine owns
/// continuation: [`runnable`] is the rule that decides what a turn may pick up,
/// so a hint derived from anything else drifts from what the turn will do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NextWork {
    /// A `pending` or `in_progress` item whose every `depends_on` completed.
    Runnable(String),
    /// Nothing runnable, and items remain that wait on an outside event — a
    /// background job, CI, a device that has to come back.
    Waiting(String),
    /// Nothing runnable, and items remain that need a human decision.
    Blocked(String),
    /// Nothing left: an empty list, or every item completed or cancelled.
    #[default]
    Nothing,
}

/// The next move this checklist implies, read with the engine's own readiness
/// rules.
///
/// `runnable()` answers first, so the hint can never name work the next turn
/// would skip because a prerequisite is still open. `waiting` items are asked
/// before `blocked` ones: a wait is what the Session is still moving on, while
/// a block is what it cannot move at all.
pub fn next_work(state: &Value) -> NextWork {
    if state["todos"].is_null() {
        return NextWork::Nothing;
    }
    let Ok(todos) = serde_json::from_value::<Vec<Todo>>(state["todos"]["value"]["todos"].clone())
    else {
        return NextWork::Nothing;
    };
    if let Some(todo) = runnable(&todos)
        .into_iter()
        .find(|todo| !is_question(&todo.content))
    {
        return NextWork::Runnable(todo.content.trim().to_string());
    }
    for status in [TodoStatus::Waiting, TodoStatus::Blocked] {
        let found = todos
            .iter()
            .find(|todo| todo.status == status && !is_question(&todo.content));
        if let Some(todo) = found {
            return match status {
                TodoStatus::Waiting => NextWork::Waiting(todo.content.trim().to_string()),
                _ => NextWork::Blocked(todo.content.trim().to_string()),
            };
        }
    }
    // A `pending` item held by an unfinished prerequisite is waiting on that
    // work, not blocked on the operator: label it the way it reads.
    if let Some(todo) = todos.iter().find(|todo| {
        matches!(&todo.status, TodoStatus::Pending | TodoStatus::InProgress)
            && !is_question(&todo.content)
    }) {
        return NextWork::Waiting(todo.content.trim().to_string());
    }
    NextWork::Nothing
}

/// A checklist entry phrased as a question is not work the Session will do
/// next: offering it would put the operator's answer where the next task
/// belongs. The test is on the terminator because that is what marks a
/// question in both languages this UI renders.
fn is_question(content: &str) -> bool {
    let content = content.trim();
    content.ends_with('?') || content.ends_with('？')
}

pub fn blocked_work(state: &Value) -> Vec<(String, Vec<String>)> {
    if state["todos"].is_null() {
        return Vec::new();
    }
    let Ok(todos) = serde_json::from_value::<Vec<Todo>>(state["todos"]["value"]["todos"].clone())
    else {
        return Vec::new();
    };
    let by_id: HashMap<&str, &Todo> = todos
        .iter()
        .filter_map(|todo| todo.id.as_deref().map(|id| (id, todo)))
        .collect();
    let unmet = |todo: &Todo| -> Vec<String> {
        todo.depends_on
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|id| {
                !by_id
                    .get(id.as_str())
                    .is_some_and(|parent| matches!(parent.status, TodoStatus::Completed))
            })
            .cloned()
            .collect()
    };
    todos
        .iter()
        .filter(|todo| match todo.status {
            TodoStatus::Blocked => true,
            TodoStatus::Pending | TodoStatus::InProgress | TodoStatus::Waiting => {
                !unmet(todo).is_empty()
            }
            TodoStatus::Completed | TodoStatus::Cancelled => false,
        })
        .map(|todo| {
            (
                todo.id.clone().unwrap_or_else(|| todo.content.clone()),
                unmet(todo),
            )
        })
        .collect()
}

/// The todo list's meaning: which items exist and what state each is in.
/// Prose, priority and dependency decoration are excluded on purpose — the
/// continuation loop asks whether a turn moved the work, and rewriting a todo's
/// words moves nothing. Ordered by the list's own order so a re-sort alone is
/// not progress either.
pub fn todo_signature(state: &Value) -> String {
    state["todos"]["value"]["todos"]
        .as_array()
        .map(|todos| {
            todos
                .iter()
                .map(|todo| {
                    format!(
                        "{}:{}",
                        todo["id"].as_str().unwrap_or_default(),
                        todo["status"].as_str().unwrap_or_default()
                    )
                })
                .collect::<Vec<String>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// The task-list protocol the system prompt carries, mirroring the
/// instructions in the TypeScript `SessionTodo.context` block
/// (`packages/engine-ts/core/src/session/todo.ts`).
///
/// It is constant text, so it costs the Session one context epoch no matter how
/// many turns run: only Anthropic personas name `todowrite`, and a model loaded
/// with any other persona asset never learned the tool exists, so nothing wrote
/// the first checklist and the sidebar stayed empty. The recorded list itself
/// keeps travelling in the per-turn `<session-state>` note.
pub const TASK_LIST_PROTOCOL: &str = "\n\n<session-task-list>\n# Current session task list\nThe persisted todo list is shown to the user beside the transcript, and `<session-state>` carries its current contents. It supersedes older todo lists in the conversation.\nUse todowrite to track multi-step work. Keep the list synchronized with actual progress: mark work in_progress when starting, completed immediately after finishing and verifying it, and cancelled when it is no longer required. Update it before a final response or handoff; do not leave finished work pending or mark unfinished work completed. If blocked, leave the task open and explain the blocker. Do not infer completion merely from an idle session or a successful tool call.\nA background wait blocks only its dependent task. Give related tasks stable ids and depends_on links; keep not-started dependent tasks pending, and mark external job waits waiting. Continue pending or in_progress tasks only when every depends_on task is completed. Mark tasks needing a human decision blocked. End the Session only when no runnable task remains. Budgets and turn limits are enforced by the harness, not by you: never declare a budget exhausted on your own initiative and never stop mid-work for budget reasons.\nMake the final item an independent check of the work: run the tests or verify the claim against its source. For high-risk changes (concurrency, permissions, protocol shapes), have a fresh `task` child that has not seen the work review the diff instead of grading it yourself.\n</session-task-list>";

pub struct Mutation {
    pub(crate) kind: &'static str,
    pub(crate) value: Value,
    pub(crate) expected: Option<u64>,
}
impl Mutation {
    pub fn parse(name: &str, value: Value) -> Result<Self, ToolError> {
        let mutation = match name {
            "todowrite" => {
                let todos: Todos =
                    serde_json::from_value(value).map_err(|_| ToolError::InvalidInput)?;
                if todos.todos.len() > 128
                    || !valid_dependencies(&todos.todos)
                    || todos
                        .todos
                        .iter()
                        .any(|todo| todo.content.trim().is_empty() || todo.content.len() > 2048)
                {
                    return Err(ToolError::InvalidInput);
                }
                Self {
                    kind: "todos",
                    expected: todos.expected_revision,
                    value: json!({"todos":todos.todos.into_iter().map(Todo::from).collect::<Vec<_>>()}),
                }
            }
            "goal" => {
                let goal: Goal =
                    serde_json::from_value(value).map_err(|_| ToolError::InvalidInput)?;
                if goal.objective.trim().is_empty()
                    || goal.objective.len() > 8192
                    || goal.evidence.as_ref().is_some_and(|text| text.len() > 8192)
                    || !valid_budget(goal.budget.as_ref())
                    || !valid_stop_when(goal.stop_when.as_ref())
                    || matches!(goal.status, GoalStatus::Done | GoalStatus::Blocked)
                        && goal
                            .evidence
                            .as_ref()
                            .is_none_or(|text| text.trim().is_empty())
                {
                    return Err(ToolError::InvalidInput);
                }
                // A block is a claim the operator acts on. "Too large to do at
                // once" is not one — it is a refusal to decompose, and honouring
                // it parks a Session the operator already approved. The engine's
                // own blocks carry a `loop: ` prefix and are exempt, because the
                // loop wrote them and no model claim is involved.
                if matches!(goal.status, GoalStatus::Blocked)
                    && goal.evidence.as_deref().is_none_or(|text| {
                        !text.starts_with("loop: ") && !blocked_on_external_work(text)
                    })
                {
                    return Err(ToolError::InvalidInput);
                }
                let mut value = json!({"objective":goal.objective,"status":goal.status,"evidence":goal.evidence});
                if let Some(budget) = goal.budget {
                    value["budget"] = json!(budget);
                }
                if let Some(stop_when) = goal.stop_when {
                    value["stop_when"] = json!(stop_when);
                }
                if let Some(autonomous) = goal.autonomous {
                    value["autonomous"] = json!(autonomous);
                }
                Self {
                    kind: "goal",
                    expected: goal.expected_revision,
                    value,
                }
            }
            "planwrite" => {
                let plan: Plan =
                    serde_json::from_value(value).map_err(|_| ToolError::InvalidInput)?;
                if plan.summary.trim().is_empty()
                    || plan.summary.len() > 8192
                    || plan.steps.len() > 64
                    || plan
                        .steps
                        .iter()
                        .any(|step| step.trim().is_empty() || step.len() > 2048)
                    || matches!(plan.status, PlanStatus::Approved) && !plan.transcribed
                {
                    return Err(ToolError::InvalidInput);
                }
                Self {
                    kind: "plan",
                    expected: plan.expected_revision,
                    value: json!({"summary":plan.summary,"steps":plan.steps,"status":plan.status,"transcribed":plan.transcribed}),
                }
            }
            _ => return Err(ToolError::Unsupported),
        };
        if serde_json::to_vec(&mutation.value)
            .map_err(|_| ToolError::InvalidInput)?
            .len()
            > 32768
            || mutation
                .expected
                .is_some_and(|revision| revision > i64::MAX as u64)
        {
            return Err(ToolError::InvalidInput);
        }
        Ok(mutation)
    }
}
/// At least one unit must be declared and no unit may be zero, so the boundary
/// stays decidable later instead of degrading back to prose.
fn valid_budget(budget: Option<&Budget>) -> bool {
    let Some(budget) = budget else {
        return true;
    };
    let declaration = crate::budget::Declaration {
        cost_usd: budget.cost_usd,
        tokens: budget.tokens,
        turns: budget.iterations,
        wallclock_ms: budget.wallclock_ms,
    };
    declaration.validate().is_ok()
}

fn valid_stop_when(conditions: Option<&Vec<String>>) -> bool {
    conditions.is_none_or(|conditions| {
        conditions.len() <= 16
            && conditions
                .iter()
                .all(|condition| !condition.trim().is_empty() && condition.len() <= 512)
    })
}
impl Store {
    pub async fn state(&self, session: &str) -> Result<Value, Error> {
        let session = session.to_owned();
        self.call(move |conn| projection(conn, &session, i64::MAX as u64))
            .await
    }
    /// The event ledger is the sole state authority. One transaction checks the
    /// optimistic revision, records the operation and commits the new value.
    pub async fn update_state(
        &self,
        session: &str,
        operation_id: &str,
        mutation: Mutation,
    ) -> Result<Value, Error> {
        if operation_id.is_empty() || operation_id.len() > 512 {
            return Err(Error::Invalid("invalid state operation id".into()));
        }
        let (session, operation_id) = (session.to_owned(), operation_id.to_owned());
        self.call(move|conn|{
            let tx=write_tx(conn)?;
            let previous:Option<(u64,String)>=tx.query_row("SELECT seq,data FROM engine_event WHERE session_id=?1 AND kind='session.state.updated' AND json_extract(data,'$.operation_id')=?2 ORDER BY seq DESC LIMIT 1",params![session,operation_id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            if let Some((revision,data))=previous {
                let data:Value=serde_json::from_str(&data)?;
                if data["kind"]!=mutation.kind||data["value"]!=mutation.value||data["expected_revision"]!=json!(mutation.expected){return Err(Error::Conflict);}
                return Ok(json!({"kind":mutation.kind,"revision":revision,"value":mutation.value,"duplicate":true}));
            }
            let state=projection(&tx,&session,i64::MAX as u64)?;
            let revision=state[mutation.kind]["revision"].as_u64().unwrap_or(0);
            if mutation.expected.is_some_and(|expected|expected!=revision){return Err(Error::Conflict);}
            let event=append(&tx,&session,"session.state.updated",json!({"operation_id":operation_id,"kind":mutation.kind,"value":mutation.value,"expected_revision":mutation.expected}))?;
            tx.commit()?;
            Ok(json!({"kind":mutation.kind,"revision":event.seq,"value":mutation.value,"duplicate":false}))
        }).await
    }
}
pub(crate) fn projection(conn: &Connection, session: &str, through: u64) -> Result<Value, Error> {
    let mut state = json!({"todos":null,"goal":null,"plan":null});
    for kind in ["todos", "goal", "plan"] {
        let row:Option<(u64,String)>=conn.query_row("SELECT seq,data FROM engine_event WHERE session_id=?1 AND seq<=?2 AND kind='session.state.updated' AND json_extract(data,'$.kind')=?3 ORDER BY seq DESC LIMIT 1",params![session,through,kind],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((revision, data)) = row {
            let data: Value = serde_json::from_str(&data)?;
            state[kind] = json!({"revision":revision,"value":data["value"]});
        }
    }
    Ok(state)
}

/// A list that stopped tracking real progress is worse than no list: the model
/// keeps reading finished work as open and open work as finished.
/// `packages/engine-ts/core/src/session/todo.ts` nudges a stale ledger through
/// `turnsBehind`; the Rust Session had no counterpart, so a long turn could run
/// dozens of tool calls without ever being asked to reconcile.
pub fn ledger_nudge(turns_since_write: u64) -> Option<&'static str> {
    const STALE_AFTER: u64 = 3;
    // `turnsBehind` assumes a reminder is enough. A long tool-driven stretch
    // showed it is not: the list went 176 turns without a write while every
    // turn carried the gentle note. Past this bound the note is an instruction
    // with the consequence spelled out, so a model that reads it has no
    // ambiguity left about what to do next.
    const IGNORED_AFTER: u64 = 8;
    if turns_since_write < STALE_AFTER {
        return None;
    }
    if turns_since_write >= IGNORED_AFTER {
        return Some(
            "The persisted task list has not been rewritten for many turns and no longer describes \
             this work. Call todowrite now, before any further tool call: mark finished and verified \
             work completed, the item you are working on in_progress, and everything else pending, \
             waiting or blocked. The operator reads this list as current progress, so a list that \
             keeps drifting is worse than none.",
        );
    }
    Some(
        "The persisted task list has not been reconciled for several turns. Update it with \
         todowrite before continuing: mark started work in_progress, finished and verified \
         work completed, and work whose prerequisites are still open pending.",
    )
}

/// A persisted list that still has open items and has been carried through
/// this many provider turns without being rewritten *and* without the Session
/// executing a tool call it had not already made stops the drain.
///
/// The text-only stall guard measures a repeated *reply*; a Session that keeps
/// calling tools never reaches it. One such Session ran 176 turns with an
/// untouched list while every turn carried `ledger_nudge`, so a reminder alone
/// is not a bound: the operator reads the sidebar as current progress.
///
/// The debt is counted only across turns that repeated themselves. An earlier
/// bound of twelve turns measured how long the checklist had gone unwritten,
/// which is bookkeeping: it stopped Sessions that were editing files and
/// closing out a release, and an operator had to send a message to continue
/// work that was never adrift. A turn that executed a new call clears the debt,
/// so the bound reports the drift it exists for — a list left behind while the
/// Session repeats itself — instead of the duration of real work.
pub const TODO_STALE_STOP: u64 = 24;

pub fn todo_stale(turns_since_write: u64, state: &Value) -> bool {
    turns_since_write >= TODO_STALE_STOP
        && state["todos"]["value"]["todos"]
            .as_array()
            .is_some_and(|todos| {
                todos.iter().any(|todo| {
                    !matches!(
                        todo["status"].as_str(),
                        Some("completed") | Some("cancelled")
                    )
                })
            })
}

/// What the operator reads when a stale list stops the drain: what happened,
/// that nothing is running, and the one move that continues.
pub fn todo_stale_note(turns: u64) -> String {
    format!(
        "Task paused: the persisted task list still has open items, and {turns} provider turns ran without rewriting it or running a tool call that was not already made, so it no longer describes this work. Nothing is running now and no message was dropped. Update it with todowrite — finished and verified work completed, the current item in_progress, everything else pending, waiting or blocked — then send a message or let the next event continue. The list is a reading of progress, not proof of it."
    )
}

#[cfg(test)]
mod tests {
    /// The hint a composer paints must name the work the next turn may pick
    /// up, so it reads the same readiness rule continuation does: a `pending`
    /// item whose prerequisite is still open is not next, and the item holding
    /// it is.
    #[test]
    fn the_next_move_respects_dependencies_the_way_continuation_does() {
        let state = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "  wire the parser  ", "status": "pending", "priority": "high"},
            {"id": "b", "content": "cover it with tests", "status": "pending", "priority": "high", "depends_on": ["a"]},
        ]}}});
        assert_eq!(
            super::next_work(&state),
            super::NextWork::Runnable("wire the parser".into())
        );

        let held = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "wire the parser", "status": "in_progress", "priority": "high"},
            {"id": "b", "content": "cover it with tests", "status": "pending", "priority": "high", "depends_on": ["a"]},
        ]}}});
        assert_eq!(
            super::next_work(&held),
            super::NextWork::Runnable("wire the parser".into()),
            "an in_progress item is still the next move, just not the dependent one"
        );

        let only_held = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "wire the parser", "status": "waiting", "priority": "high"},
            {"id": "b", "content": "cover it with tests", "status": "pending", "priority": "high", "depends_on": ["a"]},
        ]}}});
        assert_eq!(
            super::next_work(&only_held),
            super::NextWork::Waiting("wire the parser".into()),
            "the dependency that would release the rest comes first"
        );
    }

    /// A wait is not a block and a block is not an empty list: the composer
    /// says which one it is, so a stalled Session never reads as askable.
    #[test]
    fn nothing_runnable_still_names_what_holds_the_list() {
        let waiting = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "Watch CI 45021791396", "status": "waiting", "priority": "high"},
        ]}}});
        assert_eq!(
            super::next_work(&waiting),
            super::NextWork::Waiting("Watch CI 45021791396".into())
        );

        let blocked = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "decide the pricing tier", "status": "blocked", "priority": "high"},
        ]}}});
        assert_eq!(
            super::next_work(&blocked),
            super::NextWork::Blocked("decide the pricing tier".into())
        );

        let done = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "done", "status": "completed", "priority": "low"},
            {"id": "b", "content": "dropped", "status": "cancelled", "priority": "low"},
        ]}}});
        assert_eq!(super::next_work(&done), super::NextWork::Nothing);
        assert_eq!(
            super::next_work(&json!({"todos": null})),
            super::NextWork::Nothing
        );
    }

    /// A question is not work: whatever the list holds, the composer must not
    /// offer the operator's answer as the next task.
    #[test]
    fn a_question_is_never_the_next_move() {
        let state = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "confirm the port?", "status": "pending", "priority": "high"},
        ]}}});
        assert_eq!(super::next_work(&state), super::NextWork::Nothing);
    }

    /// "Too large" is not a block. Only an external dependency is.
    #[test]
    fn a_block_is_external_only_when_it_names_something_the_model_cannot_change() {
        assert!(super::blocked_on_external_work(
            "the LIO device is offline; it needs a power cycle on site"
        ));
        assert!(super::blocked_on_external_work("需要现场人工给设备通电"));
        assert!(!super::blocked_on_external_work(
            "too large to finish in one reply"
        ));
        assert!(!super::blocked_on_external_work("任务太大，一次做不完"));
        assert!(!super::blocked_on_external_work(
            "too complex, and it also needs network access"
        ));
        assert!(!super::blocked_on_external_work(""));
    }
    /// A list with nothing runnable is not the same as a finished one. This
    /// names what is holding it, and the dependency that would release it.
    #[test]
    fn blocked_work_names_the_item_and_the_dependency_holding_it() {
        let state = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "port mem", "status": "completed", "priority": "high"},
            {"id": "b", "content": "memory domain", "status": "blocked", "priority": "high", "depends_on": ["a"]},
            {"id": "c", "content": "maps", "status": "pending", "priority": "medium", "depends_on": ["z"]},
        ]}}});
        let blocked = super::blocked_work(&state);
        assert_eq!(blocked.len(), 2, "{blocked:?}");
        assert_eq!(blocked[0].0, "b");
        assert!(blocked[0].1.is_empty(), "b's only dependency completed");
        assert_eq!(blocked[1].0, "c");
        assert_eq!(blocked[1].1, vec!["z".to_string()]);
    }

    /// A finished list is not blocked: it has simply nothing left, which is a
    /// different stop and must not be nudged.
    #[test]
    fn a_finished_list_has_no_blocked_work() {
        let state = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "done", "status": "completed", "priority": "low"},
        ]}}});
        assert!(super::blocked_work(&state).is_empty());
    }
    use serde_json::{json, Value};

    #[test]
    fn a_ledger_untouched_for_several_turns_gets_a_nudge() {
        assert_eq!(super::ledger_nudge(0), None);
        assert_eq!(super::ledger_nudge(2), None);
        let nudge = super::ledger_nudge(3).expect("three turns without a write");
        assert!(nudge.contains("todowrite"), "{nudge}");
        assert!(nudge.contains("in_progress"), "{nudge}");
    }

    /// The gentle note was ignored for 176 turns in one Session, so past a
    /// bound the note has to be an instruction that names the consequence.
    #[test]
    fn a_list_ignored_for_many_turns_is_told_to_reconcile_before_working() {
        let gentle = super::ledger_nudge(3).expect("gentle nudge");
        assert!(!gentle.contains("before any further tool call"), "{gentle}");
        let firm = super::ledger_nudge(8).expect("firm nudge");
        assert!(firm.contains("before any further tool call"), "{firm}");
        assert!(firm.contains("no longer describes"), "{firm}");
        assert_eq!(super::ledger_nudge(64), super::ledger_nudge(8));
    }

    #[test]
    fn a_list_with_open_items_that_is_never_rewritten_stops_the_drain() {
        let open = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "doing", "status": "in_progress", "priority": "high"},
        ]}}});
        assert!(!super::todo_stale(23, &open));
        assert!(super::todo_stale(24, &open));
        let finished = json!({"todos": {"value": {"todos": [
            {"id": "a", "content": "done", "status": "completed", "priority": "high"},
            {"id": "b", "content": "dropped", "status": "cancelled", "priority": "low"},
        ]}}});
        assert!(
            !super::todo_stale(64, &finished),
            "a finished list is not stale"
        );
        assert!(!super::todo_stale(64, &json!({"todos": null})));
        let note = super::todo_stale_note(24);
        assert!(note.contains("todowrite"), "{note}");
        assert!(note.contains("24 provider turns"), "{note}");
        assert!(note.contains("not already made"), "{note}");
    }

    /// Only Anthropic personas name `todowrite`, so every other model learns
    /// the tool exists only from the protocol the system prompt carries.
    #[test]
    fn the_task_list_protocol_teaches_todowrite_and_its_rules() {
        let protocol = super::TASK_LIST_PROTOCOL;
        assert!(
            protocol.starts_with("\n\n<session-task-list>"),
            "{protocol}"
        );
        assert!(protocol.ends_with("\n</session-task-list>"), "{protocol}");
        assert!(
            protocol.contains("Use todowrite to track multi-step work"),
            "{protocol}"
        );
        // The rules that keep a long list honest: status timing, prerequisites,
        // and what a blocker means.
        assert!(protocol.contains("in_progress when starting"), "{protocol}");
        assert!(protocol.contains("completed immediately"), "{protocol}");
        assert!(protocol.contains("depends_on"), "{protocol}");
        assert!(protocol.contains("no runnable task remains"), "{protocol}");
        assert!(protocol.contains("independent check"), "{protocol}");
        assert!(protocol.contains("`task` child"), "{protocol}");
        // Nothing session-specific rides it: a list that changed every turn
        // would churn the context epoch the system prompt is keyed by.
        assert!(!protocol.contains("Runnable tasks"), "{protocol}");
    }

    /// A completed todo must not be presentable as verified success: the
    /// write surface rejects the field outright, and the durable value
    /// records the default until a verification receipt lands.
    #[test]
    fn a_todo_cannot_claim_verification_through_the_write_surface() {
        let rejected = super::Mutation::parse(
            "todowrite",
            json!({"todos":[{"content":"wire it","status":"completed","priority":"high","verified":true}]}),
        );
        assert!(rejected.is_err());
        let mutation = super::Mutation::parse(
            "todowrite",
            json!({"todos":[{"content":"wire it","status":"completed","priority":"high"}]}),
        )
        .unwrap();
        assert_eq!(mutation.value["todos"][0]["verified"], false);
    }

    #[test]
    fn goal_budget_is_a_structured_boundary_not_prose() {
        let parse = |value: Value| super::Mutation::parse("goal", value);
        assert!(parse(json!({"objective":"go","status":"active","budget":"two hours"})).is_err());
        assert!(parse(json!({"objective":"go","status":"active","budget":{}})).is_err());
        assert!(
            parse(json!({"objective":"go","status":"active","budget":{"iterations":0}})).is_err()
        );
        assert!(parse(
            json!({"objective":"go","status":"active","budget":{"tokens":0,"iterations":3}})
        )
        .is_err());
        assert!(
            parse(json!({"objective":"go","status":"active","stop_when":["pass", "  "]})).is_err()
        );
        let mutation = parse(json!({"objective":"go","status":"active","budget":{"iterations":25,"wallclock_ms":600000,"cost_usd":2.5},"stop_when":["cargo test green"]})).unwrap();
        assert_eq!(mutation.value["budget"]["iterations"], 25);
        assert_eq!(mutation.value["budget"]["wallclock_ms"], 600000);
        assert_eq!(mutation.value["budget"]["cost_usd"], 2.5);
        assert!(mutation.value["budget"]["tokens"].is_null());
        // COST-02 enforces the money axis, so a Goal cannot state one the run
        // loop would have to refuse: zero, negative, or past the ceiling.
        for invalid in [json!(0), json!(-1), json!(100001)] {
            assert!(parse(
                json!({"objective":"go","status":"active","budget":{"cost_usd":invalid}})
            )
            .is_err());
        }
        assert_eq!(mutation.value["stop_when"][0], "cargo test green");
        let bare = parse(json!({"objective":"go","status":"active"})).unwrap();
        assert!(bare.value["budget"].is_null());
    }

    /// Approving a plan asserts its steps already live in the checklist; a
    /// displayed plan is never itself proof of implementation. Superseded
    /// retires a plan without faking that approval.
    #[test]
    fn an_approved_plan_must_already_live_in_the_checklist() {
        let parse = |value: Value| super::Mutation::parse("planwrite", value);
        assert!(
            parse(json!({"summary":"decode first","status":"approved","transcribed":false}))
                .is_err()
        );
        assert!(parse(json!({"summary":" ","status":"pending"})).is_err());
        assert!(parse(json!({"summary":"decode first","steps":[" "],"status":"pending"})).is_err());
        let mutation = parse(json!({"summary":"decode first","steps":["decode the fixture"],"status":"approved","transcribed":true})).unwrap();
        assert_eq!(mutation.kind, "plan");
        assert_eq!(mutation.value["status"], "approved");
        assert_eq!(mutation.value["transcribed"], true);
        assert!(parse(json!({"summary":"decode first","status":"superseded"})).is_ok());
    }
}
