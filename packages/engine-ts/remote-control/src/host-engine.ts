export * as ControlHostEngine from "./host-engine"

import { ControlAgent } from "./agent"
import { ControlHost } from "./host"
import { ControlProjection } from "./projection"

/** The slice of the engine HTTP attachment this adapter needs. */
export type Engine = {
  readonly request: (method: string, params: unknown) => Promise<unknown>
  readonly command: (method: string) => Promise<unknown>
  readonly admit: (
    input: { session_id: string; input_id: string; prompt: string; delivery?: "steer" | "queue" },
    resume?: boolean,
    attachments?: ReadonlyArray<unknown>,
  ) => Promise<unknown>
}

export type Options = {
  readonly commands?: ReadonlyArray<string | ControlHost.Command>
  readonly maxAttachmentBytes?: number
  readonly maxPromptCharacters?: number
  readonly waitMsCeiling?: number
  readonly sleep?: (ms: number) => Promise<void>
}

type Row = Record<string, unknown>

export function methods(engine: Engine, options: Options = {}) {
  const host = ControlHost.methods({
    commands: options.commands,
    maxAttachmentBytes: options.maxAttachmentBytes,
    maxPromptCharacters: options.maxPromptCharacters,
    admit: (admission, request) =>
      guard(true, async () => {
        const inputID = ControlHost.inputID(request.operationID!)
        const result = record(
          await engine.admit(
            {
              session_id: request.sessionID!,
              input_id: inputID,
              prompt: admission.text,
              delivery: admission.delivery,
            },
            true,
            admission.attachments,
          ),
        )
        const admittedSeq = number(result?.admitted_seq)
        return {
          status: "accepted",
          messageID: inputID,
          sessionID: request.sessionID,
          ...(admittedSeq === undefined ? {} : { admittedSeq }),
        }
      }),
  })
  return {
    ...host,
    "session.list": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(false, async () => {
        const payload = record(request.payload) ?? {}
        const search = text(payload.search)?.toLowerCase()
        const limit = Math.min(Math.max(number(payload.limit) ?? 100, 1), 200)
        const start = offset(payload.cursor)
        const rows = catalog(await engine.command("session_catalog"))
        const scoped = request.projectID ? rows.filter((row) => row.location === request.projectID) : rows
        const matched = search
          ? scoped.filter(
              (row) =>
                text(row.title)?.toLowerCase().includes(search) === true ||
                text(record(row.preview)?.text)?.toLowerCase().includes(search) === true,
            )
          : scoped
        return {
          data: matched.slice(start, start + limit).map(info),
          cursor: { next: start + limit < matched.length ? String(start + limit) : undefined },
        }
      }),
    "session.get": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(false, async () => {
        const rows = catalog(await engine.command("session_catalog"))
        const row = rows.find((entry) => entry.session_id === request.sessionID)
        if (!row) throw new ControlAgent.RequestError("not_found")
        return info(row)
      }),
    "session.events": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(false, async () => {
        const payload = record(request.payload) ?? {}
        const after = Math.max(number(payload.after) ?? 0, 0)
        const limit = Math.min(Math.max(number(payload.limit) ?? 100, 1), 100)
        const waitMs = Math.min(Math.max(number(payload.waitMs) ?? 0, 0), options.waitMsCeiling ?? 25_000)
        const sleep = options.sleep ?? ((ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms)))
        const deadline = Date.now() + waitMs
        for (;;) {
          const snapshot = record(await engine.request("snapshot", { session_id: request.sessionID })) ?? {}
          const projection = ControlProjection.history(request.sessionID!, messages(snapshot), {
            ...(text(snapshot.title) === undefined ? {} : { title: text(snapshot.title) }),
          })
          if (projection.cursor > after || Date.now() >= deadline) {
            const page = projection.data.slice(after, after + limit)
            return {
              data: page,
              cursor: page.length ? page[page.length - 1]!.durable.seq : Math.min(after, projection.cursor),
              hasMore: after + limit < projection.cursor,
            }
          }
          await sleep(Math.min(500, Math.max(0, deadline - Date.now())))
        }
      }),
    "session.pending": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(false, async () => {
        const snapshot =
          record(await engine.request("snapshot", { session_id: request.sessionID, metadata_only: true })) ?? {}
        const active = text(snapshot.active_run)
        return {
          execution: active ? { type: "running", executionID: active } : { type: "idle" },
          permissions: Array.isArray(snapshot.approvals) ? snapshot.approvals : [],
          questions: Array.isArray(snapshot.questions) ? snapshot.questions : [],
          inputs: {
            data: Array.isArray(snapshot.pending)
              ? snapshot.pending.filter(isRow).map((input) => ({
                  ...input,
                  id: input.input_id,
                }))
              : [],
          },
        }
      }),
    "session.rename": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(true, async () => {
        const payload = record(request.payload) ?? {}
        const title = text(payload.title)
        if (!title) throw new ControlAgent.RequestError("invalid_request")
        await engine.request("rename", { session_id: request.sessionID, title })
        return { status: "completed" }
      }),
    "session.pin": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(true, async () => {
        const payload = record(request.payload) ?? {}
        if (typeof payload.pinned !== "boolean") throw new ControlAgent.RequestError("invalid_request")
        await engine.request("pin", { session_id: request.sessionID, pinned: payload.pinned })
        return { status: "completed" }
      }),
    "session.archive": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(true, async () => {
        const payload = record(request.payload) ?? {}
        if (typeof payload.archived !== "boolean") throw new ControlAgent.RequestError("invalid_request")
        await engine.request("archive", { session_id: request.sessionID, archived: payload.archived })
        return { status: "completed" }
      }),
    "session.remove": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(true, async () => {
        await engine.request("remove", { session_id: request.sessionID })
        return { status: "completed" }
      }),
    "session.interrupt": (request: ControlAgent.Request, _context: ControlAgent.Context) =>
      guard(true, async () => {
        const payload = record(request.payload) ?? {}
        const run = text(payload.executionID)
        const result = record(
          run
            ? await engine.request("cancel_if", { session_id: request.sessionID, run_id: run })
            : await engine.request("cancel", { session_id: request.sessionID }),
        )
        if (result?.accepted === false) return { status: "rejected", code: "conflict" }
        return { status: "completed" }
      }),
  } satisfies Partial<Record<ControlAgent.Method, ControlAgent.Handler>>
}

