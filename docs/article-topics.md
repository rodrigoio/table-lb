# Table Load Balancing — article topics

Outline of the topics covered by the proof of concept. It is a working list:
bullets, decisions, open questions and pointers to evidence. The article text
itself will be written separately.

Evidence convention: every number quoted here must point to a file under
`results/`. Numbers marked **(pilot)** or **(smoke)** come from the 2 vCPU
development VM (exploratory runs, 1–2 minute runs for the smoke) and still
need to be confirmed by a campaign on a proper host (`./start.sh`).

---

## 1. Problem

- High-volume ingestion into a table that is *also* the flow-control table of
  the business rules (status column polled by workers).
- Same table receives: inserts (producers), polling queries (consumers),
  updates/deletes (state transitions), business reads.
- PostgreSQL specifics that make it hurt: MVCC dead tuples, autovacuum
  pressure, non-HOT updates when the status column is indexed, index churn,
  queue-table degradation (dead index entries scanned by the poller).
- Goal: keep the business table (`orders`) away from the ingestion hot path.

## 2. Strategy A — single inbox table (baseline)

- `inbox` + `state` column (`P` pending / `F` finished), index `(state, id)`.
- Competing workers: `SELECT … FOR UPDATE SKIP LOCKED` → `MERGE` → `UPDATE state`
  (or `DELETE`, `CLEANUP_MODE=delete`) → `COMMIT`.
- Baseline is intentionally *honest*: batching, SKIP LOCKED, same MERGE as B.
- Portable composite index instead of a PostgreSQL partial index
  (see §5 — a partial index would likely help A on PostgreSQL).

## 3. Strategy B — table load balancing

- Two plain buffer tables (`buffer_0`, `buffer_1`), append-only, **no indexes**.
- Auxiliary controller table `buffer_control` (pessimistic lock):
  - producers: `BEGIN; SELECT active … FOR SHARE; INSERT INTO buffer_<active>; COMMIT;`
  - consumer: `UPDATE buffer_control SET active = …` waits for every in-flight insert.
- **The consumer drives the switch**: it is the only one that knows when the
  cold buffer was consumed completely. Cycle: wait for data in hot → switch →
  drain cold (one MERGE) → `TRUNCATE` → repeat.
  - No scheduler, no timer: cycle length adapts to load (self-clocking).
  - Batch size grows with load; latency floor = one cycle.
- Once the switch commits nobody can write into the cold buffer → `TRUNCATE`
  is safe → no dead tuples, no vacuum on the buffers.
- `TRUNCATE` kept outside the drain transaction on purpose: the design stays
  correct where `TRUNCATE` is not transactional; a crash in between only
  re-applies events idempotently.
- Two producer protocols with the same guarantee:
  - `producer.sql` (label `b`): `BEGIN; SELECT active … FOR SHARE; INSERT; COMMIT`
    — four round trips per event;
  - `producer-1rt.sql` (label `b-1rt`): one autocommit
    `INSERT INTO buffer_<last hot> … SELECT … FROM buffer_control WHERE slot = :slot AND active = <last hot> FOR SHARE`.
    The lock lives until the statement commits (the switch waits for it); a
    statement that meets an in-flight switch waits and re-checks the updated
    row under READ COMMITTED, inserts 0 rows, and the producer falls back to
    the locked lookup once per switch.
- Sequence diagram (one switch):

```mermaid
sequenceDiagram
    participant P as Producer
    participant C as buffer_control
    participant H as buffer_0 (hot)
    participant K as buffer_1 (cold → hot)
    participant W as Consumer
    participant O as orders

    P->>C: BEGIN; SELECT active … FOR SHARE (slot)
    C-->>P: active = 0
    P->>H: INSERT event
    W->>H: SELECT 1 … FETCH FIRST 1 ROWS ONLY (hot has data?)
    W->>C: UPDATE buffer_control SET active = 1
    Note over W,C: waits for the producer's FOR SHARE
    P->>P: COMMIT (lock released)
    C-->>W: UPDATE done → COMMIT (switch)
    P->>C: next event: SELECT active … FOR SHARE
    C-->>P: active = 1
    P->>K: INSERT event
    W->>H: stats + MERGE INTO orders … FROM buffer_0
    W->>O: apply (forward-only state machine)
    W->>H: COMMIT; TRUNCATE buffer_0
    Note over W: wait for data in buffer_1, then switch back
```

## 4. Correctness

- Invariants:
  - every produced event is consumed exactly once (no loss, no duplicates);
  - final state of each order = highest state produced for it.
