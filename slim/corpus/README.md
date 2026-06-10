# nebula-slim compatibility corpus

An executable suite of real `docker` invocations that scores a docker
daemon+client implementation. The same corpus runs against real dockerd
(via nebula) and against `slimd`; the SCORE is the compatibility number.

## Running

Against **real dockerd via nebula** (baseline — should score ~100%):

```sh
DOCKER_HOST="unix://$HOME/.nebula/run/docker.sock" ./run.sh
```

(or just `./run.sh` with the `nebula` docker context active.)

Against **slimd**:

```sh
DOCKER_BIN=docker DOCKER_HOST=unix:///tmp/slimd.sock ./run.sh
# or with a slim client binary:
DOCKER_BIN=/path/to/slim-docker ./run.sh
```

## Env contract

| Variable        | Default  | Meaning                                            |
|-----------------|----------|----------------------------------------------------|
| `DOCKER_BIN`    | `docker` | client binary under test                           |
| `DOCKER_HOST`   | (unset)  | daemon socket under test; passed through untouched |
| `CORPUS_FILTER` | `*`      | glob over case names, e.g. `'1*'` or `'*build*'`   |
| `CORPUS_QUICK`  | `0`      | `1` = skip heavy app cases (190, 200)              |
| `CORPUS_KEEP`   | `0`      | `1` = on failure, keep containers/volumes and scratch dirs for debugging |
| `CORPUS_VERBOSE`| `0`      | `1` = print every passing assertion, not just failures |

## Output & scoring

One line per case — `PASS|FAIL|SKIP name (Ns)` — then a summary block:

```
total=25 pass=23 fail=1 skip=1
SCORE=95%
```

`SCORE = pass / (pass + fail)`, **skips excluded** (a SKIP is "not
applicable", e.g. compose missing on slim — expected, not a defect). If
nothing ran, SCORE is `n/a`. Exit status is nonzero iff any case FAILed.

Machine-readable results go to `results/last.tsv`
(`name<TAB>status<TAB>seconds`, overwritten per run); the full log of each
FAILed case is copied to `results/<name>.log`.

## How it works

- `run.sh` executes every `cases/*.sh` in lexical order (hence the 010-,
  020- numbering), each in its own `sh -u` process — **`set -u`, not `-e`**;
  cases assert explicitly. A case PASSES if it reaches its end with zero
  failed assertions.
- `lib.sh` is sourced into each case and provides:
  - `dk <args...>` — invoke `$DOCKER_BIN` (use this, never bare `docker`)
  - `assert_ok <desc> <cmd...>` — run, capture stdout→`$OUT` / stderr→`$ERR`,
    assert exit 0 (`assert_fail` = expect nonzero)
  - `assert_exit <n> <desc> <cmd...>` — assert exact exit code
  - `assert_out_contains <desc> <pattern>` — BRE grep over last `$OUT`
  - `assert_out_eq <desc> <expected>` — exact match (trailing newline aside)
  - `assert_retry_contains <secs> <desc> <pattern> <cmd...>` — rerun ~1/s
    until the output matches or the deadline passes (port/app warm-up)
  - `skip <reason>` / `skip_if_quick` — mark case SKIP and stop it
  - `cleanup_add <cmd...>` — teardown, runs in **reverse** order at case
    exit, even after failures (suppressed by `CORPUS_KEEP=1` on failure)
  - `ensure_image <img>` — best-effort pull so cases work standalone under
    `CORPUS_FILTER`
- Everything is namespaced `slimtest-` and the runner does a best-effort
  `rm -f` sweep of `slimtest-*` containers/volumes/networks/images at start
  and end.

## House style

- **POSIX sh throughout** (`run.sh`, `lib.sh`, all cases): no arrays, no
  bashisms — verified shapes run on macOS bash 3.2, dash, and alpine ash.
  Nothing here needs bash; if a future case does, give it a `#!/bin/bash`
  shebang and note why.
- **Capture-then-grep, always.** Never `cmd | grep -q`: an early-closing
  reader SIGPIPEs the producer and manufactures phantom failures (house rule
  inherited from nebula). All assertions grep the captured `$OUT`/`$ERR`
  files. No `pipefail` + early-exit-reader patterns anywhere.
- TMPDIR-safe: all scratch goes under `mktemp -d` in `${TMPDIR:-/tmp}`. Note
  case 100 bind-mounts a TMPDIR path into a container — host file sharing of
  that path is deliberately part of the contract being scored.
- No interactive `-it` cases — see `manual-checks.md` for the 3 manual ones.

## Files

- `run.sh` — runner (env contract above)
- `lib.sh` — assertion/cleanup helpers, sourced per case
- `cases/*.sh` — 25 cases, 010–250
- `results/last.tsv` — last run's machine-readable results
- `diffproxy.sh` — stub socat record proxy for capturing API traffic to diff
  real-dockerd vs slimd wire behavior (not load-bearing)
- `manual-checks.md` — interactive TTY checks done by hand

## Known deviations / notes

- `230-events-lite` uses absolute epoch `--since/--until` instead of the
  naive `--since 0s --until 5s`: docker parses relative durations as "that
  long **ago**", which would close the window before the test container ever
  starts. `--until` + a bounded background poll makes the case hang-proof.
- `140-restart-policy` accepts `RestartCount > 0` **or** `State.Running ==
  true`: with a 1-second container under `--restart=always`, which of the
  two you observe after 5s is pure timing; either proves the policy engaged.
- All cases pin `alpine:3.19` (the spec mixed bare `alpine` and `alpine:3.19`)
  so results don't drift when `latest` moves.
