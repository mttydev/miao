import { EngineHttpClient } from "@miao/client/engine"
import { ControlHostRunner } from "../src/host-runner"

if (process.argv.includes("register")) {
  const { register } = await import("./register")
  await register()
  process.exit(0)
}

const filename = process.env.MIAO_REMOTE_CONTROL_CONFIG
const url = process.env.MIAO_ENGINE_HTTP_URL
const token = process.env.MIAO_ENGINE_HTTP_TOKEN
const runtimeID = process.env.MIAO_RUNTIME_ID
if (!filename || !url || !token || !runtimeID)
  throw new Error(
    "set MIAO_REMOTE_CONTROL_CONFIG, MIAO_ENGINE_HTTP_URL, MIAO_ENGINE_HTTP_TOKEN and MIAO_RUNTIME_ID",
  )

const configuration = await ControlHostRunner.readConfiguration(filename)
const client = new EngineHttpClient({ url, token })
const runner = await ControlHostRunner.start({
  ...configuration,
  runtimeID,
  ...(process.env.MIAO_REMOTE_STORAGE ? { storage: process.env.MIAO_REMOTE_STORAGE } : {}),
  engine: {
    request: (method, params) => client.request(method, params),
    command: (method) => client.request(method),
    admit: (input, resume, attachments) => client.admit(input, resume, [...(attachments ?? [])]),
  },
})
console.log(`remote host ${runner.hostID} runtime ${runner.runtimeID} on ${configuration.hubURL}`)

const stop = async () => {
  await runner.stop()
  await client.close()
  process.exit(0)
}
process.on("SIGINT", stop)
process.on("SIGTERM", stop)
await new Promise(() => {})
