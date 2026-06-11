#!/usr/bin/env node
// create-nebula-app: scaffold a local-first app on the Nebula engine.
//
//   npx create-nebula-app my-app            # nebula-slim engine (default)
//   npx create-nebula-app my-app --full     # full engine (dockerd + k3s)
//
// The scaffold is deliberately tiny, dependency-free, and written to be
// extended by a coding agent: see AGENTS.md inside the generated app.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const flags = new Set(args.filter((a) => a.startsWith("--")));
const name = args.find((a) => !a.startsWith("--"));

if (!name || flags.has("--help")) {
  console.log(`usage: create-nebula-app <app-name> [--full]

  --full   embed the full engine (real dockerd/containerd + k3s, ~140MB)
           instead of nebula-slim (Rust engine, ~32MB — the default)`);
  process.exit(name ? 0 : 1);
}
if (!/^[a-z0-9][a-z0-9-_]*$/.test(name)) {
  console.error(`app names are [a-z0-9-_], got \`${name}\``);
  process.exit(1);
}

const flavor = flags.has("--full") ? "full" : "slim";
const here = path.dirname(fileURLToPath(import.meta.url));
const templateDir = path.join(here, "template");
const dest = path.resolve(process.cwd(), name);

if (fs.existsSync(dest)) {
  console.error(`\`${dest}\` already exists`);
  process.exit(1);
}

// Copy the template, substituting {{APP_NAME}} / {{FLAVOR}} in text files.
const subst = (s) => s.replaceAll("{{APP_NAME}}", name).replaceAll("{{FLAVOR}}", flavor);
const walk = (from, to) => {
  fs.mkdirSync(to, { recursive: true });
  for (const entry of fs.readdirSync(from, { withFileTypes: true })) {
    const f = path.join(from, entry.name);
    const t = path.join(to, entry.name === "gitignore" ? ".gitignore" : entry.name);
    if (entry.isDirectory()) walk(f, t);
    else fs.writeFileSync(t, subst(fs.readFileSync(f, "utf8")));
  }
};
walk(templateDir, dest);

console.log(`created ${name}/ (${flavor} engine)

next:
  cd ${name}
  node scripts/engine.mjs up      # fetch engine artifacts + boot (first run downloads)
  node src/index.mjs              # run the starter app
  cat AGENTS.md                   # hand this file to your coding agent

The engine is fully isolated under ${name}/.nebula (its own VM, disks,
ports) — it cannot collide with a standalone Nebula install.`);
