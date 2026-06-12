// {{APP_NAME}} — starter UI. Replace this with your product; the engine
// panel below proves the plumbing and demos the headline primitive
// (forking a RUNNING microVM). See AGENTS.md for the project shape.

import { useEffect, useState } from "react";
import { nebula, type EngineStatus } from "./nebula";

export default function App() {
  const [status, setStatus] = useState<EngineStatus | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [demo, setDemo] = useState<string>("");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const tick = () =>
      nebula
        .status()
        .then((s) => (setStatus(s), setErr(null)))
        .catch((e) => setErr(String(e.message ?? e)));
    tick();
    const t = setInterval(tick, 3000);
    return () => clearInterval(t);
  }, []);

  const runDemo = async () => {
    setBusy(true);
    setDemo("forking a running microVM…");
    try {
      setDemo(await nebula.forkDemo());
    } catch (e) {
      setDemo(`demo failed: ${e}`);
    } finally {
      setBusy(false);
    }
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
          Snapshot a <em>running</em> Linux VM and fork it — RAM, processes and all — in
          about a second. This button does it via the app's Rust base layer.
        </p>
        <button onClick={runDemo} disabled={busy || !!err} style={btn}>
          {busy ? "running…" : "fork a live VM"}
        </button>
        {demo && <pre style={{ whiteSpace: "pre-wrap" }}>{demo}</pre>}
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
