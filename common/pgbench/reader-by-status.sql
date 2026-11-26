-- Business read: most recently updated orders in a given state
-- (dashboard-like query; hits the rows the consumers keep updating).
\set s random(0, 3)
SELECT id, status_code, updated_at
  FROM orders
 WHERE status_code = :s
 ORDER BY updated_at DESC
 FETCH FIRST 50 ROWS ONLY;
