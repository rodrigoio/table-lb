-- Business read: point lookup of a historical order by primary key.
\set id random(1, :seed_orders)
SELECT id, status_code, updated_at FROM orders WHERE id = :id;
