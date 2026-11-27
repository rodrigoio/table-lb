# table-lb — Table Load Balancing PoC

Proof of concept that compares two ways of receiving high volumes of data in
PostgreSQL:

| | Project A — single inbox | Project B — table load balancing |
|---|---|---|
| Ingestion | `INSERT` into `inbox` | `INSERT` into the *hot* buffer named by `buffer_control` |
| Flow control | `state` column polled with `FOR UPDATE SKIP LOCKED` | consumer drains the *cold* buffer, then switches |
| Cleanup | `UPDATE state = 'F'` (or `DELETE`) | `TRUNCATE` of the drained buffer |
| Concurrency | N competing workers | 1 consumer, pessimistic controller (`FOR SHARE` / `UPDATE`) |

Both apply events to the same business table (`orders`) with the same
standard-SQL `MERGE` (forward-only order state machine
`pending → payment_confirmed → sent → delivered`).

The study goes from a 16-producer saturation test to **hundreds of thousands
of concurrent producer connections** and millions of events per run (scenario
C5, through a connection pooler).

---

## Running the study on a machine — `start.sh`

The development VM (2 vCPU, 2 GB) is enough to validate the pipeline, not to
measure it. `start.sh` reproduces the whole study on a proper machine with one
command; the only requirement is Linux with Docker.

```sh
git clone <this repository> table-lb && cd table-lb
sudo ./start.sh setup          # once per machine: Docker (if missing) + kernel limits for C5
./start.sh check               # prerequisites, resource checks, resolved configuration, time estimate
./start.sh smoke               # optional: whole pipeline with 1-minute runs (~25 min)
./start.sh --detach            # the full study in the background (~6 h with the defaults)
tail -f results/latest/start.log
```

### What `./start.sh` (= `all`) does

| Step | Command | Time (defaults) | Output |
|---|---|---|---|
| 1. prerequisites | `check` | seconds | console; stops on hard failures |
| 2. images | `build` | 3–10 min (first time) | `tlb-rust`, `tlb-pgbench`, `tlb-pgbouncer` |
| 3. correctness tests | `test` | ~15 min | `results/<campaign>/tests/` — stops if a test fails |
| 4. calibration | `calibrate` | ~6 min | `results/<campaign>/calibration.env` (+ `pilot/` runs) |
| 5. benchmark matrix | `campaign` | ~5 h | `results/<campaign>/<scenario>/<label>/rep-<n>/` |
| 6. report | `report` | ~1 min | `summary.md`, `runs.csv`, `charts/` |
| 7. archive | `archive` | ~1 min | `results/<campaign>.tar.gz` (raw logs included) |

Other commands: `smoke` (same steps, short runs, small concurrency, no tests),
`stop` (stops a detached study), `clean` (removes every table-lb container,
network and volume), `--help`.
Every step can be run on its own; steps continue the latest campaign
(`results/latest`) unless `CAMPAIGN=<name>` is set, so an interrupted study is
resumed with, e.g., `SCENARIOS="c3 c5" ./start.sh campaign && ./start.sh report`.

### Recommended machine

| Goal | CPU | RAM | Disk |
|---|---|---|---|
| C1–C4 (16 producers, direct connections) | ≥ 8 vCPU | ≥ 16 GB | SSD/NVMe, ≥ 50 GB free |
| C5 with 100k connections (default) | ≥ 16 vCPU | ≥ 32 GB | NVMe, ≥ 50 GB free |
| C5 with N connections | ≥ 16 vCPU | ≈ 48 KB × N + PostgreSQL + 2 GB | NVMe |

- A dedicated host (bare metal or dedicated-CPU instance) with no other load:
  load generators, pooler, consumers and PostgreSQL share it, so noisy
  neighbours distort the comparison.
- Linux x86_64 or arm64, Docker Engine with the Compose v2 plugin.
- C5 needs kernel limits for hundreds of thousands of TCP connections
  (`nf_conntrack_max`, `fs.file-max`, `somaxconn`); `sudo ./start.sh setup`
  writes them to `/etc/sysctl.d/99-table-lb.conf`, `./start.sh check`
  verifies them.

### Configuration — `host.env`

```sh
cp host.env.example host.env    # uncomment what you need
```

Precedence: environment variables > `host.env` > defaults. The main knobs:

