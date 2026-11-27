# Results — campaigns and names

Every directory under `results/` is a **campaign**: one session of benchmark
runs. This page explains each campaign and how its names are built.

All campaigns so far ran on the **development VM**:

- 2 vCPU (Intel i5-6500T), ≈ 2 GB RAM;
- PostgreSQL 18 limited to 1.5 CPU and 1 GB.

On that VM every variant is CPU-bound: the numbers compare designs, they do not
measure capacity.

## 1. Reading a path

```
results/<campaign>/<scenario>/<label>/rep-<n>/
```

| Level | Meaning | Example |
|---|---|---|
| `<campaign>` | the session of runs (§2) | `burst60-20261127` |
| `<scenario>` | the load applied (§3) | `c6` |
| `<label>` | the design and its variant (§4) | `b-1rt-60s` |
| `rep-<n>` | repetition number of that scenario × label | `rep-1` |

Example: `burst60-20261127/c6/b-1rt-60s/rep-1` is the first repetition of
project B with the single-round-trip producer and a 60 s switch cycle, under
the periodic-burst load, in the campaign run on 27 Nov 2026.

Files:

- **Per campaign:**
  - `summary.md` — tables and charts; every chart says what it shows and what it is for;
  - `runs.csv` — every metric of every run;
  - `charts/` — SVG images, named `<scenario>-<chart>.svg` or `<scenario>-bar-<metric>.svg`;
  - `campaign.log` — the run log.
- **Some campaigns also have:** `bursts.md`, `calibration.env`, `host.txt`, `start.log`.
- **Per run:** `meta.env` (parameters and timestamps; when a key repeats, the
  last value is the one used) and `raw/` (logs and CSVs, not versioned).

## 2. Campaigns

Campaign names follow `<kind>-<UTC date>[-<HHMM>]`. The kind comes from the
tool that started the campaign or, for hand-made campaigns, from what it tests.

| Campaign | Why this name | What it is | Status | Used for |
|---|---|---|---|---|
| `pilot` | fixed name: the **calibration pilot** | 2 min saturation, 16 producers without rate limit, A vs B, 1 rep. 2026-11-26 22:35 UTC, rev `95337c0` | complete | first comparison: A ≈ 5.6k tx/s, B ≈ 2.8k; pointed at B's 4 round trips |
| `campaign-20261126` | `campaign-<date>`: default name of `bench/campaign.sh` (the **full matrix**) | c1 (5 min, a / b × 3 reps) and c2 (10 min at 2,000 tx/s, the uncalibrated scenario default; a, a-delete, b × 1). 2026-11-26 22:51 UTC, rev `d24d03e` | **interrupted** after `c2/a-delete`: the remaining c2 repetitions, C4, c3 and c5 were not run | B's cycles growing on their own to 21–32 s at saturation; consumer rate under saturation |
| `smoke-20261127-0029` | `smoke-<date>-<HHMM>`: `./start.sh smoke`, the **quick end-to-end check** | quick profile: every run 60 s (15 s warm-up), 100k seeded orders, 1 rep; calibration (`calibration.env`: 1,800 tx/s sustained, 3,600 burst); pilot, c1, c2, c3, c5 (C5 with 2,000 clients through PgBouncer). 2026-11-27 00:29 UTC | complete | pipeline validation; `b-1rt` ≈ A at saturation; 31 switches/s without a minimum cycle; 58 % 1RT re-routes in C5 |
| `burst60-20261127` | hand-made: **`burst`** = periodic-burst scenario c6, **`60`** = B with a 60 s minimum switch cycle (`MIN_CYCLE_MS=60000`), then the date | c6: 20 min, 1,800 tx/s + 200 reads/s, 4 bursts of 60 s at +3,600 tx/s every 5 min; a, a-delete, b-1rt-60s, b-60s × 1. 2026-11-27 13:09–14:34 UTC | complete | short vs long term; basis of [`docs/comparison.md`](../docs/comparison.md) and [`docs/insert-failures.md`](../docs/insert-failures.md); per-burst table in `bursts.md` |
| `tests` | not a campaign: **correctness tests** (`tests/`) | `switch-race` (4RT), `switch-race-1rt`, `no-lock` (negative control: must lose events), `equivalence` (A and B reach the same `orders`). 2026-11-26/27 | all PASS | proof that B loses no event with the lock, and does without it |

