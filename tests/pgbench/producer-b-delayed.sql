-- Test variant of project-b/pgbench/producer.sql: a random pause between
-- reading the controller (FOR SHARE) and inserting widens the window in which
-- a switch can happen. With the lock held, the switch must wait for us.
\set e :k
\set k :k + 1
\set phase (:e / :window) % 4
\set order_id :base + (:client_id + 1) * 100000000 + (:e / (4 * :window)) * :window + (:e % :window)
\set slot (:client_id + :slot_offset) % :lock_slots
\set pause random(0, :sleep_max_ms)

BEGIN;
SELECT active FROM buffer_control WHERE slot = :slot FOR SHARE \gset
\sleep :pause ms
\if :active = 0
INSERT INTO buffer_0 (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
\else
INSERT INTO buffer_1 (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
\endif
COMMIT;
