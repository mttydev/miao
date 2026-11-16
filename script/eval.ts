#!/usr/bin/env bun
/**
 * Budgeted repeated-sampling harness for the product engine.
 *
 * Each trial runs one task prompt through `miao run --format json` in a fresh
 * workspace with a fresh engine storage, under a turn and wallclock budget
 * injected through the product's `budget` config section. Every settled
 * provider step carries its own cost and token counts, so the record measures
 * what the engine reported rather than re-pricing a transcript.
 *
 * The record is append-only JSONL, one line per trial, so a run can be
 * interrupted without losing settled trials. `report` reads those lines and
 * computes the unbiased pass@k estimator
 *
 *   pass@k = 1 - C(n - c, k) / C(n, k)
 *
 * from n trials and c passes, plus token, cost, cache, budget, and policy
 * summaries. A trial passes only when the engine exits cleanly, produced at
 * least one provider step, the task's own assertion command exits zero, and
 * every declared policy bound holds.
 *
 * Usage:
 *   bun script/eval.ts run --tasks docs/evals/tasks.example.json --trials 3 \
 *       --budget-turns 40 --budget-ms 600000 [--engine-bin miao] [--model ID] \
 *       [--record docs/evals/2026-11-16-example.jsonl] [--name example]
 *   bun script/eval.ts report --record docs/evals/2026-11-16-example.jsonl \
 *       [--k 1,3] [--run ID] [--out report.md]
 */
import { appendFile, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises"
import os from "node:os"
import path from "node:path"

export type Budget = {
  turns?: number
  wallclock_ms?: number
}

export type Policy = {
  max_turns?: number
  max_wallclock_ms?: number
  max_cost_usd?: number
  max_tokens?: number
  require_cache_read?: boolean
}

export type Task = {
  id: string
  prompt: string
  files?: Record<string, string>
  assert: { command: string; timeout_ms?: number }
  budget?: Budget
  policy?: Policy
  model?: string
}

export type TaskFile = {
  name: string
  tasks: Task[]
}

export type Tokens = {
  input: number
  output: number
  reasoning: number
  cache_read: number
  cache_write: number
}

export type TrialRecord = {
  schema: "miao.eval.trial.v1"
  run: string
  task: string
  trial: number
  pass: boolean
  turns: number
  tokens: Tokens
  cost_usd: number
  cost_unknown_steps: number
  wallclock_ms: number
  exit_code: number | null
  engine_stderr?: string
  assert: { command: string; exit_code: number | null; output?: string } | null
  budget: { turns: number | null; wallclock_ms: number | null }
  budget_hit: boolean
  failures: string[]
  engine: string
  model: string | null
  started_at: string
}

export type TaskSummary = {
  task: string
  n: number
  passes: number
  pass_at: { k: number; value: number | null }[]
  turns_p50: number
  wallclock_p50: number
  tokens: Tokens
  cost_usd: number
  cost_unknown_steps: number
  cache_hit_rate: number | null
  budget_hits: number
  policy_failures: number
  failures: { trial: number; reasons: string[] }[]
}

const recordSchema = "miao.eval.trial.v1"

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value)

const numberOr = (value: unknown) => (typeof value === "number" && Number.isFinite(value) ? value : 0)

const positiveInteger = (name: string, value: unknown) => {
  if (typeof value !== "number" || !Number.isInteger(value) || value < 1)
    throw new Error(`${name} must be a positive integer`)
  return value
}

const nonNegativeNumber = (name: string, value: unknown) => {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0)
    throw new Error(`${name} must be a non-negative number`)
  return value
}

/**
 * The unbiased pass@k estimator. `n` trials contain `c` passes; the chance
 * that a random k-subset contains at least one pass is 1 - C(n-c,k)/C(n,k).
 * Zero trials make the quantity undefined, and k must not exceed n.
 */
export function passAtK(n: number, c: number, k: number): number | null {
  if (!Number.isInteger(n) || n < 0) throw new Error("n must be a non-negative integer")
  if (!Number.isInteger(c) || c < 0 || c > n) throw new Error("c must be between 0 and n")
  if (!Number.isInteger(k) || k < 1) throw new Error("k must be a positive integer")
  if (n === 0) return null
  if (k > n) throw new Error("k must not exceed n")
  if (c === 0) return 0
  if (n - c < k) return 1
  let failure = 1
  for (let i = n - c + 1; i <= n; i += 1) failure *= 1 - k / i
  return Math.min(1, Math.max(0, 1 - failure))
}

