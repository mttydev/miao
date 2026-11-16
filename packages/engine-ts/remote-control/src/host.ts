export * as ControlHost from "./host"

import { createHash } from "node:crypto"
import { Option, Schema } from "effect"
import { ControlAgent } from "./agent"

export type Command = { readonly name: string; readonly description: string }

export type Capabilities = {
  readonly protocol: 1
  readonly features: ReadonlyArray<"attachments" | "commands">
  readonly commands: ReadonlyArray<Command>
  readonly maxPromptCharacters: number
  readonly maxAttachmentBytes: number
}

export type EngineAttachment = {
  readonly mime: string
  readonly data: string
  readonly metadata?: Readonly<Record<string, string>>
}

export type Admission = {
  readonly text: string
  readonly delivery: "steer" | "queue"
  readonly attachments: ReadonlyArray<EngineAttachment>
}

export type Limits = {
  /** Commands the host can run when a client sends `/name`. */
  readonly commands?: ReadonlyArray<string | Command>
  /** Total base64 characters accepted across one prompt's attachments. */
  readonly maxAttachmentBytes?: number
  readonly maxPromptCharacters?: number
  /** Set false for a text-only host; the client then hides the entry point. */
  readonly attachments?: boolean
}

export type Options = Limits & {
  readonly admit: (admission: Admission, request: ControlAgent.Request) => Promise<unknown>
}

/**
 * The RPC channel quotes every admitted request inside one sealed frame, and
 * the Agent refuses a frame above its 512 KiB pending-input budget. Base64
 * inflation and the envelope leave headroom below that bound.
 */
export const MAX_ATTACHMENT_BYTES = 256 * 1024
export const MAX_PROMPT_CHARACTERS = 65536
const MAX_ATTACHMENTS = 8
const MAX_COMMANDS = 256

const CommandName = Schema.String.check(
  Schema.isMinLength(1),
  Schema.isMaxLength(64),
  Schema.isPattern(/^[A-Za-z0-9_.:-]+$/),
)
const CommandEntry = Schema.Struct({
  name: CommandName,
  description: Schema.optional(Schema.String.check(Schema.isMaxLength(512))),
})
const CommandInput = Schema.Union([CommandName, CommandEntry])

const AttachmentInput = Schema.Struct({
  type: Schema.optional(Schema.String.check(Schema.isMaxLength(32))),
  name: Schema.optional(Schema.String.check(Schema.isMaxLength(255))),
  mimeType: Schema.String.check(Schema.isMinLength(1), Schema.isMaxLength(128)),
  data: Schema.String,
})
const Prompt = Schema.Struct({
  text: Schema.String,
  delivery: Schema.optional(Schema.Literals(["steer", "queue"])),
  attachments: Schema.optional(Schema.Array(AttachmentInput)),
})

/**
 * Advertise only what this host actually accepts. A client that sees no
 * `attachments` feature keeps the text-only composer, and a host that omits
 * `commands` shows no slash list.
 */
export function capabilities(options: Limits = {}): Capabilities {
  const commands = normalizeCommands(options.commands ?? [])
  const attachments = options.attachments ?? true
  return {
    protocol: 1,
    features: attachments ? ["attachments", "commands"] : ["commands"],
    commands,
    maxPromptCharacters: options.maxPromptCharacters ?? MAX_PROMPT_CHARACTERS,
    maxAttachmentBytes: options.maxAttachmentBytes ?? MAX_ATTACHMENT_BYTES,
  }
}

/** The engine treats `input_id` as the durable admission identity. */
export function inputID(operationID: string) {
  return `msg_remote_${createHash("sha256").update(operationID).digest("hex")}`
}

/** Validate a prompt against the advertised contract before admission. */
export function parsePrompt(payload: unknown, options: Limits = {}): Admission {
  const decoded = Schema.decodeUnknownOption(Prompt, { onExcessProperty: "error" })(payload)
  if (Option.isNone(decoded)) throw new ControlAgent.RequestError("invalid_request")
  const prompt = decoded.value
  const maxCharacters = options.maxPromptCharacters ?? MAX_PROMPT_CHARACTERS
  if (prompt.text.length < 1 || prompt.text.length > maxCharacters)
    throw new ControlAgent.RequestError("invalid_request")
  if ((options.attachments ?? true) === false && prompt.attachments?.length)
    throw new ControlAgent.RequestError("invalid_request")
  const limit = options.maxAttachmentBytes ?? MAX_ATTACHMENT_BYTES
  if ((prompt.attachments?.length ?? 0) > MAX_ATTACHMENTS) throw new ControlAgent.RequestError("invalid_request")
  let encoded = 0
  const attachments = (prompt.attachments ?? []).map((attachment) => {
    if (!canonicalBase64(attachment.data)) throw new ControlAgent.RequestError("invalid_request")
    encoded += attachment.data.length
    if (encoded > limit) throw new ControlAgent.RequestError("invalid_request")
    const metadata: Record<string, string> = {}
    if (attachment.name) metadata.name = attachment.name
    if (attachment.type) metadata.type = attachment.type
    if (Buffer.byteLength(JSON.stringify(metadata)) > 64 * 1024)
      throw new ControlAgent.RequestError("invalid_request")
    return {
      mime: attachment.mimeType,
      data: attachment.data,
      ...(Object.keys(metadata).length ? { metadata } : {}),
    }
  })
  return {
    text: prompt.text,
    delivery: prompt.delivery ?? "steer",
    attachments,
  }
}

export function methods(options: Options): Partial<Record<ControlAgent.Method, ControlAgent.Handler>> {
  return {
    capabilities: async () => capabilities(options),
    "session.prompt": async (request) => {
      if (!request.operationID || !request.sessionID) throw new ControlAgent.RequestError("invalid_request")
      return options.admit(parsePrompt(request.payload, options), request)
    },
  }
}

function normalizeCommands(commands: ReadonlyArray<string | Command>): Command[] {
  const seen = new Map<string, Command>()
  for (const entry of commands.slice(0, MAX_COMMANDS)) {
    const decoded = Schema.decodeUnknownOption(CommandInput)(entry)
    if (Option.isNone(decoded)) continue
    const command =
      typeof decoded.value === "string" ? { name: decoded.value, description: "" } : decoded.value
    if (!seen.has(command.name))
      seen.set(command.name, { name: command.name, description: command.description ?? "" })
  }
  return [...seen.values()]
}

function canonicalBase64(value: string) {
  if (!value || value.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(value)) return false
  return Buffer.from(value, "base64").toString("base64") === value
}
