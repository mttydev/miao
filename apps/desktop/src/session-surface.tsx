import {
  createEngineReader,
  type EngineEvent,
  type EngineSessionState,
  type EngineSessionSummary,
  useLanguage,
} from "@miao/app"
import { Markdown } from "@miao/session-ui/markdown"
import { Icon } from "@miao/ui/v2/icon"
import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount } from "solid-js"
import { PinIcon, SendIcon, SparklesIcon, StopIcon, TrashIcon } from "./icons"

/**
 * One local engine's Sessions and the conversation on the selected one, with a
 * WorkBuddy-style welcome screen while the Session has no turns yet.
 *
 * This is deliberately not `AppInterface` yet. The shared renderer speaks the
 * HTTP API, while the shipped binary exposes the engine's own control plane;
 * this surface talks that control plane directly through `createEngineReader`
 * and renders the two event kinds a conversation is made of.
 */

type Turn = { role: "user" | "assistant" | "tool"; text: string }

const SUGGESTION_KEYS = [
  "desktop.surface.suggestion.data",
  "desktop.surface.suggestion.doc",
  "desktop.surface.suggestion.research",
  "desktop.surface.suggestion.report",
  "desktop.surface.suggestion.code",
  "desktop.surface.suggestion.plan",
] as const

// The shell styles the window with an overlay title bar on macOS, so the
// traffic lights sit on top of the web content and the top-left strip must
// both clear them and act as the drag handle.
const isMac = navigator.userAgent.includes("Macintosh")
const titlebarInset = isMac ? "pt-12" : "pt-3"

const isToday = (updatedAt: number) => {
  if (updatedAt === 0) return true
  const then = new Date(updatedAt)
  const now = new Date()
  return then.toDateString() === now.toDateString()
}

const dayLabel = (updatedAt: number, justNow: string) =>
  updatedAt === 0
    ? justNow
    : new Date(updatedAt).toLocaleDateString("zh-CN", { month: "long", day: "numeric" })

/** The conversation a viewer reads, derived from the raw event stream. */
const turns = (events: EngineEvent[]): Turn[] => {
  const sorted = [...events].sort((a, b) => a.seq - b.seq)
  const out: Turn[] = []
  for (const event of sorted) {
    const data = (event.data ?? {}) as Record<string, unknown>
    if (event.kind === "input.promoted" && typeof data.prompt === "string") {
      out.push({ role: "user", text: data.prompt })
    } else if (event.kind === "message.committed" && data.role === "assistant") {
      const parts = Array.isArray(data.content) ? data.content : []
      const text = parts
        .filter((part) => (part as { type?: string })?.type === "text")
        .map((part) => String((part as { text?: string }).text ?? ""))
        .join("")
      if (text) out.push({ role: "assistant", text })
    } else if (event.kind === "message.committed" && data.role === "tool") {
      const parts = Array.isArray(data.content) ? data.content : []
      const text = parts
        .map((part) => {
          const result = (part ?? {}) as { type?: string; content?: unknown }
          if (result.type !== "tool_result") return ""
          if (typeof result.content === "string") return result.content
          if (Array.isArray(result.content))
            return result.content
              .filter((inner) => (inner as { type?: string })?.type === "text")
              .map((inner) => String((inner as { text?: string }).text ?? ""))
              .join("")
          return ""
        })
        .filter(Boolean)
        .join("\n")
      if (text) out.push({ role: "tool" as Turn["role"], text })
    }
  }
  return out
}

/** Which model and agent the last turn answered with. */
const identity = (events: EngineEvent[]): { model?: string; agent?: string } => {
  const sorted = [...events].sort((a, b) => a.seq - b.seq)
  for (let index = sorted.length - 1; index >= 0; index -= 1) {
    const event = sorted[index]
    if (event.kind !== "assistant.identity") continue
    const data = (event.data ?? {}) as {
      agent?: string
      provider?: { model?: string }
    }
    return { model: data.provider?.model, agent: data.agent }
  }
  return {}
}

/** The streamed answer in flight: deltas after the last committed assistant turn. */
const streamingText = (events: EngineEvent[]): string => {
  const sorted = [...events].sort((a, b) => a.seq - b.seq)
  let cut = 0
  for (const event of sorted)
    if (event.kind === "message.committed" && (event.data as { role?: string })?.role === "assistant")
      cut = event.seq
  return sorted
    .filter((event) => event.seq > cut && event.kind === "provider.delta")
    .map((event) => {
      const data = event.data
      if (typeof data === "string") return data
      const part = (data ?? {}) as Record<string, unknown>
      return String(part.text ?? part.delta ?? "")
    })
    .join("")
}

