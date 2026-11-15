import type { Component, JSX } from "solid-js"

/**
 * Small inline icon set for the few glyphs the shared v2 sprite lacks
 * (send, stop, pin, trash, sparkles). Drawn in the same 24-grid stroke
 * style as @miao/ui's v2 icons so they sit next to them unnoticed.
 */
function make(name: string, path: JSX.Element, filled = false): Component<{ class?: string }> {
  return (props) => (
    <svg
      viewBox="0 0 24 24"
      width="16"
      height="16"
      class={props.class}
      aria-hidden="true"
      fill={filled ? "currentColor" : "none"}
      stroke={filled ? "none" : "currentColor"}
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
      data-icon={name}
      innerHTML={path as unknown as string}
    />
  )
}

export const SendIcon = make(
  "send",
  <path d="M12 19V5M5 12l7-7 7 7" />,
)

export const StopIcon = make(
  "stop",
  <rect x="6.5" y="6.5" width="11" height="11" rx="2" />,
  true,
)

export const PinIcon = make(
  "pin",
  <path d="M12 17v5M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z" />,
)

export const TrashIcon = make(
  "trash",
  <path d="M3 6h18M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2M10 11v6M14 11v6" />,
)

export const SparklesIcon = make(
  "sparkles",
  (
    <>
      <path d="M9.937 15.5A2 2 0 0 0 8.5 14.063l-6.135-1.582a.5.5 0 0 1 0-.962L8.5 9.936A2 2 0 0 0 9.937 8.5l1.582-6.135a.5.5 0 0 1 .963 0L14.063 8.5A2 2 0 0 0 15.5 9.937l6.135 1.581a.5.5 0 0 1 0 .964L15.5 14.063a2 2 0 0 0-1.437 1.437l-1.582 6.135a.5.5 0 0 1-.963 0z" />
      <path d="M20 3v4M22 5h-4M4 17v2M5 18H3" />
    </>
  ),
)
