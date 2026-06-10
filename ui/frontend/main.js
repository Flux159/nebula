// Nebula UI: a thin client of the nebulad REST API (Phase 10).
const API = "http://127.0.0.1:7440";

const $ = (id) => document.getElementById(id);
const fmtMib = (mib) =>
  mib >= 1024 ? `${(mib / 1024).toFixed(1)} GiB` : `${Math.round(mib)} MiB`;

async function getJson(path) {
  const res = await fetch(`${API}${path}`, { signal: AbortSignal.timeout(4000) });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return res.json();
}

function renderStatus(status, stats) {
  $("dot").classList.toggle("on", status.vmState === "Running");
  $("engine-state").textContent = status.vmState.toLowerCase();
  $("engine-info").textContent = `${status.cpus} cpus · max ${fmtMib(status.memMib)}`;

  if (stats.guest) {
    const balloonHeld = stats.maxMib - stats.balloonTargetMib;
    // Clamp to the current allowance: during balloon transitions the guest
    // hasn't moved the pages the commanded target implies yet, and the raw
    // subtraction briefly reads as tens of GiB.
    const usedMib = Math.min(
      stats.balloonTargetMib,
      Math.max(0, (stats.guest.total_kib - stats.guest.available_kib) / 1024 - balloonHeld),
    );
    const pct = Math.min(100, (usedMib / stats.balloonTargetMib) * 100);
    $("mem-bar").style.width = `${pct}%`;
    $("mem-text").textContent = `${fmtMib(usedMib)} / ${fmtMib(stats.balloonTargetMib)} allowed`;
    $("balloon").textContent = `${fmtMib(balloonHeld)} reclaimed`;
  }
  $("footprint").textContent = fmtMib(stats.hostFootprintMib);
}

function imageProvenance(c) {
  // Local builds have no registry prefix and digests as IDs; pulled images
  // carry a repo (and possibly registry host). Untagged shows as <none>.
  if (!c.Image || c.Image.startsWith("sha256:")) return "local (untagged)";
  if (c.Image.includes("/")) return "registry";
  return "registry (library)";
}