/** One settled provider step, or null for a line that is not `step_finish`. */
export function harvestStep(line: string): { cost: number | null; tokens: Tokens } | null {
  let event: unknown
  try {
    event = JSON.parse(line)
  } catch {
    return null
  }
  if (!isRecord(event) || event.type !== "step_finish" || !isRecord(event.part)) return null
  const part = event.part
  const tokens = isRecord(part.tokens) ? part.tokens : {}
  const cache = isRecord(tokens.cache) ? tokens.cache : {}
  return {
    cost: typeof part.cost === "number" && Number.isFinite(part.cost) ? part.cost : null,
    tokens: {
      input: numberOr(tokens.input),
      output: numberOr(tokens.output),
      reasoning: numberOr(tokens.reasoning),
      cache_read: numberOr(cache.read),
      cache_write: numberOr(cache.write),
    },
  }
}

const isTrialRecord = (value: unknown): value is TrialRecord => {
  if (!isRecord(value) || value.schema !== recordSchema) return false
  if (typeof value.run !== "string" || typeof value.task !== "string") return false
  if (typeof value.trial !== "number" || !Number.isInteger(value.trial)) return false
  if (typeof value.pass !== "boolean") return false
  if (typeof value.turns !== "number" || !Number.isInteger(value.turns)) return false
  if (!isRecord(value.tokens)) return false
  const tokens = value.tokens
  if (
    !["input", "output", "reasoning", "cache_read", "cache_write"].every(
      (key) => typeof tokens[key] === "number",
    )
  )
    return false
  if (typeof value.cost_usd !== "number" || typeof value.cost_unknown_steps !== "number") return false
  if (typeof value.wallclock_ms !== "number") return false
  if (value.exit_code !== null && typeof value.exit_code !== "number") return false
  if (value.engine_stderr !== undefined && typeof value.engine_stderr !== "string") return false
  if (!Array.isArray(value.failures) || !value.failures.every((reason) => typeof reason === "string")) return false
  if (!isRecord(value.budget)) return false
  if (value.budget.turns !== null && typeof value.budget.turns !== "number") return false
  if (value.budget.wallclock_ms !== null && typeof value.budget.wallclock_ms !== "number") return false
  if (typeof value.budget_hit !== "boolean") return false
  if (typeof value.engine !== "string" || typeof value.started_at !== "string") return false
  if (value.model !== null && typeof value.model !== "string") return false
  if (value.assert !== null) {
    if (!isRecord(value.assert) || typeof value.assert.command !== "string") return false
    if (value.assert.exit_code !== null && typeof value.assert.exit_code !== "number") return false
    if (value.assert.output !== undefined && typeof value.assert.output !== "string") return false
  }
  return true
}

export function parseRecords(text: string): TrialRecord[] {
  return text
    .split("\n")
    .flatMap((line, index) => {
      if (line.trim() === "") return []
      let value: unknown
      try {
        value = JSON.parse(line)
      } catch {
        throw new Error(`record line ${index + 1} is not JSON`)
      }
      if (!isTrialRecord(value)) throw new Error(`record line ${index + 1} is not a ${recordSchema} record`)
      return [value]
    })
}

const median = (values: number[]) => {
  if (values.length === 0) return 0
  const sorted = [...values].sort((a, b) => a - b)
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2 === 0
    ? ((sorted[middle - 1] ?? 0) + (sorted[middle] ?? 0)) / 2
    : (sorted[middle] ?? 0)
}

const sumTokens = (records: TrialRecord[]): Tokens =>
  records.reduce(
    (total, record) => ({
      input: total.input + record.tokens.input,
      output: total.output + record.tokens.output,
      reasoning: total.reasoning + record.tokens.reasoning,
      cache_read: total.cache_read + record.tokens.cache_read,
      cache_write: total.cache_write + record.tokens.cache_write,
    }),
    { input: 0, output: 0, reasoning: 0, cache_read: 0, cache_write: 0 },
  )

