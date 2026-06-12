#!/usr/bin/env node
// Engine lifecycle for this app: fetch artifacts, boot, stop, status.
//
// Fully isolated: NEBULA_HOME=<app>/.nebula gives this app its own engine
// VM, disks, sockets and ports — it cannot collide with a standalone
// Nebula install or another app's engine.
//
// Artifact resolution order:
//   1. NEBULA_BIN_DIR env (a dir containing nebula + nebulad)
//   2. nebula on PATH
//   3. download from the latest GitHub release (linux/windows tarballs)
// Guest images (kernel + rootfs) download from the same release per the
// flavor in nebula.config.json (slim -> rootfs-slim).

import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const appDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const cfg = JSON.parse(fs.readFileSync(path.join(appDir, "nebula.config.json"), "utf8"));
const home = path.join(appDir, ".nebula");
const binDir = path.join(home, "bin");
const REPO = "Flux159/nebula";

const arch = { arm64: "arm64", x64: "x86_64" }[process.arch] ?? process.arch;
const exe = process.platform === "win32" ? ".exe" : "";

function findNebula() {
  if (process.env.NEBULA_BIN_DIR) return path.join(process.env.NEBULA_BIN_DIR, `nebula${exe}`);
  const local = path.join(binDir, `nebula${exe}`);
  if (fs.existsSync(local)) return local;
  const which = spawnSync(process.platform === "win32" ? "where" : "which", ["nebula"]);
  if (which.status === 0) return which.stdout.toString().trim().split("\n")[0];
  return null;
}

async function ghAsset(name, dest) {
  // Latest-release asset by name; falls back to printing manual steps.
  const rel = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
    headers: { "User-Agent": "create-nebula-app" },
  });
  if (!rel.ok) throw new Error(`no published release yet (HTTP ${rel.status})`);
  const assets = (await rel.json()).assets ?? [];
  const asset = assets.find((a) => a.name.includes(name));
  if (!asset) throw new Error(`release has no asset matching \`${name}\``);
  const res = await fetch(asset.browser_download_url, { headers: { "User-Agent": "create-nebula-app" } });
  if (!res.ok) throw new Error(`download ${asset.name}: HTTP ${res.status}`);
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.writeFileSync(dest, Buffer.from(await res.arrayBuffer()));
  return asset.name;
}

async function ensureBinaries() {
  let nebula = findNebula();
  if (nebula) return nebula;
  console.log("fetching nebula host binaries from the latest release…");
  const plat = { darwin: "macos", linux: "linux", win32: "windows" }[process.platform];
  try {
    const tar = path.join(home, "dl", `nebula-${plat}-${arch}.tar.gz`);
    await ghAsset(`-${plat}-${arch}.tar.gz`, tar);
    fs.mkdirSync(binDir, { recursive: true });
    execFileSync("tar", ["-xzf", tar, "--strip-components=1", "-C", binDir]);
    return path.join(binDir, `nebula${exe}`);
  } catch (e) {
    console.error(`could not fetch binaries: ${e.message}

  Until a release is published (or on macOS, where the release ships an
  .app), point the app at binaries you have:
    NEBULA_BIN_DIR=/path/to/nebula/target/release node scripts/engine.mjs up
  or grab a CI artifact:
    gh run download -R ${REPO} -n nebula-${plat === "macos" ? "linux" : plat}-${arch}`);
    process.exit(1);
  }
}

async function ensureImages(nebula) {
  if (fs.existsSync(path.join(home, "kernel", "Image"))) return;
  const rootfsName = cfg.flavor === "slim" ? `rootfs-slim-${arch}.img.gz` : `rootfs-${arch}.img.gz`;
  console.log(`fetching guest images (${rootfsName})…`);
  const kdl = path.join(home, "dl", `kernel-Image-${arch}.gz`);
  const rdl = path.join(home, "dl", rootfsName);
  try {
    await ghAsset(`kernel-Image-${arch}.gz`, kdl);
    await ghAsset(rootfsName, rdl);
  } catch (e) {
    console.error(`could not fetch guest images: ${e.message}
  Manual fallback:  gh run download -R ${REPO} -n guest-images-${arch}
  then: NEBULA_HOME=.nebula ${nebula} install-image --kernel kernel-Image-${arch}.gz --rootfs ${rootfsName}`);
    process.exit(1);
  }
  run(nebula, ["install-image", "--kernel", kdl, "--rootfs", rdl]);
}

function writeConfig() {
  fs.mkdirSync(home, { recursive: true });
  fs.writeFileSync(
    path.join(home, "config.toml"),
    `api_port = ${cfg.apiPort}
dns_port = ${cfg.dnsPort}
k8s_port = ${cfg.k8sPort}
dns_zone = "${cfg.dnsZone}"
max_ram_mib = ${cfg.maxRamMib}
cpus = ${cfg.cpus}
data_disk_gib = ${cfg.dataDiskGib}
`,
  );
}

function run(nebula, args) {
  const r = spawnSync(nebula, args, {
    stdio: "inherit",
    env: { ...process.env, NEBULA_HOME: home },
  });
  if (r.status !== 0) process.exit(r.status ?? 1);
}

const cmd = process.argv[2];
const nebula = await ensureBinaries();
writeConfig();
switch (cmd) {
  case "up":
    await ensureImages(nebula);
    run(nebula, ["up"]);
    console.log(`engine API: http://127.0.0.1:${cfg.apiPort}/v1alpha1 (docs: docs/httpapi.md in the nebula repo)`);
    break;
  case "down":
    run(nebula, ["down"]);
    break;
  case "status":
    run(nebula, ["status"]);
    break;
  default:
    console.log("usage: node scripts/engine.mjs up|down|status");
    process.exit(1);
}
