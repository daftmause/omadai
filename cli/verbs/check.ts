import { copyFileSync, existsSync, mkdtempSync, readFileSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { VerbContext } from "../lib/commands.ts";
import { runCommand } from "../lib/run.ts";
import { typecheck } from "../lib/typecheck.ts";

const STEPS: string[][] = [
  ["cargo", "fmt", "--check"],
  ["cargo", "clippy", "--locked", "--workspace", "--all-targets", "--", "-D", "warnings"],
  ["cargo", "test", "--locked", "--workspace"],
];

export default async function run({ root }: VerbContext): Promise<number> {
  const typed = typecheck(root);
  if (typed !== 0) return typed;
  const srcinfo = checkSrcinfo(root);
  if (srcinfo !== 0) return srcinfo;
  for (const step of STEPS) {
    const status = runCommand(step, root);
    if (status !== 0) return status;
  }
  const validated = validatePlugin(root);
  if (validated !== 0) return validated;
  return lintQml(root);
}

// Inlined rather than imported from the aur verb, which does not ship, so the publish gate's staged copy runs it too.
function checkSrcinfo(root: string): number {
  const dir = join(root, "packaging", "aur");
  let printed: string;
  try {
    const result = Bun.spawnSync(["makepkg", "--printsrcinfo"], { cwd: dir, stdin: "ignore", stdout: "pipe", stderr: "inherit" });
    if (result.exitCode !== 0) {
      console.error(`check: makepkg --printsrcinfo failed in ${dir}`);
      return 1;
    }
    printed = result.stdout.toString();
  } catch (error) {
    console.error(`check: cannot run makepkg (pacman), so packaging/aur/.SRCINFO went unchecked: ${(error as Error).message}`);
    return 1;
  }
  let onDisk: string;
  try {
    onDisk = readFileSync(join(dir, ".SRCINFO"), "utf8");
  } catch {
    console.error("check: packaging/aur/.SRCINFO is missing; write it with makepkg --printsrcinfo > .SRCINFO in packaging/aur");
    return 1;
  }
  if (printed !== onDisk) {
    console.error("check: packaging/aur/.SRCINFO differs from makepkg --printsrcinfo; regenerate it with makepkg --printsrcinfo > .SRCINFO in packaging/aur");
    return 1;
  }
  console.log("check: packaging/aur/.SRCINFO matches the PKGBUILD");
  return 0;
}

const QMLLINT = "/usr/lib/qt6/bin/qmllint";
const OMARCHY_SHELL = "/usr/share/omarchy/shell";

// omarchy-plugin-validate reads only the manifest, so this is the one step that parses Panel.qml.
function lintQml(root: string): number {
  if (!existsSync(QMLLINT)) {
    console.error(`check: ${QMLLINT} is missing (qt6-declarative), so Panel.qml's syntax went unchecked`);
    return 1;
  }
  // Panel.qml imports qs.Commons and qs.Ui, which resolve only from a directory whose qs entry is Omarchy's shell.
  const imports = mkdtempSync(join(tmpdir(), "omadai-qmllint-"));
  try {
    symlinkSync(OMARCHY_SHELL, join(imports, "qs"));
    return runCommand([QMLLINT, "-I", imports, "Panel.qml"], root);
  } finally {
    rmSync(imports, { recursive: true, force: true });
  }
}

// The validator refuses any symlink in the folder, and the repository root holds node_modules links.
function validatePlugin(root: string): number {
  const dir = mkdtempSync(join(tmpdir(), "omadai-plugin-"));
  try {
    for (const file of ["manifest.json", "Panel.qml"]) copyFileSync(join(root, file), join(dir, file));
    return runCommand(["omarchy-plugin-validate", dir], root);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}