export function summarize(records: TrialRecord[], ks: number[]): TaskSummary[] {
  const tasks = [...new Set(records.map((record) => record.task))]
  return tasks.map((task) => {
    const group = records.filter((record) => record.task === task)
    const passes = group.filter((record) => record.pass).length
    const tokens = sumTokens(group)
    const input = tokens.input + tokens.cache_read
    return {
      task,
      n: group.length,
      passes,
      pass_at: ks.map((k) => ({ k, value: k <= group.length ? passAtK(group.length, passes, k) : null })),
      turns_p50: median(group.map((record) => record.turns)),
      wallclock_p50: median(group.map((record) => record.wallclock_ms)),
      tokens,
      cost_usd: group.reduce((total, record) => total + record.cost_usd, 0),
      cost_unknown_steps: group.reduce((total, record) => total + record.cost_unknown_steps, 0),
      cache_hit_rate: input > 0 ? tokens.cache_read / input : null,
      budget_hits: group.filter((record) => record.budget_hit).length,
      policy_failures: group.reduce(
        (total, record) => total + record.failures.filter((reason) => reason.startsWith("policy:")).length,
        0,
      ),
      failures: group
        .filter((record) => record.failures.length > 0)
        .map((record) => ({ trial: record.trial, reasons: record.failures })),
    }
  })
}

const percent = (value: number | null) => (value === null ? "—" : `${(value * 100).toFixed(1)}%`)

const count = (value: number) => value.toLocaleString("en-US")

const money = (value: number) => `$${value.toFixed(6)}`

export function renderReport(records: TrialRecord[], ks: number[], recordPath: string): string {
  if (records.length === 0) throw new Error("record has no trials")
  const summaries = summarize(records, ks)
  const runs = [...new Set(records.map((record) => record.run))]
  const total = sumTokens(records)
  const totalInput = total.input + total.cache_read
  const totalPasses = summaries.reduce((sum, summary) => sum + summary.passes, 0)
  const lines: string[] = []
  lines.push(`# Eval report — ${path.basename(recordPath)}`)
  lines.push("")
  lines.push(`- record: \`${recordPath}\``)
  lines.push(`- runs: ${runs.map((run) => `\`${run}\``).join(", ")}`)
  lines.push(`- trials: ${records.length} (${summaries.length} tasks)`)
  lines.push(`- pass@k: unbiased estimator \`1 - C(n-c,k)/C(n,k)\`; k > n is shown as \`—\`.`)
  lines.push(
    "- cost: sum of `step_finish` cost (engine usage); `unknown` steps carry no provider price.",
  )
  lines.push("- cache hit rate: `cache_read / (input + cache_read)`.")
  lines.push("")
  const headers = [
    "task",
    "n",
    "pass",
    ...ks.map((k) => `pass@${k}`),
    "turns p50",
    "input",
    "output",
    "reasoning",
    "cache read",
    "cache write",
    "$",
    "cache hit",
    "budget hits",
    "policy fails",
  ]
  lines.push(`| ${headers.join(" | ")} |`)
  lines.push(`| ${headers.map(() => "---").join(" | ")} |`)
  for (const summary of summaries) {
    lines.push(
      `| ${[
        summary.task,
        summary.n,
        summary.passes,
        ...summary.pass_at.map((at) => percent(at.value)),
        count(summary.turns_p50),
        count(summary.tokens.input),
        count(summary.tokens.output),
        count(summary.tokens.reasoning),
        count(summary.tokens.cache_read),
        count(summary.tokens.cache_write),
        money(summary.cost_usd),
        percent(summary.cache_hit_rate),
        summary.budget_hits,
        summary.policy_failures,
      ].join(" | ")} |`,
    )
  }
  lines.push(
    `| **total** | ${[
      records.length,
      totalPasses,
      ...ks.map((k) =>
        percent(k <= records.length ? passAtK(records.length, totalPasses, k) : null),
      ),
      count(median(records.map((record) => record.turns))),
      count(total.input),
      count(total.output),
      count(total.reasoning),
      count(total.cache_read),
      count(total.cache_write),
      money(records.reduce((sum, record) => sum + record.cost_usd, 0)),
      percent(totalInput > 0 ? total.cache_read / totalInput : null),
      records.filter((record) => record.budget_hit).length,
      records.reduce(
        (sum, record) => sum + record.failures.filter((reason) => reason.startsWith("policy:")).length,
        0,
      ),
    ].join(" | ")} |`,
  )
  const unpriced = records.reduce((sum, record) => sum + record.cost_unknown_steps, 0)
  if (unpriced > 0)
    lines.push(
      "",
      `${unpriced} provider step(s) carried no price, so the $ figures are a lower bound rather than a complete bill.`,
    )
  lines.push("", "## Failures", "")
  const failures = summaries.flatMap((summary) =>
    summary.failures.map((failure) => `- ${summary.task} trial ${failure.trial}: ${failure.reasons.join(", ")}`),
  )
  lines.push(...(failures.length === 0 ? ["- none"] : failures))
  return `${lines.join("\n")}\n`
}

