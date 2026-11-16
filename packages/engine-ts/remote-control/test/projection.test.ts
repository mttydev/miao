import { describe, expect, test } from "bun:test"
import { ControlProjection } from "../src/projection"

const session = "session-one"
const at = (page: ControlProjection.History, index: number) => {
  const event = page.data[index]
  if (!event) throw new Error(`missing event ${index}`)
  return event
}
const transcript: ControlProjection.Message[] = [
  { seq: 1, role: "user", content: [{ type: "text", text: "hi" }], recorded_at_ms: 1000 },
  {
    seq: 2,
    role: "assistant",
    content: [
      { type: "text", text: "hello" },
      { type: "tool_use", id: "call_1", name: "bash", input: { command: "ls" } },
    ],
    recorded_at_ms: 2000,
    identity: {
      agent: "build",
      provider: { model: "gpt" },
      tokens: { input: 1, output: 2 },
      cost: 0.01,
      ttft: 120,
      finish: "tool_use",
    },
  },
  {
    seq: 3,
    role: "user",
    content: [{ type: "tool_result", tool_use_id: "call_1", content: '{"output":"ok"}', is_error: false }],
    recorded_at_ms: 3000,
  },
  { seq: 4, role: "assistant", content: [{ type: "text", text: "done" }], recorded_at_ms: 4000 },
]

describe("ControlProjection.history", () => {
  test("publishes the engine transcript as the protocol's session.next vocabulary", () => {
    const page = ControlProjection.history(session, transcript)
    expect(page.data.map((event) => event.type)).toEqual([
      "session.next.prompted",
      "session.next.step.started",
      "session.next.text.started",
      "session.next.text.ended",
      "session.next.tool.input.started",
      "session.next.tool.input.ended",
      "session.next.tool.called",
      "session.next.step.ended",
      "session.next.tool.success",
      "session.next.step.started",
      "session.next.text.started",
      "session.next.text.ended",
      "session.next.step.ended",
    ])
    expect(page.cursor).toBe(page.data.length)
    expect(page.hasMore).toBe(false)
  })

  test("every event carries the durable identity the client validates", () => {
    const page = ControlProjection.history(session, transcript)
    page.data.forEach((event, index) => {
      expect(event.durable).toEqual({ seq: index + 1, aggregateID: session, version: 1 })
      expect(event.data.sessionID).toBe(session)
    })
  })

  test("keeps the prompt on the user message and the model on the step", () => {
    const page = ControlProjection.history(session, transcript)
    expect(at(page, 0).data).toMatchObject({
      messageID: "msg_1",
      prompt: { text: "hi", content: [{ type: "text", text: "hi" }] },
    })
    expect(at(page, 1).data).toMatchObject({ assistantMessageID: "msg_2", agent: "build", model: "gpt" })
    expect(at(page, 7).data).toMatchObject({
      assistantMessageID: "msg_2",
      finish: "tool_use",
      tokens: { input: 1, output: 2 },
      cost: 0.01,
      ttft: 120,
    })
  })

  test("pairs a tool result with the assistant that called it", () => {
    const page = ControlProjection.history(session, transcript)
    expect(at(page, 8).data).toMatchObject({
      assistantMessageID: "msg_2",
      callID: "call_1",
      structured: { output: "ok" },
      result: { output: "ok" },
    })
  })

  test("an error result becomes tool.failed", () => {
    const page = ControlProjection.history(session, [
      { seq: 1, role: "assistant", content: [{ type: "tool_use", id: "call_2", name: "bash", input: {} }] },
      { seq: 2, role: "user", content: [{ type: "tool_result", tool_use_id: "call_2", content: "boom", is_error: true }] },
    ])
    const failed = page.data.find((event) => event.type === "session.next.tool.failed")
    expect(failed?.data).toMatchObject({ callID: "call_2", error: "boom" })
  })

  test("a mixed user message stays a prompt instead of a tool result", () => {
    const page = ControlProjection.history(session, [
      { seq: 1, role: "assistant", content: [] },
      { seq: 2, role: "user", content: [{ type: "tool_result", tool_use_id: "call_3", content: "{}" }, { type: "text", text: "next" }] },
    ])
    expect(page.data.some((event) => event.type === "session.next.prompted")).toBe(true)
    expect(page.data.some((event) => event.type === "session.next.tool.success")).toBe(false)
  })

  test("a tool result with no assistant anchor is dropped rather than misattributed", () => {
    const page = ControlProjection.history(session, [
      { seq: 1, role: "user", content: [{ type: "tool_result", tool_use_id: "call_4", content: "{}" }] },
    ])
    expect(page.data).toEqual([])
  })

  test("an optional title leads the projection", () => {
    const page = ControlProjection.history(session, [], { title: "Remote" })
    expect(at(page, 0)).toMatchObject({ type: "session.next.info.updated", data: { sessionID: session, title: "Remote" } })
  })
})
