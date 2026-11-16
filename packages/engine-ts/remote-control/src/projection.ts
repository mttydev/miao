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

type Block = Record<string, unknown>

export function history(
  sessionID: string,
  messages: ReadonlyArray<Message>,
  options: { title?: string } = {},
): History {
  const events: Event[] = []
  const emit = (type: string, data: Record<string, unknown>, timestamp?: number | null, metadata?: unknown) => {
    const value: Record<string, unknown> = { sessionID, ...data }
    if (typeof timestamp === "number") value.timestamp = timestamp
    events.push({
      type,
      durable: { seq: events.length + 1, aggregateID: sessionID, version: 1 },
      data: value,
      ...(metadata === undefined ? {} : { metadata }),
    })
  }
  if (options.title) emit("session.next.info.updated", { title: options.title })
  let assistantID: string | undefined
  for (const message of messages) {
    const blocks = partBlocks(message.content)
    const timestamp = message.recorded_at_ms
    if (message.role === "assistant") {
      assistantID = messageID(message.seq)
      const profile = message.identity ?? {}
      const model = record(profile.provider)?.model ?? profile.model
      emit(
        "session.next.step.started",
        {
          assistantMessageID: assistantID,
          ...(profile.agent === undefined ? {} : { agent: profile.agent }),
          ...(model === undefined ? {} : { model }),
        },
        timestamp,
      )
      blocks.forEach((block, index) => {
        if (block.type === "text") {
          const textID = partID(message.seq, "text", index)
          emit("session.next.text.started", { assistantMessageID: assistantID, textID }, timestamp)
          emit("session.next.text.ended", { assistantMessageID: assistantID, textID, text: block.text ?? "" }, timestamp)
        }
        if (block.type === "reasoning") {
          const reasoningID = partID(message.seq, "reasoning", index)
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
          emit(
            "session.next.tool.input.started",
            { assistantMessageID: assistantID, callID, name: block.name ?? "" },
            timestamp,
          )
          emit(
            "session.next.tool.input.ended",
            { assistantMessageID: assistantID, callID, text: JSON.stringify(input) },
            timestamp,
          )
          emit("session.next.tool.called", { assistantMessageID: assistantID, callID, input, provider: { executed: true } }, timestamp)
        }
      })
      emit(
        "session.next.step.ended",
        {
          assistantMessageID: assistantID,
          finish: profile.finish ?? "stop",
          ...(profile.tokens === undefined ? {} : { tokens: profile.tokens }),
          ...(profile.cost === undefined ? {} : { cost: profile.cost }),
          ...(profile.ttft === undefined ? {} : { ttft: profile.ttft }),
        },
        timestamp,
      )
      continue
    }
    if (message.role !== "user") continue
    const results = blocks.filter((block) => block.type === "tool_result")
    if (results.length > 0 && results.length === blocks.length) {
      for (const result of results) {
        const callID = String(result.tool_use_id ?? "")
        if (!assistantID || !callID) continue
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
      continue
    }
    const text = blocks
      .filter((block) => block.type === "text")
      .map((block) => String(block.text ?? ""))
      .join("\n")
    emit(
      "session.next.prompted",
      {
        messageID: messageID(message.seq),
        prompt: {
          text,
          content: blocks
            .filter((block) => block.type === "text")
            .map((block) => ({ type: "text", text: block.text ?? "" })),
        },
      },
      timestamp,
    )
    assistantID = undefined
  }
  return { data: events, cursor: events.length, hasMore: false }
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

function partID(seq: number, kind: string, index: number) {
  return `${kind}_${seq}_${index}`
}
