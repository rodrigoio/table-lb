-- Project A producer: one event per transaction, inserted into the inbox.
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

INSERT INTO inbox (order_id, status_code, payload) VALUES (:order_id, :phase, :payload);
