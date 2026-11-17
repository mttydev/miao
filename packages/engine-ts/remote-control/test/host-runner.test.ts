import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, readFile, rm, stat } from "node:fs/promises"
import os from "node:os"
import path from "node:path"
import { ControlAgent } from "../src/agent"
import { DeviceGrants } from "../src/grants"
import { ControlHostRunner } from "../src/host-runner"
import { ControlHub } from "../src/hub"
import { SecureChannel } from "../src/secure-channel"

const cleanup: Array<() => void | Promise<void>> = []
afterEach(async () => {
  for (const close of cleanup.splice(0).reverse()) await close()
})

async function socket(base: string, hostID: string) {
  const ws = new WebSocket(`${base.replace("http:", "ws:")}/v1/client?hostID=${hostID}`)
  cleanup.push(() => ws.close())
  const queue: string[] = []
  const waiting: Array<(message: string) => void> = []
  ws.addEventListener("message", (event) => {
    const next = waiting.shift()
    if (next) next(String(event.data))
    else queue.push(String(event.data))
  })
  await new Promise<void>((resolve, reject) => {
    ws.addEventListener("open", () => resolve(), { once: true })
    ws.addEventListener("error", () => reject(new Error("WebSocket failed")), { once: true })
  })
  const receive = () =>
    queue.length
      ? Promise.resolve(queue.shift()!)
      : new Promise<string>((resolve, reject) => {
          const timer = setTimeout(() => reject(new Error("Agent response timeout")), 3000)
          waiting.push((message) => {
            clearTimeout(timer)
            resolve(message)
          })
        })
  return { ws, receive }
}

