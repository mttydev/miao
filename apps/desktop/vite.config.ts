import appPlugin from "@miao/app/vite"
import { resolve } from "node:path"
import { fileURLToPath } from "node:url"
import { defineConfig } from "vite"

// `tauri dev` serves this bundle on 1420 and renders it in the system WebView;
// the same config produces the `dist/` that the bundle embeds. The shared
// renderer's stylesheet references /assets fonts that live in the app
// package's public directory.
export default defineConfig({
  plugins: [appPlugin],
  publicDir: resolve(fileURLToPath(new URL(".", import.meta.url)), "../../packages/app/public"),
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  envPrefix: ["VITE_", "TAURI_"],
  build: { target: "esnext" },
})