/** Runs that ended failed, after the last committed answer. */
const failures = (events: EngineEvent[]): string[] => {
  const sorted = [...events].sort((a, b) => a.seq - b.seq)
  let cut = 0
  for (const event of sorted)
    if (event.kind === "message.committed" && (event.data as { role?: string })?.role === "assistant")
      cut = event.seq
  return sorted
    .filter((event) => event.seq > cut && event.kind === "run.finished")
    .map((event) => String((event.data as { reason?: string })?.reason ?? ""))
    .filter((reason) => reason !== "" && reason !== "completed")
}

/** Tool calls this run started, after the last committed answer. */
const toolRows = (events: EngineEvent[]): Array<{ tool: string; resource: string }> => {
  const sorted = [...events].sort((a, b) => a.seq - b.seq)
  let cut = 0
  for (const event of sorted)
    if (event.kind === "message.committed" && (event.data as { role?: string })?.role === "assistant")
      cut = event.seq
  return sorted
    .filter((event) => event.seq > cut && event.kind === "tool.started")
    .map((event) => {
      const data = (event.data ?? {}) as Record<string, unknown>
      return { tool: String(data.tool ?? "tool"), resource: String(data.resource ?? "") }
    })
}

/** A run is open when the stream's last lifecycle kind started one. */
const running = (events: EngineEvent[]): boolean => {
  const sorted = [...events].sort((a, b) => a.seq - b.seq)
  for (let index = sorted.length - 1; index >= 0; index -= 1) {
    const kind = sorted[index].kind
    if (kind === "run.started") return true
    if (kind === "run.finished" || kind === "stop") return false
  }
  return false
}

/** Copies one message's text; the label flips to a confirmation for a moment. */
function CopyButton(props: { text: string }) {
  const language = useLanguage()
  const [copied, setCopied] = createSignal(false)
  return (
    <button
      type="button"
      class="mt-1 inline-flex items-center gap-1 text-10-regular text-v2-text-text-faint opacity-0 transition-opacity hover:text-v2-text-text-base group-hover:opacity-100 focus:opacity-100"
      onClick={() => {
        void navigator.clipboard.writeText(props.text).then(() => {
          setCopied(true)
          setTimeout(() => setCopied(false), 1500)
        })
      }}
    >
      <Icon name="outline-copy" size="small" />
      {copied() ? language.t("desktop.surface.action.copied") : language.t("desktop.surface.action.copy")}
    </button>
  )
}