| Variable | Default | Meaning |
|---|---|---|
| `PROFILE` | `auto` | `auto`: PostgreSQL gets half of the CPUs and a quarter of the RAM; `reference`: the limits of the development VM (1.5 CPU, 1 GB) |
| `PG_CPUS`, `PG_MEM`, `PG_SHARED_BUFFERS`, `PG_EFFECTIVE_CACHE_SIZE` | from `PROFILE` | PostgreSQL container size (identical for A and B) |
| `SCENARIOS` | `c1 c2 c3 c5` | scenarios to run (`c2` includes C4) |
| `REPS_C1` … `REPS_C5`, `REPS_C4`, `REPS_C5_VARIANTS` | 3 3 2 2 / 2 / 1 | repetitions |
| `CALIBRATION_FRACTION` | 0.6 | C2/C3 rate = fraction × min(A, B) saturation throughput of the pilot |
| `C2_PRODUCER_RATE`, `C3_PRODUCER_RATE`, `C3_BURST_RATE` | calibrated | fixed rates instead (the development VM used 2000 / 2000 / 4000) |
| `C5_PRODUCER_CLIENTS` | 100000 | concurrent producer connections in C5 (one pgbench container per ≤ 50k) |
| `C5_PGBOUNCER_PROCESSES`, `C5_PGBOUNCER_POOL_SIZE` | 4, 16 | pooler processes and server connections per process |
| `<SCENARIO>_<VAR>` | — | overrides any variable of `bench/scenarios/<scenario>.env`, e.g. `C1_PRODUCER_CLIENTS=64` |
| `FORCE` | 0 | 1 = resource checks only warn |
| `SKIP_TESTS` | 0 | 1 = skip the correctness tests |

The effective values of a campaign are recorded in `results/<campaign>/host.txt`
and, per run, in `meta.env`.

### Where the artifacts are

Everything a campaign produces stays under `results/<campaign>/` (the default
name is `campaign-<UTC date>-<time>`, `smoke-…` for `smoke`):

```
results/
├── latest -> campaign-20270105-0930      last campaign started (used by the step commands)
├── campaign-20270105-0930/
│   ├── start.log                         full console output of start.sh
│   ├── campaign.log                      one line per step of every run (bench/run.sh)
│   ├── host.txt                          CPU model, cores, RAM, kernel, Docker, effective configuration
│   ├── calibration.env                   pilot throughput of A and B and the derived C2/C3 rates
│   ├── summary.md                        THE REPORT: one table per scenario, mean ± sd per label
│   ├── runs.csv                          one row per run with every metric
│   ├── charts/*.svg                      time series (producer p99, backlog, lag, WAL, …) and bar charts
│   ├── tests/<test>/summary.txt          correctness tests executed on this machine
│   ├── pilot/{a,b}/rep-1/                calibration runs
│   └── <scenario>/<label>/rep-<n>/       one benchmark run, e.g. c5/b-1rt/rep-2/
│       ├── meta.env                      parameters, overrides and timestamps of the run
│       └── raw/                          NOT versioned (large):
│           ├── producer[-<i>].{out,cmd,summary.txt}   pgbench command and summary per replica
│           ├── producer[-<i>].<pid>.<thread>         pgbench per-transaction logs (sampled)
│           ├── reader.*, burst.*                       same for readers and the burst group
│           ├── collector/{tables,database,waits,progress,batch_log,statements}.csv
│           ├── docker_stats.csv                        CPU / memory per container, every second
│           └── {consumer,postgres,pgbouncer}.log       service logs
├── campaign-20270105-0930.tar.gz         archive step: the campaign directory, raw/ included
├── tests/                                correctness tests of the development VM
└── pilot/                                calibration pilot of the development VM
```

Bringing results back from a remote machine:

```sh
scp bench-host:table-lb/results/campaign-20270105-0930.tar.gz results/
tar -xzf results/campaign-20270105-0930.tar.gz -C results
git add results/campaign-20270105-0930   # raw/ is ignored by .gitignore; summaries, charts and meta are kept
```

The report can be rebuilt at any time from the raw data:
`CAMPAIGN=campaign-20270105-0930 ./start.sh report`.

---

## Scenarios

| Scenario | Load | Labels |
|---|---|---|
| `pilot` | 2 min saturation, used by the calibration | a, b |
| `c1` | saturation: 16 producers without rate limit, no readers (5 min) | a, b, b-1rt |
| `c2` | sustained rate (calibrated) + 200 reads/s (10 min) | a, a-delete, b, b-slots1 (= C4) |
| `c3` | `c2` + a 60 s burst of 2× the rate from a second producer group (6 min) | a, b |
| `c5` | **massive concurrency**: 100k producer connections, closed loop, through PgBouncer (10 min) | a, b, b-1rt, b-slots64 |

Labels: `a-delete` = A with `CLEANUP_MODE=delete`; `b-slots1` = every producer
locks the same controller row; `b-1rt` = B with the single-round-trip producer
(`project-b/pgbench/producer-1rt.sql`); `b-slots64` = 64 controller rows for
64 pooled server connections.

### C5 — hundreds of thousands of concurrent requests

