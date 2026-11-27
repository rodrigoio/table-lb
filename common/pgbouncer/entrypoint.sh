#!/bin/bash
# Starts PGBOUNCER_PROCESSES PgBouncer processes that share port 6432
# (so_reuseport): one PgBouncer process is single-threaded, so several of
# them are needed to multiplex hundreds of thousands of client connections.
#
#   PGBOUNCER_PROCESSES    processes sharing the port (default 4)
#   PGBOUNCER_POOL_SIZE    server connections per process (default 16);
#                          PostgreSQL sees PROCESSES × POOL_SIZE connections
#   PGBOUNCER_MAX_CLIENTS  client connections accepted per process (default 200000)
#   PGBOUNCER_UPSTREAM     PostgreSQL host (default postgres)
#
# Benchmark-only settings: trust authentication inside the private compose
# network, transaction pooling (a server connection is held only for the
# duration of one transaction).
set -euo pipefail

processes="${PGBOUNCER_PROCESSES:-4}"
dir=/tmp/pgbouncer
mkdir -p "$dir"

echo '"tlb" "tlb"' >"$dir/userlist.txt"
cat >"$dir/pgbouncer.ini" <<EOF
[databases]
tlb = host=${PGBOUNCER_UPSTREAM:-postgres} port=5432 dbname=tlb user=tlb password=tlb

[pgbouncer]
listen_addr = 0.0.0.0
listen_port = 6432
unix_socket_dir =
so_reuseport = 1
listen_backlog = 4096
auth_type = trust
auth_file = $dir/userlist.txt
pool_mode = transaction
max_client_conn = ${PGBOUNCER_MAX_CLIENTS:-200000}
default_pool_size = ${PGBOUNCER_POOL_SIZE:-16}
min_pool_size = 0
reserve_pool_size = 0
max_db_connections = 0
max_prepared_statements = 512
query_wait_timeout = 300
ignore_startup_parameters = extra_float_digits
admin_users = tlb
stats_users = tlb
log_connections = 0
log_disconnections = 0
log_pooler_errors = 1
stats_period = 10
EOF

echo "pgbouncer: $processes processes × pool ${PGBOUNCER_POOL_SIZE:-16}, max ${PGBOUNCER_MAX_CLIENTS:-200000} clients each, nofile $(ulimit -n)"
pids=()
for _ in $(seq 1 "$processes"); do
  pgbouncer "$dir/pgbouncer.ini" &
  pids+=($!)
done
trap 'kill -TERM "${pids[@]}" 2>/dev/null' TERM INT
# Exit (and let compose report it) as soon as any process dies.
wait -n
kill -TERM "${pids[@]}" 2>/dev/null || true
wait
