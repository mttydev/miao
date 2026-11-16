export * as ControlHostRunner from "./host-runner"

import { readFile } from "node:fs/promises"
import path from "node:path"
import { ControlAgent } from "./agent"
import { DeviceGrants } from "./grants"
import { ControlHostEngine } from "./host-engine"
import { ControlPairing } from "./pairing"
import { PushSender } from "./push-sender"

export type Configuration = {
  readonly hubURL: string
  readonly hostToken: string
  readonly grantFile: string
  readonly accountID?: string
  readonly allowLoopbackHTTP?: boolean
}

export async function readConfiguration(filename: string): Promise<Configuration> {
  const raw: unknown = JSON.parse(await readFile(filename, "utf8"))
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) throw new Error("Invalid Remote Control configuration")
  const value = raw as Record<string, unknown>
  if (typeof value.hubURL !== "string" || typeof value.hostToken !== "string")
    throw new Error("Remote Control configuration needs hubURL and hostToken")
  const grant = typeof value.grantFile === "string" && value.grantFile ? value.grantFile : "devices.json"
  return {
    hubURL: value.hubURL,
    hostToken: value.hostToken,
    grantFile: path.resolve(path.dirname(filename), grant),
    ...(typeof value.accountID === "string" ? { accountID: value.accountID } : {}),
    ...(typeof value.allowLoopbackHTTP === "boolean" ? { allowLoopbackHTTP: value.allowLoopbackHTTP } : {}),
  }
}

export type Options = {
  readonly engine: ControlHostEngine.Engine
  readonly hubURL: string
  readonly hostToken: string
  readonly runtimeID: string
  readonly grantFile: string
  readonly accountID?: string
  readonly allowLoopbackHTTP?: boolean
  readonly commands?: ControlHostEngine.Options["commands"]
  /** Sessions this Runtime publishes; the RPC can see nothing else. */
  readonly sessionIDs?: ReadonlyArray<string>
  readonly push?: boolean
}

export type Runner = {
  readonly hostID: string
  readonly runtimeID: string
  readonly connected: () => boolean
  readonly publish: (sessionID: string) => void
  readonly unpublish: (sessionID: string) => void
  readonly status: () => {
    enabled: boolean
    connected: boolean
    hostID: string
    runtimeID: string
    sessionIDs: string[]
  }
  readonly stop: () => Promise<void>
}

/** Serve one Runtime's published Sessions to its approved devices. */
export async function start(options: Options): Promise<Runner> {
  const grants = await DeviceGrants.load(options.grantFile)
  const published = new Set(options.sessionIDs ?? [])
  const visible = (sessionID: string) => published.has(sessionID)
  const projectForSession = async (sessionID: string) => {
    const rows = await options.engine.command("session_catalog")
    if (!Array.isArray(rows)) return undefined
    const row = rows.find(
      (entry): entry is Record<string, unknown> =>
        typeof entry === "object" && entry !== null && (entry as Record<string, unknown>).session_id === sessionID,
    )
    return typeof row?.location === "string" ? row.location : undefined
  }
  const pairing = ControlPairing.make({
    grants,
    target: { hostID: grants.hostID, runtimeID: options.runtimeID },
    hubURL: options.hubURL,
  })
  const methods = ControlHostEngine.methods(options.engine, { commands: options.commands, visible })
  const agent = ControlAgent.connect({
    hubURL: options.hubURL,
    hostToken: options.hostToken,
    runtimeID: options.runtimeID,
    grants,
    methods,
    pairing,
    accountID: options.accountID,
    allowLoopbackHTTP: options.allowLoopbackHTTP,
    sessionEnabled: visible,
    projectForSession,
    accountScope: async () => {
      const sessionIDs = [...published]
      const projectIDs = [
        ...new Set(
          (await Promise.all(sessionIDs.map((sessionID) => projectForSession(sessionID)))).filter(
            (projectID): projectID is string => typeof projectID === "string",
          ),
        ),
      ]
      return { projectIDs, sessionIDs }
    },
  })
  const push = options.push
    ? PushSender.make({
        hubURL: options.hubURL,
        hostToken: options.hostToken,
        runtimeID: options.runtimeID,
        grants,
        connected: () => agent.connected(),
        projectForSession: (sessionID) => projectForSession(sessionID),
        sessionEnabled: visible,
        allowLoopbackHTTP: options.allowLoopbackHTTP,
      })
    : undefined
  const state = { stopped: false }
  return {
    hostID: grants.hostID,
    runtimeID: options.runtimeID,
    connected: () => !state.stopped && agent.connected(),
    publish: (sessionID) => {
      published.add(sessionID)
    },
    unpublish: (sessionID) => {
      published.delete(sessionID)
    },
    status: () => ({
      enabled: !state.stopped,
      connected: !state.stopped && agent.connected(),
      hostID: grants.hostID,
      runtimeID: options.runtimeID,
      sessionIDs: [...published],
    }),
    stop: async () => {
      if (state.stopped) return
      state.stopped = true
      agent.stop()
      await Promise.all([pairing.stop(), push?.stop()])
    },
  }
}
