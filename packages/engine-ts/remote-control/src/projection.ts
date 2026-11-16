export * as ControlProjection from "./projection"

/**
 * The engine stores the model-facing transcript: provider content blocks such
 * as `text`, `reasoning`, `tool_use` and `tool_result`, plus an assistant
 * identity event keyed by the assistant message sequence. The remote protocol
 * publishes the UI-facing `session.next.*` vocabulary instead.
 */

export type Event = {
  readonly type: string
  readonly durable: { readonly seq: number; readonly aggregateID: string; readonly version: 1 }
  readonly data: Record<string, unknown>
  readonly metadata?: unknown
}

export type Message = {
  readonly seq: number
  readonly role: string
  readonly content: unknown
  readonly recorded_at_ms?: number | null
  readonly identity?: Record<string, unknown> | null
}

export type History = {
  readonly data: Event[]
  readonly cursor: number
  readonly hasMore: boolean
}

export type EngineEvent = {
  readonly seq: number
  readonly kind: string
  readonly data: unknown
  readonly recorded_at_ms?: number | null
}

/** One provider step: its assistant id and the identity committed with it. */
export type RunState = {
  assistantID?: string
  profile?: Record<string, unknown>
}

type Block = Record<string, unknown>
type Emit = (type: string, data: Record<string, unknown>, timestamp?: number | null) => void

export function history(
  sessionID: string,
  messages: ReadonlyArray<Message>,
  options: { title?: string } = {},
): History {
  const events: Event[] = []
  const emit: Emit = (type, data, timestamp) => {
    const value: Record<string, unknown> = { sessionID, ...data }
    if (typeof timestamp === "number") value.timestamp = timestamp
    events.push({ type, durable: { seq: events.length + 1, aggregateID: sessionID, version: 1 }, data: value })
  }
  if (options.title) emit("session.next.info.updated", { title: options.title })
  let assistantID: string | undefined
  for (const message of messages) {
    const timestamp = message.recorded_at_ms
    if (message.role === "assistant") {
      assistantID = messageID(anchorOf(message))
      const profile = message.identity ?? {}
      emit(
        "session.next.step.started",
        { assistantMessageID: assistantID, agent: profile.agent, model: modelOf(profile) },
        timestamp,
      )
      emitParts(emit, assistantID, message.content, timestamp)
      emit("session.next.step.ended", { assistantMessageID: assistantID, ...stepEnd(profile) }, timestamp)
      continue
    }
    if (message.role !== "user") continue
    const blocks = partBlocks(message.content)
    const results = blocks.filter((block) => block.type === "tool_result")
    if (results.length > 0 && results.length === blocks.length) {
      if (assistantID) emitToolResults(emit, assistantID, results, timestamp)
      continue
    }
    emitPrompt(emit, message.seq, blocks, timestamp)
    assistantID = undefined
  }
  return { data: events, cursor: events.length, hasMore: false }
}

/**
 * Translate one committed engine event. `state` carries the open provider step
 * across calls; `next` supplies the host's protocol cursor.
 */
export function committed(
  sessionID: string,
  event: EngineEvent,
  state: RunState,
  next: () => number,
): Event[] {
  const events: Event[] = []
  const emit: Emit = (type, data, timestamp) => {
    const value: Record<string, unknown> = { sessionID, ...data }
    if (typeof timestamp === "number") value.timestamp = timestamp
    events.push({ type, durable: { seq: next(), aggregateID: sessionID, version: 1 }, data: value })
  }
  const data = record(event.data) ?? {}
  if (event.kind === "provider.started") {
    const assistantID = messageID(event.seq)
    state.assistantID = assistantID
    const model = modelOf({ provider: data.identity })
    emit("session.next.step.started", { assistantMessageID: assistantID, model }, event.recorded_at_ms)
    return events
  }
  if (event.kind === "assistant.identity") {
    state.profile = data
    return events
  }
  if (event.kind === "message.committed") {
    if (data.role === "assistant") {
      const assistantID = messageID(typeof data.anchor_seq === "number" ? data.anchor_seq : event.seq)
      if (state.assistantID !== assistantID) {
        state.assistantID = assistantID
        emit(
          "session.next.step.started",
          { assistantMessageID: assistantID, agent: state.profile?.agent, model: modelOf(state.profile ?? {}) },
          event.recorded_at_ms,
        )
      }
      emitParts(emit, assistantID, data.content, event.recorded_at_ms)
    }
    if (data.role === "user") {
      const blocks = partBlocks(data.content)
      const results = blocks.filter((block) => block.type === "tool_result")
      if (results.length > 0 && results.length === blocks.length) {
        if (state.assistantID) emitToolResults(emit, state.assistantID, results, event.recorded_at_ms)
      } else {
        emitPrompt(emit, event.seq, blocks, event.recorded_at_ms)
      }
    }
    return events
  }
  if (event.kind === "run.finished") {
    if (state.assistantID) {
      if (data.reason === "failed") {
        emit(
          "session.next.step.failed",
          { assistantMessageID: state.assistantID, error: data.message ?? "failed" },
          event.recorded_at_ms,
        )
      } else
        emit(
          "session.next.step.ended",
          { assistantMessageID: state.assistantID, ...stepEnd(state.profile ?? {}) },
          event.recorded_at_ms,
        )
    }
    state.assistantID = undefined
    state.profile = undefined
  }
  return events
}

