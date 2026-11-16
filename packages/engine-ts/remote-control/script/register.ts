import { chmod, mkdir, readFile, rename, writeFile } from "node:fs/promises"
import path from "node:path"
import { DeviceGrants } from "../src/grants"
import { HubSetup } from "../src/hub-setup"

/**
 * Register this machine's stable host identity with a Hub account and write the
 * owner-only control document. Account credentials stay in memory: the file
 * receives only the host-scoped relay token.
 */
export async function register() {
  const filename = required("MIAO_REMOTE_CONTROL_CONFIG")
  const hubURL = required("MIAO_HUB_URL")
  const email = required("MIAO_HUB_EMAIL")
  const password = required("MIAO_HUB_PASSWORD")
  const name = process.env.MIAO_HUB_HOST_NAME?.trim() || "miao"
  const grantFile = path.resolve(
    path.dirname(filename),
    process.env.MIAO_REMOTE_GRANT_FILE ?? "devices.json",
  )
  const allowLoopbackHTTP = process.env.MIAO_HUB_ALLOW_LOOPBACK === "1"
  const grants = await DeviceGrants.load(grantFile)
  const existing = await readConfiguration(filename)
  let configuration: { hubURL: string; hostToken: string; accountID?: string } | undefined
  const runtime: HubSetup.Runtime = {
    get: async () => ({
      enabled: configuration !== undefined,
      connected: false,
      hostID: grants.hostID,
      hostPublicKey: grants.identity.publicKey,
      ...(configuration ?? {}),
    }),
    configure: async (input) => {
      configuration = input
      const value = {
        ...existing,
        ...input,
        grantFile: path.relative(path.dirname(filename), grantFile) || path.basename(grantFile),
      }
      await mkdir(path.dirname(filename), { recursive: true })
      const temporary = `${filename}.${process.pid}.tmp`
      await writeFile(temporary, JSON.stringify(value, null, 2) + "\n", { mode: 0o600 })
      await chmod(temporary, 0o600)
      await rename(temporary, filename)
      return runtime.get()
    },
  }
  await HubSetup.connect({ hubURL, email, password, name, runtime, allowLoopbackHTTP })
  console.log(`registered host ${grants.hostID} in ${filename}`)
  console.log(`run: miao serve --remote-host ${filename}`)
}

if (import.meta.main) await register()

function required(name: string) {
  const value = process.env[name]
  if (!value) throw new Error(`set ${name}`)
  return value
}

async function readConfiguration(filename: string) {
  return readFile(filename, "utf8").then(
    (text) => {
      const value: unknown = JSON.parse(text)
      if (typeof value !== "object" || value === null || Array.isArray(value))
        throw new Error("the control document is not an object")
      return value as Record<string, unknown>
    },
    (error: NodeJS.ErrnoException) => {
      if (error.code === "ENOENT") return {}
      throw new Error(`refusing to replace ${filename}: ${error.message}`)
    },
  )
}
