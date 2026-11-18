# Anti-splitting: finish the task instead of narrating the plan

Status: design, decisions made. Implements the operator complaint "it lists the
work, then stops" without asking the model to be better.

## Observed failure

A text-only turn ends the drain. If the model answers an approved instruction
with a plan ("1–4, here is the order, starting with 1") and no tool call, the
engine sees a finished turn and nothing continues it. The operator then prompts
again for each step they had already asked for once.

The engine's only continuation signal today is `runnable_state()` over the todo
list. In the reported session that list read `5/5 completed` because the plan
lived in prose, so there was nothing to continue and nothing to say: no
`loop.stopped`, no `loop.blocked`, just an idle Session.

## Decision: "done" is a predicate over durable state, not over the last message

A drain may end only when every one of these holds. If any fails, the engine
admits exactly one continuation that names the first unmet item.

1. **No active goal.** `goal.status == active` means the operator asked for an
   outcome that is still being pursued. `blocked` (with evidence) and `done`
   (with evidence) both permit the end; absence of a goal does not block it.
2. **No runnable todo.** Unchanged from today: `pending`/`in_progress` items
   whose dependencies are complete.
3. **No plan step without a completed todo.** A `planwrite` plan is the durable
   shape of approved work. If the plan has four steps and only one has a
   completed todo, the remaining three are the continuation's target — even when
   the todo list claims to be finished, because a plan that outlives its
   checklist is exactly the "listed, then stopped" state.
4. **No blocked todo still unfed.** The first continuation for a given todo
   signature names the blocked item and the dependency it waits on (landed in
   #3161). A second nudge for the same signature never happens.

The continuation text names ids from durable state, never a paraphrase of the
last message, so it cannot be satisfied by restating the plan.

## Boundedness (this is what keeps it from spinning)

- Per-signature idempotency: each unmet condition is keyed by
  `(todo_signature, plan revision, round)`; a signature that already produced a
  nudge produces no second one.
- The existing `goal_stalls` / `goal_flat` / `no-progress` guards still run and
  still pause the loop after real repetition (they are what a weak model hits).
- A pause is recorded as `loop.stopped` with its reason, so the operator can see
  which bound fired instead of a silent idle.
- An idle Session with no goal, no runnable todo and no plan asks the engine
  nothing.

## Plan granularity (what makes #3 usable)

`planwrite` rejects a plan whose steps are not deliverable-level:

- every step must name an artifact or a verification ("ship X", "test Y"), and
  a step may not be a narration verb alone ("think", "investigate", "review");
- a plan with more than one step must have at least as many distinct artifacts
  or commands as steps;
- a rejected plan returns the offending step, so the model can restate it rather
  than silently keeping the fine-grained version.

Without this, a model can satisfy the plan gate by writing ten micro-steps that
no single turn can close, which converts the fix into a longer loop.

## Turn boundaries stay invisible

Continuations already run inside the same drain. The TUI must not paint a
"Worked for Xs" footer per internal turn: one drain, one elapsed line, one final
report. A continuation that pauses does paint — that is a real stop and the
reason is in `loop.stopped`.

## What this does not do

It cannot make a weak model finish work it will not do. It removes the free
option of ending the turn after announcing the plan, and it makes every stop
state its cause. Nothing here gates a tool call or rewrites the transcript.

## Rollout

1. `WHOLE_TASK_NOTE` in the request-scoped guidance slot (landed, #3162).
2. `blocked_work()` + the blocked continuation (#3161, landed).
3. The plan/stop_when predicate above, evaluated where the drain decides whether
   to continue (`runtime.rs`, the `loop_allowed` branch).
4. `planwrite` granularity validation.
5. TUI: one elapsed line per drain, and a `Blocked · <item>` status distinct from
   `Waiting for model`.
