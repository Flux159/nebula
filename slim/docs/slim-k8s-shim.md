# Slim Kubernetes — the facade-over-Docker shim (what we built)

nebula-slim implements `kubectl` and `helm` **without a Kubernetes control
plane**. There is no apiserver, no etcd, no scheduler, no controller-manager,
no kubelet, and no k3s binary in the guest (that row in the size ledger is
`0`). Instead, `kubectl-slim` and `helm-slim` are host binaries that translate
a useful subset of Kubernetes into plain containers on the slim engine, over
the Docker Engine API that `slimd` already serves.

This is the deliberate slim trade: **the primitives, at near-zero added size**
(kubectl-slim ≈ 0.8 MB, helm-slim ≈ 0.9 MB, vs 67 MB for k3s + 55/59 MB for
real kubectl/helm). If you need real Kubernetes semantics, use the **full**
nebula flavor (k3s) — see "When to use which" below.

## How it works

```
  kubectl-slim apply -f app.yaml          helm-slim install rel ./chart
            |                                        |
            v                                        v
        slim-kube  (parse k8s YAML ──► Engine API calls)   <── slim-helm renders
            |                                                    charts → YAML, then
            v                                                    hands to slim-kube
   POST /containers/create  →  slimd  →  real Linux containers in the vessel
```

- The clients speak the **Docker Engine API** to `slimd` on the same
  `/var/run/docker.sock` the host proxy already forwards. No new plumbing.
- Identity is carried in container **labels** (`io.nebula.kube.*`) so
  `get`/`delete`/`scale`/`logs` reconstruct the Kubernetes view from the
  running containers — there is no separate object store.

## Kind → container mapping

| Kubernetes kind | Slim mapping |
|---|---|
| **Deployment** / ReplicaSet / StatefulSet | N restart-supervised containers `<name>-<i>` (replicas), labeled, alias = service name for DNS |
| **DaemonSet** | 1 container (single-node model) |
| **Pod** | one container `<name>` |
| **Job** | run-to-completion container (restart=no) |
| **Service** | published ports (NodePort/LoadBalancer) + container DNS aliases (ClusterIP resolves by service name) |
| **ConfigMap** | env vars (and, later, mounted files) into selecting workloads |
| **Secret** | same as ConfigMap, base64 values decoded |
| everything else | **skipped with a warning** (see below) |

`apply` is two-pass: it indexes ConfigMaps/Secrets/Services first, then
creates workloads with their env and published ports already resolved.

## Verbs

`kubectl-slim`: `apply -f`, `delete -f`/`delete KIND NAME`, `get KIND [NAME]`
(`-o json|name|wide`), `scale --replicas=N deployment/NAME`, `logs [-f] POD`,
`exec [-it] POD -- CMD`, `describe`.

`helm-slim`: `install`, `upgrade`, `template`, `uninstall`, `list`. Charts
load from a local dir or `.tgz`; values merge is
`chart defaults ← -f files ← --set`; rendering is a Go-template subset plus a
sprig subset (`default`, `quote`, `indent`/`nindent`, `toYaml`/`toJson`,
`b64enc`, `required`, `include`/`tpl`, arithmetic, …). Release state is a
local file (no in-cluster Secrets).

## Unsupported kinds: skip, don't fail (and `--strict`)

Any kind outside the table above (CRDs, CustomResources, RBAC —
Role/RoleBinding/ClusterRole/ServiceAccount, Ingress, PersistentVolumeClaim,
NetworkPolicy, HPA, …) is **parsed and accepted, then skipped** with:

```
warning: kind <X> is not supported by slim — skipped
```

The rest of the file still applies. By default `apply`/`install` exit `0` even
when something was skipped (the "mostly-compatible, warn-don't-fail" stance).
Pass **`--strict`** to make them exit non-zero and list what was dropped — use
this in CI so a silently-ignored CRD doesn't read as success.

### Why skipping RBAC is correct, not lossy

There is no apiserver to enforce Roles/bindings, and the microVM itself is the
security boundary. Accepting-and-ignoring RBAC produces the same end state for
your workloads that a real cluster would — it is honest, not a gap.

### Why operators don't work today

An operator is just a Deployment, so its pod **does** start — but inside it
tries to reach the (nonexistent) apiserver, watch its (skipped) CRDs, and win a
leader-election Lease. None of that exists, so it crashloops. CRD + CR pairs
both skip, so anything built on custom resources is a no-op. Making operators
work needs an actual (if lightweight) apiserver — see
[`slim-k8s-roadmap.md`](./slim-k8s-roadmap.md).

## When to use which

- **slim** — embedding, size/resource budget matters, you need container +
  basic-k8s + helm **primitives** and won't use CRDs/operators/RBAC/the k8s
  API. ~26 MB total embed.
- **full** — you want real Kubernetes (k3s): the API, CRDs, operators,
  admission, RBAC enforcement, the ecosystem. Larger image, real control
  plane.

This is the core nebula split: slim gives you a tiny, fast VMM-with-batteries
to drop into an app; full gives you the genuine article when you actually need
its advanced features.
