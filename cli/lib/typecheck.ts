import { existsSync } from "node:fs";
import { join } from "node:path";
import { runCommand } from "./run.ts";

export function typecheck(root: string): number {
  const tsc = join(root, "cli", "node_modules", ".bin", "tsc");
  if (!existsSync(tsc)) {
    console.error("typecheck: cli/node_modules/.bin/tsc is not installed; run bin/omadai setup, or bun install --cwd cli while setup is a stub");
    return 1;
  }
  return runCommand([tsc, "--noEmit", "-p", "cli/tsconfig.json"], root);
}
