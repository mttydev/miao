import { afterEach, describe, expect, test } from "bun:test"
import { mkdtemp, readFile, rm, stat } from "node:fs/promises"
import os from "node:os"
import path from "node:path"
import { register } from "../script/register"

const cleanup: Array<() => void | Promise<void>> = []
afterEach(async () => {
  for (const close of cleanup.splice(0).reverse()) await close()
})

describe("Remote Control registration", () => {
  test("registers the grants identity and writes an owner-only control document", async () => {
    const directory = await mkdtemp(path.join(os.tmpdir(), "miao-register-"))
    cleanup.push(() => rm(directory, { recursive: true, force: true }))
    const configFile = path.join(directory, "control.json")
    let registered: { hostID?: string; publicKey?: string; name?: string } | undefined
    const token = "host-token-00000000000000000000000000000000"
    const hub = Bun.serve({
      hostname: "127.0.0.1",
      port: 0,
      async fetch(request) {
        const url = new URL(request.url)
        if (url.pathname === "/api/auth/sign-in/email")
          return new Response(JSON.stringify({ user: { id: "account_register_fixture_001" } }), {
            headers: { "content-type": "application/json", "set-auth-token": "login-token" },
          })
        if (url.pathname === "/api/auth/token") return Response.json({ token: "access-token" })
        if (url.pathname === "/api/hub/hosts" && request.method === "POST") {
          registered = (await request.json()) as typeof registered
          return Response.json({ token })
        }
        if (url.pathname === "/api/auth/sign-out") return Response.json({ ok: true })
        return new Response("not found", { status: 404 })
      },
    })
    cleanup.push(() => hub.stop(true))

    const saved = new Map<string, string | undefined>()
    const set = (key: string, value: string) => {
      saved.set(key, process.env[key])
      process.env[key] = value
    }
    try {
      set("MIAO_REMOTE_CONTROL_CONFIG", configFile)
      set("MIAO_HUB_URL", hub.url.origin)
      set("MIAO_HUB_EMAIL", "owner@example.invalid")
      set("MIAO_HUB_PASSWORD", "fixture-password")
      set("MIAO_HUB_ALLOW_LOOPBACK", "1")
      await register()
    } finally {
      for (const [key, value] of saved) {
        if (value === undefined) delete process.env[key]
        else process.env[key] = value
      }
    }

    const configuration = JSON.parse(await readFile(configFile, "utf8")) as Record<string, unknown>
    expect(configuration).toMatchObject({
      hubURL: hub.url.origin,
      hostToken: token,
      grantFile: "devices.json",
      accountID: "account_register_fixture_001",
    })
    const grants = JSON.parse(await readFile(path.join(directory, "devices.json"), "utf8")) as {
      hostID: string
    }
    expect(registered?.hostID).toBe(grants.hostID)
    expect(registered?.publicKey).toEqual(expect.stringMatching(/^[A-Za-z0-9_-]{87}$/))
    if (process.platform !== "win32")
      expect((await stat(configFile)).mode & 0o777).toBe(0o600)
  })
})
