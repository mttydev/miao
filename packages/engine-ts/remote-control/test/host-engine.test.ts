import { describe, expect, test } from "bun:test"
import { ControlAgent } from "../src/agent"
import { ControlHostEngine } from "../src/host-engine"

const id = crypto.randomUUID()
const request = (payload: unknown, extra: Partial<ControlAgent.Request> = {}): ControlAgent.Request => ({
  version: 1,
  requestID: id,
  hostID: "host-000000000000",
  runtimeID: "runtime-00000000",
  grantID: "grant-0000000000",
  grantVersion: 1,
  method: "session.list",
  operationID: id,
  payload,
  ...extra,
})
const context = {} as ControlAgent.Context

function fixture(handlers: Record<string, (params: unknown) => unknown> = {}) {
  const calls: Array<{ method: string; params: unknown }> = []
  const engine: ControlHostEngine.Engine = {
    request: async (method, params) => {
      calls.push({ method, params })
      const handler = handlers[method]
      if (!handler) throw Object.assign(new Error("missing"), { code: "invalid_request" })
      return handler(params)
    },
    command: async (method) => {
      calls.push({ method, params: undefined })
      const handler = handlers[method]
      if (!handler) throw Object.assign(new Error("missing"), { code: "invalid_request" })
      return handler(undefined)
    },
    admit: async (input, resume, attachments) => {
      calls.push({ method: "admit", params: { input, resume, attachments } })
      return { input_id: input.input_id, admitted_seq: 4, duplicate: false, pending: true }
    },
  }
  return { engine, calls, methods: ControlHostEngine.methods(engine) }
}

const rows = [
  { session_id: "one", title: "First", location: "p1", updated_at_ms: 1000, pinned_at_ms: 500, preview: { text: "alpha" } },
  { session_id: "two", title: "Second", location: "p2", updated_at_ms: 2000, archived_at_ms: 1500 },
  { session_id: "three", title: "Third", location: "p1", parent_session_id: "one" },
]

describe("ControlHostEngine session reads", () => {
  test("lists catalog rows as Session.Info and scopes by project", async () => {
    const { methods } = fixture({ session_catalog: () => rows })
    const page = (await methods["session.list"]!(request({}, { projectID: "p1" }), context)) as {
      data: Array<Record<string, unknown>>
      cursor: { next?: string }
    }
    expect(page.data.map((row) => row.id)).toEqual(["one", "three"])
    expect(page.data[0]).toMatchObject({ title: "First", projectID: "p1", time: { updated: 1000, pinned: 500 } })
    expect(page.data[1]).toMatchObject({ parentID: "one" })
    expect(page.cursor.next).toBeUndefined()
  })

  test("searches titles and previews and pages with an opaque cursor", async () => {
    const { methods } = fixture({ session_catalog: () => rows })
    const first = (await methods["session.list"]!(request({ limit: 2 }), context)) as {
      data: Array<{ id: string }>
      cursor: { next?: string }
    }
    expect(first.data.map((row) => row.id)).toEqual(["one", "two"])
    expect(first.cursor.next).toBe("2")
    const second = (await methods["session.list"]!(request({ limit: 2, cursor: "2" }), context)) as {
      data: Array<{ id: string }>
    }
    expect(second.data.map((row) => row.id)).toEqual(["three"])
    const search = (await methods["session.list"]!(request({ search: "alpha" }), context)) as {
      data: Array<{ id: string }>
    }
    expect(search.data.map((row) => row.id)).toEqual(["one"])
  })

  test("session.get answers not_found for an unknown Session", async () => {
    const { methods } = fixture({ session_catalog: () => rows })
    await expect(methods["session.get"]!(request({}, { sessionID: "missing" }), context)).rejects.toBeInstanceOf(
      ControlAgent.RequestError,
    )
  })
})

