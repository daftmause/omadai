export function runCommand(command: string[], cwd: string): number {
  const result = Bun.spawnSync(command, { cwd, stdin: "inherit", stdout: "inherit", stderr: "inherit" });
  return result.exitCode ?? 1;
}
