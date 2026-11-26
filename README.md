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

## Layout

```
common/       shared pieces: postgresql.conf, base schema, pgbench image and
              read scripts, Rust crates (tlb-core library, tools: seed /
              collector / report / verify) and the Rust Dockerfile
project-a/    single inbox table: schema, pgbench producer, Rust consumer, compose
project-b/    two buffers + controller: schema, pgbench producer, Rust consumer, compose
tests/        correctness tests (switch race, negative control, A × B equivalence)
bench/        benchmark scenarios, runner and report
results/      benchmark summaries (raw logs are not versioned)
docs/         article topics
```

## Requirements

Docker with Compose v2. Nothing else is needed on the host: PostgreSQL 18,
pgbench and the Rust toolchain all run in containers.

## Quick start

```sh
cd project-b                                   # or project-a
mkdir -p ../results/local/project-b            # collector / pgbench output
docker compose up -d                           # postgres, seed (1M orders), consumer, collector
docker compose run --rm producer               # 60 s of ingestion (pgbench)
docker compose run --rm reader                 # business reads (pgbench)
docker compose exec postgres psql -U tlb -d tlb -c 'SELECT * FROM batch_log ORDER BY id DESC LIMIT 5'
docker compose --profile load --profile burst down -v
```

Every knob is an environment variable with a default; see the project
`docker-compose.yml` files and `bench/scenarios/`.

## Portability

The strategies use plain SQL only (no triggers, stored procedures, advisory
locks or native enum/json types). Accepted PostgreSQL specifics are listed in
[`docs/article-topics.md`](docs/article-topics.md#5-portability).

## License

MIT
