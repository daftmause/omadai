import { existsSync } from "node:fs";
import { join } from "node:path";
import type { VerbContext } from "../lib/commands.ts";

const TOOLS: string[][] = [
  ["rustc", "--version"],
  ["cargo", "--version"],
  ["rustfmt", "--version"],
  ["cargo", "clippy", "--version"],
  ["bun", "--version"],
];

export default async function run({ root }: VerbContext): Promise<number> {
  let failed = 0;
  for (const command of TOOLS) {
    const tool = command[0] ?? "";
    if (Bun.which(tool) === null) {
      console.log(`FAIL ${command.join(" ")}: ${tool} not on PATH`);
      failed++;
      continue;
    }
    const result = Bun.spawnSync(command, { cwd: root, stdout: "pipe", stderr: "pipe", timeout: 10_000 });
    const out = result.stdout.toString().trim().split("\n")[0];
    if (result.exitCode === 0) console.log(`ok   ${command.join(" ")}: ${out}`);
    else {
      console.log(`FAIL ${command.join(" ")}: exit ${result.exitCode ?? "timeout"}: ${result.stderr.toString().trim().split("\n")[0]}`);
      failed++;
    }
  }
  if (Bun.which("omarchy-plugin-validate")) console.log("ok   omarchy-plugin-validate on PATH");
  else console.log("warn omarchy-plugin-validate not on PATH (Omarchy only); check needs it");
  // The publish verb is not shipped, so a checkout without it has no use for gitleaks.
  if (existsSync(join(root, "cli", "verbs", "publish.ts"))) {
    if (Bun.which("gitleaks")) console.log("ok   gitleaks on PATH");
    else console.log("warn gitleaks not on PATH; publishing needs it");
  }
  if (existsSync("/usr/lib/qt6/bin/qmllint")) console.log("ok   /usr/lib/qt6/bin/qmllint present");
  else console.log("warn /usr/lib/qt6/bin/qmllint missing (qt6-declarative); check needs it");
  for (const dir of ["cli/node_modules", "apps/macos/node_modules"]) {
    console.log(existsSync(join(root, dir)) ? `ok   ${dir} present` : `warn ${dir} missing; run bin/omadai setup`);
  }
  return failed === 0 ? 0 : 1;
}
