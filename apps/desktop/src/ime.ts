import { createSignal } from "solid-js"

/**
 * Track an IME composition the way the shared prompt input does: keydowns
 * during composition (picking hanzi, confirming pinyin) must never submit.
 * WKWebView reports composition through events as well as `isComposing`, and
 * some keystrokes arrive as keyCode 229, so all three signals gate the guard.
 */
export function createImeGuard() {
  const [composing, setComposing] = createSignal(false)
  return {
    composing,
    isImeComposing: (event: KeyboardEvent) => event.isComposing || composing() || event.keyCode === 229,
    onCompositionStart: () => setComposing(true),
    onCompositionUpdate: () => setComposing(true),
    onCompositionEnd: () => setComposing(false),
    onBlur: () => setComposing(false),
  }
}

export type ImeGuard = ReturnType<typeof createImeGuard>
