import { createSignal } from "solid-js"

/**
 * Track an IME composition the way the shared prompt input does, hardened for
 * WKWebView: on the Enter that confirms a composition, Safari fires the
 * keydown after compositionend with isComposing already false, so a signal
 * alone misses it. A short window after compositionend also counts as
 * composing; a deliberate second Enter to send lands outside it.
 */
export function createImeGuard() {
  const [composing, setComposing] = createSignal(false)
  let endedAt = 0
  return {
    composing,
    isImeComposing: (event: KeyboardEvent) =>
      event.isComposing || composing() || event.keyCode === 229 || Date.now() - endedAt < 150,
    onCompositionStart: () => setComposing(true),
    onCompositionUpdate: () => setComposing(true),
    onCompositionEnd: () => {
      setComposing(false)
      endedAt = Date.now()
    },
    onBlur: () => setComposing(false),
  }
}

export type ImeGuard = ReturnType<typeof createImeGuard>