const slug = (value: string) =>
  value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 60) || "eval"

const parseTaskFile = (value: unknown, origin: string): TaskFile => {
  if (!isRecord(value)) throw new Error(`${origin}: task file must be an object`)
  if (typeof value.name !== "string" || value.name.trim() === "")
    throw new Error(`${origin}: name must be a non-empty string`)
  if (!Array.isArray(value.tasks) || value.tasks.length === 0)
    throw new Error(`${origin}: tasks must be a non-empty array`)
  const ids = new Set<string>()
  const tasks = value.tasks.map((task, index) => {
    const at = `${origin}: tasks[${index}]`
    if (!isRecord(task)) throw new Error(`${at} must be an object`)
    if (typeof task.id !== "string" || task.id.trim() === "") throw new Error(`${at}.id must be a non-empty string`)
    if (ids.has(task.id)) throw new Error(`${at}.id duplicates ${task.id}`)
    ids.add(task.id)
    if (typeof task.prompt !== "string" || task.prompt.trim() === "")
      throw new Error(`${at}.prompt must be a non-empty string`)
    if (!isRecord(task.assert) || typeof task.assert.command !== "string" || task.assert.command.trim() === "")
      throw new Error(`${at}.assert.command must be a non-empty string`)
    const assert = {
      command: task.assert.command,
      ...(task.assert.timeout_ms === undefined
        ? {}
        : { timeout_ms: positiveInteger(`${at}.assert.timeout_ms`, task.assert.timeout_ms) }),
    }
    const files = parseFiles(task.files, `${at}.files`)
    const budget = task.budget === undefined ? undefined : parseBudget(task.budget, `${at}.budget`)
    const policy = task.policy === undefined ? undefined : parsePolicy(task.policy, `${at}.policy`)
    return {
      id: task.id,
      prompt: task.prompt,
      ...(files === undefined ? {} : { files }),
      assert,
      ...(budget === undefined ? {} : { budget }),
      ...(policy === undefined ? {} : { policy }),
      ...(typeof task.model === "string" ? { model: task.model } : {}),
    }
  })
  return { name: value.name, tasks }
}

const parseFiles = (value: unknown, at: string) => {
  if (value === undefined) return undefined
  if (!isRecord(value)) throw new Error(`${at} must be an object`)
  return Object.fromEntries(
    Object.entries(value).map(([name, content]) => {
      if (typeof content !== "string") throw new Error(`${at}.${name} must be a string`)
      const root = path.resolve(os.tmpdir(), "miao-eval-workspace")
      const relative = path.relative(root, path.resolve(root, name))
      if (path.isAbsolute(name) || relative.startsWith("..") || path.isAbsolute(relative))
        throw new Error(`${at}.${name} must stay inside the trial workspace`)
      return [name, content]
    }),
  )
}

const parseBudget = (value: unknown, at: string): Budget => {
  if (!isRecord(value)) throw new Error(`${at} must be an object`)
  const turns = value.turns === undefined ? undefined : positiveInteger(`${at}.turns`, value.turns)
  const wallclock_ms =
    value.wallclock_ms === undefined ? undefined : positiveInteger(`${at}.wallclock_ms`, value.wallclock_ms)
  if (turns === undefined && wallclock_ms === undefined)
    throw new Error(`${at} must state turns or wallclock_ms`)
  return { ...(turns === undefined ? {} : { turns }), ...(wallclock_ms === undefined ? {} : { wallclock_ms }) }
}

const parsePolicy = (value: unknown, at: string): Policy => {
  if (!isRecord(value)) throw new Error(`${at} must be an object`)
  const policy: Policy = {}
  if (value.max_turns !== undefined) policy.max_turns = nonNegativeNumber(`${at}.max_turns`, value.max_turns)
  if (value.max_wallclock_ms !== undefined)
    policy.max_wallclock_ms = nonNegativeNumber(`${at}.max_wallclock_ms`, value.max_wallclock_ms)
  if (value.max_cost_usd !== undefined) policy.max_cost_usd = nonNegativeNumber(`${at}.max_cost_usd`, value.max_cost_usd)
  if (value.max_tokens !== undefined) policy.max_tokens = nonNegativeNumber(`${at}.max_tokens`, value.max_tokens)
  if (value.require_cache_read !== undefined) {
    if (typeof value.require_cache_read !== "boolean")
      throw new Error(`${at}.require_cache_read must be a boolean`)
    policy.require_cache_read = value.require_cache_read
  }
  return policy
}

