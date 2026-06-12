#!/usr/bin/env node
// {{APP_NAME}} — starter app on the Nebula engine ({{FLAVOR}} flavor).
//
// This demo proves the three planes your app builds on, then gets out of
// the way. Replace it with your product; AGENTS.md explains the shape.

import { nebula } from "./nebula.mjs";

const log = (s) => console.log(`\x1b[36m▸\x1b[0m ${s}`);

// 1. Engine: a real Linux microVM, booted by `npm run engine:up`.
const status = await nebula.status();
log(`engine: ${status.vmState}, kernel ${status.agent?.kernel}, ${status.cpus} cpus`);

// 2. Containers: the engine's Docker API, verbatim ({{FLAVOR}} engine).
const version = await nebula.docker("GET", "/v1.43/version");
log(`container engine: ${version.Components?.[0]?.Name ?? "engine"} ${version.Components?.[0]?.Version ?? ""}`);
const ps = await nebula.docker("GET", "/v1.43/containers/json?all=true");
log(`containers: ${ps.length}`);

// 3. The primitive nothing else gives you: isolated microVMs ("vessels")
//    with live memory snapshots — fork a RUNNING machine, RAM and all.
const backend = process.platform === "darwin" ? "vz" : "krun";
log(`vessel demo (backend: ${backend})…`);
await nebula.vessels.create({ name: "demo", backend, mem_mib: 1024 });
await nebula.vessels.exec("demo", "sh", ["-c", "echo hello-from-the-past > /run/state"]);
await nebula.vessels.snapshot("demo", "t0");
const fork = await nebula.vessels.branch("demo", "fork", "t0", 2);
log(`branched ${fork.vessels.length} live clones in ${fork.ms}ms (woke mid-execution: ${fork.vessels.every((v) => v.live)})`);
const mem = await nebula.vessels.exec("fork-1", "cat", ["/run/state"]);
log(`fork-1 remembers: ${mem.stdout.trim()}`);
for (const name of ["demo", "fork-1", "fork-2"]) await nebula.vessels.rm(name);
log("demo vessels cleaned up — now build something. (see AGENTS.md)");