- Tests (`tests/`):
  - switch race: random sleep *between* `FOR SHARE` and `INSERT`, many switches;
  - negative control: same test **without** `FOR SHARE` must lose events;
  - A × B equivalence: identical deterministic event set → identical `orders`.
- Results:
  - switch race — 32 producers, 0–5 ms pause while holding `FOR SHARE`,
    96,000 events, 73 switches: 0 missing, 0 duplicated
    (`results/tests/switch-race/summary.txt`);
  - switch race, single-round-trip producer — 32 producers, 320,000 events,
    24 switches (each one makes every producer miss and fall back): 0 missing,
    0 duplicated (`results/tests/switch-race-1rt/summary.txt`);
  - negative control — same load without `FOR SHARE`: 240 of 96,000 events
    lost in 89 switches (`results/tests/no-lock/summary.txt`). The lock is what
    makes B correct, and the race test is able to detect a broken protocol;
  - equivalence — 32,000 events, 9,600 orders: A and B both exact, same state
    hash `821b8cf55f7ac935` (`results/tests/equivalence/summary.txt`).
- Side note (tooling): without the lock, stale producers queue behind the
  `TRUNCATE`'s lock request; pgbench prepares statements synchronously and
  multiplexes clients per thread, so one blocked client froze the thread that
  also held the transaction `TRUNCATE` was waiting for — a client-side deadlock
  invisible to the server. Tests run one pgbench thread per client.
  Real producers with independent connections/threads are not exposed to it;
  with the lock the cold buffer has no writers at all.

## 5. Portability

- Standard SQL used: `MERGE`, `ROW_NUMBER() OVER`, `FETCH FIRST n ROWS ONLY`,
  `PERCENTILE_DISC … WITHIN GROUP`, `CAST`, `GENERATED ALWAYS AS IDENTITY`,
  `WITH RECURSIVE`, `TRUNCATE`, lookup table instead of enum.
- No triggers, stored procedures, PL/pgSQL, advisory locks, `ON CONFLICT`,
  `DISTINCT ON`, `jsonb`, native enums, partial indexes, `generate_series`.
- Accepted PostgreSQL exceptions and their equivalents elsewhere:
  - `DEFAULT uuidv7()` (PG 18) — elsewhere generate UUIDv7 in the application;
  - `SELECT … FOR SHARE` — MySQL 8 has it; Oracle: `LOCK TABLE … IN SHARE MODE`
    or `FOR UPDATE`; SQL Server: `WITH (HOLDLOCK, ROWLOCK)` hints;
  - `INSERT … SELECT … FOR SHARE` (`b-1rt`) — PostgreSQL, MySQL 8; elsewhere
    the two-statement protocol of `producer.sql`;
  - `RETURNING` (`b-1rt`, pgbench only, to see whether a row was inserted) —
    an application reads the affected-row count of the `INSERT` instead;
  - `FOR UPDATE SKIP LOCKED` (A only) — SQL Server: `READPAST`;
  - `MERGE` — not available in MySQL (`INSERT … ON DUPLICATE KEY UPDATE`);
  - `TRUNCATE` is DDL / implicit commit in some engines (design tolerates it);
  - `pg_stat_*` views — instrumentation only.
- Connection pooler (C5) is infrastructure, not SQL: PgBouncer here; ProxySQL
  (MySQL), Oracle DRCP, application-side pools elsewhere. Transaction pooling
  works with both strategies because every protocol step is transaction-scoped
  (no session state, no session-level locks).
- PostgreSQL-only optimizations deliberately **not** used: partial index,
  `UNLOGGED` buffers, `fillfactor`, native partitioning, advisory locks.

## 6. Event identity

- UUIDv7 instead of `BIGINT` + sequence:
  - 16 vs 8 bytes; no shared sequence (contention point, coordination between tables);
  - unpredictable volume / generated anywhere; time-ordered → B-tree locality
    (unlike UUIDv4); embedded timestamp.
  - Buffers have no index, so the UUID only costs storage there.
- `orders.id` stays `BIGINT` (business key, addressed deterministically by the load generator).

## 7. Traceability

- `orders.source_ref = '<table_name>:<uuid>'` of the event that set the current state.
- Provisional reference for analysis only; costs ~45 bytes per update.

## 8. Order state machine

