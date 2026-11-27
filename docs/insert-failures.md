# Insert failures — A vs B

**Requirement: no insert may fail and no event may be lost, in either design.**
This document covers four things:

- what counts as a failure;
- what the PoC measured;
- every known way an insert can fail or be lost, for both designs;
- what each design needs to meet the requirement.

Variant names are explained in [`comparison.md`](comparison.md#1-variants-and-nomenclature).

## 1. Terms

| Class | What happens | Client knows? | Severity |
|---|---|---|---|
| **Failed insert** | the database returns an error, nothing is stored | yes, it must retry | high if not retried |
| **Ambiguous insert** | the connection drops after `COMMIT` was sent; the row may or may not be stored | no, a retry may duplicate the event | medium |
| **Late insert** | the insert finishes after the client's timeout; a real client turns it into a failed or ambiguous insert | yes, at the timeout | high under load |
| **Lost event** (silent loss) | the producer got success, but the event never reaches `orders` | **no** | **critical** |

A **B 1RT re-route is not a failure.** If a switch happens between two inserts
of a client, its single statement inserts 0 rows into the buffer that just
turned cold. The client sees that the affected-row count is 0 and inserts the
event through the locked path (`producer.sql`) in the same attempt. This
happened 416 times among the 3.02 M inserts of c6 (0.014 %), about once per
client per switch. Every one of those events was stored.

## 2. What was measured

### 2.1 No failed insert, no lost event

| Evidence | Result |
|---|---|
| c6, 4 runs of 20 min (`results/burst60-20261127`) | 11.47 M inserts; pgbench: 0 failed transactions; PostgreSQL log: 0 errors during the load; backlog 0 after the drain |
| c6, produced = consumed | events committed by pgbench = rows read by the consumer, per run: `a` 2,962,701 · `a-delete` 2,988,142 · `b-1rt-60s` 3,017,595 · `b-60s` 2,500,688 |
| Earlier runs (`smoke-20261127-0029`, `campaign-20261126`) | 24 runs: 0 failed transactions, `load_status=0`, backlog 0 |
| Switch race, 4RT (`results/tests/switch-race`) | 96,000 events, 73 switches: 0 missing, 0 duplicated |
| Switch race, 1RT (`results/tests/switch-race-1rt`) | 320,000 events, 24 switches (each one re-routes every producer): 0 missing, 0 duplicated |
| **Negative control: B without `FOR SHARE`** (`results/tests/no-lock`) | **240 of 96,000 events lost** in 89 switches: without the lock B loses events silently |
| A × B equivalence (`results/tests/equivalence`) | 32,000 events: identical `orders` state in A and B |

Durability settings were `synchronous_commit = on` with the default `fsync`,
so a committed insert survives a crash.

### 2.2 Late inserts: the practical risk

The PoC clients have no timeout, so nothing failed. A real client with a
timeout would have failed the inserts below. Source: c6 pgbench
per-transaction logs (a ≈ 5 % sample). Latency runs from the scheduled send
time to the commit, so it includes time spent queued in the client.

| Share of inserts slower than … | `a` | `a-delete` | `b-1rt-60s` | `b-60s` |
|---|---:|---:|---:|---:|
| 1 s, base producer | 1.4 % | 0.9 % | 0.06 % | 27 % |
| 2 s, base producer | 0.3 % | 0.1 % | **0** | 25 % |
| 2 s, bursts 1–3 | 0–0.3 % | 0 | **0** | 95–97 % |
| 2 s, **burst 4 (after 17 min)** | **63 %** | **49 %** | **0** | 96 % |
| 5 s, all bursts | 11 % | 5.9 % | **0** | 88 % |
| Slowest insert on the server (queueing excluded) | 2.4 s | 1.5 s | 0.8 s | 0.9 s |
| Events still unsent at the end of the load (≈ of 3.02 M) | 61k (2.0 %) | 36k (1.2 %) | 6k (0.2 %) | 523k (17 %) |

Reading:

- **The delay is queueing, not slow statements.** No insert ran longer than
  2.4 s on the server. The long times come from load above capacity: requests
  wait in the client for a free connection.
- **A** is safe in the short term but fails in the long term. Bursts 1–3 were
  absorbed; at burst 4 half or more of the burst inserts would have exceeded a
  2 s timeout. By then bloat, vacuum and WAL had taken the headroom.
- **B 1RT** stayed below 1 s for 99.95 % of the inserts and never reached 2 s.
- **B 4RT** saturates on routing. Almost every burst insert would have timed
  out, and 17 % of the events were never sent.

## 3. Causes common to A and B

| Cause | Mechanism | Class | Mitigation |
|---|---|---|---|
| Server crash before commit | the transaction is rolled back | failed | client retry |
| Connection lost during `COMMIT` | outcome unknown | ambiguous | retry with an idempotency key (see §6) |
| `synchronous_commit = off` / `fsync = off` | a crash loses acknowledged commits | **lost** | keep both `on` (PoC: `on`) |
| Asynchronous replica promoted on failover | the last commits are not on the new primary | **lost** | synchronous replication if failover must be lossless |
| `max_connections` or pool exhausted | connection refused or wait for a connection | failed / late | size the pool; PgBouncer; client timeouts above the measured tail |
| Constraint or type violation (`NOT NULL`, `payload > 512`) | error | failed | validate in the producer; dead-letter |
| Disk full (data or WAL) | every write fails | failed | monitor; see the growth rows in §4 and §5 |
| XID or MultiXact wraparound protection | the database refuses writes | failed | autovacuum headroom; monitor `age(datfrozenxid)` and `mxid_age` |
| Load above capacity | requests queue in the client until the timeout | late → failed | headroom; measured in §2.2 |

## 4. Causes specific to A

| Cause | Mechanism | Class | Measured / mitigation |
|---|---|---|---|
| Degradation over time | each event is written 2–3× (insert into heap + 2 indexes, then `UPDATE`/`DELETE`, then vacuum); the extra work takes headroom from the inserts | late → failed | steady p99 14 → 777 ms in 20 min; burst 4: 49–63 % over 2 s. Mitigation: `CLEANUP_MODE=delete`, more CPU, aggressive autovacuum on `inbox` |
| Unbounded growth (update mode) | finished rows stay in `inbox` (1.5 GB in 20 min at 1,800 tx/s) | failed (disk) | purge job for `state = 'F'` (never for `'P'`), or delete mode |
| WAL volume | 1.6–2.7× B 1RT per event | failed (disk, archive) / late (replica) | size WAL storage and archiving |
| Duplicate event id on retry (if the client generates the UUID) | primary key violation | failed (by design) | treat `unique_violation` as success: the PK gives A insert idempotency for free |

**Silent loss in A:** none by design. The database enforces everything. A
committed row stays `P` until a worker merges it and marks or deletes it in the
**same** transaction, and a worker crash rolls back its batch. The remaining
risks are operational, for example a purge job that deletes `P` rows.

## 5. Causes specific to B

| Cause | Mechanism | Class | Measured / mitigation |
|---|---|---|---|
| **Protocol bypass** | a writer inserts into a buffer without the `FOR SHARE` lock (ad-hoc script, old deploy, ORM, wrong buffer); its row can commit after the drain read it and is removed by `TRUNCATE` | **lost** | negative control: 240 / 96,000 lost. Enforce in the database: `REVOKE INSERT` on the buffers and expose one routing function (also 1 round trip), or a `BEFORE INSERT` trigger that takes the lock and raises when the buffer is cold (not measured) |
| **1RT: affected rows not checked** | after a switch the statement inserts 0 rows and still returns success | **lost** | the client must check `rows == 1` and fall back (PoC: `RETURNING` + `\if`); a routing function removes the risk |
| Missing controller row (`slot` ≥ rows, `LOCK_SLOTS` mismatch) | the lookup finds no row | failed (PoC: pgbench aborts the client); **lost** if code then picks a default buffer or skips the 0-row result | never default a buffer; check the slot count at startup |
| Wait behind the switch | the switch `UPDATE` waits for in-flight inserts and new inserts wait for it; with one shared row, new share lockers can starve it for seconds | late; failed with `lock_timeout` / `statement_timeout` | c6: switch wait max 156 ms; `b-slots1`: 421 ms. `LOCK_SLOTS` ≥ concurrent producers (or server connections behind a pooler), long cycle, timeouts well above the max |
| Stale 1RT insert queued behind `TRUNCATE` | an insert aimed at the cold buffer waits for its `ACCESS EXCLUSIVE` lock, then re-routes | late (ms) | same as above |
| **4RT routing cost** | 4 round trips per event saturate the CPU | late → failed | c6: 95 % of burst inserts over 2 s, 17 % never sent. **Use 1RT** |
| Consumer stopped or a poison event | one `MERGE` per cycle: one bad event fails the whole cycle; the hot buffer keeps growing (no backpressure) | failed (disk) | monitor cycle age and hot buffer size; dead-letter invalid events before the `MERGE`; restart is idempotent |
| Shared slot rows (pooler, `LOCK_SLOTS` < producers) | concurrent `FOR SHARE` lockers create MultiXacts | failed (MultiXact wraparound) | c6: 0 MultiXact blocks (one locker per row). Keep slots ≥ lockers; vacuum `buffer_control` |
| `UNLOGGED` buffers (an alternative considered) | a crash empties the buffers | **lost** | do not use with this requirement |
| No unique index on the buffers | a retried duplicate is accepted | duplicate, not lost | the forward-only `MERGE` makes a repeated state a no-op; dedup by event id in the drain if events have side effects |

What is **not** a loss in B:

- **A crash between the `MERGE` commit and the `TRUNCATE`.** The cycle is
  re-applied idempotently.
- **Coalescing.** With long cycles only the latest state of an order in a cycle
  is written to `orders`, and the intermediate events disappear with the
  `TRUNCATE`. The final state is exact (equivalence test). If every event must
  be kept as history, the drain must also append them to a history table.

## 6. What each design needs for zero failures

Both designs need the same baseline:

- `synchronous_commit = on` and `fsync = on`; synchronous replication if failover must be lossless;
- client retry on error, with an event id generated **by the producer** so a retry is idempotent;
- a client timeout above the measured tail and enough headroom for the peak;
- disk and wraparound monitoring.

| A | B |
|---|---|
| Delete mode, or a purge job that only removes `F` rows | **1RT only**, with the affected-row check |
| CPU and autovacuum headroom sized for the **long term**: A's tail grows with time (burst 4) | The protocol **enforced by the database**: no direct `INSERT` on the buffers (routing function or guard trigger) |
| Treat a PK violation on retry as success | `LOCK_SLOTS` ≥ concurrent lockers; abort when there is no controller row |
| | Consumer liveness and hot-buffer size alerts; dead-letter before the `MERGE` |
| | Long cycle (fewer switches, fewer re-routes, fewer lock waits) |

## 7. Conclusion

- **Neither design lost or failed an insert in the PoC:** 0 of 11.47 M in c6,
  produced = consumed in every run. Both can meet the requirement.
- **A is safer by construction.** The database enforces the invariant, and the
  primary key gives idempotent retries. Its risk is **operational and grows
  with time**: tail latency (late inserts) and storage. In c6, at 17 minutes,
  49–63 % of the burst inserts would have exceeded a 2 s timeout.
- **B 1RT has the lowest measured risk of late inserts** (0 over 2 s, steady
  over 20 min). But it has **more ways to lose events silently**, and all of
  them come from writers that do not follow the protocol. The requirement only
  holds if the database itself enforces the protocol and the consumer is
  monitored.
- **B 4RT does not meet the requirement under bursts.** Its routing cost turns
  peaks into timeouts.
