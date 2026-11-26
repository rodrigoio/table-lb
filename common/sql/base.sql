-- Business model shared by project A and project B.
-- Written in plain, portable SQL: no triggers, no stored procedures,
-- no native enum/json types.

-- Order state machine: pending -> payment_confirmed -> sent -> delivered.
-- A lookup table instead of a native enum keeps the model portable;
-- the numeric code order IS the state machine order.
CREATE TABLE order_status (
    code SMALLINT    NOT NULL PRIMARY KEY,
    name VARCHAR(32) NOT NULL UNIQUE
);

INSERT INTO order_status (code, name) VALUES
    (0, 'pending'),
    (1, 'payment_confirmed'),
    (2, 'sent'),
    (3, 'delivered');

-- Business table holding the real data.
-- source_ref = '<table_name>:<event uuid>' of the event that set the current
-- state. Provisional reference for analysis only.
CREATE TABLE orders (
    id          BIGINT       NOT NULL PRIMARY KEY,
    status_code SMALLINT     NOT NULL REFERENCES order_status (code),
    source_ref  VARCHAR(64),
    data        VARCHAR(512),
    created_at  TIMESTAMP    NOT NULL,
    updated_at  TIMESTAMP    NOT NULL
);

CREATE INDEX orders_status_updated_idx ON orders (status_code, updated_at);

-- One row per batch (project A) or per switch/drain cycle (project B).
CREATE TABLE batch_log (
    id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    project          CHAR(1)          NOT NULL,
    worker           SMALLINT         NOT NULL,
    source_table     VARCHAR(32)      NOT NULL,
    generation       BIGINT,
    started_at       TIMESTAMP        NOT NULL,
    finished_at      TIMESTAMP        NOT NULL,
    rows_read        INTEGER          NOT NULL,
    rows_distinct    INTEGER          NOT NULL,
    rows_applied     INTEGER          NOT NULL,
    lag_p50_ms       DOUBLE PRECISION NOT NULL,
    lag_p95_ms       DOUBLE PRECISION NOT NULL,
    lag_max_ms       DOUBLE PRECISION NOT NULL,
    apply_ms         DOUBLE PRECISION NOT NULL,
    cleanup_ms       DOUBLE PRECISION,
    switch_wait_ms   DOUBLE PRECISION,
    wait_for_data_ms DOUBLE PRECISION,
    retries          INTEGER          NOT NULL DEFAULT 0
);

-- Filled only when a consumer runs with AUDIT=1 (correctness tests).
-- No primary key on purpose: duplicates must be detectable, not rejected.
CREATE TABLE audit_consumed (
    order_id    BIGINT    NOT NULL,
    status_code SMALLINT  NOT NULL,
    consumed_at TIMESTAMP NOT NULL
);
