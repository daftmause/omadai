import { type Commands, EXTENSIONS, type Verb } from "./commands.ts";

export function usage(app: string, verb: Verb): string {
  const part = verb.parts === undefined ? [] : [verb.long_running ? `<${verb.parts.join("|")}>` : `[${verb.parts.join("|")}]`];
  return [`bin/${app}`, verb.name, ...part, ...verb.args].join(" ");
}

export function byCategory(verbs: Verb[]): Map<string, Verb[]> {
  const groups = new Map<string, Verb[]>();
  for (const verb of verbs) groups.set(verb.category, [...(groups.get(verb.category) ?? []), verb]);
  return groups;
}

export function renderHelp({ app, verbs }: Commands): string {
  // A usage line past the cap takes its own line, so one long verb does not push every summary right.
  const cap = 48;
  const width = Math.max(0, ...verbs.map((verb) => usage(app, verb).length).filter((length) => length <= cap));
  const lines = [`Usage: bin/${app} <verb> [args]`, ""];
  for (const [category, group] of byCategory(verbs)) {
    lines.push(`${category}:`);
    for (const verb of group) {
      const line = usage(app, verb);
      if (line.length > cap) lines.push(`  ${line}`, `  ${"".padEnd(width)}  ${verb.summary}`);
      else lines.push(`  ${line.padEnd(width)}  ${verb.summary}`);
    }
    lines.push("");
  }
  lines.push(`Run bin/${app} <verb> --help for a verb's examples.`);
  return `${lines.join("\n")}\n`;
}

export function renderVerbHelp(app: string, verb: Verb): string {
  const lines = [`Usage: ${usage(app, verb)}`, "", verb.summary];
  if (verb.parts !== undefined && !verb.long_running) {
    lines.push("", `Without a part, runs ${verb.parts.join(", then ")}, stopping at the first failure.`);
  }
  if (verb.examples.length > 0) lines.push("", "Examples:", ...verb.examples.map((example) => `  ${example}`));
  return `${lines.join("\n")}\n`;
}

// What a usage error prints on stderr: the problem, the usage line and the verb's first example.
export function renderUsageError(app: string, verb: Verb, problem: string): string {
  const lines = [`${app} ${verb.name}: ${problem}`, `Usage: ${usage(app, verb)}`];
  if (verb.examples.length > 0) lines.push(`Example: ${verb.examples[0]}`);
  return `${lines.join("\n")}\n`;
}

export function moduleFile(host: Commands["host"], name: string): string {
  return `cli/verbs/${name}${EXTENSIONS[host]}`;
}