- `pending → payment_confirmed → sent → delivered` (lookup table `order_status`).
- Forward-only rule in the MERGE: a state only replaces a lower one; `pending` creates the order.
- Idempotent and order-independent → safe with concurrent workers (A) and re-drains (B).
- Coalescing per order inside a batch (`ROW_NUMBER() … rn = 1`) in both projects.
  - Loses intermediate transitions when several arrive in the same batch:
    rules that must fire on *every* transition need a different apply step.
  - Load generator spaces the phases of one order by `WINDOW` events to avoid
    inflating coalescing artificially.

## 9. Cost of the pessimistic lock on PostgreSQL

- **(pilot)** A single controller row shared by all producers: PostgreSQL lets
  new `FOR SHARE` lockers overtake a waiting `UPDATE` (share-lockers do not
  queue behind it), so the switch is delayed until a gap appears —
  switch wait from 2 ms up to 3.7 s at ~4.3k tps with 16 producers.
- **(pilot)** Lock striping — one controller row per producer
  (`slot = client % lock_slots`) — bounded the switch wait to 6–33 ms with
  the same throughput: each row has a single locker (no MultiXact), and the
  `UPDATE` only waits for that producer's current transaction.
- Every `FOR SHARE` writes the tuple header (lock WAL record) and all slots
  share one 8 kB page → `BufferContent` contention candidate. To check in C4.
- Producers blocked during a switch show up as `Lock:transactionid` waits.
- Scenario C4 (`LOCK_SLOTS=1` vs `64`) to confirm in `results/`.
- Behind a transaction pooler the lockers of a controller row are the pooled
  *server* connections, not the clients: striping by client id stops bounding
  the lockers per row. Rule: slots ≫ server connections (C5: 1024 slots for
  64 connections; `b-slots64` = one slot per connection on average).

## 10. Massive concurrency (C5)

- Question: millions of events with hundreds of thousands of concurrent
  producers. PostgreSQL forks one backend per connection (`max_connections` in
  the hundreds/low thousands): 100k concurrent requests reach the database
  only through a pooler (or an application tier with its own pools).
  Concurrency at the edge ≠ concurrency at the database.
- Topology (`bench/scenarios/c5.env`): N pgbench containers (≤ 50k connections
  each: ephemeral ports per source IP) → PgBouncer, several processes on one
  port (`so_reuseport`), transaction pooling, 4 × 16 server connections →
  PostgreSQL. Consumers, readers and collector connect directly.
