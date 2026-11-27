# Connection pooler used by the massive-concurrency scenario (C5).
# Build context: repository root.
#
# The official PostgreSQL image already has the PGDG apt repository, which
# ships a recent PgBouncer (>= 1.21 is needed for protocol-level prepared
# statements in transaction pooling, used by `pgbench -M prepared`).
FROM postgres:18

RUN apt-get update \
 && apt-get install -y --no-install-recommends pgbouncer \
 && rm -rf /var/lib/apt/lists/*

COPY common/pgbouncer/entrypoint.sh /usr/local/bin/pgbouncer-entrypoint
RUN chmod 0755 /usr/local/bin/pgbouncer-entrypoint

USER postgres
EXPOSE 6432
ENTRYPOINT ["/usr/local/bin/pgbouncer-entrypoint"]
