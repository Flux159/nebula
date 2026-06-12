// {{APP_NAME}} — starter UI. Replace this with your product; the panels
// prove the plumbing: engine status (direct), the fork demo and a
// sqlite-persisted note (both via the app's own hyper server).
// See AGENTS.md for the project shape.

import { useEffect, useMemo, useState } from "react";
import { app, nebula, type EngineStatus } from "./nebula";

// figlet "ANSI Shadow" — gradient-filled by .wordmark in styles.css
const NEBULA = String.raw`
███╗   ██╗███████╗██████╗ ██╗   ██╗██╗      █████╗
████╗  ██║██╔════╝██╔══██╗██║   ██║██║     ██╔══██╗
██╔██╗ ██║█████╗  ██████╔╝██║   ██║██║     ███████║
██║╚██╗██║██╔══╝  ██╔══██╗██║   ██║██║     ██╔══██║
██║ ╚████║███████╗██████╔╝╚██████╔╝███████╗██║  ██║
╚═╝  ╚═══╝╚══════╝╚═════╝  ╚═════╝ ╚══════╝╚═╝  ╚═╝`;

function Starfield() {
  // ASCII stars, twinkling on individual delays. Generated once per mount.
  const stars = useMemo(
    () =>
      Array.from({ length: 90 }, (_, i) => ({
        id: i,
        ch: ["·", "✦", "·", "*", "·", "."][i % 6],
        left: `${Math.random() * 100}%`,
        top: `${Math.random() * 100}%`,
        size: `${8 + Math.random() * 8}px`,
        delay: `${-Math.random() * 6}s`,
        dur: `${2 + Math.random() * 4}s`,
      })),
    [],
  );
  return (
    <div className="backdrop" aria-hidden>
      {stars.map((s) => (
        <span
          key={s.id}
          className="star"
          style={{
            left: s.left,
            top: s.top,
            fontSize: s.size,
            animationDelay: s.delay,
            ["--tw" as never]: s.dur,
          }}
        >
          {s.ch}
        </span>
      ))}
    </div>
  );
}

export default function App() {
  const [status, setStatus] = useState<EngineStatus | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [demo, setDemo] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState("");
  const [savedAt, setSavedAt] = useState("");

  useEffect(() => {
    const tick = () =>
      nebula
        .status()
        .then((s) => (setStatus(s), setErr(null)))
        .catch((e) => setErr(String(e.message ?? e)));
    tick();
    const t = setInterval(tick, 3000);
    app.getSetting("note").then((v) => v !== null && setNote(v));
    return () => clearInterval(t);
  }, []);

  const runDemo = async () => {
    setBusy(true);
    setDemo("forking a running microVM…");
    try {
      setDemo(await app.forkDemo());
    } catch (e) {
      setDemo(`demo failed: ${e}`);
    } finally {
      setBusy(false);
    }
  };

  const saveNote = async () => {
    await app.setSetting("note", note);
    setSavedAt(new Date().toLocaleTimeString());
  };

  return (
    <>
      <Starfield />
      <main className="shell">
        <pre className="wordmark">{NEBULA}</pre>
        <p className="tagline">
          <span className="sparkle">✦</span> Create your own{" "}
          <span className="grad">agent orchestrator</span>{" "}
          <span className="sparkle s2">✦</span>
        </p>

        <section className="card">
          <h3>engine · {{FLAVOR}}</h3>
          {err ? (
            <p>
              <span className="dot" /> unreachable — run <code>npm run engine:up</code>
              <br />
              <small>{err}</small>
            </p>
          ) : status ? (
            <p>
              <span className="dot ok" />
              {status.vmState} · kernel {status.agent?.kernel} · {status.cpus} cpus ·{" "}
              {status.memMib} MiB
            </p>
          ) : (
            <p>connecting…</p>
          )}
        </section>

        <section className="card">
          <h3>the primitive</h3>
          <p>
            Snapshot a <em>running</em> Linux VM and fork it — RAM, processes and all — in
            about a second. Served by the app's hyper backend (
            <code>POST /api/fork-demo</code>).
          </p>
          <button onClick={runDemo} disabled={busy || !!err}>
            {busy ? "running…" : "✦ fork a live VM"}
          </button>
          {demo && <pre className="term">{demo}</pre>}
        </section>

        <section className="card">
          <h3>app persistence · sqlite</h3>
          <p>
            Stored via <code>PUT /api/settings/note</code> → rusqlite in the OS app-data dir —
            the same table components like model-config keep API keys in.
          </p>
          <input
            value={note}
            onChange={(e) => setNote(e.target.value)}
            placeholder="type something, restart the app, it's still here"
          />
          <button onClick={saveNote}>save</button>
          {savedAt && <small> saved {savedAt}</small>}
        </section>

        <p className="foot">
          Build something: hand <code>AGENTS.md</code> to your coding agent · components live
          in <code>components/</code>
        </p>
      </main>
    </>
  );
}
