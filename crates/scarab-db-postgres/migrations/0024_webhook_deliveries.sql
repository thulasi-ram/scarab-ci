-- ADR-0046: webhook delivery-id replay guard. A delivery id is recorded the
-- first time it is processed; a replayed (even correctly-signed) delivery is
-- acknowledged without re-processing.
CREATE TABLE webhook_deliveries (
    forge TEXT   NOT NULL,
    id    TEXT   NOT NULL,
    at    BIGINT NOT NULL,
    PRIMARY KEY (forge, id)
);