function renderContainers(list) {
  const root = $("containers");
  if (!list.length) {
    root.innerHTML = `<div class="empty">No containers. Try: <code>nebula setup docker && docker run -d -p 8080:80 nginx</code></div>`;
    return;
  }
  const rows = list
    .map((c) => {
      const name = (c.Names?.[0] || c.Id.slice(0, 12)).replace(/^\//, "");
      const ports = (c.Ports || [])
        .filter((p) => p.PublicPort)
        .map(
          (p) =>
            `<a href="http://localhost:${p.PublicPort}" target="_blank">:${p.PublicPort}</a>`,
        )
        .join("") || "–";
      const stateClass = c.State === "running" ? "state-running" : "state-exited";
      const actions = window.__TAURI__
        ? `<button data-logs="${c.Id.slice(0, 12)}" data-name="${name}" data-running="${c.State === "running"}">Logs</button>`
        : "–";
      return `<tr>
        <td>${name}</td>
        <td>${c.Image}<div class="src">${imageProvenance(c)}</div></td>
        <td class="${stateClass}">${c.Status}</td>
        <td class="ports">${ports}</td>
        <td class="row-actions">${actions}</td>
      </tr>`;
    })
    .join("");
  root.innerHTML = `<table>
    <thead><tr><th>Name</th><th>Image</th><th>Status</th><th>Ports</th><th></th></tr></thead>
    <tbody>${rows}</tbody>
  </table>`;
  root.querySelectorAll("button[data-logs]").forEach((btn) => {
    btn.onclick = () =>
      openLogs(btn.dataset.logs, btn.dataset.name, btn.dataset.running === "true");
  });
}

// --- logs modal (+ copyable shell command) -------------------------------
let logsId = null;
async function openLogs(id, name, running) {
  logsId = id;
  $("logs-title").textContent = `Logs — ${name}`;
  $("logs-body").textContent = "loading…";
  // Interactive shells need a real terminal; hand the user the exact command.
  $("shell-cmd").textContent = running
    ? `nebula docker exec -it ${id} sh`
    : `nebula docker start -ai ${id}`;
  $("logs-modal").style.display = "flex";
  await loadLogs();
}
async function loadLogs() {
  if (!logsId) return;
  try {
    const text = await window.__TAURI__.core.invoke("container_logs", { id: logsId });
    $("logs-body").textContent = text.trim() || "(no output yet)";
    $("logs-body").scrollTop = $("logs-body").scrollHeight;
  } catch (e) {
    $("logs-body").textContent = `failed to fetch logs: ${e}`;
  }
}

// --- kubernetes view ------------------------------------------------------
let kubeKind = null;
async function loadKube(kind) {
  kubeKind = kind;
  document.querySelectorAll("#kube-kinds button").forEach((b) =>
    b.classList.toggle("active", b.dataset.kind === kind),
  );
  const out = $("kube-out");
  if (!window.__TAURI__) {
    out.innerHTML = `<div class="empty">Open the Nebula app (or run <code>nebula kubectl get ${kind} -A</code>).</div>`;
    return;
  }
  out.innerHTML = `<div class="empty">loading ${kind}… (first use starts k3s, ~20s)</div>`;
  try {
    const text = await window.__TAURI__.core.invoke("kube_get", { kind });
    if (kubeKind !== kind) return; // user clicked another kind meanwhile
    out.innerHTML = `<pre class="log"></pre>`;
    out.firstChild.textContent = text.trim() || `(no ${kind})`;
  } catch (e) {
    if (kubeKind !== kind) return;
    out.innerHTML = `<div class="err"></div>`;
    out.firstChild.textContent = `kubectl failed: ${e}`;
  }
}

// --- apps catalog -----------------------------------------------------------
let catalog = null;
async function loadApps() {
  const grid = $("apps-grid");
  if (!window.__TAURI__) {
    grid.innerHTML = `<div class="empty">Open the Nebula app to install apps (or use <code>nebula docker run …</code>).</div>`;
    return;
  }
  try {
    if (!catalog) catalog = await window.__TAURI__.core.invoke("apps_catalog");
    const status = await window.__TAURI__.core.invoke("apps_status");
    grid.innerHTML = catalog.apps.map((a) => appCard(a, status[a.id])).join("");
    grid.querySelectorAll("button[data-app]").forEach((btn) => {
      btn.onclick = () => appAction(btn.dataset.app, btn.dataset.action);
    });
  } catch (e) {
    grid.innerHTML = `<div class="err">catalog failed: ${e}</div>`;
  }
}

function appCard(a, st) {
  const state = st?.state || "";
  const running = state === "running";
  const installed = !!st;
  const links = running
    ? a.ports
        .map((p) => `<a href="http://localhost:${p.host}" target="_blank">${p.label} :${p.host}</a>`)
        .join("")
    : "";
  const buttons = !installed
    ? `<button class="primary" data-app="${a.id}" data-action="install">Install</button>`
    : running
      ? `<button data-app="${a.id}" data-action="logs">Logs</button>
         <button data-app="${a.id}" data-action="stop">Stop</button>
         <button data-app="${a.id}" data-action="uninstall">Uninstall</button>`
      : `<button class="primary" data-app="${a.id}" data-action="start">Start</button>
         <button data-app="${a.id}" data-action="logs">Logs</button>
         <button data-app="${a.id}" data-action="uninstall">Uninstall</button>`;
  return `<div class="app-card">
    <div class="head"><span class="ico">${a.icon}</span><div>
      <h3>${a.name}</h3>
      <span class="state ${state}">${st ? st.status : "not installed"}</span>
    </div></div>
    <div class="desc">${a.description}</div>
    <div class="actions">${buttons}${links}</div>
  </div>`;
}

async function appAction(id, action) {
  const app = catalog.apps.find((a) => a.id === id);
  try {
    if (action === "install") {
      await window.__TAURI__.core.invoke("app_install", { spec: app });
    } else if (action === "logs") {
      openLogs(`nebula-app-${id}`, app.name, true);
      return;
    } else {
      if (action === "uninstall" && !confirmUninstall(app.name)) return;
      await window.__TAURI__.core.invoke("app_ctl", { id, action });
    }
  } catch (e) {
    $("apps-grid").insertAdjacentHTML(
      "afterbegin",
      `<div class="err">${app.name}: ${String(e).slice(0, 300)}</div>`,
    );
  }
  loadApps();
}

function confirmUninstall(name) {
  // Data lives in named volumes and survives — this only removes the container.
  return window.confirm
    ? confirm(`Uninstall ${name}? Its data volumes are kept.`)
    : true;
}

// --- run-command boxes -------------------------------------------------------
function wireRunBox(btnId, inputId, outId, invoke, payload) {
  const btn = $(btnId);
  if (!btn) return;
  btn.onclick = async () => {
    if (!window.__TAURI__) return;
    const out = $(outId);
    out.style.display = "block";
    out.textContent = "running…";
    btn.disabled = true;
    try {
      const text = await window.__TAURI__.core.invoke(invoke, payload());
      out.textContent = text.trim() || "(no output)";
    } catch (e) {
      out.textContent = String(e);
    }
    btn.disabled = false;
    refresh();
  };
}

// --- sidebar navigation ----------------------------------------------------
function showView(view) {
  document.querySelectorAll("#nav a").forEach((a) =>
    a.classList.toggle("active", a.dataset.view === view),
  );
  document.querySelectorAll(".view").forEach((v) =>
    v.classList.toggle("active", v.id === `view-${view}`),
  );
  if (view === "kubernetes" && !kubeKind) loadKube("pods");
  if (view === "apps") loadApps();
}

async function refresh() {
  try {
    const [status, stats, containers] = await Promise.all([
      getJson("/v1alpha1/status"),
      getJson("/v1alpha1/stats"),
      getJson("/v1alpha1/containers"),
    ]);
    renderStatus(status, stats);
    renderContainers(containers);
  } catch (e) {
    $("dot").classList.remove("on");
    $("engine-state").textContent = "engine offline";
    // Don't re-render (and wipe button state) on every poll while offline.
    if ($("start-engine")) return;
    $("containers").innerHTML =
      `<div class="err">Cannot reach the Nebula engine (${e.message}).</div>
       <button id="start-engine">Start engine</button>
       <div class="src" style="margin-top:6px">or run <code>nebula up</code> · <code>nebula autostart enable</code> starts it at login</div>`;
    const btn = $("start-engine");
    if (btn && window.__TAURI__) {
      btn.onclick = async () => {
        btn.disabled = true;
        btn.textContent = "starting…";
        try {
          await window.__TAURI__.core.invoke("start_engine");
          await refresh();
        } catch (err) {
          $("containers").innerHTML = `<div class="err">Start failed: ${err}</div>`;
        }
      };
    } else if (btn) {
      btn.style.display = "none"; // not running inside Tauri (plain browser)
    }
  }
}

// --- bundled CLI tools (docker/kubectl/helm) setup flow -----------------
async function cliToolsCheck() {
  if (!window.__TAURI__) return; // plain-browser dev: no sidecar to manage
  let st;
  try {
    st = await window.__TAURI__.core.invoke("cli_tools_status");
  } catch {
    return;
  }
  const chip = $("cli-chip");
  const modal = $("cli-modal");
  if (!st.missing.length) {
    chip.style.display = "none";
    return;
  }
  $("cli-modal-detail").textContent =
    `Not found on your PATH: ${st.missing.join(", ")}.`;
  chip.style.display = "flex";
  chip.onclick = () => (modal.style.display = "flex");
  $("cli-later").onclick = () => {
    modal.style.display = "none";
    localStorage.setItem("cli-modal-dismissed", "1");
  };
  $("cli-install").onclick = async () => {
    const btn = $("cli-install");
    btn.disabled = true;
    btn.textContent = "linking…";
    try {
      await window.__TAURI__.core.invoke("setup_cli_tools");
      $("cli-modal-detail").textContent =
        "Done — restart your terminal (or `source` your shell profile) to pick it up.";
      btn.textContent = "Added ✓";
      setTimeout(() => {
        modal.style.display = "none";
        cliToolsCheck();
      }, 2200);
    } catch (err) {
      $("cli-modal-detail").textContent = `Setup failed: ${err}`;
      btn.disabled = false;
      btn.textContent = "Add to PATH";
    }
  };
  // Auto-open once per dismissal; the chip stays for later.
  if (!localStorage.getItem("cli-modal-dismissed")) {
    modal.style.display = "flex";
  }
}

document.querySelectorAll("#nav a").forEach((a) => {
  a.onclick = (e) => {
    e.preventDefault();
    showView(a.dataset.view);
  };
});
document.querySelectorAll("#kube-kinds button").forEach((b) => {
  b.onclick = () => loadKube(b.dataset.kind);
});
$("logs-close").onclick = () => {
  $("logs-modal").style.display = "none";
  logsId = null;
};
$("logs-refresh").onclick = loadLogs;
$("shell-copy").onclick = async () => {
  try {
    await navigator.clipboard.writeText($("shell-cmd").textContent);
    $("shell-copy").textContent = "Copied ✓";
    setTimeout(() => ($("shell-copy").textContent = "Copy"), 1500);
  } catch {
    /* clipboard unavailable outside secure contexts */
  }
};

wireRunBox("docker-cmd-run", "docker-cmd", "docker-cmd-out", "docker_command", () => ({
  command: $("docker-cmd").value,
}));
wireRunBox("kube-apply", "kube-yaml", "kube-apply-out", "kube_apply", () => ({
  yaml: $("kube-yaml").value,
}));

refresh();
setInterval(refresh, 2000);
setInterval(() => {
  // Keep app cards fresh while the Apps view is visible.
  if (document.querySelector("#view-apps.active") && catalog) loadApps();
}, 5000);
cliToolsCheck();
loadApps();
