# Better slim Kubernetes — the apiserver-lite roadmap

> **Status (2026-06): Tier A is built.** `slim-kubeapi` is a passive
> apiserver-lite — discovery, generic CRUD, **watch** (resourceVersion +
> 410-on-stale), and **dynamic CRD registration** over an in-memory store.
> Validated against **real `kubectl` v1.35** (12/12): `api-resources`, core
> CRUD, applying a CRD → it appears in discovery → custom-resource round-trip,
> and `kubectl get -w` streaming both replayed and live events. It does NOT
> reconcile (a stored Deployment doesn't run — that's the facade / Tier B).
> One caveat: `kubectl apply` does a client-side OpenAPI-v2-*protobuf*
> preflight slim doesn't serve, so CLI users pass `--validate=false`;
> client-go operators don't do that preflight and need no flag. Remaining to
> ship it to in-pod operators: host it in the vessel on a TLS port + project a
> ServiceAccount token/CA + a `kubernetes.default` Service (integration, below).



Today's slim k8s is a [facade over the Docker engine](./slim-k8s-shim.md): it
maps a useful subset of manifests onto containers, and **skips** CRDs,
CustomResources, RBAC, and anything operator-shaped. This doc scopes what it
would take to go further — specifically, to stop operators from crashlooping —
and argues for *how far* to go.

## What an operator actually requires

An operator's pod already runs under the facade. It dies because it can't
reach a **Kubernetes API server**. controller-runtime (what almost all
operators use) needs, in rough order of difficulty:

1. **Discovery** — `/api`, `/apis`, `/openapi/v3`: the catalog of groups,
   versions, and resources.
2. **CRUD** — GET/LIST/POST/PUT/PATCH/DELETE on `/api/v1/...` and
   `/apis/<group>/<version>/<plural>`, namespaced and cluster-scoped.
3. **Watch** — `?watch=true` streaming add/update/delete. **This is the
   crux.** Every operator does LIST-then-WATCH via an informer cache, and the
   cache is unforgiving: a dropped event, a wrong `resourceVersion`, or a
   missing `410 Gone` on a stale RV wedges or hot-loops the controller. ~60%
   of the implementation effort lives here.
4. **Dynamic CRD registration** — applying a CRD must make
   `/apis/<group>/<version>/<plural>` start serving immediately, and discovery
   must reflect it.
5. The long tail:
   - **status subresource** (`/status`) updates,
   - **Leases** for leader election (most operators won't reconcile until they
     win one),
   - **ownerReferences + garbage collection**, **finalizers**,
   - **field/label selectors** on list/watch,
   - **admission & conversion webhooks** (a chunk of operators register them),
   - **server-side apply**, multi-version CRD conversion.
6. And separately: a CR the operator creates usually expands to a
   Deployment/StatefulSet that must **actually run** — i.e. the facade has to
   be reframed as a reconcile loop reading the store and writing Pod status.

## Effort, tiered (honest estimates)

| Tier | Scope | Est. | Gets you |
|---|---|---|---|
| **A. apiserver-lite core** | generic typeless store (sqlite/in-mem) + CRUD + discovery + CRD registration + correct watch/`resourceVersion` | ~1–2 wks (watch is most of it) | clients & operators can connect, list, watch without crashlooping |
| **B. facade → controller bridge** | Deployments/Pods created via the API actually run and report Pod status back into the store | ~3–5 days (reshapes existing facade code) | workloads the operator creates come up |
| **C. simple/home-grown operators** | A + B + Leases + status subresource + ownerRefs | ~2–3 wks total | a CR → "run this Deployment with these knobs" operator works |
| **D. real ecosystem operators** (cert-manager, prometheus-operator, …) | webhooks, SSA, multi-version conversion, the full informer-consistency contract | **open-ended, weeks→months**, per-operator debugging | the genuine article — at which point you've largely rebuilt kube-apiserver |

Binary size is a non-issue (an apiserver-lite is ~a few MB). The real cost is
**conceptual surface and permanent bug-compatibility maintenance**: informers
break on subtle wire-protocol drift, and that's miserable to debug remotely.
This is exactly why k3s exists and why "lightweight k8s" is hard.

## The uncanny-valley risk

The failure mode to avoid is a thing that *looks* enough like Kubernetes that
people aim operators and CRD-bearing Helm charts at it, then it fails subtly
(a dropped watch event, a missing `/status`). That erodes trust far more than
today's clean "unsupported in slim — use the full flavor" message. **Don't
ship Tier C/D unless we commit to maintaining the informer contract.**

## Recommended path

1. **Keep the two-tier story** (slim = primitives + size; full = real k8s via
   k3s). It is the honest framing and already shipped.
2. **Done now:** `--strict` on `kubectl-slim apply` / `helm-slim install` so a
   skipped CRD fails loudly instead of reading as success.
3. **If/when operator support is genuinely requested**, build the cheap
   **middle path** first — a *passive generic apiserver* (Tier A only): CRUD +
   watch + discovery over arbitrary objects, **no CR reconciliation**. That
   alone buys:
   - `kubectl get/apply/delete` round-tripping **every** kind (better fidelity
     than today's skip), and
   - operators that connect and watch without crashlooping,
   while being honest that it won't *act* on custom resources.
4. Only pursue Tier B/C (real reconciliation) behind explicit demand, and
   treat Tier D as "use the full flavor" indefinitely.

## Integration notes (when we build it)

- The operator runs **inside** the vessel (as a container), so the
  apiserver-lite is easiest to host **in the vessel** too — either a second
  port on `slimd` or a sidecar — reachable via a `kubernetes.default` Service
  ClusterIP + a synthesized ServiceAccount token & CA the operator's pod
  mounts.
- Reuse `slim-kube`'s label model as the projection layer: store objects in
  the apiserver, project Deployments/Pods to containers via the controller
  bridge, and write observed container state back as Pod status.
- Store: sqlite keeps it crash-consistent and tiny; `resourceVersion` is a
  single monotonic counter; watch is a broadcast log tailed per-connection
  with `410 Gone` when a client's RV is older than the log's floor.
