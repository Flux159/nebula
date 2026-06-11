# slim k8s — pod fidelity roadmap (4 phases)

The slim apiserver + controller bridge run real workloads (Deployments/
Jobs/Pods → engine containers, with restart supervision and correct lifecycle
*phase*). This doc tracks closing the remaining pod-fidelity gaps so the slim
k8s view matches what real `kubectl`/operators expect — the things that matter
for embedding (Galaxy: "is my service actually ready", sidecars, init setup).

**Out of scope (deliberate):** the level-based 1s reconcile loop stays. It
polls *in-process, in-memory* state (a BTreeMap walk + a few mutex reads per
pod, not etcd/network), so at embedding scale it's sub-millisecond CPU and
self-healing. Event-driven triggers would only buy sub-second *latency*, at the
cost of informer-consistency fragility — not worth it now. (Both event sources
exist if we ever want a hybrid: the store's `watch` + the engine's `/events`.)

All bridge work lives in `crates/slimd/src/kube_bridge.rs`; status shaping is in
`crates/slim-kube/src/lib.rs` (`summarize`) for the kubectl-slim table.

---

## Phase 1 — `containerStatuses` (pod status fidelity)  ✅ DONE

**Goal:** populate `status.containerStatuses[]` so real `kubectl`'s READY /
RESTARTS columns are correct and probes (Phase 2) have somewhere to write.

**Why cheap:** the data already exists — the engine tracks `restart_count`
(`engine.rs` increments it; `inspect.rs` exposes it) and `State` carries
`status`/`exit_code`/`started_at`/`finished_at`.

**Approach:**
- In `ensure_container`, read the live container `State` and emit one
  `containerStatus` per pod container: `{name, image, imageID, containerID,
  ready, restartCount, started, state:{running|terminated|waiting}}`.
- State map: `running`→`running{startedAt}`; `exited`/`dead`→
  `terminated{exitCode,startedAt,finishedAt,reason}`; `created`→
  `waiting{reason:"ContainerCreating"}`.
- `ready` = running (Phase 2 makes it probe-driven). Pod `Ready` condition =
  all containers ready. Pod `phase` logic unchanged.
- `sync_pod` writes the array into `status`.

**Validate:** real `kubectl get pods` shows correct READY (`1/1`) + RESTARTS;
crash a pod, confirm RESTARTS increments. Extend test/kube-bridge.sh.

## Phase 2 — readiness / liveness probes  ✅ DONE

**Goal:** honor `readinessProbe` / `livenessProbe` so READY reflects real
health and hung-but-alive containers get restarted.

**Approach:**
- Parse probes from the container spec (httpGet/tcpSocket/exec + initialDelay/
  period/timeout/success+failureThreshold).
- Per-pod prober thread: exec → reuse `exec_in_container`; tcp → connect
  `podIP:port`; http → GET `podIP:port/path`.
- Readiness result → `containerStatuses[].ready` + pod `Ready` condition.
- Liveness failure → kill the container; the engine's restartPolicy
  supervision (already present) recreates it. Track restart attribution.

**Validate:** a Deployment whose container is up but failing its readiness
probe shows `0/1` and `Ready=False`; a liveness-failing container restarts
(RESTARTS climbs). New test/kube-probes.sh.

## Phase 3 — multi-container pods / sidecars  ✅ DONE

**Goal:** run all `spec.containers[]`, not just `[0]`, sharing one network
namespace (and emptyDir volumes) — the sidecar pattern.

**Why feasible:** the runtime primitive already exists — `spec.netns:
Option<PathBuf>` + `setns(fd, CLONE_NEWNET)` in `slim-runtime/linux.rs` joins an
existing netns. The gap is engine wiring: `engine.rs` currently rejects
`network:container:<id>` ("unsupported; using bridge").

**Approach:**
- Per-pod **sandbox**: designate a netns holder (an infra/pause container or
  `containers[0]`); create the rest with `spec.netns` → the holder's netns.
- Wire the engine `container:<id>` network mode to resolve the target's netns
  path and pass it through (instead of falling back to bridge).
- Shared emptyDir volumes across the pod's containers.
- Aggregate status across N containers (pod Running iff all running; phase from
  the set); `containerStatuses[]` becomes N-element (Phase 1 already arrayed).
- exec/logs gain a `-c <container>` selector (proxy maps `pod`+`container` →
  `<ns>_<pod>_<container>`).

**Validate:** a pod with app+sidecar containers — both run, share localhost,
`kubectl logs -c sidecar` works, READY is `2/2`. New test/kube-sidecar.sh.

## Phase 4 — init containers  ⬜

**Goal:** run `spec.initContainers[]` sequentially to completion before the
main containers start (the setup/migration pattern).

**Approach (on top of Phase 3):**
- Bridge runs init containers in order, each to exit-0 before the next; pod
  phase `Pending` with `Init:N/M` until all complete; a failing init blocks
  (honoring its restartPolicy) and surfaces in status.
- `initContainerStatuses[]` in pod status.

**Validate:** a pod with an init container that writes a file the main
container reads; main starts only after init exits 0; a failing init keeps the
pod `Pending`. Extend test/kube-sidecar.sh.

---

## Status ledger
- Phase 1 — containerStatuses: ✅ DONE (also added server-side `Table` printing
  so real kubectl renders NAME/READY/STATUS/RESTARTS/AGE). kube-bridge.sh 13/13.
- Phase 2 — probes: ✅ DONE. exec/tcp/httpGet readiness+liveness via a 1-thread
  prober; readiness gates Ready, liveness SIGKILLs → restart supervision recreates
  (restartCount++). kube-probes.sh 7/7. **Also fixed a latent engine deadlock**
  (E↔C lock-order: get_entry/list/name_taken held the entries-map lock across a
  blocking container lock, inverting start_entry's order via refresh_network_hosts;
  the new prober thread, as a second concurrent engine accessor, exposed it). Fix:
  snapshot Arc<Entry> under the map lock, release, then lock containers.
  Liberties: exec probe output >64KB or a hung probe command stalls the prober
  tick; named ports unresolved.
- Phase 3 — multi-container/sidecars: ✅ DONE. Pod = N engine containers;
  container 0 is the sandbox/netns holder (`<ns>_<pod>`), the rest
  (`<ns>_<pod>.<name>`) join its netns via `container:<holder>` (engine wired to
  setns `/proc/<pid>/ns/net`). Shared emptyDir volumes; aggregated pod
  status/phase; exec/logs gain `-c`. kube-sidecar.sh 8/8 (shared localhost +
  shared volume). Liberties: emptyDir only (no configMap/secret/hostPath
  volumes); if the holder restarts, sidecars keep the stale netns until they
  exit and the level reconcile recreates them.
- Phase 4 — init containers: ⬜
