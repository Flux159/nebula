# Battle-testing Nebula: scale limits + balloon regression harness

Status: **implemented** (2026-06-12) — `crates/nebula-battletest`, entry points
`scripts/battletest.{sh,ps1}`. Quick balloon suite passed end-to-end on the
128 GiB M-series Mac (all 6 checks); full runs + sweeps in `bench/results/`,
baseline in `bench/baselines/`. Open question defaults below were approved.

## Goal

Get real numbers — not vibes — for where Nebula breaks on a given RAM budget,
for both flavors (full Go stack and slim), and turn the memory-balloon contract
into a repeatable regression suite. Concretely:

1. **Containers in vessel 0** (the engine vessel): how many containers can run
   per configured `max_ram_mib`, where and how it breaks. Both **full**
   (dockerd/containerd) and **slim** (`slimd`).
2. **Vessels without containers**: how many concurrent vessels (vz and krun
   backends) fit per-vessel `--mem` × count, where and how it breaks.
3. **Balloon battle tests**: extend the phase-4 single-hog check into a
   multi-cycle, multi-container contract suite with machine-readable baselines,
   so a regression shows up as a diff, not a hunch.
4. Output as **CSV/JSON + charts** we can put in the README/docs.

Test machine for the first pass: this Mac (M-series, 16 cores, **128 GiB**).
Scripts must *run* on macOS/Linux/Windows but only macOS is exercised now (no
fast access to the ubuntu/Windows boxes this week).

## Harness: one Rust binary, not three shell scripts

A new workspace crate **`crates/nebula-battletest`** (binary `nebula-battletest`,
excluded from release packaging). Rationale:

- Must run on Windows — bash is out, and maintaining `.sh` + `.ps1` twins of a
  long test driver is how scripts rot. Python isn't guaranteed on Windows
  runners. A Rust binary compiles for all three from the workspace we already
  have, and gets `cargo build` CI compile coverage for free even though the
  tests themselves never run on hosted runners.
- It drives everything through surfaces that are already cross-platform:
  the **REST API** (`127.0.0.1:7440/v1alpha1` — stats, vessels CRUD) and the
  docker socket/named pipe via the user's `docker`/`docker-slim` CLI. Footprint
  numbers come from `nebulad`'s stats endpoint, which already does the
  per-platform accounting (VZ XPC processes on macOS, etc.) — the harness never
  reimplements platform memory probing.
- Thin entry points for humans: `scripts/battletest.sh` and
  `scripts/battletest.ps1` that build, **re-sign (macOS)**, and exec the binary.

CLI shape (subcommands = scenarios):

```
nebula-battletest container-scale --flavor full|slim --max-ram <MiB,...> --workload idle|nginx|hog:256m
nebula-battletest vessel-scale    --backend vz|krun --mem <MiB> [--with-snapshots]
nebula-battletest balloon         [--cycles N] [--baseline bench/baselines/<host>.json]
nebula-battletest report          --in bench/results/ --out bench/report/   # tables + SVG charts
```

Shared infra inside the crate:

- **Sampler**: 1–2 s poll loop of `/v1alpha1/stats` (+ host free RAM) for the
  whole run, written as a timeseries CSV per scenario. Every scenario gets a
  footprint/balloon/pressure trace for free.
- **Config juggling**: scenarios that sweep `max_ram_mib` rewrite
  `~/.nebula/config.toml` + `nebula down`/`up` between points. The harness
  snapshots the user's config at start and **always restores it** (incl. on
  panic/ctrl-C).