describe("ControlHostEngine events and pending", () => {
  test("projects the snapshot into a page and respects after", async () => {
    const snapshot = { title: "One", messages: [{ seq: 1, role: "user", content: [{ type: "text", text: "hi" }] }] }
    const { methods } = fixture({ snapshot: () => snapshot })
    const page = (await methods["session.events"]!(request({ after: 0, limit: 100 }), context)) as {
      data: Array<{ type: string }>
      cursor: number
      hasMore: boolean
    }
    expect(page.data.map((event) => event.type)).toEqual(["session.next.info.updated", "session.next.prompted"])
    expect(page.cursor).toBe(2)
    expect(page.hasMore).toBe(false)
    const after = (await methods["session.events"]!(request({ after: 2 }), context)) as { data: unknown[] }
    expect(after.data).toEqual([])
  })

  test("maps the snapshot's waiting work onto the pending shape", async () => {
    const { methods } = fixture({
      snapshot: () => ({
        active_run: "run-1",
        approvals: [{ id: "approval-1", action: "bash", resources: ["ls"] }],
        questions: [{ id: "question-1" }],
        pending: [{ input_id: "input-1", delivery: "steer" }],
      }),
    })
    const pending = (await methods["session.pending"]!(request({}, { sessionID: "one" }), context)) as Record<string, unknown>
    expect(pending).toEqual({
      execution: { type: "running", executionID: "run-1" },
      permissions: [{ id: "approval-1", action: "bash", resources: ["ls"] }],
      questions: [{ id: "question-1" }],
      inputs: { data: [{ input_id: "input-1", delivery: "steer", id: "input-1" }] },
    })
  })
})

describe("ControlHostEngine writes", () => {
  test("prompt admits under a stable input id and keeps attachments", async () => {
    const { calls, methods } = fixture({})
    const data = Buffer.from("hello").toString("base64")
    const result = await methods["session.prompt"]!(
      request({ text: "hi", attachments: [{ mimeType: "text/plain", name: "a.txt", data }] }, { sessionID: "one" }),
      context,
    )
    expect(result).toMatchObject({ status: "accepted", sessionID: "one", admittedSeq: 4 })
    const admission = calls.find((call) => call.method === "admit")?.params as {
      input: { session_id: string; input_id: string; prompt: string; delivery: string }
      attachments: Array<{ mime: string; data: string; metadata?: Record<string, string> }>
    }
    expect(admission.input).toMatchObject({ session_id: "one", prompt: "hi", delivery: "steer" })
    expect(admission.input.input_id).toBe(admission.input.input_id.toLowerCase())
    expect(admission.attachments).toEqual([{ mime: "text/plain", data, metadata: { name: "a.txt" } }])
  })

  test("rename, pin, archive and remove reach their engine commands", async () => {
    const { calls, methods } = fixture({
      rename: () => ({ accepted: true }),
      pin: () => ({ accepted: true }),
      archive: () => ({ accepted: true }),
      remove: () => ({ accepted: true }),
    })
    await methods["session.rename"]!(request({ title: "New" }, { sessionID: "one" }), context)
    await methods["session.pin"]!(request({ pinned: true }, { sessionID: "one" }), context)
    await methods["session.archive"]!(request({ archived: true }, { sessionID: "one" }), context)
    await methods["session.remove"]!(request({}, { sessionID: "one" }), context)
    expect(calls.map((call) => call.method)).toEqual(["rename", "pin", "archive", "remove"])
    expect(calls[0]?.params).toEqual({ session_id: "one", title: "New" })
  })

  test("interrupt pins the observed run so a new run cannot be cancelled", async () => {
    const { calls, methods } = fixture({ cancel_if: () => ({ accepted: true }) })
    expect(await methods["session.interrupt"]!(request({ executionID: "run-1" }, { sessionID: "one" }), context)).toEqual({
      status: "completed",
    })
    expect(calls[0]).toEqual({ method: "cancel_if", params: { session_id: "one", run_id: "run-1" } })
  })

  test("a rejected interrupt answers rejected instead of claiming success", async () => {
    const { methods } = fixture({ cancel_if: () => ({ accepted: false }) })
    expect(await methods["session.interrupt"]!(request({ executionID: "old" }, { sessionID: "one" }), context)).toEqual({
      status: "rejected",
      code: "conflict",
    })
  })
})