const effectiveBudget = (task: Task, cli: Budget): Budget => {
  const turns = task.budget?.turns === undefined ? cli.turns : Math.min(task.budget.turns, cli.turns ?? Infinity)
  const wallclock_ms =
    task.budget?.wallclock_ms === undefined
      ? cli.wallclock_ms
      : Math.min(task.budget.wallclock_ms, cli.wallclock_ms ?? Infinity)
  if (turns === undefined && wallclock_ms === undefined)
    throw new Error(`task ${task.id} has no turn or wallclock budget`)
  return {
    ...(turns === undefined ? {} : { turns }),
    ...(wallclock_ms === undefined ? {} : { wallclock_ms }),
  }
}

const budgetConfig = (budget: Budget) => {
  const existing = process.env.MIAO_CONFIG_CONTENT
  if (existing === undefined || existing.trim() === "") return JSON.stringify({ budget })
  const parsed = Bun.JSONC.parse(existing)
  if (!isRecord(parsed)) throw new Error("MIAO_CONFIG_CONTENT must contain a JSON object")
  const prior = parsed.budget === undefined ? {} : parsed.budget
  if (!isRecord(prior)) throw new Error("MIAO_CONFIG_CONTENT.budget must be an object")
  return JSON.stringify({ ...parsed, budget: { ...prior, ...budget } })
}

const writeFixture = async (workspace: string, files: Record<string, string>) => {
  for (const [name, content] of Object.entries(files)) {
    const destination = path.join(workspace, name)
    await mkdir(path.dirname(destination), { recursive: true })
    await writeFile(destination, content)
  }
}

const readLines = async (stream: ReadableStream<Uint8Array>, onLine: (line: string) => void) => {
  const reader = stream.getReader()
  const decoder = new TextDecoder()
  let pending = ""
  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    pending += decoder.decode(value, { stream: true })
    let index = pending.indexOf("\n")
    while (index >= 0) {
      onLine(pending.slice(0, index).trim())
      pending = pending.slice(index + 1)
      index = pending.indexOf("\n")
    }
  }
  pending += decoder.decode()
  if (pending.trim() !== "") onLine(pending.trim())
}

const shellArgv = (command: string) =>
  process.platform === "win32" ? ["cmd.exe", "/c", command] : ["sh", "-c", command]

const runAssertion = async (task: Task, workspace: string) => {
  const child = Bun.spawn(shellArgv(task.assert.command), {
    cwd: workspace,
    stdout: "pipe",
    stderr: "pipe",
  })
  let timedOut = false
  const timer = setTimeout(() => {
    timedOut = true
    child.kill(9)
  }, task.assert.timeout_ms ?? 120_000)
  const [exit, stdout, stderr] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ])
  clearTimeout(timer)
  const output = `${stdout}\n${stderr}`.trim().slice(0, 1000)
  return {
    code: timedOut ? null : exit,
    timedOut,
    ...(output === "" ? {} : { output }),
  }
}

