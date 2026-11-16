import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import path from "node:path"
import { expect, test } from "bun:test"
import { harvestStep, parseRecords, passAtK, renderReport, summarize, type TrialRecord } from "./eval"

const trialDefaults: Omit<TrialRecord, "task" | "trial" | "pass"> = {
  schema: "miao.eval.trial.v1",
  run: "2026-11-16T000000-smoke",
  turns: 2,
  tokens: { input: 100, output: 10, reasoning: 0, cache_read: 50, cache_write: 0 },
  cost_usd: 0.01,
  cost_unknown_steps: 0,
  wallclock_ms: 1000,
  exit_code: 0,
  assert: { command: "true", exit_code: 0 },
  budget: { turns: 5, wallclock_ms: 10_000 },
  budget_hit: false,
  failures: [],
  engine: "fake",
  model: null,
  started_at: "2026-11-16T00:00:00.000Z",
}

const trial = (
  overrides: Pick<TrialRecord, "task" | "trial" | "pass"> & Partial<TrialRecord>,
): TrialRecord => ({
  ...trialDefaults,
  ...overrides,
})

test("pass@1 is the pass rate", () => {
  expect(passAtK(10, 3, 1)).toBeCloseTo(0.3, 12)
  expect(passAtK(1, 1, 1)).toBe(1)
  expect(passAtK(1, 0, 1)).toBe(0)
})

test("pass@k equals 1 when every trial is needed", () => {
  expect(passAtK(5, 2, 5)).toBe(1)
  expect(passAtK(5, 5, 5)).toBe(1)
  expect(passAtK(5, 0, 5)).toBe(0)
})

test("the n - c < k boundary is exactly one", () => {
  expect(passAtK(4, 3, 2)).toBe(1)
  expect(passAtK(3, 1, 3)).toBe(1)
})

test("zero trials make pass@k undefined", () => {
  expect(passAtK(0, 0, 1)).toBeNull()
})

test("known unbiased values match the combinatorial definition", () => {
  expect(passAtK(4, 2, 2)).toBeCloseTo(5 / 6, 12)
  expect(passAtK(3, 1, 2)).toBeCloseTo(2 / 3, 12)
})

test("rejects k > n and impossible counts", () => {
  expect(() => passAtK(2, 1, 3)).toThrow("k must not exceed n")
  expect(() => passAtK(2, 3, 1)).toThrow("c must be between 0 and n")
  expect(() => passAtK(-1, 0, 1)).toThrow("n must be a non-negative integer")
  expect(() => passAtK(2, 1, 0)).toThrow("k must be a positive integer")
})

test("harvests only settled provider steps", () => {
  const line = JSON.stringify({
    type: "step_finish",
    part: { cost: 0.5, tokens: { input: 7, output: 3, reasoning: 1, cache: { read: 4 } } },
  })
  expect(harvestStep(line)).toEqual({
    cost: 0.5,
    tokens: { input: 7, output: 3, reasoning: 1, cache_read: 4, cache_write: 0 },
  })
  expect(harvestStep(JSON.stringify({ type: "text", part: { text: "hi" } }))).toBeNull()
  expect(harvestStep("not json")).toBeNull()
})

test("summarizes pass@k, spend, cache, budget, and policy failures", () => {
  const records = [
    trial({ task: "a", trial: 0, pass: true }),
    trial({ task: "a", trial: 1, pass: true, tokens: { input: 200, output: 20, reasoning: 0, cache_read: 100, cache_write: 0 } }),
    trial({ task: "a", trial: 2, pass: false, budget_hit: true, failures: ["policy:max_turns 3 > 2"] }),
    trial({ task: "b", trial: 0, pass: false, failures: ["assert_exit:1"] }),
  ]
  const summaries = summarize(records, [1, 2])
  const a = summaries[0]!
  expect(a.task).toBe("a")
  expect(a.n).toBe(3)
  expect(a.passes).toBe(2)
  expect(a.pass_at[0]!.value).toBeCloseTo(2 / 3, 12)
  expect(a.pass_at[1]!.value).toBe(1)
  expect(a.tokens).toEqual({ input: 400, output: 40, reasoning: 0, cache_read: 200, cache_write: 0 })
  expect(a.cost_usd).toBeCloseTo(0.03, 12)
  expect(a.cache_hit_rate).toBeCloseTo(1 / 3, 12)
  expect(a.budget_hits).toBe(1)
  expect(a.policy_failures).toBe(1)
  expect(a.failures).toEqual([{ trial: 2, reasons: ["policy:max_turns 3 > 2"] }])
})

