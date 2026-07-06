import { useState } from 'react';
import { Link } from 'react-router-dom';

const quickStart = `nebula up                 # boots the Vessel (~0.6s to a healthy engine)
nebula setup docker       # point docker at Nebula (revert anytime)
docker run -d -p 8080:80 nginx     # localhost:8080 just works`;

export default function HomePage() {
  const [copied, setCopied] = useState(false);

  return (
    <>
      <nav className="navbar">
        <Link to="/" className="navbar-brand">
          Nebula
        </Link>
        <div className="navbar-links">
          <Link to="/docs/getting-started">Docs</Link>
          <a href="https://github.com/Flux159/nebula" target="_blank" rel="noopener">
            GitHub
          </a>
        </div>
      </nav>

      <div style={{ marginTop: 'var(--navbar-height)' }}>
        <section className="hero">
          <h1>Nebula</h1>
          <p>
            Open source, simple, and performant container, Kubernetes &amp; microVM
            manager for macOS, Linux, and Windows.
          </p>

          <div className="install-tabs">
            <div className="install-tab-buttons">
              <span className="install-tab-btn active">Quick start</span>
              <button
                className="install-copy-btn"
                onClick={async () => {
                  await navigator.clipboard.writeText(quickStart);
                  setCopied(true);
                  setTimeout(() => setCopied(false), 1000);
                }}
              >
                {copied ? 'Copied!' : 'Copy'}
              </button>
            </div>
            <div className="install-command">
              <code style={{ whiteSpace: 'pre', display: 'block', textAlign: 'left' }}>
                {quickStart}
              </code>
            </div>
          </div>

          <div className="hero-buttons">
            <Link to="/docs/getting-started" className="btn btn-primary">
              Get Started
            </Link>
            <Link to="/docs/http-api" className="btn btn-secondary">
              HTTP API
            </Link>
          </div>

          <p style={{ marginTop: '24px', fontSize: '0.95em', opacity: 0.8 }}>
            The source will be open-sourced in this repository.
          </p>
        </section>

        <section className="features">
          <div className="feature">
            <h3>One Elastic Vessel VM</h3>
            <p>
              Nebula runs one elastically-sized Linux VM (the Vessel) on the
              platform's native hypervisor for your everyday containers and
              Kubernetes.
            </p>
          </div>

          <div className="feature">
            <h3>Millisecond-Boot microVMs</h3>
            <p>
              Isolated microVMs on a vendored libkrun fork for sandboxes and GPU
              workloads — <code>nebula sandbox run</code> boots, runs, and tears
              down a VM in ~250ms.
            </p>
          </div>

          <div className="feature">
            <h3>Memory Ballooning</h3>
            <p>
              A balloon controller returns idle RAM, so the whole stack only holds
              the memory your workloads actually use — a 32 GiB Vessel idles at
              ~1.1 GiB host-visible footprint.
            </p>
          </div>

          <div className="feature">
            <h3>Truly Cross-Platform</h3>
            <p>
              macOS (Virtualization.framework), Linux (KVM), and Windows
              (Hyper-V/WHP) — no WSL2 — with CI/CD release builds for all three.
            </p>
          </div>

          <div className="feature">
            <h3>Two Flavors, One Host</h3>
            <p>
              Full Nebula ships the real Go stack (dockerd/containerd, k3s,
              kubectl, helm). Nebula-slim swaps the guest for slimd, a small Rust
              reimplementation — ~32 MB and built to embed.
            </p>
          </div>

          <div className="feature">
            <h3>Your Tools Just Work</h3>
            <p>
              <code>nebula setup docker</code> points the standard docker CLI at
              Nebula (revert anytime), and Rosetta runs x86_64 containers on
              Apple Silicon at near-native speed.
            </p>
          </div>
        </section>
      </div>
    </>
  );
}
