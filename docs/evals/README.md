# Evaluation harness

`script/eval.ts` runs a task set repeatedly through `miao run --format json`
under a turn and wallclock budget, records one JSONL line per trial, and reports
unbiased pass@k plus the spend, cache, budget, and policy signals a regression
review needs.

The harness never calls a model in CI. Tests use a fake engine executable that
only emits `step_finish` events; real evaluation needs credentials and network
and is an operator action.

## Why these numbers

Repeated sampling turns coverage into a measurable quantity
([Large Language Monkeys, arXiv:2407.21787](https://arxiv.org/abs/2407.21787)).
The pass@k estimator used here is the unbiased one from Chen et al. (2021),
which is also the estimator behind that repeated-sampling literature:

```
pass@k = 1 - C(n - c, k) / C(n, k)
```

with `n` trials and `c` passes. The harness adds a fixed budget per trial, so
every sample is comparable: a session that runs out of turns or wallclock time
is a failed trial, not a cheaper success.

Step-level review ([Agent-as-a-Judge, arXiv:2410.10934](https://arxiv.org/abs/2410.10934))
needs a per-step record of what the agent did and what it cost. This harness
records that substrate (turns, tokens, cache reads/writes, provider-reported
cost) but keeps pass/fail deterministic through the task's assertion command and
policy bounds. An LLM judge is deliberately not in the hot path.

## Quick start

```sh
# 1. Run every task 5 times. CLI budgets are upper bounds; a task's own
#    budget is used when it is stricter.
bun script/eval.ts run \
  --tasks docs/evals/tasks.example.json \
  --trials 5 \
  --budget-turns 40 \
  --budget-ms 600000 \
  --model provider/model \
  --record docs/evals/2026-11-16-example.jsonl

# 2. Report pass@1 and pass@5 for that record.
bun script/eval.ts report \
  --record docs/evals/2026-11-16-example.jsonl \
  --k 1,5
```

`--engine-bin` defaults to `$MIAO_ENGINE_BIN`, then `miao`. Pass a prebuilt
binary (for example `miao-dev` or an artifact from a build host) to evaluate the
exact artifact under test.

`run` exits 0 when every trial was recorded, even if some trials failed; a
failed trial is data, not a harness error. A missing task file, an unreadable
record, or a missing engine binary exits non-zero before any record is written.

Each trial gets a fresh temporary workspace and a fresh
`MIAO_ENGINE_STORAGE`, so sessions cannot leak state into each other. The
workspace is deleted after the assertion runs.

## Budgets

`run` requires `--budget-turns`, `--budget-ms`, or both, unless every task in
the file declares its own budget. When both the CLI and a task state an axis,
the stricter value wins:

```
effective_turns       = min(task.budget.turns,       --budget-turns)
effective_wallclock   = min(task.budget.wallclock_ms, --budget-ms)
```

The budget is injected through the product configuration path, not by changing
the engine protocol:

```json
{ "budget": { "turns": 40, "wallclock_ms": 600000 } }
```

`run` writes that document to `MIAO_CONFIG_CONTENT` for the child process. If the
operator already set `MIAO_CONFIG_CONTENT`, the harness parses it as JSON/JSONC
and merges the budget keys, preserving every other section and any other budget
axis. This is the same `budget` section the engine evaluates at provider-turn
boundaries; a spent budget is visible in the trial's `budget_hit`.

A trial is one fresh Session, so its externally enforceable budget is the
session-scoped `budget` section. The engine's `goal.budget` uses the same turns
and wallclock axes and is evaluated alongside it, but a Goal is recorded by the
model, not by the host, so it cannot be imposed before the first prompt. This is
the deliberate equivalent: same axes, same turn-boundary gate, one Session per
trial. If the engine later grows a host-side per-run Goal flag, switch it here.

## Task set

`docs/evals/tasks.example.json` is the template. A task file is one object:

```json
{
  "name": "example",
  "tasks": [
    {
      "id": "bugfix-median",
      "prompt": "bun test fails. Fix src/math.ts without editing the test.",
      "files": { "src/math.ts": "...", "src/math.test.ts": "..." },
      "assert": { "command": "bun test", "timeout_ms": 120000 },
      "budget": { "turns": 30, "wallclock_ms": 300000 },
      "policy": { "max_turns": 30, "max_cost_usd": 0.25 }
    }
  ]
}
```

| field | meaning |
| --- | --- |
| `id` | unique task name in the file; appears in every record |
| `prompt` | the single user prompt sent to `miao run` |
| `files` | optional fixture written into the fresh workspace; paths must stay inside it |
| `assert.command` | shell command run in the workspace after the session; exit 0 means satisfied |
| `assert.timeout_ms` | assertion timeout, default 120000 |
| `budget` | optional per-task `turns` / `wallclock_ms` |
| `policy` | optional bounds checked against the trial's telemetry |
| `model` | optional per-task model override |

Policy bounds turn a costly or looping run into a failed trial even if its
assertion happens to pass:

- `max_turns`
- `max_wallclock_ms`
- `max_cost_usd`
- `max_tokens` (input + output + reasoning + cache read + cache write)
- `require_cache_read` (true means `cache_read` must be positive)

The assertion command is the task's exit condition. Keep it deterministic and
self-contained; `bun test`, `cmp`, and shell checks work well. A task whose
assertion passes without any provider step is still failed (`no_provider_step`),
so an untouched workspace cannot fake a refusal task.

## Record format

The default record path is `docs/evals/<date>-<name>.jsonl`, where `name` comes
from the task file or `--name`. Files are append-only: `run` appends one line
per settled trial and never truncates the file. Use `--record` to choose another
path. A run ID such as `2026-11-16T143000-example` is stamped on every line.

```json
{
  "schema": "miao.eval.trial.v1",
  "run": "2026-11-16T143000-example",
  "task": "bugfix-median",
  "trial": 0,
  "pass": true,
  "turns": 6,
  "tokens": { "input": 1234, "output": 456, "reasoning": 0, "cache_read": 1000, "cache_write": 0 },
  "cost_usd": 0.0042,
  "cost_unknown_steps": 0,
  "wallclock_ms": 32100,
  "exit_code": 0,
  "assert": { "command": "bun test", "exit_code": 0 },
  "budget": { "turns": 30, "wallclock_ms": 300000 },
  "budget_hit": false,
  "failures": [],
  "engine": "miao",
  "model": "provider/model",
  "started_at": "2026-11-16T14:30:00.000Z"
}
```

| field | meaning |
| --- | --- |
| `schema` | record version, currently `miao.eval.trial.v1` |
| `run` | one invocation; `report --run ID` isolates it |
| `task`, `trial` | task ID and zero-based trial number |
| `pass` | every gate below succeeded |
| `turns` | number of settled `step_finish` events |
| `tokens` | engine step counters; billed input is `input + cache_read` |
| `cost_usd` | sum of engine-reported step cost |
| `cost_unknown_steps` | steps with no provider price; makes `$` a lower bound |
| `wallclock_ms` | engine process wall time, measured before the assertion runs |
| `exit_code` | engine exit code (`null` when killed for timeout) |
| `assert` | assertion command and its exit code; output only on failure |
| `budget`, `budget_hit` | effective limits and whether the trial reached one |
| `failures` | every failed gate: `timeout`, `run_exit:N`, `no_provider_step`, `assert_timeout`, `assert_exit:N`, `policy:*`, and a `budget:*` diagnosis when a failed trial also hit its budget |
| `engine`, `model` | artifact and model under test |

`cache_write` is present in the schema but the current CLI `step_finish`
projection carries only `tokens.cache.read`; a record therefore shows
`cache_write: 0` even when a provider wrote a cache. Treat that axis as
unavailable until the event projection carries it, rather than as a measured
zero.

## Report and regression review

```sh
bun script/eval.ts report --record docs/evals/2026-11-16-example.jsonl --k 1,5
bun script/eval.ts report --record docs/evals/2026-11-16-example.jsonl --run 2026-11-16T143000-example
bun script/eval.ts report --record docs/evals/2026-11-16-example.jsonl --k 1,5 --out docs/evals/2026-11-16-example.md
```

The report prints one row per task and a totals row:

- `pass@1` is the pass rate; `pass@k` uses the unbiased estimator. `k > n` is
  shown as `—` rather than pretending a larger sample exists.
- `turns p50` is the median provider-turn count. A rising median with a stable
  pass rate usually means more retries or exploration.
- `input`/`output`/`reasoning`/`cache read`/`cache write` are sums over the
  selected trials. Compare them only at the same task count and model.
- `$` is the sum of provider-reported step cost. `unknown` steps are disclosed;
  a run whose cost is mostly unknown cannot support a cost regression claim.
- `cache hit` is `cache_read / (input + cache_read)`.
- `budget hits` counts trials that reached a turn or wallclock limit. A rise
  means either the task got harder or the budget is too tight.
- `policy fails` counts failed policy bounds; the failure list names each one.

For a regression comparison:

1. Freeze the task file, model, engine artifact, trials, and budget.
2. Run the old and new artifacts into separate records (or separate run IDs).
3. Compare `pass@1`, then the same `pass@k`, then turns/tokens/cost/cache.
4. Read the failure list before attributing a pass-rate change to the model:
   `budget:*` and `assert_timeout` mean the harness, not model quality, may have
   changed the result.

Use `--run` when one file holds several runs; without it the report aggregates
every run in the file and lists them in the header.

## Tests

```sh
bun test -c /dev/null script/eval.test.ts
# or, from the script directory:
cd script && bun test eval.test.ts
```

The repository root `bunfig.toml` deliberately points `test.root` at a
non-existent guard directory, so a bare `bun test script/eval.test.ts` from the
root is refused (that guard applies to every existing test). Passing
`-c /dev/null` for this one file, or running from `script/`, avoids the guard
without changing it. The suite covers the estimator (including `n = 0`, `k = 1`,
`k = n`, and `n - c < k`), harvest/report computation, and one end-to-end smoke
test that spawns a fake engine executable, writes JSONL, and renders a report.
It needs no credentials and no network.
