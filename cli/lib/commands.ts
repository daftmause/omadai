import { readFileSync } from "node:fs";
import { join } from "node:path";

export type Verb = {
  name: string;
  summary: string;
  args: string[];
  examples: string[];
  category: string;
  parts?: string[];
  long_running?: boolean;
};

export const HOSTS = ["bun", "ruby", "python"] as const;
export type Host = (typeof HOSTS)[number];
export const EXTENSIONS: Record<Host, string> = { bun: ".ts", ruby: ".rb", python: ".py" };

export type Commands = {
  app: string;
  host: Host;
  parts: string[];
  verbs: Verb[];
};

export type VerbContext = {
  root: string;
  args: string[];
};

const SLUG = /^[a-z][a-z0-9-]*$/;

export function isRequired(arg: string): boolean {
  return /^<[^<>]+>$/.test(arg);
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item) => typeof item === "string");
}

export function parseCommands(text: string): Commands {
  const data: unknown = JSON.parse(text);
  if (typeof data !== "object" || data === null || !Array.isArray((data as { verbs?: unknown }).verbs)) {
    throw new Error('cli/commands.json: expected an object with a "verbs" array');
  }
  const raw = data as { app?: unknown; host?: unknown; parts?: unknown; verbs: unknown[] };
  if (typeof raw.app !== "string" || !SLUG.test(raw.app)) throw new Error("cli/commands.json: app must be a lowercase slug");
  if (typeof raw.host !== "string" || !(HOSTS as readonly string[]).includes(raw.host)) {
    throw new Error(`cli/commands.json: host must be one of ${HOSTS.join(", ")}`);
  }
  const parts = raw.parts ?? [];
  if (raw.parts !== undefined && (!isStringArray(parts) || parts.length === 0 || !parts.every((part) => SLUG.test(part)))) {
    throw new Error("cli/commands.json: parts, when present, must be a non-empty array of lowercase slugs");
  }
  if (new Set(parts as string[]).size !== (parts as string[]).length) throw new Error("cli/commands.json: parts must not repeat");
  const seen = new Set<string>();
  const verbs = raw.verbs.map((entry, index) => {
    const verb = entry as Partial<Verb>;
    const where = `cli/commands.json: verbs[${index}]`;
    if (typeof verb.name !== "string" || !SLUG.test(verb.name)) throw new Error(`${where}: name must be a lowercase slug`);
    if (seen.has(verb.name)) throw new Error(`${where}: duplicate verb "${verb.name}"`);
    seen.add(verb.name);
    if (typeof verb.summary !== "string" || verb.summary === "") throw new Error(`${where}: summary must be a non-empty string`);
    if (!isStringArray(verb.args)) throw new Error(`${where}: args must be an array of strings`);
    if (!isStringArray(verb.examples)) throw new Error(`${where}: examples must be an array of strings`);
    if (typeof verb.category !== "string" || verb.category === "") throw new Error(`${where}: category must be a non-empty string`);
    const result: Verb = { name: verb.name, summary: verb.summary, args: verb.args, examples: verb.examples, category: verb.category };
    if (verb.args.some(isRequired) && verb.examples.length === 0) throw new Error(`${where}: a verb with a required argument needs an example`);
    if (verb.parts !== undefined) {
      const own = verb.parts;
      if (!isStringArray(own) || own.length === 0 || new Set(own).size !== own.length || !own.every((part) => (parts as string[]).includes(part))) {
        throw new Error(`${where}: parts, when present, must be a non-empty list of distinct names from the top-level parts`);
      }
      result.parts = own;
    }
    if (verb.long_running !== undefined) {
      if (typeof verb.long_running !== "boolean") throw new Error(`${where}: long_running must be true or false`);
      if (verb.long_running && result.parts === undefined) throw new Error(`${where}: long_running needs the verb's parts, since it means the part must be named`);
      result.long_running = verb.long_running;
    }
    return result;
  });
  return { app: raw.app, host: raw.host as Host, parts: parts as string[], verbs };
}

export function loadCommands(root: string): Commands {
  return parseCommands(readFileSync(join(root, "cli", "commands.json"), "utf8"));
}
