-- Project B producer, single-round-trip variant (label b-1rt).
--
-- Same lifecycle generator and the same pessimistic guarantee as
-- producer.sql, but the controller check, the lock and the insert are ONE
-- autocommit statement into the buffer this client saw as hot last time:
--
--   INSERT INTO buffer_<hot> (...)
--   SELECT ... FROM buffer_control WHERE slot = :slot AND active = <hot> FOR SHARE
--
-- * The FOR SHARE lock lives until the statement commits, so a switch
--   (UPDATE of the controller rows) waits for this insert, as before.
-- * If a switch is in flight, the statement waits for it and, under READ
--   COMMITTED, re-checks the updated row: active no longer matches, no row
--   is inserted.
-- * A miss (0 rows) falls back to the locked lookup of producer.sql and
--   remembers the new hot buffer: one extra transaction per client per switch.
--
-- Behind a transaction pooler this holds a server connection for one round
-- trip instead of four (BEGIN, SELECT ... FOR SHARE, INSERT, COMMIT).
-- RETURNING only tells pgbench whether a row was inserted; an application
-- would use the affected-row count of the INSERT instead (portable).
\set e :k
\set k :k + 1
\set phase (:e / :window) % 4
\set order_id :base + (:client_id + 1) * 100000000 + (:e / (4 * :window)) * :window + (:e % :window)
\set slot (:client_id + :slot_offset) % :lock_slots
\set ok 0

\if :hot = 0
INSERT INTO buffer_0 (order_id, status_code, payload)
SELECT CAST(:order_id AS BIGINT), CAST(:phase AS SMALLINT), CAST(:payload AS VARCHAR(512))
  FROM buffer_control WHERE slot = :slot AND active = 0 FOR SHARE
RETURNING 1 AS ok \aset
\else
INSERT INTO buffer_1 (order_id, status_code, payload)
SELECT CAST(:order_id AS BIGINT), CAST(:phase AS SMALLINT), CAST(:payload AS VARCHAR(512))
  FROM buffer_control WHERE slot = :slot AND active = 1 FOR SHARE
RETURNING 1 AS ok \aset
\endif

\if :ok = 0
BEGIN;
SELECT active AS hot FROM buffer_control WHERE slot = :slot FOR SHARE \gset
\if :hot = 0
INSERT INTO buffer_0 (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
\else
INSERT INTO buffer_1 (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
\endif
COMMIT;
\endif
