// {{APP_NAME}} — starter UI. Replace this with your product; the panels
// below prove the plumbing: engine status (direct), the fork demo and a
// sqlite-persisted note (both via the app's own hyper server).
// See AGENTS.md for the project shape.

import { useEffect, useState } from "react";
import { app, nebula, type EngineStatus } from "./nebula";

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
    <main style={{ fontFamily: "system-ui", padding: "2rem", maxWidth: 640, margin: "0 auto" }}>
      <h1>{{APP_NAME}}</h1>
      <section style={card}>
        <h3 style={{ marginTop: 0 }}>engine ({{FLAVOR}})</h3>
        {err ? (
          <p>
            unreachable — run <code>npm run engine:up</code>
            <br />
            <small>{err}</small>
          </p>
        ) : status ? (
          <p>
            {status.vmState} · kernel {status.agent?.kernel} · {status.cpus} cpus ·{" "}
            {status.memMib} MiB
          </p>
        ) : (
          <p>connecting…</p>
        )}
      </section>
      <section style={card}>
        <h3 style={{ marginTop: 0 }}>the primitive</h3>
        <p>
          Snapshot a <em>running</em> Linux VM and fork it — RAM, processes and all — in about
          a second. Served by the app's hyper backend (<code>POST /api/fork-demo</code>).
        </p>
        <button onClick={runDemo} disabled={busy || !!err} style={btn}>
          {busy ? "running…" : "fork a live VM"}
        </button>
        {demo && <pre style={{ whiteSpace: "pre-wrap" }}>{demo}</pre>}
      </section>
      <section style={card}>
        <h3 style={{ marginTop: 0 }}>app persistence (sqlite)</h3>
        <p>
          Stored via <code>PUT /api/settings/note</code> → rusqlite in <code>data/app.db</code> —
          the same table components like model-config keep API keys in.
        </p>
        <input
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="type something, restart the app, it's still here"
          style={{ width: "70%", padding: "0.4rem" }}
        />{" "}
        <button onClick={saveNote} style={btn}>
          save
        </button>
        {savedAt && <small> saved {savedAt}</small>}
      </section>
      <p style={{ opacity: 0.6 }}>
        Build something: hand <code>AGENTS.md</code> to your coding agent. Components live in{" "}
        <code>components/</code>.
      </p>
    </main>
  );
}

const card: React.CSSProperties = {
  border: "1px solid #ddd",
  borderRadius: 8,
  padding: "1rem 1.25rem",
  marginBottom: "1rem",
};
const btn: React.CSSProperties = { padding: "0.5rem 1rem", fontSize: "1rem" };
