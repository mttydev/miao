// The engine answers `session_catalog` with its own record shape (`session_id`,
// `updated_at_ms`, `parent_session_id`, ...), which is not the view model the app
// renders. One mapping layer owns that translation so a UI component never sees
// the wire shape and a shape change cannot silently render as blank rows.

/** One `session_catalog` row, exactly as the engine serializes it. */
export type EngineCatalogRow = {
  /** Fields this layer does not read stay visible rather than rejected. */
  [key: string]: unknown
  session_id?: unknown
  title?: unknown
  location?: unknown
  updated_at_ms?: unknown
  pinned_at_ms?: unknown
  archived_at_ms?: unknown
  parent_session_id?: unknown
  preview?: unknown
}

export type EngineSessionSummary = {
  id: string
  title: string
  /** Commit time of the last change, or 0 when the engine has none yet. */
  updatedAt: number
  pinned: boolean
  archived: boolean
  parentID?: string
  preview?: string
}

export class EngineSessionShapeError extends Error {
  constructor(readonly field: string) {
    super(`the engine session catalog row has no usable \`${field}\``)
    this.name = "EngineSessionShapeError"
  }
}

/**
 * Translate one catalog row. A missing required field is an error rather than a
 * dropped row: an unreadable row is a defect the caller has to see.
 */
export function catalogRowToSummary(row: EngineCatalogRow): EngineSessionSummary {
  if (typeof row.session_id !== "string" || !row.session_id) throw new EngineSessionShapeError("session_id")
  if (typeof row.title !== "string") throw new EngineSessionShapeError("title")
  const updated = typeof row.updated_at_ms === "number" && Number.isFinite(row.updated_at_ms) ? row.updated_at_ms : 0
  return {
    id: row.session_id,
    title: row.title,
    updatedAt: updated,
    pinned: typeof row.pinned_at_ms === "number",
    archived: typeof row.archived_at_ms === "number",
    ...(typeof row.parent_session_id === "string" ? { parentID: row.parent_session_id } : {}),
    ...(typeof row.preview === "string" ? { preview: row.preview } : {}),
  }
}

/** The whole catalog, newest first, with archived rows kept only on request. */
export function catalogToSummaries(
  rows: ReadonlyArray<EngineCatalogRow>,
  options: { includeArchived?: boolean } = {},
): EngineSessionSummary[] {
  return rows
    .map(catalogRowToSummary)
    .filter((session) => options.includeArchived || !session.archived)
    .sort((a, b) => b.updatedAt - a.updatedAt)
}

/** The `snapshot` record the engine returns for one Session. */
export type EngineSnapshot = {
  [key: string]: unknown
  session_id?: unknown
  title?: unknown
  cursor?: unknown
  message_count?: unknown
  has_messages?: unknown
  pending?: unknown
  approvals?: unknown
  messages?: unknown
  selection?: unknown
}

export type EngineSessionState = {
  id: string
  title: string
  /** Last committed event cursor the snapshot was taken at. */
  cursor: number
  messageCount: number
  /** Messages a viewer should render, which is fewer after an explicit clear. */
  visibleMessages: number
  /**
   * The sequence an explicit `/clear` cut the history at, or 0 while nothing
   * has been cleared. A viewer attaches to the event stream after it.
   */
  clearedThrough: number
  hasMessages: boolean
  pending: number
  approvals: number
}

/**
 * The sequence an explicit `/clear` cut the history at, or 0.
 *
 * The engine deletes nothing: it writes a compaction checkpoint so the next
 * provider prefix starts empty. A summarising checkpoint carries text and hides
 * nothing from the reader, so only an empty summary is a cut a viewer follows.
 */
function clearCut(selection: unknown) {
  if (typeof selection !== "object" || selection === null) return 0
  if (Reflect.get(selection, "summary") !== "") return 0
  const through = Reflect.get(selection, "through_seq")
  return typeof through === "number" && Number.isSafeInteger(through) ? through : 0
}

/** One snapshot message's sequence, or 0 when it carries none. */
function messageSeq(message: unknown) {
  if (typeof message !== "object" || message === null) return 0
  const seq = Reflect.get(message, "seq")
  return typeof seq === "number" ? seq : 0
}

/**
 * Translate the Session header a snapshot carries. Transcript entries are not
 * part of this record's contract here: the durable view is the committed event
 * stream, so this layer only keeps what the header can answer.
 */
export function snapshotToState(snapshot: EngineSnapshot): EngineSessionState {
  if (typeof snapshot.session_id !== "string" || !snapshot.session_id)
    throw new EngineSessionShapeError("session_id")
  if (typeof snapshot.title !== "string") throw new EngineSessionShapeError("title")
  const count = (value: unknown) => (Array.isArray(value) ? value.length : 0)
  const clearedThrough = clearCut(snapshot.selection)
  const messages = Array.isArray(snapshot.messages) ? snapshot.messages : undefined
  return {
    id: snapshot.session_id,
    title: snapshot.title,
    cursor: typeof snapshot.cursor === "number" ? snapshot.cursor : 0,
    messageCount: typeof snapshot.message_count === "number" ? snapshot.message_count : 0,
    visibleMessages: messages
      ? messages.filter((message) => messageSeq(message) > clearedThrough).length
      : typeof snapshot.message_count === "number"
        ? snapshot.message_count
        : 0,
    clearedThrough,
    hasMessages: snapshot.has_messages === true,
    pending: count(snapshot.pending),
    approvals: count(snapshot.approvals),
  }
}
