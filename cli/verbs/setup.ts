import { existsSync } from "node:fs";
import { join } from "node:path";
import type { VerbContext } from "../lib/commands.ts";
import { runCommand } from "../lib/run.ts";

export default async function run({ root, args }: VerbContext): Promise<number> {
  const part = args[0];
  if (part === "core") {
    const fetched = runCommand(["cargo", "fetch"], root);
    if (fetched !== 0) return fetched;
    // cli/package.json pins exact versions and the template ignores its bun.lock.
    return runCommand(["bun", "install"], join(root, "cli"));
  }
  if (part === "macos") {
    const installed = bunInstall(join(root, "apps", "macos"));
    if (installed !== 0) return installed;
    return runCommand(["cargo", "fetch", "--locked", "--manifest-path", "apps/macos/src-tauri/Cargo.toml"], root);
  }
  console.error(`omadai setup: unknown part ${part ?? "(none)"}`);
  return 2;
}

function bunInstall(dir: string): number {
  if (!existsSync(join(dir, "bun.lock"))) {
    console.error(`omadai setup: ${join(dir, "bun.lock")} is missing; refusing to resolve unpinned ranges`);
    return 1;
  }
  return runCommand(["bun", "install", "--frozen-lockfile"], dir);
}
