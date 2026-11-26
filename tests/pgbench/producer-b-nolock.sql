-- NEGATIVE CONTROL. Same as producer-b-delayed.sql but the controller is read
-- WITHOUT FOR SHARE: the switch no longer waits for in-flight inserts, so an
-- insert can land in a buffer that is already being drained and then
-- truncated. The test expects events to be lost.
\set e :k
\set k :k + 1
\set phase (:e / :window) % 4
\set order_id :base + (:client_id + 1) * 100000000 + (:e / (4 * :window)) * :window + (:e % :window)
\set slot (:client_id + :slot_offset) % :lock_slots
\set pause random(0, :sleep_max_ms)

BEGIN;
SELECT active FROM buffer_control WHERE slot = :slot \gset
\sleep :pause ms
\if :active = 0
INSERT INTO buffer_0 (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
\else
INSERT INTO buffer_1 (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
\endif
COMMIT;
