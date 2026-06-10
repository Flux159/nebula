# microvm-k8s — research brief ("Kubernetes for microVMs")

Thesis (Suyog, 2026-06-10): production workloads as microVMs instead of
containers, behind a Kubernetes-compatible API, with **snapshot/branch/
restore as first-class cluster primitives**. Research project until a cloud
provider picks it up; the adoption path matters as much as the tech.

## Why this isn't already a product (and what is)

- **AWS Fargate/Lambda** already run every workload in a Firecracker
  microVM — proof of demand and of the density model — but the snapshot
  machinery is internal plumbing, never exposed as an API.
- **Kata Containers** = pods-as-microVMs behind CRI on stock k8s. Proves
  the integration shape; doesn't change what the API can express.
- **fly.io** built a Firecracker orchestrator with its own API
  (deliberately not k8s); their Machines API has suspend/resume but no
  k8s compatibility and no branch.
- **k3s/kine** already proved the k8s API runs over pluggable stores
  (SQLite/dqlite/NATS) — "replace etcd" is solved-ish plumbing, NOT the
  research contribution.

The gap nobody fills: `kubectl checkpoint deploy/agent` /
`kubectl branch deploy/agent --count 16` with live memory state, ms-scale
scale-from-zero via **resume instead of cold start**, and hard multi-tenant
isolation by default. Nebula uniquely has the substrate: ~100ms cold boots,
live memory snapshots (vz today; fork-native cross-platform per the
krun-snapshot track in tasks/issues.md), CoW disk + CoW memory clones,
and a guest agent/vsock control plane.

## Phase 0 (prerequisite, in the nebula fork): krun-snapshot

Firecracker-style snapshot/restore in third_party/libkrun: RAM file +
vCPU/device state, `mmap(MAP_PRIVATE)` restore (<10ms target), N clones
sharing pages CoW. KVM first, HVF second. Detailed in tasks/issues.md.
Everything below consumes this.

## Phase 1 (credible, weeks): a CRI runtime backed by nebula

`nebula-cri`: implement the Kubernetes CRI gRPC surface (RuntimeService +
ImageService) so EXISTING clusters schedule pods as nebula microVMs via
RuntimeClass — the Kata adoption path, but riding our boot times and
snapshots. Concretely:

- Pod sandbox = vessel (krun/KVM on Linux hosts); containers-in-pod =
  processes supervised by vessel-agent inside it (single-container pods
  first; that's the overwhelming majority).
- ImageService backed by the existing convert-image pipeline + the
  base/upper overlayfs split (disk layer dedup).
- Networking: vessel NIC (usernet) joined to the node's pod CIDR via a
  bridge backend — phase 1 can cheat with the agent vsock TCP proxy for
  service traffic the way nebula already does ports.
- Demoable on the ubuntu box against the k3s that nebula itself runs:
  the cluster schedules its pods INTO nebula microVMs. Good demo, honest
  dogfood.
- Success metric: conformance subset (pod lifecycle, logs, exec, probes)
  + pod-start latency vs containerd + density curve (pods/GB).

## Phase 2 (the swing): microVM-native control plane

k8s-API-compatible frontend (apiserver wire compat for the core types;
kine-style pluggable store — Raft over a modern embedded store, not etcd)
with a scheduler that thinks in:

- **Snapshot lineage**: checkpoint/branch/restore verbs on Deployments and
  Jobs (the cluster-scale agent tree-search primitive; also bisectable
  prod incidents — "branch prod at the snapshot before the bad deploy").
- **Resume pools**: scale-from-zero = restore from a pre-warmed snapshot
  (ms), not image pull + boot (s). Autoscaling becomes page-fault-bounded.
- **Hard tenancy**: every pod is a kernel; no shared-kernel escape class.

Deliberately out of scope until phase 2 is real: live migration,
cross-host PV story, full CNI compatibility, multi-arch image fan-out.
These are the known production gaps — name them in every demo so the
research stays honest.

## Relationship to the other tracks

- **nebula-slim** (slim/BRIEF.md): its kubectl facade is the single-node
  embryo of phase 2's API surface; its size work shrinks the per-node
  footprint this needs.
- **Windows/WHP**: phase 1's runtime is Linux-host; WHP joins later for
  dev parity, not prod.
- The Claude Cowork datapoint (anthropics/claude-code#29045, HN 48479452):
  agents-in-VMs is being deployed at consumer scale TODAY with 10GB
  bundles and no lifecycle control — the demand side of this brief.

## Working agreement

Separate repo when it starts (this brief moves there); same logging
discipline (issues.md pattern); compatibility/perf numbers in every
commit message the way slim reports its corpus percentage.