describe("ControlHostRunner", () => {
  test("serves published sessions and admits a prompt over the real Hub", async () => {
    const directory = await mkdtemp(path.join(os.tmpdir(), "miao-runner-"))
    cleanup.push(() => rm(directory, { recursive: true, force: true }))
    const grants = await DeviceGrants.load(path.join(directory, "devices.json"))
    const device = await SecureChannel.createIdentity()
    const grant = await grants.approve({
      publicKey: device.publicKey,
      label: "phone",
      permissions: ["read", "prompt"],
      projectIDs: ["p1"],
      sessionIDs: [],
      expiresAt: Date.now() + 60_000,
    })
    const token = "test-host-credential-0000000000000000000000"
    const runtimeID = crypto.randomUUID()
    const hub = ControlHub.listen({ hosts: new Map([[grants.hostID, token]]), port: 0 })
    cleanup.push(() => hub.stop())
    const base = `http://127.0.0.1:${hub.port}`
    const calls: Array<{ method: string; params: unknown }> = []
    const runner = await ControlHostRunner.start({
      engine: {
        request: async (method, params) => {
          calls.push({ method, params })
          if (method === "snapshot")
            return { title: "One", messages: [{ seq: 1, role: "user", content: [{ type: "text", text: "hello" }] }] }
          throw Object.assign(new Error("unexpected"), { code: "invalid_request" })
        },
        command: async (method) => {
          calls.push({ method, params: undefined })
          if (method === "session_catalog")
            return [{ session_id: "one", title: "One", location: "p1", updated_at_ms: 1000 }]
          throw Object.assign(new Error("unexpected"), { code: "invalid_request" })
        },
        admit: async (input, resume, attachments) => {
          calls.push({ method: "admit", params: { input, resume, attachments } })
          return { input_id: input.input_id, admitted_seq: 7, duplicate: false, pending: true }
        },
      },
      hubURL: base,
      hostToken: token,
      runtimeID,
      grantFile: path.join(directory, "devices.json"),
      allowLoopbackHTTP: true,
      sessionIDs: ["one"],
      push: false,
      storage: path.join(directory, "miao.db"),
    })
    cleanup.push(() => runner.stop())
    const deadline = Date.now() + 3000
    while (!runner.connected() && Date.now() < deadline) await Bun.sleep(5)
    expect(runner.connected()).toBe(true)

    const transport = await socket(base, grants.hostID)
    const pending = await SecureChannel.startClient(device, { hostID: grants.hostID, runtimeID })
    transport.ws.send(Buffer.from(JSON.stringify(pending.hello)).toString("base64url"))
    const accepted = await pending.finish(
      JSON.parse(Buffer.from(await transport.receive(), "base64url").toString()),
      grants.identity.publicKey,
    )
    const call = async (extra: Partial<ControlAgent.Request>) => {
      const request: ControlAgent.Request = {
        version: 1,
        requestID: crypto.randomUUID(),
        hostID: grants.hostID,
        runtimeID,
        grantID: grant.id,
        grantVersion: grant.version,
        method: "capabilities",
        payload: {},
        ...extra,
      }
      transport.ws.send(await accepted.channel.seal(new TextEncoder().encode(JSON.stringify(request))))
      return JSON.parse(new TextDecoder().decode(await accepted.channel.open(await transport.receive()))) as {
        type: string
        data?: Record<string, unknown>
      }
    }

    expect(runner.admin).toBeDefined()
    const registration = JSON.parse(await readFile(runner.admin!.file, "utf8")) as {
      url: string
      token: string
      runtimeID: string
      hostID: string
    }
    expect(registration).toMatchObject({ version: 1, runtimeID, hostID: grants.hostID })
    if (process.platform !== "win32")
      expect((await stat(runner.admin!.file)).mode & 0o777).toBe(0o600)
    const administration = async (body: unknown, token = registration.token) => {
      const response = await fetch(registration.url, {
        method: "POST",
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: JSON.stringify(body),
      })
      const text = await response.text()
      let parsed: Record<string, unknown> = {}
      if (text.startsWith("{")) parsed = JSON.parse(text) as Record<string, unknown>
      return { status: response.status, body: parsed }
    }
    const status = await administration({ version: 1, runtimeID, method: "status" })
    expect(status.body).toMatchObject({
      ok: true,
      data: { hostID: grants.hostID, runtimeID, sessionIDs: ["one"], connected: true },
    })
    const devices = await administration({ version: 1, runtimeID, method: "devices" })
    expect(devices.body.data).toHaveLength(1)
    const pairingPending = await administration({ version: 1, runtimeID, method: "pending" })
    expect(pairingPending.body.data).toEqual([])
    const invite = await administration({
      version: 1,
      runtimeID,
      method: "invite",
      policy: { permissions: ["read"], projectIDs: ["p1"], sessionIDs: [], expiresAt: Date.now() + 60_000 },
    })
    expect(invite.body).toMatchObject({ ok: true, data: { version: 1, hostID: grants.hostID, runtimeID } })
    expect((await administration({ version: 1, runtimeID, method: "status" }, "wrong")).status).toBe(403)

    const capabilities = await call({ method: "capabilities" })
    expect(capabilities.type).toBe("result")
    expect(capabilities.data).toMatchObject({ protocol: 1, features: ["attachments", "commands"] })

    const list = await call({ method: "session.list", projectID: "p1", payload: {} })
    expect(list.data).toMatchObject({ data: [{ id: "one", title: "One", projectID: "p1" }] })

    const attachment = { mimeType: "text/plain", name: "a.txt", data: Buffer.from("hi").toString("base64") }
    const prompt = await call({
      method: "session.prompt",
      sessionID: "one",
      operationID: crypto.randomUUID(),
      payload: { text: "hello", attachments: [attachment] },
    })
    expect(prompt.data).toMatchObject({ status: "accepted", sessionID: "one", admittedSeq: 7 })
    const admitted = calls.find((entry) => entry.method === "admit")?.params as {
      attachments: Array<{ mime: string; data: string; metadata?: Record<string, string> }>
    }
    expect(admitted.attachments).toEqual([{ mime: "text/plain", data: attachment.data, metadata: { name: "a.txt" } }])
  })
})
