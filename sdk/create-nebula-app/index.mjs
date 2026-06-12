#!/usr/bin/env node
// create-nebula-app: scaffold a local-first app on the Nebula engine.
//
//   npm create @nebula-vm/app my-app                    # Tauri+React app, slim engine
//   npm create @nebula-vm/app my-app --template headless # no UI (daemon/CLI shape)
//   npm create @nebula-vm/app my-app --full              # full engine (dockerd + k3s)
//
// (the unscoped npm name `create-nebula-app` is squatted by an abandoned
// 2021 package — dispute filed-able; the bin keeps the familiar name)
//
// The scaffold is deliberately tiny, dependency-free, and written to be
// extended by a coding agent: see AGENTS.md inside the generated app.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const flags = new Set(args.filter((a) => a.startsWith("--")));
const name = args.find((a) => !a.startsWith("--"));

const templateArg = args.find((a) => a.startsWith("--template="))?.split("=")[1]
  ?? (args.includes("--template") ? args[args.indexOf("--template") + 1] : undefined)
  ?? "app";

if (!name || flags.has("--help")) {
  console.log(`usage: create-nebula-app <app-name> [--template app|headless] [--full]

  --template app       Tauri 2 + Vite + React frontend with a hyper-based
                       Rust base layer (default)
  --template headless  no UI: a daemon/CLI shape (plain Node, zero deps)
  --full               embed the full engine (real dockerd/containerd + k3s,
                       ~140MB) instead of nebula-slim (Rust engine, ~32MB)`);
  process.exit(name ? 0 : 1);
}
if (!["app", "headless"].includes(templateArg)) {
  console.error(`unknown template \`${templateArg}\` (app | headless)`);
  process.exit(1);
}
if (!/^[a-z0-9][a-z0-9-_]*$/.test(name)) {
  console.error(`app names are [a-z0-9-_], got \`${name}\``);
  process.exit(1);
}

const flavor = flags.has("--full") ? "full" : "slim";
const here = path.dirname(fileURLToPath(import.meta.url));
const templateDir = path.join(here, "templates", templateArg);
const dest = path.resolve(process.cwd(), name);

if (fs.existsSync(dest)) {
  console.error(`\`${dest}\` already exists`);
  process.exit(1);
}

// Copy the template, substituting {{APP_NAME}} / {{FLAVOR}} in text files.
const crate = name.replaceAll("-", "_");
const subst = (s) =>
  s.replaceAll("{{APP_NAME}}", name).replaceAll("{{FLAVOR}}", flavor).replaceAll("{{APP_CRATE}}", crate);
const walk = (from, to) => {
  fs.mkdirSync(to, { recursive: true });
  for (const entry of fs.readdirSync(from, { withFileTypes: true })) {
    const f = path.join(from, entry.name);
    const t = path.join(to, entry.name === "gitignore" ? ".gitignore" : entry.name);
    if (entry.isDirectory()) walk(f, t);
    else if (/\.(png|ico|icns|jpg|gif|woff2?)$/.test(entry.name)) fs.copyFileSync(f, t);
    else fs.writeFileSync(t, subst(fs.readFileSync(f, "utf8")));
  }
};
walk(templateDir, dest);

const next =
  templateArg === "headless"
    ? `  node scripts/engine.mjs up      # fetch engine artifacts + boot (first run downloads)
  node src/index.mjs              # run the starter app`
    : `  npm install
  npm run dev                     # boots the engine, vite + the Tauri window`;
console.log(`created ${name}/ (${templateArg} template, ${flavor} engine)

next:
  cd ${name}
${next}
  cat AGENTS.md                   # hand this file to your coding agent

The engine is fully isolated under ${name}/.nebula (its own VM, disks,
ports) — it cannot collide with a standalone Nebula install.`);
