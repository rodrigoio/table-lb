-- Project B producer: one event per transaction, routed to the HOT buffer.
--
-- Order lifecycle generator (identical in project A and project B):
-- each client walks a window of :window orders through the state machine,
-- sending 'pending' to every order of the window, then 'payment_confirmed'
-- to every order, and so on. Every (order_id, phase) pair is produced
-- exactly once, and consecutive phases of one order are :window events apart.
\set e :k
\set k :k + 1
\set phase (:e / :window) % 4
\set order_id :base + (:client_id + 1) * 100000000 + (:e / (4 * :window)) * :window + (:e % :window)
\set slot (:client_id + :slot_offset) % :lock_slots

-- Pessimistic routing: the shared row lock on the controller slot is held
-- until COMMIT, so the consumer's switch (an UPDATE on the same rows) waits
-- for this insert to finish before the buffer can turn cold.
BEGIN;
SELECT active FROM buffer_control WHERE slot = :slot FOR SHARE \gset
\if :active = 0
INSERT INTO buffer_0 (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
\else
INSERT INTO buffer_1 (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
\endif
COMMIT;
