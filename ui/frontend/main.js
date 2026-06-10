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
    root.innerHTML = `<div class="empty">No containers. Try: <code>nebula use docker && docker run -d -p 8080:80 nginx</code></div>`;
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
      return `<tr>
        <td>${name}</td>
        <td>${c.Image}<div class="src">${imageProvenance(c)}</div></td>
        <td class="${stateClass}">${c.Status}</td>
        <td class="ports">${ports}</td>
      </tr>`;
    })
    .join("");
  root.innerHTML = `<table>
    <thead><tr><th>Name</th><th>Image</th><th>Status</th><th>Ports</th></tr></thead>
    <tbody>${rows}</tbody>
  </table>`;
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

refresh();
setInterval(refresh, 2000);
cliToolsCheck();
