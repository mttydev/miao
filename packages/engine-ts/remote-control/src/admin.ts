export * as ControlAdmin from "./admin"

import { Option, Schema } from "effect"
import { RemoteAccess } from "@miao/schema/remote-access"
import { DeviceGrants } from "./grants"
import { ControlPairing } from "./pairing"

/** The administration surface mtty's private pipe drives. */
export type Options = {
  readonly runtimeID: string
  readonly hostID: string
  readonly hostPublicKey: string
  readonly hubURL: string
  readonly accountID?: string
  readonly sessionIDs: () => ReadonlyArray<string>
  readonly connected: () => boolean
  readonly enabled: () => boolean
  readonly pairing: ReturnType<typeof ControlPairing.make>
  readonly grants: DeviceGrants.Store
  readonly revoke: (grantID: string, version: number) => Promise<unknown>
}

export type Admin = {
  readonly url: string
  readonly token: string
  readonly stop: () => Promise<void>
}

const MAX_REQUEST = 64 * 1024

/**
 * Owner-only loopback administration. The caller supplies the bounded request
 * and receives one JSON reply; no credential or pairing secret is logged.
 */
export function listen(options: Options): Admin {
  const token = Buffer.from(crypto.getRandomValues(new Uint8Array(32))).toString("base64url")
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      if (new URL(request.url).pathname !== "/access" || request.method !== "POST")
        return new Response("not found", { status: 404 })
      if (request.headers.get("authorization") !== `Bearer ${token}`)
        return new Response("forbidden", { status: 403 })
      const bytes = new Uint8Array(await request.arrayBuffer())
      if (bytes.byteLength > MAX_REQUEST) return failure("invalidRequest")
      const value = parse(bytes)
      if (!value || value.version !== 1 || value.runtimeID !== options.runtimeID)
        return failure("runtimeChanged")
      try {
        return Response.json({ version: 1, ok: true, runtimeID: options.runtimeID, data: await dispatch(value) })
      } catch (error) {
        return failure(error instanceof AdminError ? error.code : "unavailable")
      }
    },
  })
  return {
    url: `http://127.0.0.1:${server.port}/access`,
    token,
    stop: async () => {
      await server.stop(true)
    },
  }

  async function dispatch(request: Record<string, unknown>) {
    switch (request.method) {
      case "status":
        return status()
      case "session": {
        const sessionID = text(request.sessionID)
        if (!sessionID) throw new AdminError("invalidRequest")
        return { sessionID, published: options.sessionIDs().includes(sessionID) }
      }
      case "invite": {
        const policy = Schema.decodeUnknownOption(RemoteAccess.Policy, { onExcessProperty: "error" })(request.policy)
        if (Option.isNone(policy)) throw new AdminError("invalidRequest")
        return options.pairing.issue(policy.value)
      }
      case "pending":
        return options.pairing.list()
      case "devices":
        return options.grants.list()
      case "approve": {
        const pairingID = text(request.pairingID)
        const publicKey = text(request.publicKey)
        if (!pairingID || !publicKey) throw new AdminError("invalidRequest")
        return options.pairing.approve(pairingID, publicKey)
      }
      case "reject": {
        const pairingID = text(request.pairingID)
        if (!pairingID) throw new AdminError("invalidRequest")
        options.pairing.reject(pairingID)
        return null
      }
      case "revoke": {
        const grantID = text(request.grantID)
        const version = number(request.grantVersion)
        if (!grantID || version === undefined) throw new AdminError("invalidRequest")
        return options.revoke(grantID, version)
      }
      case "setup":
        throw new AdminError("unavailable")
      default:
        throw new AdminError("invalidRequest")
    }
  }

  function status() {
    return {
      enabled: options.enabled(),
      connected: options.connected(),
      hostID: options.hostID,
      runtimeID: options.runtimeID,
      hostPublicKey: options.hostPublicKey,
      hubURL: options.hubURL,
      sessionIDs: [...options.sessionIDs()],
      ...(options.accountID === undefined ? {} : { accountID: options.accountID }),
    }
  }
}

class AdminError extends Error {
  constructor(readonly code: "invalidRequest" | "noRuntime" | "runtimeChanged" | "unavailable") {
    super(code)
  }
}

function failure(code: AdminError["code"]) {
  return Response.json({ version: 1, ok: false, error: code })
}

function parse(bytes: Uint8Array): Record<string, unknown> | undefined {
  try {
    const value: unknown = JSON.parse(new TextDecoder().decode(bytes))
    return typeof value === "object" && value !== null && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : undefined
  } catch {
    return undefined
  }
}

function text(value: unknown) {
  return typeof value === "string" && value ? value : undefined
}

function number(value: unknown) {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0 ? value : undefined
}