const runTrial = async (
  task: Task,
  trial: number,
  options: {
    engine: string
    model: string | null
    cliBudget: Budget
    timeoutMs: number | null
    run: string
  },
): Promise<TrialRecord> => {
  const workspace = await mkdtemp(path.join(os.tmpdir(), `miao-eval-${slug(task.id)}-`))
  const budget = effectiveBudget(task, options.cliBudget)
  const startedAt = new Date().toISOString()
  const started = Date.now()
  const tokens: Tokens = { input: 0, output: 0, reasoning: 0, cache_read: 0, cache_write: 0 }
  let turns = 0
  let cost = 0
  let unknown = 0
  let exitCode: number | null = null
  let timedOut = false
  let engineStderr = ""
  try {
    await writeFixture(workspace, task.files ?? {})
    const child = Bun.spawn(
      [
        options.engine,
        "run",
        "--format",
        "json",
        ...(options.model === null ? [] : ["--model", options.model]),
        task.prompt,
      ],
      {
        cwd: workspace,
        stdout: "pipe",
        stderr: "pipe",
        env: {
          ...process.env,
          PWD: workspace,
          MIAO_ENGINE_STORAGE: path.join(workspace, ".eval-storage"),
          MIAO_CONFIG_CONTENT: budgetConfig(budget),
        },
      },
    )
    const timeoutMs =
      options.timeoutMs ??
      (budget.wallclock_ms === undefined ? 300_000 : budget.wallclock_ms + 30_000)
    const timer = setTimeout(() => {
      timedOut = true
      child.kill(9)
    }, timeoutMs)
    const [, stderr, exit] = await Promise.all([
      readLines(child.stdout, (line) => {
        const step = harvestStep(line)
        if (step === null) return
        turns += 1
        if (step.cost === null) unknown += 1
        else cost += step.cost
        tokens.input += step.tokens.input
        tokens.output += step.tokens.output
        tokens.reasoning += step.tokens.reasoning
        tokens.cache_read += step.tokens.cache_read
        tokens.cache_write += step.tokens.cache_write
      }),
      new Response(child.stderr).text(),
      child.exited,
    ])
    clearTimeout(timer)
    exitCode = timedOut ? null : exit
    engineStderr = stderr.trim().slice(0, 1000)
    const wallclock = Date.now() - started
    const failures: string[] = []
    const assertion = await runAssertion(task, workspace)
    if (timedOut) failures.push("timeout")
    if (exitCode !== 0) failures.push(`run_exit:${exitCode ?? "signal"}`)
    if (turns === 0) failures.push("no_provider_step")
    if (assertion.timedOut) failures.push("assert_timeout")
    if (!assertion.timedOut && assertion.code !== 0)
      failures.push(`assert_exit:${assertion.code ?? "signal"}`)
    const totalTokens =
      tokens.input + tokens.output + tokens.reasoning + tokens.cache_read + tokens.cache_write
    if (task.policy?.max_turns !== undefined && turns > task.policy.max_turns)
      failures.push(`policy:max_turns ${turns} > ${task.policy.max_turns}`)
    if (task.policy?.max_wallclock_ms !== undefined && wallclock > task.policy.max_wallclock_ms)
      failures.push(`policy:max_wallclock_ms ${wallclock} > ${task.policy.max_wallclock_ms}`)
    if (task.policy?.max_cost_usd !== undefined && cost > task.policy.max_cost_usd)
      failures.push(`policy:max_cost_usd ${cost.toFixed(6)} > ${task.policy.max_cost_usd}`)
    if (task.policy?.max_tokens !== undefined && totalTokens > task.policy.max_tokens)
      failures.push(`policy:max_tokens ${totalTokens} > ${task.policy.max_tokens}`)
    if (task.policy?.require_cache_read === true && tokens.cache_read === 0)
      failures.push("policy:require_cache_read")
    const pass = failures.length === 0
    const budgetHit =
      (budget.turns !== undefined && turns >= budget.turns) ||
      (budget.wallclock_ms !== undefined && wallclock >= budget.wallclock_ms)
    if (!pass && budgetHit && budget.turns !== undefined && turns >= budget.turns)
      failures.push(`budget:turns ${turns}/${budget.turns}`)
    if (!pass && budgetHit && budget.wallclock_ms !== undefined && wallclock >= budget.wallclock_ms)
      failures.push(`budget:wallclock_ms ${wallclock}/${budget.wallclock_ms}`)
    return {
      schema: recordSchema,
      run: options.run,
      task: task.id,
      trial,
      pass,
      turns,
      tokens,
      cost_usd: cost,
      cost_unknown_steps: unknown,
      wallclock_ms: wallclock,
      exit_code: exitCode,
      ...(engineStderr === "" ? {} : { engine_stderr: engineStderr }),
      assert: {
        command: task.assert.command,
        exit_code: assertion.code,
        ...(assertion.output === undefined || (assertion.code === 0 && !assertion.timedOut)
          ? {}
          : { output: assertion.output }),
      },
      budget: {
        turns: budget.turns ?? null,
        wallclock_ms: budget.wallclock_ms ?? null,
      },
      budget_hit: budgetHit,
      failures,
      engine: options.engine,
      model: options.model,
      started_at: startedAt,
    }
  } finally {
    await rm(workspace, { recursive: true, force: true })
  }
}

const loadTaskFile = async (file: string) => {
  const raw = (await Bun.file(file).json()) as unknown
  return parseTaskFile(raw, file)
}

const option = (args: string[], name: string) => {
  const index = args.indexOf(`--${name}`)
  return index >= 0 ? args[index + 1] : undefined
}