The `git_rev` in `meta.env` is the commit checked out at run time;
uncommitted changes are not recorded.

- **`smoke-20261127-0029`** records `d24d03e`, but it ran `start.sh`, C5 and
  `producer-1rt.sql` from the working tree. That code was committed afterwards
  as `8fcd7bd`.
- **`burst60-20261127`** records `8fcd7bd` plus `MIN_CYCLE_MS` and the c6
  scenario, committed afterwards as `3173001`.

Not versioned:

- `local/` — ad-hoc runs;
- `latest` — link to the last campaign started by `start.sh`;
- `*.tar.gz` — archives that include the raw logs.

## 3. Scenarios (`<scenario>`)

The definitions are in `bench/scenarios/<scenario>.env`. The values below are
the scenario defaults; a campaign can override them (see §2).

| Scenario | Load | Question it answers |
|---|---|---|
| `pilot` | 2 min saturation, 16 producers, no readers | how fast is each design flat out? (used to calibrate the rates) |
| `c1` | 5 min saturation, 16 producers, no readers | ingestion capacity |
| `c2` | 10 min at a sustained rate + 200 reads/s | behaviour at a normal load, impact on readers |
| C4 | `c2` with `LOCK_SLOTS=1`; stored as label `b-slots1` under `c2/`, not as a directory | cost of one shared controller row |
| `c3` | `c2` + one 60 s burst at 2× the rate (6 min) | one peak |
| `c5` | 100k producer connections through PgBouncer (10 min) | massive concurrency behind a pooler |
| `c6` | `c2` for 20 min + 4 bursts of 60 s every 5 min | short term (first burst) vs long term (last burst) |

## 4. Labels (`<label>`)

A label is the project letter followed by the variant suffixes:
`<project>[-<variant>…]`.

| Part | Meaning |
|---|---|
| `a` | Project A: one `inbox` table, competing workers (`FOR UPDATE SKIP LOCKED`), cleanup by `UPDATE state = 'F'` |
| `b` | Project B: two buffers without indexes, controller table, one consumer, cleanup by `TRUNCATE`. Producer in **4 round trips** (`BEGIN` · `SELECT … FOR SHARE` · `INSERT` · `COMMIT`) — "**B 4RT**" in the docs |
| `-delete` | A with `CLEANUP_MODE=delete` (rows deleted instead of marked) |
| `-1rt` | B with the **single-round-trip** producer (`producer-1rt.sql`, one `INSERT … SELECT … FOR SHARE`) — "**B 1RT**" in the docs |
| `-30s`, `-60s` | B with a minimum switch cycle (`MIN_CYCLE_MS` = 30000 / 60000): the hot buffer stays open at least that long. No suffix = switch as soon as the hot buffer has a row |
| `-slots1`, `-slots64` | B with `LOCK_SLOTS` = 1 / 64 controller rows (default 64, 1024 in C5) |

Labels used so far:

| Label | Reads as | Campaigns |
|---|---|---|
| `a` | A, update cleanup | all |
| `a-delete` | A, delete cleanup | campaign-20261126, smoke, burst60 |
| `b` | B 4RT, no minimum cycle | pilot, campaign-20261126, smoke |
| `b-1rt` | B 1RT, no minimum cycle | smoke |
| `b-slots1` | B 4RT, one controller row (C4) | smoke |
| `b-slots64` | B 4RT, 64 controller rows behind the pooler | smoke |
| `b-60s` | B 4RT, 60 s cycle | burst60 |
| `b-1rt-60s` | B 1RT, 60 s cycle | burst60 |
| `b-30s`, `b-1rt-30s` | same with 30 s | defined in `bench/campaign.sh`, not run yet |

Full description of the variants and the conclusions:
[`docs/comparison.md`](../docs/comparison.md).
