-- Project B: table load balancing with two plain buffer tables and a
-- pessimistic controller table.
--
-- * Producers insert into the HOT buffer (the one named by buffer_control).
-- * The single consumer drains the COLD buffer, applies it to orders,
--   truncates it and only then switches: the drained buffer becomes hot.
-- * Producers hold a shared row lock (SELECT ... FOR SHARE) on their
--   controller slot while inserting; the switch UPDATE therefore waits for
--   every in-flight insert, so no write can land in a buffer after it
--   turned cold.

-- Buffers are append-only and always read in full: no indexes at all.
CREATE TABLE buffer_0 (
    id          UUID         NOT NULL DEFAULT uuidv7(),
    order_id    BIGINT       NOT NULL,
    status_code SMALLINT     NOT NULL,
    payload     VARCHAR(512),
    received_at TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE buffer_1 (
    id          UUID         NOT NULL DEFAULT uuidv7(),
    order_id    BIGINT       NOT NULL,
    status_code SMALLINT     NOT NULL,
    payload     VARCHAR(512),
    received_at TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Controller / lock table. Every slot row carries the same 'active' value
-- and is updated together by the switch. Producers lock one slot each
-- (slot = (client + offset) % lock_slots): with one slot per producer the
-- switch waits only for the producers' in-flight transactions; with a
-- single slot shared by everybody, PostgreSQL lets new FOR SHARE lockers
-- overtake the waiting UPDATE and the switch can be delayed for seconds.
CREATE TABLE buffer_control (
    slot        SMALLINT  NOT NULL PRIMARY KEY,
    active      SMALLINT  NOT NULL CHECK (active IN (0, 1)),
    generation  BIGINT    NOT NULL,
    switched_at TIMESTAMP NOT NULL
);

-- 64 slots (0..63), all pointing to buffer_0.
INSERT INTO buffer_control (slot, active, generation, switched_at)
WITH RECURSIVE slots (n) AS (
    SELECT 0
    UNION ALL
    SELECT n + 1 FROM slots WHERE n < 63
)
SELECT n, 0, 0, CURRENT_TIMESTAMP FROM slots;
