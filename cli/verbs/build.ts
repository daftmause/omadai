import type { VerbContext } from "../lib/commands.ts";
import { runCommand } from "../lib/run.ts";

export default async function run({ root }: VerbContext): Promise<number> {
  return runCommand(["cargo", "build", "--release", "--workspace"], root);
}