function info(row: Row) {
  const updated = number(row.updated_at_ms)
  return {
    id: String(row.session_id),
    title: text(row.title) ?? "",
    projectID: text(row.location) ?? "",
    time: {
      ...(updated === undefined ? {} : { created: updated, updated }),
      ...(number(row.archived_at_ms) === undefined ? {} : { archived: row.archived_at_ms }),
      ...(number(row.pinned_at_ms) === undefined ? {} : { pinned: row.pinned_at_ms }),
    },
    ...(text(row.parent_session_id) === undefined ? {} : { parentID: row.parent_session_id }),
  }
}

function messages(snapshot: Row): ControlProjection.Message[] {
  return Array.isArray(snapshot.messages) ? snapshot.messages.filter(isRow).map((message) => message as ControlProjection.Message) : []
}

async function guard<A>(write: boolean, action: () => Promise<A>): Promise<A> {
  try {
    return await action()
  } catch (error) {
    if (error instanceof ControlAgent.RequestError) throw error
    const code = record(error)?.code
    if (typeof code === "string" && ["forbidden", "not_found", "conflict", "expired", "outcome_unknown", "invalid_request"].includes(code))
      throw new ControlAgent.RequestError(code as "forbidden")
    if (write) throw new ControlAgent.RequestError("outcome_unknown")
    throw error
  }
}

function offset(value: unknown) {
  const parsed = typeof value === "string" ? Number.parseInt(value, 10) : typeof value === "number" ? value : 0
  return Number.isSafeInteger(parsed) && parsed >= 0 ? parsed : 0
}

function number(value: unknown) {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined
}

function text(value: unknown) {
  return typeof value === "string" ? value : undefined
}

function catalog(value: unknown): Row[] {
  return Array.isArray(value) ? value.filter(isRow) : []
}

function isRow(value: unknown): value is Row {
  return record(value) !== undefined
}

function record(value: unknown): Row | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Row) : undefined
}
