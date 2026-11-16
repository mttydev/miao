import { describe, expect, test } from "bun:test"
import { ControlAgent } from "../src/agent"
import { ControlHost } from "../src/host"

const id = crypto.randomUUID()
const request = (payload: unknown, extra: Partial<ControlAgent.Request> = {}): ControlAgent.Request => ({
  version: 1,
  requestID: id,
  hostID: "host-000000000000",
  runtimeID: "runtime-00000000",
  grantID: "grant-0000000000",
  grantVersion: 1,
  method: "session.prompt",
  sessionID: "session-one",
  operationID: id,
  payload,
  ...extra,
})

describe("ControlHost.capabilities", () => {
  test("advertises attachments, commands and the limits the host enforces", () => {
    const value = ControlHost.capabilities({
      commands: ["review", { name: "review", description: "Review the change" }, { name: "test", description: "" }],
      maxAttachmentBytes: 64 * 1024,
      maxPromptCharacters: 1024,
    })
    expect(value.protocol).toBe(1)
    expect(value.features).toEqual(["attachments", "commands"])
    expect(value.commands).toEqual([
      { name: "review", description: "" },
      { name: "test", description: "" },
    ])
    expect(value.maxAttachmentBytes).toBe(64 * 1024)
    expect(value.maxPromptCharacters).toBe(1024)
  })

  test("a text-only host drops the attachment feature instead of lying about it", () => {
    expect(ControlHost.capabilities({ attachments: false }).features).toEqual(["commands"])
  })
})

describe("ControlHost.parsePrompt", () => {
  test("defaults delivery and keeps a text-only prompt unchanged", () => {
    expect(ControlHost.parsePrompt({ text: "hello" })).toEqual({
      text: "hello",
      delivery: "steer",
      attachments: [],
    })
  })

  test("maps attachments onto the engine's media shape", () => {
    const data = Buffer.from("hello").toString("base64")
    expect(
      ControlHost.parsePrompt({
        text: "看看这个",
        delivery: "queue",
        attachments: [{ type: "file", name: "notes.txt", mimeType: "text/plain", data }],
      }),
    ).toEqual({
      text: "看看这个",
      delivery: "queue",
      attachments: [{ mime: "text/plain", data, metadata: { name: "notes.txt", type: "file" } }],
    })
  })

  test("rejects an attachment set above the advertised encoded budget", () => {
    const data = Buffer.alloc(8).toString("base64")
    expect(() =>
      ControlHost.parsePrompt(
        { text: "x", attachments: [{ mimeType: "text/plain", data }, { mimeType: "text/plain", data }] },
        { maxAttachmentBytes: data.length },
      ),
    ).toThrow(ControlAgent.RequestError)
  })

  test("rejects a non-canonical payload rather than admitting it", () => {
    for (const payload of [
      { text: "" },
      { text: "x", delivery: "later" },
      { text: "x", extra: true },
      { text: "x", attachments: [{ mimeType: "text/plain", data: "not base64" }] },
      { text: "x", attachments: [{ mimeType: "text/plain", data: "" }] },
      { text: "x", attachments: [] , wrong: 1 },
    ])
      expect(() => ControlHost.parsePrompt(payload)).toThrow(ControlAgent.RequestError)
  })
})

describe("ControlHost.methods", () => {
  test("capabilities is served from the same limits the prompt parser uses", async () => {
    const methods = ControlHost.methods({ commands: ["review"], admit: async () => ({ status: "accepted" }) })
    expect(await methods.capabilities!(request({}), {} as ControlAgent.Context)).toMatchObject({
      protocol: 1,
      features: ["attachments", "commands"],
      maxAttachmentBytes: 256 * 1024,
    })
  })

  test("session.prompt hands the host a validated admission", async () => {
    const calls: ControlHost.Admission[] = []
    const methods = ControlHost.methods({
      admit: async (admission) => {
        calls.push(admission)
        return { status: "accepted" }
      },
    })
    const data = Buffer.from("png").toString("base64")
    await methods["session.prompt"]!(
      request({ text: "hi", attachments: [{ mimeType: "image/png", data }] }),
      {} as ControlAgent.Context,
    )
    expect(calls).toEqual([{ text: "hi", delivery: "steer", attachments: [{ mime: "image/png", data }] }])
  })

  test("session.prompt refuses a mutation without identity", async () => {
    const methods = ControlHost.methods({ admit: async () => ({ status: "accepted" }) })
    for (const extra of [{ operationID: undefined }, { sessionID: undefined }])
      await expect(
        methods["session.prompt"]!(request({ text: "hi" }, extra), {} as ControlAgent.Context),
      ).rejects.toBeInstanceOf(ControlAgent.RequestError)
  })

  test("the engine input identity is stable across a client retry", () => {
    expect(ControlHost.inputID(id)).toBe(ControlHost.inputID(id))
    expect(ControlHost.inputID(id)).toMatch(/^msg_remote_[a-f0-9]{64}$/)
  })
})
