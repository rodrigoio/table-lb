-- Project A: a single inbound table that also drives the business flow.
-- Producers INSERT with state 'P' (pending); consumers claim rows with
-- FOR UPDATE SKIP LOCKED, apply them to orders and mark them 'F' (finished)
-- or delete them (CLEANUP_MODE=delete).

CREATE TABLE inbox (
    id           UUID         NOT NULL DEFAULT uuidv7() PRIMARY KEY,
    order_id     BIGINT       NOT NULL,
    status_code  SMALLINT     NOT NULL,
    payload      VARCHAR(512),
    received_at  TIMESTAMP    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    state        CHAR(1)      NOT NULL DEFAULT 'P',
    processed_at TIMESTAMP
);

-- Composite index instead of a partial index (WHERE state = 'P') to stay
-- portable across SQL dialects.
CREATE INDEX inbox_state_id_idx ON inbox (state, id);