const requiredOption = (args: string[], name: string) => {
  const value = option(args, name)
  if (value === undefined || value === "") throw new Error(`--${name} is required`)
  return value
}

const integerOption = (args: string[], name: string, fallback?: number) => {
  const value = option(args, name)
  if (value === undefined) return fallback
  const parsed = Number(value)
  if (!Number.isInteger(parsed) || parsed < 1) throw new Error(`--${name} must be a positive integer`)
  return parsed
}

const listOption = (args: string[], name: string, fallback: number[]) => {
  const value = option(args, name)
  if (value === undefined) return fallback
  const parsed = value.split(",").map((item) => Number(item.trim()))
  if (parsed.length === 0 || parsed.some((item) => !Number.isInteger(item) || item < 1))
    throw new Error(`--${name} must be a comma-separated list of positive integers`)
  return parsed
}

const runCommand = async (args: string[]) => {
  const tasksPath = requiredOption(args, "tasks")
  const trials = integerOption(args, "trials", 1) ?? 1
  const cliBudget: Budget = {
    ...(option(args, "budget-turns") === undefined
      ? {}
      : { turns: integerOption(args, "budget-turns") }),
    ...(option(args, "budget-ms") === undefined ? {} : { wallclock_ms: integerOption(args, "budget-ms") }),
  }
  if (cliBudget.turns === undefined && cliBudget.wallclock_ms === undefined)
    throw new Error("--budget-turns or --budget-ms is required")
  const file = await loadTaskFile(tasksPath)
  const engine = option(args, "engine-bin") ?? process.env.MIAO_ENGINE_BIN ?? "miao"
  const model = option(args, "model") ?? null
  const name = option(args, "name") ?? file.name
  const date = new Date().toISOString().slice(0, 10)
  const record = option(args, "record") ?? path.join("docs", "evals", `${date}-${slug(name)}.jsonl`)
  const run = `${date}T${new Date().toISOString().slice(11, 19).replaceAll(":", "")}-${slug(name)}`
  const timeoutMs = integerOption(args, "timeout-ms")
  await mkdir(path.dirname(path.resolve(record)), { recursive: true })
  console.log(`eval run ${run}`)
  console.log(`tasks ${file.tasks.length} · trials ${trials} · record ${record}`)
  let passed = 0
  let total = 0
  for (const task of file.tasks) {
    for (let trial = 0; trial < trials; trial += 1) {
      total += 1
      const recordLine = await runTrial(task, trial, {
        engine,
        model: task.model ?? model,
        cliBudget,
        timeoutMs: timeoutMs ?? null,
        run,
      })
      passed += recordLine.pass ? 1 : 0
      await appendFile(record, `${JSON.stringify(recordLine)}\n`)
      const outcome = recordLine.pass ? "pass" : `fail (${recordLine.failures.join(", ")})`
      console.log(
        `[${total}/${file.tasks.length * trials}] ${task.id} trial ${trial + 1}/${trials}: ${outcome} · ${recordLine.turns}t · ${money(recordLine.cost_usd)} · ${(recordLine.wallclock_ms / 1000).toFixed(1)}s`,
      )
    }
  }
  console.log(`recorded ${total} trials, ${passed} passed: ${record}`)
  console.log(`report: bun script/eval.ts report --record ${record}`)
}

const reportCommand = async (args: string[]) => {
  const recordPath = requiredOption(args, "record")
  const ks = listOption(args, "k", [1])
  const run = option(args, "run")
  const text = await Bun.file(recordPath).text()
  const records = parseRecords(text)
  const selected = run === undefined ? records : records.filter((record) => record.run === run)
  if (selected.length === 0) throw new Error(`record has no trials for run ${run}`)
  const report = renderReport(selected, ks, recordPath)
  const out = option(args, "out")
  if (out === undefined) process.stdout.write(report)
  else {
    await mkdir(path.dirname(path.resolve(out)), { recursive: true })
    await writeFile(out, report)
    console.log(`report: ${out}`)
  }
}

const main = async (args: string[]) => {
  const [command, ...rest] = args
  if (command === "run") return runCommand(rest)
  if (command === "report") return reportCommand(rest)
  throw new Error("usage: bun script/eval.ts run ... | report ...")
}

if (import.meta.main) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(`eval: ${error instanceof Error ? error.message : String(error)}`)
    process.exitCode = 1
  })
}
