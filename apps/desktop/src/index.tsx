import { LanguageProvider, loadInitialLocale, PlatformProvider, type Platform, useLanguage } from "@miao/app"
import "@miao/app/index.css"
import { ThemeProvider } from "@miao/ui/theme/context"
import { Channel, invoke } from "@tauri-apps/api/core"
import { open } from "@tauri-apps/plugin-dialog"
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification"
import { openUrl } from "@tauri-apps/plugin-opener"
import { relaunch } from "@tauri-apps/plugin-process"
import { createResource, Show } from "solid-js"
import { render } from "solid-js/web"
import { SessionSurface } from "./session-surface"

// The shell hands the renderer one sidecar and nothing else: the loopback URL
// plus the credentials it generated for this launch. Every other capability the
// app needs is the engine's own protocol.
type ServerReadyData = { url: string; username?: string | null; password?: string | null }

const platform: Platform = {
  platform: "desktop",
  openExternal: (url) => void openUrl(url),
  restart: () => relaunch(),
  notify: async (title, description) => {
    if (!(await isPermissionGranted()) && (await requestPermission()) !== "granted") return
    sendNotification({ title, body: description })
  },
  openDirectoryPickerDialog: (options) =>
    open({ directory: true, multiple: options?.multiple ?? false, title: options?.title }),
}

const isTauri = "__TAURI_INTERNALS__" in window

function awaitInitialization() {
  // Outside the shell (plain `vite dev` in a browser) there is no sidecar to
  // await; hand the surface a dead URL so the UI still renders for design work.
  if (import.meta.env.DEV && !isTauri)
    return Promise.resolve<ServerReadyData>({ url: "http://127.0.0.1:4999", username: null, password: "dev" })
  return invoke<ServerReadyData>("await_initialization", { events: new Channel() })
}

function Desktop() {
  const [ready, { refetch }] = createResource(awaitInitialization)
  const failure = () => {
    const cause = ready.error
    return cause instanceof Error ? cause.message : String(cause ?? "")
  }

  return (
    <Show
      when={ready.latest}
      fallback={
        <Show when={!ready.error} fallback={<StartupError message={failure()} onRetry={() => void refetch()} />}>
          <div class="h-dvh bg-v2-background-bg-deep" />
        </Show>
      }
    >
      {(data) => (
        <PlatformProvider value={platform}>
          <SessionSurface url={data().url} token={data().password ?? ""} />
        </PlatformProvider>
      )}
    </Show>
  )
}

function StartupError(props: { message: string; onRetry: () => void }) {
  const language = useLanguage()
  return (
    <div class="flex h-dvh flex-col items-center justify-center gap-4 bg-v2-background-bg-deep px-8 text-center text-v2-text-text-base">
      <h1 class="text-15-medium">{language.t("desktop.surface.startup.title")}</h1>
      <p class="max-w-md text-12-regular text-v2-text-text-muted break-words">{props.message}</p>
      <button
        type="button"
        class="rounded-full bg-v2-background-bg-accent px-4 py-1.5 text-12-medium text-v2-text-text-inverse"
        onClick={props.onRetry}
      >
        {language.t("desktop.surface.action.retry")}
      </button>
    </div>
  )
}

void loadInitialLocale().then((locale) => {
  render(
    () => (
      <ThemeProvider>
        <LanguageProvider locale={locale}>
          <Desktop />
        </LanguageProvider>
      </ThemeProvider>
    ),
    document.getElementById("root")!,
  )
})
