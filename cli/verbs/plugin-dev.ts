import { createHash } from "node:crypto";
import { chmodSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import type { VerbContext } from "../lib/commands.ts";
import { runCommand } from "../lib/run.ts";

const FILES = ["manifest.json", "Panel.qml"] as const;
const PLUGIN = "daftmause.omadai";
const STAGING_PREFIX = "omadai-plugin-dev-";
const DIGEST_FILE = "omadai-plugin-dev.json";
const USAGE = "usage: omadai plugin-dev [--dry-run]";

type Digest = Record<string, string>;

function sha256(data: Uint8Array): string {
  return createHash("sha256").update(data).digest("hex");
}

function commonGitDir(root: string): string | undefined {
  const result = Bun.spawnSync(["git", "rev-parse", "--git-common-dir"], { cwd: root, stdout: "pipe", stderr: "inherit" });
  if (result.exitCode !== 0) return undefined;
  const dir = result.stdout.toString().trim();
  return dir === "" ? undefined : resolve(root, dir);
}

function readDigest(path: string): Digest {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch {
    return {};
  }
  try {
    const data: unknown = JSON.parse(text);
    if (typeof data !== "object" || data === null) return {};
    const digest: Digest = {};
    for (const [key, value] of Object.entries(data)) if (typeof value === "string") digest[key] = value;
    return digest;
  } catch {
    return {};
  }
}

function refusal(dest: string, source: Digest, saved: Digest): string | undefined {
  let stat;
  try {
    stat = lstatSync(dest);
  } catch {
    return undefined;
  }
  if (stat.isSymbolicLink()) return `${dest} is a symlink`;
  if (!stat.isDirectory()) return `${dest} is not a directory`;
  const entries = readdirSync(dest, { withFileTypes: true });
  if (entries.some((entry) => entry.name === ".git")) return `${dest} is a git checkout`;
  for (const entry of entries) {
    if (!(FILES as readonly string[]).includes(entry.name)) return `${dest} holds ${entry.name}, which plugin-dev did not install`;
    if (!entry.isFile()) return `${join(dest, entry.name)} is not a regular file`;
    const hash = sha256(readFileSync(join(dest, entry.name)));
    if (hash !== source[entry.name] && hash !== saved[entry.name]) {
      return `${join(dest, entry.name)} matches neither this checkout nor the last plugin-dev install`;
    }
  }
  return undefined;
}

export default async function run({ root, args }: VerbContext): Promise<number> {
  const dryRun = args.length === 1 && args[0] === "--dry-run";
  if (args.length > 0 && !dryRun) {
    console.error(USAGE);
    return 2;
  }
  const home = process.env.HOME;
  if (!home) {
    console.error("omadai plugin-dev: HOME is not set");
    return 1;
  }
  const omarchyDir = join(home, ".config", "omarchy");
  const dest = join(omarchyDir, "plugins", PLUGIN);
  const git = commonGitDir(root);
  if (git === undefined) {
    console.error(`omadai plugin-dev: cannot find the git directory of ${root}`);
    return 1;
  }
  const digestPath = join(git, DIGEST_FILE);

  const contents = new Map<string, Uint8Array>();
  const source: Digest = {};
  for (const file of FILES) {
    const data = readFileSync(join(root, file));
    contents.set(file, data);
    source[file] = sha256(data);
  }
  const saved = readDigest(digestPath);
  const reason = refusal(dest, source, saved);
  let exists = true;
  try {
    lstatSync(dest);
  } catch {
    exists = false;
  }

  if (dryRun) {
    for (const file of FILES) console.log(`source: ${join(root, file)}`);
    console.log(`destination: ${dest}`);
    console.log(`staging: ${join(omarchyDir, `${STAGING_PREFIX}XXXXXX`)}`);
    console.log(`digest: ${digestPath}`);
    if (reason !== undefined) {
      console.log(`would refuse: ${reason}`);
      return 0;
    }
    console.log(`1. copy ${FILES.join(" and ")} into a new staging directory, mode 0755`);
    console.log("2. omarchy-plugin-validate <staging>");
    console.log(exists ? "3. mv -T --exchange <staging> <destination>" : "3. mv -T --no-copy <staging> <destination>");
    console.log(exists ? "4. delete the displaced tree now at <staging>" : "4. nothing displaced");
    console.log("5. save the installed files' sha256 to the digest file");
    return 0;
  }

  if (reason !== undefined) {
    console.error(`omadai plugin-dev: refusing, changing nothing: ${reason}`);
    return 1;
  }

  mkdirSync(omarchyDir, { recursive: true });
  const staging = mkdtempSync(join(omarchyDir, STAGING_PREFIX));
  let kept: string | undefined;
  try {
    chmodSync(staging, 0o755);
    for (const [file, data] of contents) writeFileSync(join(staging, file), data);
    const validated = runCommand(["omarchy-plugin-validate", staging], root);
    if (validated !== 0) {
      console.error("omadai plugin-dev: validation failed; the destination is unchanged");
      return validated;
    }
    if (!exists) mkdirSync(dirname(dest), { recursive: true });
    // --no-copy keeps a move onto another filesystem from filling the watched tree file by file.
    const swap = exists ? ["mv", "-T", "--exchange", staging, dest] : ["mv", "-T", "--no-copy", staging, dest];
    const moved = runCommand(swap, root);
    // The destination can change after the first check, and mv can fail after exchanging, so staging is checked before deletion whatever mv returned.
    if (exists) {
      try {
        kept = refusal(staging, source, saved);
      } catch (error) {
        kept = `it could not be read: ${String(error)}`;
      }
    }
    if (moved !== 0) {
      const where = kept === undefined ? "" : `; ${staging} is kept: ${kept}`;
      console.error(`omadai plugin-dev: the swap reported failure (exit ${moved})${where}`);
      return moved;
    }
  } finally {
    // Staging now holds the unused copy after a failure, the displaced tree after an exchange, or nothing after a fresh move.
    if (kept === undefined) rmSync(staging, { recursive: true, force: true });
  }

  if (kept !== undefined) {
    console.error(`omadai plugin-dev: installed ${dest}, but the tree it replaced changed after the check, so it is kept at ${staging}: ${kept}`);
  }
  try {
    writeFileSync(digestPath, `${JSON.stringify(source, null, 2)}\n`);
  } catch (error) {
    console.error(`omadai plugin-dev: installed ${dest} but could not save ${digestPath}: ${String(error)}`);
    return 1;
  }
  if (kept !== undefined) return 1;
  console.log(`omadai plugin-dev: installed ${dest}`);
  return 0;
}