export function SessionSurface(props: { url: string; token: string }) {
  const reader = createEngineReader({ url: props.url, token: props.token })
  const [sessions, setSessions] = createSignal<EngineSessionSummary[]>([])
  const [selected, setSelected] = createSignal<string>()
  const [state, setState] = createSignal<EngineSessionState>()
  const [events, setEvents] = createSignal<EngineEvent[]>([])
  const [live, setLive] = createSignal(false)
  const [error, setError] = createSignal<string>()
  const [draft, setDraft] = createSignal("")
  const [attached, setAttached] = createSignal<Array<{ name: string; data: string; mime: string }>>([])
  const [busy, setBusy] = createSignal(false)
  const [query, setQuery] = createSignal("")
  const [menu, setMenu] = createSignal<{ session: EngineSessionSummary; x: number; y: number }>()
  const [dialog, setDialog] = createSignal<{ kind: "rename" | "delete"; session: EngineSessionSummary; title: string }>()
  const language = useLanguage()

  // Pinned rows float above the time buckets, the same order the TUI sidebar keeps.
  // The filter runs on title only: that is the one field every row always has.
  const groups = createMemo(() => {
    const needle = query().trim().toLowerCase()
    const rows = sessions().filter((row) => !needle || row.title.toLowerCase().includes(needle))
    const pinned = rows.filter((row) => row.pinned)
    const others = rows.filter((row) => !row.pinned)
    return [
      { label: language.t("desktop.surface.group.pinned"), rows: pinned },
      { label: language.t("desktop.surface.group.today"), rows: others.filter((row) => isToday(row.updatedAt)) },
      { label: language.t("desktop.surface.group.earlier"), rows: others.filter((row) => !isToday(row.updatedAt)) },
    ]
  })

  // Questions this window already answered; the effect below must not
  // re-answer the same question.requested on every repaint.
  const answered = new Set<string>()

  // Unattended like the TUI's --auto: a question is answered with the first
  // option of each asked question instead of blocking the conversation.
  createEffect(() => {
    const sessionID = selected()
    if (!sessionID) return
    for (const event of events()) {
      if (event.kind !== "question.requested") continue
      const data = (event.data ?? {}) as {
        question_id?: string
        input_hash?: string
        input?: { questions?: Array<{ options?: Array<{ label?: string }> }> }
      }
      const id = data.question_id
      if (!id || answered.has(id)) continue
      answered.add(id)
      const questions = Array.isArray(data.input?.questions) ? data.input.questions : []
      const answers = questions.map((question) => {
        const option = question?.options?.[0]
        return option?.label ? [String(option.label)] : []
      })
      void reader.answer(sessionID, { question_id: id, input_hash: String(data.input_hash ?? ""), answers }).catch(fail)
    }
  })
  let scroller: HTMLDivElement | undefined
  let composer: HTMLTextAreaElement | undefined

  // Follow the tail while the reader is already near it; a scrollback the
  // operator is reading must not be dragged back down by a streamed delta.
  createEffect(() => {
    events()
    const element = scroller
    if (element && element.scrollHeight - element.scrollTop - element.clientHeight < 160) {
      queueMicrotask(() => {
        if (scroller) scroller.scrollTop = scroller.scrollHeight
      })
    }
  })

  const fail = (cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause))

  let detach: (() => void) | undefined

  const open = async (session: EngineSessionSummary) => {
    detach?.()
    detach = undefined
    setSelected(session.id)
    setState(undefined)
    setEvents([])
    setLive(false)
    setError(undefined)
    // `/clear` keeps its rows for audit behind an empty-summary checkpoint, and
    // the snapshot names the cut. Attaching after it is what keeps a reopened
    // window from replaying the cleared turns; `0` is a Session nobody cleared.
    const opened = await reader
      .snapshot(session.id)
      .catch((cause: unknown) => {
        fail(cause)
        return undefined
      })
    if (opened) setState(opened)
    const after = opened?.clearedThrough ?? 0
    const merge = (incoming: EngineEvent[]) =>
      setEvents((current) => {
        const seen = new Set(current.map((event) => event.seq))
        return [...current, ...incoming.filter((event) => !seen.has(event.seq))]
      })
    const unsubscribe = await reader
      .subscribe(
        session.id,
        (event) => {
          if (event.seq > after) merge([event])
        },
        after,
      )
      .catch((cause: unknown) => {
        fail(cause)
        return undefined
      })
    detach = unsubscribe
    await reader
      .events(session.id, after)
      .then(merge)
      .catch(fail)
    setLive(true)
  }

  const closeSelected = () => {
    detach?.()
    detach = undefined
    setSelected(undefined)
    setState(undefined)
    setEvents([])
  }

  const newSession = async () => {
    const id = `ses_${crypto.randomUUID().replaceAll("-", "").slice(0, 24)}`
    await reader.initialize(id, "新的任务").catch(fail)
    const list = await reader.sessions().catch(() => sessions())
    setSessions(list)
    await open(
      list.find((session) => session.id === id) ?? {
        id,
        title: "新的任务",
        updatedAt: 0,
        pinned: false,
        archived: false,
      },
    )
    composer?.focus()
  }

  const attach = async (files: FileList) => {
    const next: Array<{ name: string; data: string; mime: string }> = []
    for (const file of Array.from(files)) {
      if (!file.type) continue
      const data = await new Promise<string>((resolve, reject) => {
        const reader = new FileReader()
        reader.onload = () => resolve(String(reader.result).split(",")[1] ?? "")
        reader.onerror = () => reject(reader.error)
        reader.readAsDataURL(file)
      })
      next.push({ name: file.name, data, mime: file.type })
    }
    if (next.length > 0) setAttached((current) => [...current, ...next])
  }

  const refreshSessions = async () => {
    const list = await reader.sessions({ includeArchived: false })
    setSessions(list)
    return list
  }

  const togglePin = async (session: EngineSessionSummary) => {
    setMenu(undefined)
    try {
      await reader.pin(session.id, !session.pinned)
      await refreshSessions()
    } catch (cause) {
      fail(cause)
    }
  }

  const toggleArchive = async (session: EngineSessionSummary) => {
    setMenu(undefined)
    try {
      await reader.archive(session.id, !session.archived)
      await refreshSessions()
      if (session.id === selected()) closeSelected()
    } catch (cause) {
      fail(cause)
    }
  }

  const confirmDialog = async () => {
    const current = dialog()
    if (!current) return
    setDialog(undefined)
    if (current.kind === "rename") {
      const title = current.title.trim()
      if (!title || title === current.session.title) return
      try {
        await reader.rename(current.session.id, title)
        await refreshSessions()
        if (current.session.id === selected()) setState((previous) => (previous ? { ...previous, title } : previous))
      } catch (cause) {
        fail(cause)
      }
      return
    }
    try {
      await reader.remove(current.session.id)
      if (current.session.id === selected()) closeSelected()
      await refreshSessions()
    } catch (cause) {
      fail(cause)
    }
  }

  const renameSession = (session: EngineSessionSummary) => {
    setMenu(undefined)
    setDialog({ kind: "rename", session, title: session.title })
  }

  const submit = async (text: string) => {
    const trimmed = text.trim()
    if ((!trimmed && attached().length === 0) || busy()) return
    // The two commands a conversation needs on day one, matching the TUI's
    // spelling. Everything else waits for the full command surface.
    if (trimmed === "/new") {
      setDraft("")
      await newSession()
      return
    }
    if (trimmed === "/clear") {
      const sessionID = selected()
      if (!sessionID) return
      setDraft("")
      try {
        await reader.clear(sessionID)
        // The engine checkpoints the history and keeps every row; the next
        // snapshot carries the cut at which the pane has to attach again, so the
        // transcript empties now and stays empty when the Session reopens.
        setEvents([])
        await reader.snapshot(sessionID).then(setState).catch(fail)
      } catch (cause) {
        fail(cause)
      }
      return
    }
    if (!selected()) await newSession()
    const sessionID = selected()
    if (!sessionID) return
    setBusy(true)
    try {
      await reader.admit(sessionID, trimmed, attached().map(({ data, mime }) => ({ data, mime })))
      // Clear only after the engine has durably admitted the prompt, so a
      // failed send keeps the text and attachments for a retry.
      setDraft("")
      setAttached([])
    } catch (cause) {
      fail(cause)
    } finally {
      setBusy(false)
    }
  }

  onMount(async () => {
    const list = await reader.sessions().catch((cause: unknown) => {
      fail(cause)
      return [] as EngineSessionSummary[]
    })
    setSessions(list)
    if (list[0]) await open(list[0])
  })

  onCleanup(() => {
    detach?.()
    void reader.close()
  })

  const conversation = () => turns(events())
  // A Session that was cleared holds no turns but still holds a durable
  // history, so it keeps the conversation screen instead of falling back to
  // the first-run welcome — the same screen the TUI leaves after `/clear`.
  // With no Session at all the window shows the welcome: that is the screen a
  // first-time operator opens into.
  const welcome = () =>
    !selected() || (conversation().length === 0 && !running(events()) && !state()?.hasMessages)

  const sendDisabled = () => (!draft().trim() && attached().length === 0) || busy()

  return (
    <div
      class="flex h-dvh bg-v2-background-bg-deep text-v2-text-text-base"
      onKeyDown={(event) => {
        if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "n") {
          event.preventDefault()
          void newSession()
        }
      }}
      onClick={() => setMenu(undefined)}
    >
      <aside class={`flex w-[264px] shrink-0 flex-col border-r border-v2-border-border-weak ${titlebarInset}`}>
        <div class="flex items-center gap-2.5 px-4 pb-3" data-tauri-drag-region={isMac}>
          <span class="flex size-7 items-center justify-center rounded-lg bg-v2-background-bg-accent text-v2-text-text-inverse">
            <SparklesIcon class="size-4" />
          </span>
          <span class="flex min-w-0 flex-col">
            <span class="text-14-medium leading-5">miao</span>
            <span class="text-10-regular leading-4 text-v2-text-text-faint">local engine</span>
          </span>
        </div>
        <div class="px-3">
          <button
            type="button"
            class="flex w-full items-center gap-2 rounded-lg bg-v2-background-bg-accent px-3 py-2 text-left text-13-medium text-v2-text-text-inverse transition-opacity hover:opacity-90"
            onClick={() => void newSession()}
          >
            <Icon name="plus" size="small" />
            {language.t("desktop.surface.sidebar.newSession")}
          </button>
          <div class="relative mt-2">
            <span class="absolute left-2.5 top-1/2 -translate-y-1/2 text-v2-icon-icon-muted">
              <Icon name="magnifying-glass" size="small" />
            </span>
            <input
              type="search"
              class="w-full rounded-lg border border-v2-border-border-weak bg-v2-background-bg-layer-01 py-1.5 pl-8 pr-2 text-12-regular outline-none placeholder:text-v2-text-text-faint"
              placeholder={language.t("desktop.surface.sidebar.search")}
              value={query()}
              onInput={(event) => setQuery(event.currentTarget.value)}
            />
          </div>
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto pb-2">
          <For each={groups()}>
            {(group) => (
              <Show when={group.rows.length > 0}>
                <div class="px-4 pb-1 pt-4 text-10-medium tracking-wide text-v2-text-text-faint">{group.label}</div>
                <For each={group.rows}>
                  {(session) => (
                    <button
                      type="button"
                      class="mx-2 flex w-[calc(100%-1rem)] items-center justify-between gap-2 rounded-lg px-2.5 py-1.5 text-left transition-colors hover:bg-v2-background-bg-layer-01"
                      classList={{ "bg-v2-background-bg-layer-01": session.id === selected() }}
                      onClick={() => void open(session)}
                      onContextMenu={(event) => {
                        event.preventDefault()
                        setMenu({ session, x: event.clientX, y: event.clientY })
                      }}
                    >
                      <span class="flex min-w-0 flex-1 items-center gap-1.5">
                        <Show when={session.pinned}>
                          <PinIcon class="size-3 shrink-0 text-v2-icon-icon-muted" />
                        </Show>
                        <span class="min-w-0 flex-1 truncate text-12-regular">{session.title}</span>
                      </span>
                      <span class="shrink-0 text-10-regular text-v2-text-text-faint">{dayLabel(session.updatedAt, language.t("desktop.surface.group.justNow"))}</span>
                    </button>
                  )}
                </For>
              </Show>
            )}
          </For>
          <Show when={groups().every((group) => group.rows.length === 0)}>
            <p class="px-4 py-3 text-11-regular text-v2-text-text-faint">{query() ? language.t("desktop.surface.sidebar.noMatch") : language.t("desktop.surface.sidebar.empty")}</p>
          </Show>
        </div>
        <div class="border-t border-v2-border-border-weak px-4 py-3 text-10-regular text-v2-text-text-faint">
          {language.t("desktop.surface.sidebar.engineNote")}
        </div>
      </aside>
      <main class="flex min-w-0 flex-1 flex-col">
        <Show
          when={!welcome()}
          fallback={
            <div class={`flex min-h-0 flex-1 flex-col ${titlebarInset}`}>
              <div class="flex min-h-0 flex-1 flex-col items-center justify-center px-8 pb-8">
              <span class="mb-5 flex size-12 items-center justify-center rounded-2xl bg-v2-background-bg-accent text-v2-text-text-inverse shadow-sm">
                <SparklesIcon class="size-6" />
              </span>
              <h1 class="text-24-medium">{language.t("desktop.surface.welcome.title")}</h1>
              <p class="mt-2 text-13-regular text-v2-text-text-muted">{language.t("desktop.surface.welcome.subtitle")}</p>
              <div class="mt-8 flex w-full max-w-2xl flex-col rounded-2xl border border-v2-border-border-weak bg-v2-background-bg-layer-01 shadow-sm transition-colors focus-within:border-v2-border-border-muted">
                <textarea
                  ref={composer}
                  class="max-h-48 min-h-28 w-full resize-none bg-transparent px-4 pt-3.5 text-13-regular outline-none placeholder:text-v2-text-text-faint"
                  placeholder={language.t("desktop.surface.welcome.placeholder")}
                  value={draft()}
                  rows={4}
                  onInput={(event) => setDraft(event.currentTarget.value)}
                  onPaste={(event) => {
                    if (event.clipboardData?.files.length) {
                      event.preventDefault()
                      void attach(event.clipboardData.files)
                    }
                  }}
                  onKeyDown={(event) => {
                    if (event.key === "Escape") event.currentTarget.blur()
                    if (event.key === "Enter" && !event.shiftKey) {
                      event.preventDefault()
                      void submit(draft())
                    }
                  }}
                />
                <div class="flex items-center justify-between px-4 pb-3.5">
                  <span class="text-11-regular text-v2-text-text-faint">{language.t("desktop.surface.welcome.hint")}</span>
                  <button
                    type="button"
                    class="flex h-8 items-center gap-1.5 rounded-full bg-v2-background-bg-accent px-4 text-12-medium text-v2-text-text-inverse transition-opacity hover:opacity-90 disabled:opacity-40"
                    disabled={sendDisabled()}
                    onClick={() => void submit(draft())}
                  >
                    {busy() ? language.t("desktop.surface.action.sending") : language.t("desktop.surface.action.send")}
                    <SendIcon class="size-3.5" />
                  </button>
                </div>
              </div>
              <div class="mt-5 flex flex-wrap justify-center gap-2">
                <For each={SUGGESTION_KEYS}>
                  {(suggestion) => (
                    <button
                      type="button"
                      class="rounded-full border border-v2-border-border-weak px-3.5 py-1.5 text-12-regular text-v2-text-text-muted transition-colors hover:bg-v2-background-bg-layer-01 hover:text-v2-text-text-base"
                      onClick={() => void submit(suggestion)}
                    >
                      {language.t(suggestion)}
                    </button>
                  )}
                </For>
              </div>
              </div>
              <div class="pb-3 pt-2 text-center text-11-regular text-v2-text-text-faint">{language.t("desktop.surface.disclaimer")}</div>
            </div>
          }
        >
          <header
            class={`flex h-12 shrink-0 items-center gap-2 border-b border-v2-border-border-weak px-4 ${isMac ? "pl-20" : ""}`}
            data-tauri-drag-region={isMac}
          >
            <Show when={state()} fallback={<span class="text-12-regular text-v2-text-text-faint">{language.t("desktop.surface.header.selectTask")}</span>}>
              {(current) => (
                <div class="flex min-w-0 flex-1 items-center gap-2">
                  <h1 class="min-w-0 truncate text-13-medium" data-tauri-drag-region={isMac}>
                    {current().title}
                  </h1>
                  <Show when={identity(events()).model || identity(events()).agent}>
                    <span class="hidden shrink-0 text-11-regular text-v2-text-text-faint lg:inline">
                      {[identity(events()).model, identity(events()).agent].filter(Boolean).join(" · ")}
                    </span>
                  </Show>
                  <Show when={current().pending > 0}>
                    <span class="inline-flex shrink-0 items-center rounded-full bg-v2-background-bg-layer-01 px-2 py-0.5 text-10-regular text-v2-text-text-muted">
                      {language.t("desktop.surface.header.pending", { count: current().pending })}
                    </span>
                  </Show>
                  <Show when={current().approvals > 0}>
                    <span class="inline-flex shrink-0 items-center rounded-full bg-v2-background-bg-layer-01 px-2 py-0.5 text-10-regular text-v2-text-text-muted">
                      {language.t("desktop.surface.header.approvals", { count: current().approvals })}
                    </span>
                  </Show>
                  <Show when={live()}>
                    <span class="inline-flex shrink-0 items-center gap-1 rounded-full bg-v2-background-bg-layer-01 px-2 py-0.5 text-10-regular text-v2-text-text-muted">
                      <Icon name="status-active" size="small" />
                      {language.t("desktop.surface.header.live")}
                    </span>
                  </Show>
                </div>
              )}
            </Show>
          </header>
          <div ref={scroller} class="min-h-0 flex-1 overflow-y-auto">
            <div class="mx-auto w-full max-w-3xl px-6 py-4">
              <For each={conversation()}>
                {(turn) => (
                  <Show
                    when={turn.role === "user"}
                    fallback={
                      <Show
                        when={turn.role === "assistant"}
                        fallback={
                          <div class="py-1.5 text-11-regular text-v2-text-text-faint">
                            <Markdown text={turn.text} />
                          </div>
                        }
                      >
                        <div class="group py-2 pr-6 text-13-regular leading-relaxed">
                          <Markdown text={turn.text} />
                          <CopyButton text={turn.text} />
                        </div>
                      </Show>
                    }
                  >
                    <div class="group flex flex-col items-end py-2">
                      <div class="max-w-[75%] whitespace-pre-wrap rounded-2xl rounded-br-md bg-v2-background-bg-layer-01 px-4 py-2.5 text-13-regular">
                        {turn.text}
                      </div>
                      <CopyButton text={turn.text} />
                    </div>
                  </Show>
                )}
              </For>
              <For each={toolRows(events())}>
                {(row) => (
                  <div class="py-0.5">
                    <span class="inline-flex max-w-full items-center gap-1.5 rounded-md bg-v2-background-bg-layer-01 px-2 py-1 text-10-regular text-v2-text-text-muted">
                      <span class="font-medium">{row.tool}</span>
                      <span class="min-w-0 truncate opacity-70">{row.resource}</span>
                    </span>
                  </div>
                )}
              </For>
              <Show when={streamingText(events())}>
                {(text) => (
                  <div class="py-2 pr-6 text-13-regular leading-relaxed">
                    <Markdown text={text()} streaming={true} />
                  </div>
                )}
              </Show>
              <For each={failures(events())}>
                {(reason) => (
                  <div class="my-1 inline-flex items-center gap-1.5 rounded-lg bg-v2-background-bg-layer-01 px-2.5 py-1.5 text-11-regular text-v2-state-fg-danger">
                    {language.t("desktop.surface.transcript.runFailed", { reason })}
                  </div>
                )}
              </For>
              <Show when={running(events()) && !streamingText(events())}>
                <div class="flex items-center gap-2 py-2 text-12-regular text-v2-text-text-faint">
                  <span class="size-1.5 animate-pulse rounded-full bg-v2-background-bg-accent" />
                  {language.t("desktop.surface.transcript.thinking")}
                </div>
              </Show>
            </div>
          </div>
          <Show when={error()}>
            {(message) => (
              <div class="mx-4 mb-2 flex items-start gap-2 rounded-lg border border-v2-border-border-weak bg-v2-background-bg-layer-01 px-3 py-2 text-11-regular text-v2-state-fg-danger">
                <span class="min-w-0 flex-1 break-words" role="alert">{message()}</span>
                <button
                  type="button"
                  class="shrink-0 opacity-70 hover:opacity-100"
                  aria-label={language.t("desktop.surface.action.close")}
                  onClick={() => setError(undefined)}
                >
                  <Icon name="xmark-small" size="small" />
                </button>
              </div>
            )}
          </Show>
          <div class="shrink-0 px-4 pb-1">
            <div class="flex items-end gap-2 rounded-2xl border border-v2-border-border-weak bg-v2-background-bg-layer-01 px-3 py-2 transition-colors focus-within:border-v2-border-border-muted">
              <textarea
                class="max-h-40 min-h-9 flex-1 resize-none bg-transparent py-1.5 text-13-regular outline-none placeholder:text-v2-text-text-faint"
                placeholder={language.t("desktop.surface.composer.placeholder")}
                value={draft()}
                rows={2}
                onInput={(event) => setDraft(event.currentTarget.value)}
                onKeyDown={(event) => {
                  if (event.key === "Escape") {
                    event.preventDefault()
                    const sessionID = selected()
                    if (sessionID && running(events())) void reader.cancel(sessionID).catch(fail)
                    return
                  }
                  if (event.key === "Enter" && !event.shiftKey) {
                    event.preventDefault()
                    void submit(draft())
                  }
                }}
              />
              <button
                type="button"
                class="flex h-8 shrink-0 items-center gap-1.5 rounded-full px-3.5 text-12-medium transition-opacity hover:opacity-90 disabled:opacity-40"
                classList={{
                  "bg-v2-background-bg-accent text-v2-text-text-inverse": !running(events()),
                  "border border-v2-border-border-weak text-v2-state-fg-danger": running(events()),
                }}
                disabled={!draft().trim() && !running(events())}
                onClick={() => {
                  const sessionID = selected()
                  if (running(events()) && sessionID) {
                    void reader.cancel(sessionID).catch(fail)
                    return
                  }
                  void submit(draft())
                }}
              >
                <Show
                  when={!running(events())}
                  fallback={
                    <>
                      <StopIcon class="size-3.5" />
                      {language.t("desktop.surface.action.stop")}
                    </>
                  }
                >
                  <>
                    {busy() ? language.t("desktop.surface.action.sending") : language.t("desktop.surface.action.send")}
                    <SendIcon class="size-3.5" />
                  </>
                </Show>
              </button>
            </div>
          </div>
          <div class="shrink-0 pb-2 pt-1 text-center text-10-regular text-v2-text-text-faint">
            {language.t("desktop.surface.disclaimer")}
          </div>
        </Show>
      </main>
      <Show when={menu()}>
        {(current) => (
          <div
            role="menu"
            class="fixed z-50 min-w-44 rounded-xl border border-v2-border-border-weak bg-v2-background-bg-layer-01 py-1 text-12-regular shadow-lg"
            style={{ left: `${current().x}px`, top: `${current().y}px` }}
            onClick={(event) => event.stopPropagation()}
          >
            <button
              type="button"
              role="menuitem"
              class="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-v2-background-bg-layer-02"
              onClick={() => void togglePin(current().session)}
            >
              <PinIcon class="size-3.5 text-v2-icon-icon-muted" />
              {current().session.pinned ? language.t("desktop.surface.action.unpin") : language.t("desktop.surface.action.pin")}
            </button>
            <button
              type="button"
              role="menuitem"
              class="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-v2-background-bg-layer-02"
              onClick={() => renameSession(current().session)}
            >
              <Icon name="edit" size="small" />
              {language.t("desktop.surface.action.rename")}
            </button>
            <button
              type="button"
              role="menuitem"
              class="flex w-full items-center gap-2 px-3 py-1.5 text-left hover:bg-v2-background-bg-layer-02"
              onClick={() => void toggleArchive(current().session)}
            >
              <Icon name="archive" size="small" />
              {current().session.archived ? language.t("desktop.surface.action.restore") : language.t("desktop.surface.action.archive")}
            </button>
            <button
              type="button"
              role="menuitem"
              class="flex w-full items-center gap-2 px-3 py-1.5 text-left text-v2-state-fg-danger hover:bg-v2-background-bg-layer-02"
              onClick={() => {
                setMenu(undefined)
                setDialog({ kind: "delete", session: current().session, title: current().session.title })
              }}
            >
              <TrashIcon class="size-3.5" />
              {language.t("desktop.surface.action.delete")}
            </button>
          </div>
        )}
      </Show>
      <Show when={dialog()}>
        {(current) => (
          <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/40" onClick={() => setDialog(undefined)}>
            <form
              class="w-96 max-w-[calc(100vw-4rem)] rounded-2xl border border-v2-border-border-weak bg-v2-background-bg-base p-5 shadow-xl"
              onClick={(event) => event.stopPropagation()}
              onSubmit={(event) => {
                event.preventDefault()
                void confirmDialog()
              }}
            >
              <Show
                when={current().kind === "rename"}
                fallback={
                  <>
                    <h2 class="text-14-medium">{language.t("desktop.surface.dialog.deleteTitle")}</h2>
                    <p class="mt-2 text-12-regular leading-relaxed text-v2-text-text-muted">
                      {language.t("desktop.surface.dialog.deleteWarning", { title: current().title })}
                    </p>
                  </>
                }
              >
                <h2 class="text-14-medium">{language.t("desktop.surface.dialog.renameTitle")}</h2>
                <input
                  class="mt-3 w-full rounded-lg border border-v2-border-border-weak bg-v2-background-bg-deep px-3 py-2 text-13-regular outline-none focus:border-v2-border-border-muted"
                  value={current().title}
                  autofocus
                  onInput={(event) => {
                    const title = event.currentTarget.value
                    setDialog((previous) => (previous ? { ...previous, title } : previous))
                  }}
                />
              </Show>
              <div class="mt-5 flex justify-end gap-2">
                <button
                  type="button"
                  class="rounded-full px-4 py-1.5 text-12-medium text-v2-text-text-muted hover:bg-v2-background-bg-layer-02"
                  onClick={() => setDialog(undefined)}
                >
                  {language.t("desktop.surface.action.cancel")}
                </button>
                <button
                  type="submit"
                  class="rounded-full px-4 py-1.5 text-12-medium transition-opacity hover:opacity-90"
                  classList={{
                    "border border-v2-border-border-weak text-v2-state-fg-danger": current().kind === "delete",
                    "bg-v2-background-bg-accent text-v2-text-text-inverse": current().kind === "rename",
                  }}
                >
                  {current().kind === "delete" ? language.t("desktop.surface.action.delete") : language.t("desktop.surface.action.save")}
                </button>
              </div>
            </form>
          </div>
        )}
      </Show>
    </div>
  )
}
