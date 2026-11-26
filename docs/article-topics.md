# Table Load Balancing — article topics

Outline of the topics covered by the proof of concept. It is a working list:
bullets, decisions, open questions and pointers to evidence. The article text
itself will be written separately.

Evidence convention: every number quoted here must point to a file under
`results/`. Numbers marked **(pilot)** come from exploratory runs and still
need to be confirmed by the benchmark campaign.

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
- Diagram: Mermaid sequence diagram (producer / controller / consumer / buffers / orders) — TODO.

## 4. Correctness

- Invariants:
  - every produced event is consumed exactly once (no loss, no duplicates);
  - final state of each order = highest state produced for it.
- Tests (`tests/`):
  - switch race: random sleep *between* `FOR SHARE` and `INSERT`, many switches;
  - negative control: same test **without** `FOR SHARE` must lose events;
  - A × B equivalence: identical deterministic event set → identical `orders`.
- Results: TODO (`results/…/tests/`).

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
  - `FOR UPDATE SKIP LOCKED` (A only) — SQL Server: `READPAST`;
  - `MERGE` — not available in MySQL (`INSERT … ON DUPLICATE KEY UPDATE`);
  - `TRUNCATE` is DDL / implicit commit in some engines (design tolerates it);
  - `pg_stat_*` views — instrumentation only.
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

## 10. Number of tables in the load balancer (explicit note)

- This PoC uses **exactly two** buffer tables.
- More tables would open other possibilities: parallel drains, more writers /
  consumers at the same time, buffers per key or event type, less contention
  per controller row, overlapping drain of one buffer with the switch of another.
- Cost: more complex routing (dynamic SQL or generated statements), more
  controller state.
- To be cited and weighed against the data obtained — not implemented.

## 11. Methodology and threats to validity

- Hardware: 2 vCPU (i5-6500T) / 1.9 GB RAM VM; PostgreSQL 18.7 in Docker
  (1.5 CPU, 1 GB limit); pgbench and consumers on the same host.
- Identical `postgresql.conf` for A and B (`common/postgres/postgresql.conf`).
- Fresh database per run (`docker compose down -v`), 60 s warm-up discarded.
- Scenarios C1–C4 (`bench/scenarios/`), repetitions per scenario.
- Threats: CPU contention between load generator and server; short runs;
  single event per producer transaction (worst case for B's routing).

## 12. Results by metric

- **(pilot)** Producer throughput at saturation, 16 clients, one event per
  transaction: A ≈ 5.6k tps vs B ≈ 2.8k tps. B's producer does four round
  trips (BEGIN, FOR SHARE, INSERT, COMMIT) against one for A, on a CPU-bound
  VM. To confirm in C1.
- TODO per metric (links to SVG / `summary.md`):
  - ingestion throughput and producer latency (p50/p95/p99, spikes at switch);
  - business read latency on `orders`;
  - end-to-end lag; backlog over time;
  - table size, dead tuples, autovacuum activity;
  - WAL volume; deadlocks / retries; wait events; CPU.

## 13. Trade-offs

- Latency floor: end-to-end lag ≥ one cycle (switch + drain).
- Switch pause: producers wait for the switch transaction (`switch_wait_ms`).
- Single consumer: single point of failure, but restart is idempotent.
- No backpressure: hot buffer grows freely while a drain is slow.
- Large drain transactions: long MERGE, holds row locks on many orders,
  holds back the xmin horizon for vacuum.
- Producer routing cost: extra statements and an explicit transaction per insert.
- More moving parts: controller table, slots, consumer state machine.

## 14. Alternatives / when to use

- Alternatives: transaction-id (snapshot) barrier, per-buffer locks with
  double-check, N buffers in a ring, native partitioning (attach/detach),
  `UNLOGGED` buffers, pgmq / external queues (Kafka, RabbitMQ).
- When to use / when not to use: TODO after results.