test("renders pass@k and every failure in the report", () => {
  const records = [
    trial({ task: "a", trial: 0, pass: true }),
    trial({ task: "a", trial: 1, pass: true }),
    trial({ task: "b", trial: 0, pass: false, failures: ["policy:require_cache_read"] }),
  ]
  const report = renderReport(records, [1, 2], "record.jsonl")
  expect(report).toContain("pass@2")
  expect(report).toContain("100.0%")
  expect(report).toContain("0.0%")
  expect(report).toContain("- b trial 0: policy:require_cache_read")
})

test("run writes JSONL trials and report consumes them through a fake engine", () => {
  const directory = mkdtempSync(path.join(tmpdir(), "miao-eval-test-"))
  try {
    const engine = path.join(directory, "fake-engine")
    writeFileSync(
      engine,
      `#!/usr/bin/env bun
import { writeFileSync } from "node:fs"

const config = JSON.parse(process.env.MIAO_CONFIG_CONTENT ?? "{}")
if (!config.budget?.turns || !config.budget?.wallclock_ms) {
  console.error("budget was not injected")
  process.exit(9)
}
const prompt = process.argv.at(-1) ?? ""
const emit = (cost, input, output, read) =>
  console.log(JSON.stringify({
    type: "step_finish",
    part: { cost, tokens: { input, output, reasoning: 0, cache: { read } } },
  }))
if (prompt.includes("force failure")) {
  emit(0.002, 100, 10, 50)
  emit(0.001, 100, 10, 50)
  process.exit(2)
}
writeFileSync("done.txt", "done")
emit(0.002, 1000, 100, 800)
emit(0.001, 500, 50, 400)
`,
    )
    chmodSync(engine, 0o755)
    const tasks = path.join(directory, "tasks.json")
    writeFileSync(
      tasks,
      JSON.stringify({
        name: "smoke",
        tasks: [
          {
            id: "pass",
            prompt: "write done.txt",
            assert: { command: "test -f done.txt" },
            budget: { turns: 5, wallclock_ms: 10_000 },
          },
          {
            id: "fail",
            prompt: "force failure",
            assert: { command: "test -f done.txt" },
            budget: { turns: 5, wallclock_ms: 10_000 },
            policy: { max_turns: 1 },
          },
        ],
      }),
    )
    const record = path.join(directory, "out.jsonl")
    const env = { ...process.env }
    delete env.MIAO_CONFIG
    delete env.MIAO_CONFIG_CONTENT
    delete env.MIAO_ENGINE_STORAGE
    const run = Bun.spawnSync(
      [
        process.execPath,
        path.join(import.meta.dir, "eval.ts"),
        "run",
        "--tasks",
        tasks,
        "--trials",
        "2",
        "--budget-turns",
        "5",
        "--budget-ms",
        "10000",
        "--engine-bin",
        engine,
        "--record",
        record,
      ],
      { cwd: directory, env, stdout: "pipe", stderr: "pipe" },
    )
    if (run.exitCode !== 0) throw new Error(`run failed: ${run.stderr.toString()}`)
    const records = parseRecords(readFileSync(record, "utf8"))
    expect(records.length).toBe(4)
    const passing = records.filter((entry) => entry.task === "pass")
    expect(passing.every((entry) => entry.pass)).toBe(true)
    expect(passing[0]!.turns).toBe(2)
    expect(passing[0]!.tokens.cache_read).toBe(1200)
    expect(passing[0]!.tokens.input).toBe(1500)
    expect(passing[0]!.cost_usd).toBeCloseTo(0.003, 12)
    expect(passing[0]!.budget).toEqual({ turns: 5, wallclock_ms: 10_000 })
    const failing = records.filter((entry) => entry.task === "fail")
    expect(failing.every((entry) => !entry.pass)).toBe(true)
    expect(failing[0]!.failures).toContain("policy:max_turns 2 > 1")
    expect(failing[0]!.budget_hit).toBe(false)

    const report = Bun.spawnSync(
      [process.execPath, path.join(import.meta.dir, "eval.ts"), "report", "--record", record, "--k", "1,2"],
      { cwd: directory, env, stdout: "pipe", stderr: "pipe" },
    )
    if (report.exitCode !== 0) throw new Error(`report failed: ${report.stderr.toString()}`)
    const text = report.stdout.toString()
    expect(text).toContain("pass@2")
    expect(text).toContain("100.0%")
    expect(text).toContain("0.0%")
    expect(text).toContain("83.3%")
    expect(text).toContain("policy:max_turns 2 > 1")
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})
