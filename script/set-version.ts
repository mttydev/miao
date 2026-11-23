#!/usr/bin/env bun
// Root package.json owns the version; workspace manifests are synchronized copies.
// bun script/set-version.ts X.Y.Z updates the root and all workspace versions.
// bun script/set-version.ts synchronizes workspaces with the existing root version.
// bun script/set-version.ts --bump patch|minor|major derives the next version.
// Add --dry-run to print it without writing anything.
import { $, Glob } from "bun"
import semver from "semver"
import path from "node:path"

const root = path.resolve(import.meta.dir, "..")
const manifest = await Bun.file(path.join(root, "package.json")).json()
const args = process.argv.slice(2)
const dryRun = args.includes("--dry-run")
const bump = args.includes("--bump") ? args[args.indexOf("--bump") + 1] : args.find((arg) => !arg.startsWith("--"))
const version = bump && !semver.valid(bump) ? next(manifest.version, bump) : (bump ?? manifest.version)
if (typeof version !== "string" || !semver.valid(version)) throw new Error("Expected a semantic version X.Y.Z")

/**
 * The next version on this house's line: a third number never reaches 100, so
 * `0.3.99` is followed by `0.4.0` and `0.99.99` by `1.0.0`. Capping keeps a
 * release line readable. `--bump minor` is the owner's explicit instruction,
 * not the assistant's: patch releases run until 99, then carry.
 */
function next(current: string, kind: string) {
  const [major, minor, patch] = current.split(".").map(Number)
  if (kind === "major") return `${major + 1}.0.0`
  if (kind === "minor" || (kind === "patch" && patch + 1 > 99)) {
    return minor + 1 > 99 ? `${major + 1}.0.0` : `${major}.${minor + 1}.0`
  }
  if (kind !== "patch") throw new Error(`Expected --bump patch|minor|major, got ${kind}`)
  return `${major}.${minor}.${patch + 1}`
}

if (dryRun) {
  console.log(version)
  process.exit(0)
}

const files = new Set(["package.json"])
for (const pattern of manifest.workspaces.packages) {
  for await (const file of new Glob(`${pattern}/package.json`).scan({ cwd: root })) files.add(file)
}
for (const file of files) {
  const target = Bun.file(path.join(root, file))
  const text = await target.text()
  const next = text.replace(/^(\s*"version":\s*)"[^"]*"/m, (_match, prefix: string) => `${prefix}"${version}"`)
  if (next === text) continue
  await Bun.write(target, next)
  console.log(file)
}
// Mirror the workspace versions into bun.lock. Without this, CI's `bun install`
// rewrites the lockfile's workspace entries, and the generate job (which can
// only fail on a protected branch) reports drift nobody committed.
await $`bun install --lockfile-only`.cwd(root)
console.log(`workspace versions synchronized to ${version}`)
