# Project A — single inbox table

Baseline: one inbound table that also drives the business flow.

```
producers (pgbench) ──INSERT──▶ inbox (state = 'P')
                                  │  SELECT … FOR UPDATE SKIP LOCKED (N workers)
                                  ▼
                           MERGE INTO orders
                                  │
                       UPDATE inbox SET state = 'F'   (or DELETE)
```

## Files

- `sql/schema.sql` — `inbox` table, index `(state, id)`.
- `pgbench/producer.sql` — order lifecycle generator, one `INSERT` per transaction.
- `consumer/` — Rust worker pool (`consumer-a`).

## Consumer settings

| Variable | Default | Meaning |
|---|---|---|
| `WORKERS` | 4 | competing workers (one connection each) |
| `BATCH_SIZE` | 500 | rows claimed per transaction |
| `CLEANUP_MODE` | `update` | `update` marks rows `F`; `delete` removes them |
| `EMPTY_SLEEP_MS` | 50 | back-off when nothing is pending |
| `AUDIT` | 0 | 1 = record consumed events in `audit_consumed` (tests) |

## Run

```sh
mkdir -p ../results/local/project-a
docker compose up -d
DURATION=120 PRODUCER_RATE=2000 docker compose run --rm producer
docker compose --profile load --profile burst down -v
```