function emitParts(emit: Emit, assistantID: string, content: unknown, timestamp?: number | null) {
  partBlocks(content).forEach((block, index) => {
    if (block.type === "text") {
      const textID = partID(assistantID, "text", index)
      emit("session.next.text.started", { assistantMessageID: assistantID, textID }, timestamp)
      emit("session.next.text.ended", { assistantMessageID: assistantID, textID, text: block.text ?? "" }, timestamp)
    }
    if (block.type === "reasoning") {
      const reasoningID = partID(assistantID, "reasoning", index)
      emit("session.next.reasoning.started", { assistantMessageID: assistantID, reasoningID }, timestamp)
      emit(
        "session.next.reasoning.ended",
        {
          assistantMessageID: assistantID,
          reasoningID,
          text: block.text ?? "",
          ...(block.providerMetadata === undefined ? {} : { providerMetadata: block.providerMetadata }),
        },
        timestamp,
      )
    }
    if (block.type === "tool_use") {
      const callID = String(block.id ?? "")
      if (!callID) return
      const input = record(block.input) ?? {}
      emit("session.next.tool.input.started", { assistantMessageID: assistantID, callID, name: block.name ?? "" }, timestamp)
      emit("session.next.tool.input.ended", { assistantMessageID: assistantID, callID, text: JSON.stringify(input) }, timestamp)
      emit("session.next.tool.called", { assistantMessageID: assistantID, callID, input, provider: { executed: true } }, timestamp)
    }
  })
}

function emitPrompt(emit: Emit, seq: number, blocks: Block[], timestamp?: number | null) {
  const text = blocks
    .filter((block) => block.type === "text")
    .map((block) => String(block.text ?? ""))
    .join("\n")
  emit(
    "session.next.prompted",
    {
      messageID: messageID(seq),
      prompt: {
        text,
        content: blocks
          .filter((block) => block.type === "text")
          .map((block) => ({ type: "text", text: block.text ?? "" })),
      },
    },
    timestamp,
  )
}

function emitToolResults(emit: Emit, assistantID: string, results: Block[], timestamp?: number | null) {
  for (const result of results) {
    const callID = String(result.tool_use_id ?? "")
    if (!callID) continue
    const text = typeof result.content === "string" ? result.content : JSON.stringify(result.content ?? "")
    const parsed = parse(text)
    const failed = result.is_error === true
    emit(
      failed ? "session.next.tool.failed" : "session.next.tool.success",
      {
        assistantMessageID: assistantID,
        callID,
        content: [{ type: "text", text }],
        structured: parsed ?? {},
        result: parsed,
        ...(failed ? { error: text } : {}),
      },
      timestamp,
    )
  }
}

function stepEnd(profile: Record<string, unknown>) {
  const tokens = profile.tokens ?? profile.usage
  return {
    finish: profile.finish ?? "stop",
    ...(tokens === undefined ? {} : { tokens }),
    ...(profile.cost === undefined ? {} : { cost: profile.cost }),
    ...(profile.ttft === undefined ? {} : { ttft: profile.ttft }),
  }
}

function modelOf(profile: Record<string, unknown>) {
  return record(profile.provider)?.model ?? profile.model
}

function anchorOf(message: Message) {
  const anchor = message.identity?.anchor_seq
  return typeof anchor === "number" ? anchor : message.seq
}

function partBlocks(value: unknown): Block[] {
  return Array.isArray(value) ? value.filter((entry): entry is Block => record(entry) !== undefined) : []
}

function record(value: unknown): Block | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Block) : undefined
}

function parse(value: string): unknown {
  try {
    return JSON.parse(value)
  } catch {
    return undefined
  }
}

function messageID(seq: number) {
  return `msg_${seq}`
}

function partID(assistantID: string, kind: string, index: number) {
  return `${kind}_${assistantID}_${index}`
}
