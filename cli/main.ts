import { existsSync } from "node:fs";
import { join } from "node:path";
import { type Commands, isRequired, loadCommands, type VerbContext } from "./lib/commands.ts";
import { moduleFile, renderHelp, renderUsageError, renderVerbHelp } from "./lib/help.ts";

// Resolves to the repository root from both cli/main.ts and the built dist/main.js.
const root = join(import.meta.dir, "..");
const [name, ...rest] = process.argv.slice(2);

let commands: Commands;
try {
  commands = loadCommands(root);
} catch (error) {
  console.error((error as Error).message);
  process.exit(1);
}

if (name === undefined || name === "--help" || name === "-h" || name === "help") {
  process.stdout.write(renderHelp(commands));
  process.exit(0);
}

const { app, verbs } = commands;
const verb = verbs.find((entry) => entry.name === name);
if (verb === undefined) {
  console.error(`${app}: unknown verb "${name}"; run bin/${app} --help`);
  process.exit(2);
}

const split = rest.indexOf("--");
const before = split === -1 ? rest : rest.slice(0, split);
const after = split === -1 ? [] : rest.slice(split + 1);
if (before.includes("--help") || before.includes("-h")) {
  process.stdout.write(renderVerbHelp(app, verb));
  process.exit(0);
}

const usageError = (problem: string): never => {
  process.stderr.write(renderUsageError(app, verb, problem));
  process.exit(2);
};

let targets: (string | undefined)[] = [undefined];
let own = before;
if (verb.parts !== undefined) {
  const first = before[0];
  if (first !== undefined && verb.parts.includes(first)) {
    targets = [first];
    own = before.slice(1);
  } else if (first !== undefined && commands.parts.includes(first)) {
    usageError(`has no part ${first}; its parts are ${verb.parts.join(", ")}`);
  } else if (verb.long_running) {
    usageError(`stays running, so name one part: ${verb.parts.join(", ")}`);
  } else if (first !== undefined && !first.startsWith("-") && !verb.args.some((arg) => !/^\[?-/.test(arg))) {
    usageError(`has no part ${first}; its parts are ${verb.parts.join(", ")}`);
  } else {
    targets = verb.parts;
  }
}

const required = verb.args.filter(isRequired);
const given = own.filter((arg) => arg === "-" || !arg.startsWith("-")).length + after.length;
if (given < required.length) usageError(`missing ${required.slice(given).join(" ")}`);

const file = moduleFile("bun", verb.name);
if (!existsSync(join(root, file))) {
  console.error(`${app} ${verb.name}: unavailable, ${file} is missing`);
  process.exit(3);
}

const module: { default: (context: VerbContext) => Promise<number> } = await import(`./verbs/${verb.name}`);
for (const part of targets) {
  const status = await module.default({ root, args: [...(part === undefined ? [] : [part]), ...own, ...after] });
  if (status !== 0) {
    if (targets.length > 1) console.error(`${app} ${verb.name} ${part}: exit ${status}`);
    process.exit(status);
  }
}
process.exit(0);
