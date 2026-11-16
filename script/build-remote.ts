#!/usr/bin/env bun

import { $ } from "bun"
import { mkdir } from "fs/promises"
import path from "path"
import { fileURLToPath } from "url"

const dir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
process.chdir(dir)
const index = process.argv.indexOf("--target")
const target = index >= 0 ? process.argv[index + 1] : undefined
const outfile = path.join(dir, "dist", process.platform === "win32" ? "miao-remote.exe" : "miao-remote")
const entry = path.join(dir, "packages/engine-ts/remote-control/script/host.ts")
await mkdir(path.dirname(outfile), { recursive: true })
if (target) await $`bun build --compile --minify --target ${target} --outfile ${outfile} ${entry}`
else await $`bun build --compile --minify --outfile ${outfile} ${entry}`
console.log(`remote host sidecar: ${outfile}`)
