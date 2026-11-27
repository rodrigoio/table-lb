# Project B — table load balancing

Two plain buffer tables and a pessimistic controller table. Producers always
write into the **hot** buffer; the single consumer drains the **cold** one and,
because it is the only one that knows when the cold buffer is empty, it is the
one that switches.

```
producer:  BEGIN
           SELECT active FROM buffer_control WHERE slot = :slot FOR SHARE
           INSERT INTO buffer_<active> …
           COMMIT

consumer:  loop
             wait until the hot buffer has rows
             UPDATE buffer_control SET active = <cold>     -- waits for in-flight inserts
             MERGE INTO orders … FROM buffer_<now cold>    -- one transaction
             TRUNCATE buffer_<now cold>
```

After the switch commits, no transaction can still be writing into the buffer
that turned cold, so the consumer can read it completely and `TRUNCATE` it
without losing events and without leaving dead tuples behind.

## Files

- `sql/schema.sql` — `buffer_0`, `buffer_1` (no indexes) and `buffer_control` (64 slot rows;
  the `seed` service resizes it to `LOCK_SLOTS` rows before the load starts).
- `pgbench/producer.sql` — same lifecycle generator as project A, routed through the controller
  (`BEGIN` / `SELECT … FOR SHARE` / `INSERT` / `COMMIT`: four round trips).
- `pgbench/producer-1rt.sql` — single-round-trip variant (label `b-1rt`): one autocommit
  `INSERT INTO buffer_<hot> … SELECT … FROM buffer_control WHERE slot = … AND active = <hot> FOR SHARE`
  into the buffer the client saw last; when a switch made it miss (0 rows), it falls back to the
  locked lookup of `producer.sql`. Same guarantee, one round trip, which matters when a
  transaction pooler hands out server connections per transaction.
- `consumer/` — Rust consumer (`consumer-b`).

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `LOCK_SLOTS` | 64 | controller rows used by producers (`slot = client % LOCK_SLOTS`); 1 = everybody shares one row |
| `PRODUCER_SCRIPT` | `/scripts/project/producer.sql` | `/scripts/project/producer-1rt.sql` = single-round-trip routing |
| `POLL_MS` | 100 | how often the consumer checks the hot buffer when idle |
| `AUDIT` | 0 | 1 = record consumed events in `audit_consumed` (tests) |

Lock striping matters on PostgreSQL: with a single shared row, new
`FOR SHARE` lockers can overtake the waiting switch `UPDATE` and delay it for
seconds; with one row per producer the switch only waits for each producer's
current transaction. Behind a connection pooler the lockers of a row are the
pooled *server* connections, so keep `LOCK_SLOTS` well above the number of
server connections (C5 uses 1024 for 64).

## Run

```sh
mkdir -p ../results/local/project-b
docker compose up -d
DURATION=120 PRODUCER_RATE=2000 docker compose run --rm producer
docker compose --profile load --profile burst down -v
```
