# Load generator image: the official PostgreSQL image already ships pgbench.
# Build context: repository root.
FROM postgres:18

COPY common/pgbench/*.sql /scripts/common/
COPY common/pgbench/run-pgbench.sh /usr/local/bin/run-pgbench
RUN chmod 0755 /usr/local/bin/run-pgbench

ENV PGHOST=postgres PGUSER=tlb PGPASSWORD=tlb PGDATABASE=tlb
ENTRYPOINT ["/usr/local/bin/run-pgbench"]