- **Break detection** (a "point" ends when any of): container/vessel create
  returns an error; start latency > 10× the rolling median; guest OOM-kill seen
  (`dmesg` via `nebula exec`); agent health check fails; docker/API call times
  out (30 s); host free RAM < 4 GiB (safety stop so we don't wedge the Mac).
  The *first* tripped condition is recorded as the failure mode — "it broke at
  N=312 because dockerd hit pid limits" is the actual deliverable.
- **Results**: `bench/results/<date>-<host>-<scenario>.{csv,json}` (committed —
  they're the data behind the charts), `bench/report/` for rendered output.
  JSON carries host metadata (chip, RAM, OS build, nebula git sha, flavor).

## Scenario 1 — container scale in vessel 0 (full + slim)

Sweep matrix (first pass on this 128 GiB machine):

| axis | values |
|---|---|
| `max_ram_mib` | 4096, 8192, 16384, 32768, 65536 |
| flavor | full (dockerd), slim (`slimd`) |
| workload | **idle** (`alpine sleep inf`), **nginx** (real server + published port every 10th container), **hog:256m** (each container touches 256 MiB then holds) |

Procedure per point: set config → fresh `up` → pre-pull images once → add
containers in batches of 10 (`--restart no`, named `bt-<n>`), after each batch
wait for all running + sample stats → continue until a break condition trips →
record N_max, failure mode, per-batch start latency, footprint/balloon trace →
teardown, next point.

Expected outputs:

- **Chart: containers vs max RAM** (one line per workload, full vs slim
  side-by-side). The idle line measures engine overhead-per-container; the hog
  line should be ≈ `(max_ram − engine_base) / 256 MiB` if ballooning is honest —
  deviation from that line *is* the finding.
- Failure-mode table (what actually broke at each point — guest OOM, dockerd
  limits, agent timeouts, slimd fd limits…).
- Full-vs-slim delta: slimd's per-container overhead and its breaking point are
  unknown today; this is the first real stress slimd gets. Expect to file
  issues; the suite should keep running past *expected* per-container failures
  (count them) and only stop the point on engine-level breakage.

Slim mechanics: build slim rootfs (`FLAVOR=slim vessel/build-rootfs.sh` with
slimd from `slim/scripts/build-musl.sh`), install to a **separate image path**
and point `rootfs =` at it in the temp config, so full/slim runs don't trample
each other's images. Workloads driven via `DOCKER_HOST` + stock docker CLI for
full, `docker-slim` for slim (same harness code path, different CLI name).

## Scenario 2 — vessel scale (no containers)

Sweep: per-vessel `--mem` ∈ {1024, 2048, 4096} × backend ∈ {vz, krun}, engine
vessel left up (it's the realistic baseline). Procedure: `vessels new bt-v<n>`
one at a time → wait booted (vessel exec true) → sample → continue to break.

Measure: max concurrent vessels, boot latency vs N (does the 80–96 ms VZ
create→run degrade?), host footprint vs N (idle vessels should cost ~tens of
MiB each if ballooning works; krun has no balloon — that contrast is the
chart), failure mode. Known unknowns to characterize: a hard VZ concurrent-VM
cap (Apple is rumored to limit concurrent VZ VMs — find the real number),
fd/uffd limits for krun, nebulad task limits.

Chart: **vessels vs per-vessel max RAM**, vz vs krun; plus footprint-per-idle-
vessel.

## Scenario 3 — balloon contract + regression suite

Phase 4's script proves one cycle of one hog. The battle version codifies the
*contract* (from `tasks/issues.md` characterization — high-water-mark
semantics, so the enforceable promises are bounded growth + re-inflate, not
shrink) and stresses it:

Checks, each emitting numbers into the JSON (thresholds in one place):

1. **Idle reclaim**: after settle, balloon holds > 75 % of max; footprint < 4 GiB.
2. **Single-hog cycle** (the phase-4 test, absorbed here): deflate under load,
   no OOM, re-inflate ≤ 90 s after release, footprint ≤ peak + 512 MiB.
3. **Repeat-cycle drift**: run the hog cycle **10×**; idle-balloon level and
   re-inflate latency must not trend (fit a slope; fail if idle held degrades
   > 5 % over the run). Catches slow leaks in the controller or guest.
4. **Concurrent hogs**: 4 × 2 GiB hogs starting 3 s apart against a 16 GiB max —
   deflate must keep pace (no OOM-kill), DEFLATE_ON_OOM backstop counted but
   bounded.
5. **Pressure at the ceiling**: one hog sized to ~95 % of available guest RAM —
   guest survives, hog may die (OOM-killing the hog is acceptable; killing
   dockerd/slimd/agent is a failure).
6. **Sawtooth**: alternate 30 s hog / 30 s idle for 10 min; assert balloon
   resize *count* stays ~2/cycle (the "0 resizes/hour at steady state" claim,
   exercised).

Regression mode: `--baseline bench/baselines/<host>.json` compares every metric
against the stored baseline with per-metric tolerance (default ±15 %); exit
non-zero on regression. First green run on this Mac *writes* the baseline and
commits it. This is the "verify ballooning works without regressions in the
future" artifact — runnable on demand and on future self-hosted runners.

## CI stance

- **Not** on GitHub-hosted runners (RAM-starved). The harness crate *compiles*
  in normal CI (it's in the workspace) so it can't rot; scenarios refuse to run
  unless `NEBULA_BATTLETEST=1` or `--yes` is passed (they rewrite config and
  down/up the engine — destructive to a dev's session).
- Structure everything so a future self-hosted runner just runs
  `scripts/battletest.sh all --baseline …`. A `--quick` tier (one sweep point,
  3 balloon cycles, ~10 min) for pre-merge use on those runners later.

## Execution order (once plan is approved)

1. Harness crate skeleton: sampler, config save/restore, break detection, CSV/
   JSON writers, `report` (tables + simple SVG line charts, no plotting deps).
2. Scenario 3 (balloon) first — smallest, absorbs test-phase4, produces the
   regression baseline immediately.
3. Scenario 1 full flavor sweep on this Mac (longest wall-clock; runs
   backgrounded per CLAUDE.md while building scenario 2).
4. Scenario 2 vessel sweep.
5. Slim rootfs build + scenario 1 slim sweep; file slimd issues as found.
6. `report`, commit results + charts, link from README benchmarks section;
   update `tasks/issues.md` with every surprise.

Runtime estimate for the full first pass on this machine: the sweeps are
hours-scale, so they run as background jobs with the monitor pattern; the
balloon suite alone is ~45 min.

## Open questions (defaults chosen, flag if wrong)

1. **Sweep ceiling 64 GiB ok?** Host has 128; I stop sweeps when host free RAM
   < 4 GiB regardless.
2. **Results committed to the repo?** Plan says yes (`bench/results/` +
   baseline JSON). Alternative: gitignore results, commit only the report.
3. **Hog granularity 256 MiB per container** for the density line — fine, or do
   you want a second size (e.g. 1 GiB) for the chart?
4. **Windows/Linux**: code paths compile + `--dry-run` smoke only for now;
   real runs when the boxes are reachable / custom runners exist. Assumed fine.