- Closed loop: latency ≈ clients / throughput (Little's law) and includes the
  wait for a pooled connection; events per run = throughput × duration.
- What a pooler does to each strategy:
  - A: one autocommit `INSERT` holds a server connection for one round trip;
  - B (`producer.sql`): the routing transaction holds it for four round trips
    → fewer events per pooled connection; `b-1rt` brings it back to one;
  - controller slots must scale with server connections (§9).
- **(smoke)** 2,000 clients, 2 × 8 server connections, 60 s
  (`results/smoke-20261127-0029/summary.md`, section c5): A 2,144 tps,
  b 852, b-1rt 849, b-slots64 922; reader p99 unchanged (≈ 17.5 ms for all);
  end-to-end lag p95 ≈ 1.0–1.5 s for all. Inconclusive on that host: the load
  generator (pgbench + PgBouncer) used 91–96 % CPU for B vs 84 % for A while
  PostgreSQL stayed at 78–86 %, and B's pooled connections show
  `Lock:transactionid` waits (parked behind each switch, ≈ 1 of 16 on average).
- Hypothesis to test on proper hardware: behind a pooler a switch stall parks
  pooled connections, i.e. it costs capacity for *all* clients, not only for
  the producers of the locked rows. `b` vs `b-1rt` isolates round trips,
  `a` vs `b-1rt` isolates the controller lock + consumer.
- Harness finding: `pgbench -M prepared` deadlocks behind a transaction
  pooler with B — pgbench prepares synchronously (blocks every client of the
  thread) while the pooled server connections are held by clients of that
  same thread in the middle of `BEGIN … COMMIT` → nothing moves until
  `query_wait_timeout`. C5 uses the extended protocol. Real applications with
  asynchronous drivers are not exposed; synchronous drivers with fewer
  threads than in-flight transactions per pool are.
- Host limits that bound the experiment (checked by `start.sh check`):
  ephemeral ports per source IP, `nf_conntrack_max` (Docker bridge),
  `fs.file-max` / `nofile`, `somaxconn`, ≈ 48 KB RAM per connection (pgbench
  + PgBouncer).

## 11. Number of tables in the load balancer (explicit note)

- This PoC uses **exactly two** buffer tables.
- More tables would open other possibilities: parallel drains, more writers /
  consumers at the same time, buffers per key or event type, less contention
  per controller row, overlapping drain of one buffer with the switch of another.
- Cost: more complex routing (dynamic SQL or generated statements), more
  controller state.
- To be cited and weighed against the data obtained — not implemented.

## 12. Methodology and threats to validity

- Reproduction: `./start.sh` on a dedicated host (check → build → tests →
  calibration → campaign → report → archive); hardware and effective
  configuration recorded per campaign in `results/<campaign>/host.txt`.
- Development VM (pilot, smoke, tests): 2 vCPU (i5-6500T) / 1.9 GB RAM;
  PostgreSQL 18.7 in Docker (1.5 CPU, 1 GB limit) — `PROFILE=reference`.
- Campaign hosts: PostgreSQL container sized from the host (`PROFILE=auto`:
  half of the CPUs, a quarter of the RAM), identical for A and B.
- Identical `postgresql.conf` for A and B (`common/postgres/postgresql.conf`);
  only `max_connections`, `shared_buffers`, `effective_cache_size` follow the host.
- Rates of C2/C3 calibrated per host: 0.6 × min(A, B) pilot saturation
  throughput (`results/<campaign>/calibration.env`), burst = 2× that rate.
- Fresh database per run (`docker compose down -v`), warm-up discarded.
- Scenarios C1–C5 (`bench/scenarios/`), repetitions per scenario, A/B order
  alternating between repetitions.
- Threats: load generators, pooler, consumers and server on the same host
  (CPU contention); short runs; single event per producer transaction (worst
  case for B's routing); C5 load generator limits (pgbench threads, pooler
  processes) may cap throughput before PostgreSQL does — check
  `docker_stats.csv`.

## 13. Results by metric

- **(pilot)** Producer throughput at saturation, 16 clients, one event per
  transaction: A ≈ 5.6k tps vs B ≈ 2.8k tps. B's producer does four round
  trips (BEGIN, FOR SHARE, INSERT, COMMIT) against one for A, on a CPU-bound
  VM. To confirm in C1.
- **(smoke)** C1, 16 direct clients, 60 s (`results/smoke-20261127-0029/summary.md`,
  section c1): A 5,789 tps, b 2,958, **b-1rt 6,167** — with single-statement
  routing B ingests faster than A, with 0 dead tuples in the inbound tables
  (A: 196k) and 1,032 vs 1,847 WAL bytes per event. The four round trips, not
  the lock, were B's ingestion cost on direct connections. End-to-end lag
  p95 at saturation: A 11.3 s, b 3.9 s, b-1rt 12.4 s (single consumer behind
  a faster producer: 32 cycles of ≈ 11.5k rows).
- **(smoke)** Calibration on the dev VM: pilot A 5,677 tps, B 3,021 tps → C2/C3
  rate 1,800 tps (`results/smoke-20261127-0029/calibration.env`).
- TODO per metric (links to SVG / `summary.md`):
  - ingestion throughput and producer latency (p50/p95/p99, spikes at switch);
  - business read latency on `orders`;
  - end-to-end lag; backlog over time;
  - table size, dead tuples, autovacuum activity;
  - WAL volume; deadlocks / retries; wait events; CPU.

## 14. Trade-offs

- Latency floor: end-to-end lag ≥ one cycle (switch + drain).
- Switch pause: producers wait for the switch transaction (`switch_wait_ms`).
- Single consumer: single point of failure, but restart is idempotent.
- No backpressure: hot buffer grows freely while a drain is slow.
- Large drain transactions: long MERGE, holds row locks on many orders,
  holds back the xmin horizon for vacuum.
- Producer routing cost: extra statements and an explicit transaction per insert
  (`b-1rt` reduces it to one statement, plus one fallback per switch).
- Behind a transaction pooler: B's routing transaction holds a pooled
  connection longer than A's insert; controller slots must follow the number
  of server connections.
- More moving parts: controller table, slots, consumer state machine.

## 15. Alternatives / when to use

- Alternatives: transaction-id (snapshot) barrier, per-buffer locks with
  double-check, N buffers in a ring, native partitioning (attach/detach),
  `UNLOGGED` buffers, pgmq / external queues (Kafka, RabbitMQ).
- When to use / when not to use: TODO after the campaign on a proper host.
- Open questions for the campaign: does `b-1rt` keep its C1 advantage with
  more CPU? Does the consumer keep up (lag at saturation)? What does a switch
  cost behind a pooler at 100k clients (C5)?
