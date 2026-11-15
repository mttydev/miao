import type { AssistantMessage, Message } from "@miao/schema/view-models"
import type { SessionMessageInfo } from "@/utils/server"

type Provider = {
  id: string
  name?: string
  models: Record<string, Model | undefined>
}

type Model = {
  name?: string
  limit: {
    context: number
  }
}

type ContextMessage = {
  id: string
  providerID: string
  modelID: string
  tokens: AssistantMessage["tokens"]
  /**
   * The occupancy and the input ceiling the engine planned the turn against.
   * When present this is the number every surface must agree on: the window
   * already excludes the output the turn reserves, and the input is the
   * provider-counted occupancy rather than a sum of token buckets.
   */
  contextBudget?: {
    readonly input: number
    readonly window: number
    readonly limit?: number
  }
  time: { created: number | null; completed?: number }
}

type Context = {
  message: ContextMessage
  provider?: Provider
  model?: Model
  providerLabel: string
  modelLabel: string
  limit: number | undefined
  input: number
  total: number
  usage: number | null
}

const tokenTotal = (msg: ContextMessage) => {
  return msg.tokens.input + msg.tokens.output + msg.tokens.reasoning + msg.tokens.cache.read + msg.tokens.cache.write
}

const lastAssistantWithTokens = (messages: readonly ContextMessage[]) => {
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i]
    if (tokenTotal(msg) <= 0) continue
    return msg
  }
}

const build = (messages: readonly ContextMessage[] = [], providers: Provider[] = []): Context | undefined => {
  const message = lastAssistantWithTokens(messages)
  if (!message) return undefined

  const provider = providers.find((item) => item.id === message.providerID)
  const model = provider?.models[message.modelID]
  // The engine's own budget wins: its window is the input ceiling a request
  // can actually send, and its input is the counted occupancy. Summing message
  // tokens against the raw catalog window double-counted cached input and
  // divided by a window no request could fill, so the same Session read 16% in
  // the TUI and 100% here.
  const budget = message.contextBudget
  const limit = budget?.window ?? model?.limit.context
  const total = budget?.input ?? tokenTotal(message)

  return {
    message,
    provider,
    model,
    providerLabel: provider?.name ?? (message.providerID || "—"),
    modelLabel: model?.name ?? (message.modelID || "—"),
    limit,
    input: message.tokens.input,
    total,
    // Floor, the way the TUI's `context_percent` does: the same occupancy
    // must not read 17% in one surface and 16% in the other.
    usage: limit ? Math.floor((total / limit) * 100) : null,
  }
}

export function getSessionContext(messages: Message[] = [], providers: Provider[] = []) {
  return build(
    messages.flatMap((message) => (message.role === "assistant" ? [message] : [])),
    providers,
  )
}

export function getSessionContextFromRecords(records: SessionMessageInfo[] = [], providers: Provider[] = []) {
  return build(
    records.flatMap((record) =>
      record.type === "assistant" && record.tokens
        ? [
            {
              id: record.id,
              providerID: record.model?.providerID ?? "",
              modelID: record.model?.id ?? "",
              tokens: record.tokens,
              ...(record.contextBudget
                ? {
                    contextBudget: {
                      input: record.contextBudget.input,
                      window: record.contextBudget.window,
                      limit: record.contextBudget.limit,
                    },
                  }
                : {}),
              time: record.time,
            },
          ]
        : [],
    ),
    providers,
  )
}

type UsageModel = {
  readonly providerID: string
  readonly id: string
  readonly turns: number
  readonly cost: number
  readonly tokens: {
    readonly input: number
    readonly output: number
    readonly cache: { readonly read: number; readonly write: number }
  }
  readonly ttftMs?: number
}

/** Lifetime usage rows, most expensive first, for the per-model panel. */
export function getUsageModelRows(usage: { readonly models?: readonly UsageModel[] } | undefined) {
  return (usage?.models ?? []).toSorted((a, b) => b.cost - a.cost || b.turns - a.turns)
}
