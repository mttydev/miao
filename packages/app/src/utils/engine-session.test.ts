import { describe, expect, test } from "bun:test"
import {
  EngineSessionShapeError,
  catalogRowToSummary,
  catalogToSummaries,
  snapshotToState,
  type EngineCatalogRow,
} from "./engine-session"

// Captured verbatim from `session_catalog` on `miao serve` built from this tree,
// so the fixtures fail as soon as the engine's row shape changes.
const live: EngineCatalogRow = {
  account_selection: null,
  archived_at_ms: null,
  location: "/private/tmp/ws2",
  model_selection: null,
  parent_session_id: null,
  pinned_at_ms: null,
  preview: null,
  session_id: "ses_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  title: "Catalog probe",
  updated_at_ms: 1794232836295,
}

const older: EngineCatalogRow = {
  session_id: "ses_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  title: "Older",
  updated_at_ms: 1794232800000,
}

const archived: EngineCatalogRow = {
  session_id: "ses_cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
  title: "Archived",
  updated_at_ms: 1794232900000,
  archived_at_ms: 1794232900001,
}

describe("catalogRowToSummary", () => {
  test("maps a live engine row without losing identity or timing", () => {
    expect(catalogRowToSummary(live)).toEqual({
      id: "ses_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      title: "Catalog probe",
      updatedAt: 1794232836295,
      pinned: false,
      archived: false,
    })
  })

  test("carries lineage, preview and pin state when the engine has them", () => {
    const row: EngineCatalogRow = {
      ...live,
      parent_session_id: "ses_parent",
      pinned_at_ms: 1794232836300,
      preview: "last message",
    }
    expect(catalogRowToSummary(row)).toMatchObject({
      parentID: "ses_parent",
      pinned: true,
      preview: "last message",
    })
  })

  test("treats a session without timing as the oldest, not as an invalid row", () => {
    expect(catalogRowToSummary({ session_id: "s", title: "t" }).updatedAt).toBe(0)
  })

  test("fails on a row that has no usable identity or title", () => {
    expect(() => catalogRowToSummary({ title: "t" })).toThrow(EngineSessionShapeError)
    expect(() => catalogRowToSummary({ session_id: "s" })).toThrow(EngineSessionShapeError)
    try {
      catalogRowToSummary({ session_id: 7, title: "t" })
    } catch (error) {
      expect((error as EngineSessionShapeError).field).toBe("session_id")
    }
  })
})

describe("catalogToSummaries", () => {
  test("orders newest first and hides archived rows by default", () => {
    expect(catalogToSummaries([older, archived, live]).map((session) => session.title)).toEqual([
      "Catalog probe",
      "Older",
    ])
  })

  test("keeps archived rows on request", () => {
    expect(catalogToSummaries([older, archived, live], { includeArchived: true }).map((s) => s.title)).toEqual([
      "Archived",
      "Catalog probe",
      "Older",
    ])
  })
})

// Captured verbatim from `snapshot` on the same engine build.
const liveSnapshot = {
  account_selection: null,
  active_run: null,
  agent: null,
  approvals: [],
  archived_at_ms: null,
  context: null,
  created_at_ms: 1794232836295,
  crons: [],
  cursor: 1,
  event_base: 0,
  has_messages: false,
  location: "/private/tmp/ws2",
  message_count: 0,
  messages: [],
  mode: "build",
  model_selection: null,
  parent_session_id: null,
  pending: [],
  pinned_at_ms: null,
  preview: null,
  questions: [],
  reverted_count: 0,
  selection: null,
  session_id: "ses_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  staged_revert: null,
  state: { goal: null, todos: null },
  tag: "",
  title: "Catalog probe",
  transfer_import: null,
  updated_at_ms: 1794232836295,
  wakeups: [],
}

describe("snapshotToState", () => {
  test("reads the header of a live snapshot", () => {
    expect(snapshotToState(liveSnapshot)).toEqual({
      id: "ses_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      title: "Catalog probe",
      cursor: 1,
      messageCount: 0,
      hasMessages: false,
      pending: 0,
      approvals: 0,
      visibleMessages: 0,
      clearedThrough: 0,
    })
  })

  // `session_clear` deletes nothing: it checkpoints the history with an empty
  // summary so the next provider prefix starts fresh. A viewer that attaches at
  // zero replaying the journal re-rendered every cleared turn, and the header
  // kept counting them, so the cut has to come from the snapshot.
  test("follows an explicit clear's cut instead of replaying the cleared turns", () => {
    const state = snapshotToState({
      ...liveSnapshot,
      has_messages: true,
      message_count: 3,
      selection: { through_seq: 4, summary: "", seq: 9, recorded_at_ms: 1794232836295 },
      messages: [
        { seq: 2, role: "user", content: "cleared ask" },
        { seq: 4, role: "assistant", content: [{ type: "text", text: "cleared answer" }] },
        { seq: 6, role: "user", content: "after the clear" },
      ],
    })
    expect(state).toMatchObject({
      clearedThrough: 4,
      visibleMessages: 1,
      messageCount: 3,
      hasMessages: true,
    })
  })

  test("hides nothing behind a summarising checkpoint", () => {
    const state = snapshotToState({
      ...liveSnapshot,
      selection: { through_seq: 4, summary: "earlier work", seq: 9 },
      messages: [
        { seq: 2, role: "user", content: "compacted ask" },
        { seq: 6, role: "user", content: "after it" },
      ],
    })
    expect(state).toMatchObject({ clearedThrough: 0, visibleMessages: 2 })
  })

  test("falls back to the durable count when the snapshot carries no messages", () => {
    const state = snapshotToState({ ...liveSnapshot, messages: undefined, message_count: 7 })
    expect(state.visibleMessages).toBe(7)
  })

  test("counts pending inputs and approvals, and does not invent a cursor", () => {
    expect(
      snapshotToState({ ...liveSnapshot, pending: [{}, {}], approvals: [{}], cursor: undefined }),
    ).toMatchObject({ pending: 2, approvals: 1, cursor: 0 })
  })

  test("fails when the snapshot cannot name its Session", () => {
    expect(() => snapshotToState({ title: "t" })).toThrow(EngineSessionShapeError)
  })
})
