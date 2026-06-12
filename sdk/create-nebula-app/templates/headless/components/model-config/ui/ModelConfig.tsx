// Model & connections settings section — pairs with src/routes.rs.
// Self-contained: plain fetch + Tailwind (dark theme). Drop into your
// settings page as <ModelConfig />; pass apiBase if /api isn't same-origin.
//
// Conventions encoded (COMPONENT.md): secrets are write-only (status pill +
// last-4 hint come back, never the value); every key row says what it
// unlocks; the local base URL is the server ROOT (no /v1) and must be
// host-reachable from containers (192.168.64.1 under nebula on macOS).

import { useCallback, useEffect, useState } from 'react';

interface Connection {
  key: string;
  set: boolean;
  unlocks: string;
  hint?: string | null;
}

interface SettingsPayload {
  connections: Connection[];
  modelProvider?: string | null;
  localModelBaseUrl?: string | null;
  localModelName?: string | null;
}

async function getJson<T>(url: string): Promise<T> {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${r.status}: ${await r.text()}`);
  return r.json();
}

async function patchJson(url: string, body: Record<string, string>) {
  const r = await fetch(url, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!r.ok) throw new Error(`${r.status}: ${await r.text()}`);
}

function ConnectionRow({ conn, url, onSaved }: { conn: Connection; url: string; onSaved: () => void }) {
  const [value, setValue] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    if (!value.trim()) return;
    setSaving(true);
    setError(null);
    try {
      await patchJson(url, { [conn.key]: value.trim() });
      setValue('');
      onSaved();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'failed');
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="py-4 border-b border-gray-700 last:border-0">
      <div className="flex items-center gap-3 flex-wrap">
        <span className="font-mono text-sm text-white">{conn.key}</span>
        {conn.set ? (
          <span className="px-2 py-0.5 bg-green-900/60 text-green-300 rounded text-xs font-medium">
            Connected{conn.hint ? ` · ${conn.hint}` : ''}
          </span>
        ) : (
          <span className="px-2 py-0.5 bg-gray-700 text-gray-400 rounded text-xs font-medium">Not set</span>
        )}
      </div>
      <p className="text-xs text-gray-400 mt-1">Unlocks: {conn.unlocks}</p>
      <div className="flex gap-2 mt-2">
        <input
          type="password"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder={conn.set ? 'Replace key…' : 'Enter key…'}
          className="flex-1 max-w-md bg-gray-900 border border-gray-700 rounded px-3 py-1.5 text-sm text-white focus:outline-none focus:border-blue-500"
        />
        <button
          onClick={save}
          disabled={saving || !value.trim()}
          className="px-3 py-1.5 bg-blue-600 hover:bg-blue-500 rounded text-sm font-medium text-white disabled:opacity-50"
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
      </div>
      {error && <p className="text-xs text-red-400 mt-1">{error}</p>}
    </div>
  );
}

function InputRow({
  label, settingKey, initialValue, placeholder, note, url, onSaved,
}: {
  label: string; settingKey: string; initialValue: string; placeholder: string;
  note?: string; url: string; onSaved: () => void;
}) {
  const [value, setValue] = useState(initialValue);
  const [saving, setSaving] = useState(false);
  return (
    <div>
      <label className="block text-sm text-gray-300 mb-1">{label}</label>
      <div className="flex gap-2">
        <input
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder={placeholder}
          className="flex-1 max-w-md bg-gray-900 border border-gray-700 rounded px-3 py-1.5 text-sm text-white focus:outline-none focus:border-blue-500"
        />
        <button
          onClick={async () => {
            setSaving(true);
            try { await patchJson(url, { [settingKey]: value.trim() }); onSaved(); } finally { setSaving(false); }
          }}
          disabled={saving}
          className="px-3 py-1.5 bg-blue-600 hover:bg-blue-500 rounded text-sm font-medium text-white disabled:opacity-50"
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
      </div>
      {note && <p className="text-xs text-gray-500 mt-1">{note}</p>}
    </div>
  );
}

export default function ModelConfig({ apiBase = '' }: { apiBase?: string }) {
  const url = `${apiBase}/api/settings`;
  const [settings, setSettings] = useState<SettingsPayload | null>(null);

  const load = useCallback(async () => {
    setSettings(await getJson<SettingsPayload>(url));
  }, [url]);

  useEffect(() => {
    load().catch(() => {});
  }, [load]);

  if (!settings) return <p className="text-gray-500 text-sm">Loading…</p>;

  const provider = settings.modelProvider || 'openrouter';

  return (
    <div className="space-y-8">
      <section className="bg-gray-800 border border-gray-700 rounded-lg p-5">
        <h2 className="text-lg font-semibold text-white mb-2">Connections</h2>
        <p className="text-xs text-gray-500 mb-3">
          This app ships with no keys — every integration is yours, stored locally on this machine.
        </p>
        {settings.connections.map((conn) => (
          <ConnectionRow key={conn.key} conn={conn} url={url} onSaved={load} />
        ))}
      </section>

      <section className="bg-gray-800 border border-gray-700 rounded-lg p-5 space-y-4">
        <h2 className="text-lg font-semibold text-white">Model Provider</h2>
        <div>
          <label className="block text-sm text-gray-300 mb-1">Provider</label>
          <select
            value={provider}
            onChange={async (e) => {
              await patchJson(url, { model_provider: e.target.value });
              load();
            }}
            className="bg-gray-900 border border-gray-700 rounded px-3 py-1.5 text-sm text-white focus:outline-none focus:border-blue-500"
          >
            <option value="openrouter">OpenRouter (cloud)</option>
            <option value="local">Local (llama.cpp / LM Studio / any OpenAI-compatible)</option>
          </select>
        </div>
        {provider === 'local' && (
          <>
            <InputRow
              label="Base URL"
              settingKey="local_model_base_url"
              initialValue={settings.localModelBaseUrl ?? ''}
              placeholder="http://192.168.64.1:8080"
              note="Server ROOT url (no /v1 — clients append it). Containerized workloads can't use localhost: on macOS with nebula use 192.168.64.1 for a server on this machine."
              url={url}
              onSaved={load}
            />
            <InputRow
              label="Model name"
              settingKey="local_model_name"
              initialValue={settings.localModelName ?? ''}
              placeholder="qwen3-30b-a3b"
              url={url}
              onSaved={load}
            />
          </>
        )}
      </section>
    </div>
  );
}