```
 pgbench replica 1 (≤ 50k conns) ─┐
 pgbench replica 2 (≤ 50k conns) ─┼─▶ PgBouncer: 4 processes, port 6432 (so_reuseport),
 …                                │   transaction pooling, 4 × 16 server connections ─▶ PostgreSQL
 pgbench replica N (≤ 50k conns) ─┘
 consumers, readers, collector ─────────────────────────── direct connections ──────▶ PostgreSQL
```

- PostgreSQL runs one process per connection: 100k clients reach the database
  only through a pooler. Concurrency at the edge (100k) and at the database
  (64 server connections) are different numbers, and that is what C5 measures.
- Closed loop: each client sends its next event as soon as the previous one
  commits. Latency ≈ clients / throughput (Little's law) and includes the wait
  for a pooled connection; the number of events is throughput × duration
  (millions per run).
- Each pgbench replica is a separate container (own IP: ~60k ephemeral ports
  each), with its own order-id range and controller slots.
- Producers use the extended query protocol: `pgbench -M prepared` prepares
  synchronously and deadlocks behind a transaction pooler when B's
  transactions hold the pooled connections (see
  `common/pgbench/run-pgbench.sh`).
- B's controller rows are locked by pooled server connections, not by
  clients: `LOCK_SLOTS=1024` for 64 server connections (`b-slots64` shows the
  effect of fewer rows).

---

## Layout

```
start.sh      entry point: setup, check, build, test, calibrate, campaign, report, archive
host.env.example  machine-specific configuration template
common/       shared pieces: postgresql.conf, base schema, pgbench image and read scripts,
              PgBouncer image, Rust crates (tlb-core library, tools: seed / collector /
              report / verify) and the Rust Dockerfile
project-a/    single inbox table: schema, pgbench producer, Rust consumer, compose
project-b/    two buffers + controller: schema, pgbench producers, Rust consumer, compose
tests/        correctness tests (switch race, single-round-trip race, negative control, A × B equivalence)
bench/        scenarios, one-run runner (run.sh) and the campaign matrix (campaign.sh)
results/      campaigns (summaries versioned, raw logs not)
docs/         article topics
```

## Running pieces by hand

One project, interactively:

```sh
cd project-b                                   # or project-a
mkdir -p ../results/local/project-b            # collector / pgbench output
docker compose up -d                           # postgres, pgbouncer, seed (1M orders), consumer, collector
docker compose run --rm producer               # 60 s of ingestion (pgbench)
docker compose run --rm reader                 # business reads (pgbench)
docker compose exec postgres psql -U tlb -d tlb -c 'SELECT * FROM batch_log ORDER BY id DESC LIMIT 5'
docker compose --profile load --profile burst down -v
```

One benchmark run, or the matrix without `start.sh`:

```sh
bench/run.sh b c2 1                                   # project, scenario, repetition
C5_PRODUCER_CLIENTS=20000 LABEL=b-1rt \
  PRODUCER_SCRIPT=/scripts/project/producer-1rt.sql bench/run.sh b c5 1
CAMPAIGN=my-campaign SCENARIOS="c1 c5" bench/campaign.sh
```

Correctness tests:

```sh
tests/run-all.sh          # or tests/switch-race.sh | switch-race-1rt.sh | no-lock.sh | equivalence.sh
```

| Test | What it proves |
|---|---|
| `switch-race` | B under 32 producers that pause while holding `FOR SHARE`, consumer switching as fast as it can: every event consumed exactly once |
| `switch-race-1rt` | the same with the single-round-trip producer (constant misses and fallbacks) |
| `no-lock` | negative control: the same load **without** `FOR SHARE` loses events (the lock is what makes B correct) |
| `equivalence` | the same deterministic event set through A and B ends in the same `orders` state |

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `check`: `nf_conntrack_max` / `fs.file-max` too low | `sudo ./start.sh setup` (or lower `C5_PRODUCER_CLIENTS`) |
| `check`: RAM | lower `C5_PRODUCER_CLIENTS`, or `FORCE=1` to try anyway |
| `permission denied` on the Docker socket | add the user to the `docker` group and log in again (`setup` does it) |
| a run logs `WARNING: a load generator exited with an error` | see `raw/producer*.out` of that run; connection errors in C5 usually mean kernel limits |
| stop a detached study | `./start.sh stop` (kills the runner, removes the stacks; results so far are kept) |
| rerun only some scenarios | `SCENARIOS="c5" ./start.sh campaign && ./start.sh report` (same campaign) |

## Portability

The strategies use plain SQL only (no triggers, stored procedures, advisory
locks or native enum/json types). Accepted PostgreSQL specifics are listed in
[`docs/article-topics.md`](docs/article-topics.md#5-portability).

## License

MIT
