# Nebula

Open source, simple, and performant container, Kubernetes & microVM manager
for macOS, Linux, and Windows.

Nebula runs one elastically-sized Linux VM (the **Vessel**) on the platform's
native hypervisor for your everyday containers and Kubernetes, plus
millisecond-boot isolated microVMs on a vendored
[libkrun](https://github.com/containers/libkrun) fork for sandboxes and GPU
workloads — with memory ballooning so the whole stack only holds the RAM your
workloads actually use.

Runs on **macOS** (Virtualization.framework), **Linux** (KVM), and
**Windows** (Hyper-V/WHP) — no WSL2 — with CI/CD release builds for all
three.

```
nebula up                 # boots the Vessel (~0.6s to a healthy engine)
nebula setup docker       # point docker at Nebula (revert anytime)
docker run -d -p 8080:80 nginx     # localhost:8080 just works
```

## Two flavors, one host

**Full** Nebula ships the real Go stack (dockerd/containerd, k3s, kubectl,
helm) — the genuine article. **Nebula-slim** swaps the guest for `slimd`, a
from-scratch Rust reimplementation of a useful container + Kubernetes + Helm
subset that's small enough to **embed** (~32 MB, no Go runtime). Pick full
when you need real k8s; pick slim to embed an engine or when size/RAM is the
budget.

## Documentation

- [The HTTP API (v1alpha1)](httpapi.html) — the REST embedding surface shared
  by full Nebula and Nebula-slim
- [Embedding Nebula in your own app](embedding.html) — artifact inventory,
  embed kits, and the consuming app's responsibilities
- [Field notes: embedding nebula-slim in a shipped app](embeddingronotes.html)
  — what actually cost time building the first embedder that shipped
- [slim engine configuration](slim-config.html) — every `slimd` and host CLI
  environment variable

## Status

Nebula is under active development and will be open-sourced here. Watch this
repo for the source release.
