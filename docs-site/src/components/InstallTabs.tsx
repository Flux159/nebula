import { useState } from 'react';

function detectOS(): 'unix' | 'windows' {
  if (typeof navigator === 'undefined') return 'unix';
  const ua = navigator.userAgent.toLowerCase();
  if (ua.includes('win')) return 'windows';
  return 'unix';
}

const commands = {
  install: {
    unix: 'curl -fsSL https://flux159.github.io/nebula/install.sh | bash',
    windows: 'irm https://flux159.github.io/nebula/install.ps1 | iex',
  },
  verify: {
    unix: `nebula up                 # boots the Vessel
nebula setup docker       # point docker at Nebula
docker run -d -p 8080:80 nginx     # localhost:8080 just works`,
    windows: `nebula up                 # boots the Vessel
nebula setup docker       # point docker at Nebula
docker run -d -p 8080:80 nginx     # localhost:8080 just works`,
  },
};

export function InstallTabs() {
  return <PlatformTabs type="install" />;
}

export function VerifyTabs() {
  return <PlatformTabs type="verify" />;
}

function PlatformTabs({ type }: { type: 'install' | 'verify' }) {
  const [tab, setTab] = useState<'unix' | 'windows'>(detectOS);
  const [copied, setCopied] = useState(false);

  const text = commands[type][tab];

  const handleCopy = async () => {
    await navigator.clipboard.writeText(text);
    setCopied(true);
    setTimeout(() => setCopied(false), 1000);
  };

  return (
    <div className="install-tabs">
      <div className="install-tab-buttons">
        <button
          className={`install-tab-btn ${tab === 'unix' ? 'active' : ''}`}
          onClick={() => setTab('unix')}
        >
          macOS / Linux
        </button>
        <button
          className={`install-tab-btn ${tab === 'windows' ? 'active' : ''}`}
          onClick={() => setTab('windows')}
        >
          Windows
        </button>
        <button className="install-copy-btn" onClick={handleCopy}>
          {copied ? 'Copied!' : 'Copy'}
        </button>
      </div>
      <div className="install-command">
        <pre style={{ margin: 0, background: 'none', border: 'none', padding: 0, overflow: 'auto' }}>
          <code style={{ background: 'none', padding: 0, fontSize: 'inherit', fontFamily: 'inherit' }}>
            {text}
          </code>
        </pre>
      </div>
    </div>
  );
}
