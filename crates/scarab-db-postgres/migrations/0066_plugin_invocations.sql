-- ADR-0074 §5: polling `plugin_action` Steps. One row per Attempt of such a
-- Step, written when the Attempt starts: the invocation the plugin pulls from
-- the broker.
--
-- The queue IS these rows joined to their Attempt: an invocation is offered
-- only while its Attempt is the Step's latest, still `running`, on a `running`
-- Step. Nothing about the queue lives in a process, so a control-plane
-- restart loses no invocation, and a cancel, a Retry or a superseded Take
-- stops the offer without touching this table.
--
-- `label` and `last_answer_at` are overwritten by each `pending` answer and
-- are not journaled; a `succeeded`/`failed` answer closes the Attempt through
-- the normal Step-finish commit, which is.
--
-- Forward-only, additive (ADR-0003): an old binary never names the table.
CREATE TABLE plugin_invocations (
    -- A function of the fence (`scarab_engine::plugin_invocation_id`), so a
    -- redelivered launch records nothing new.
    id               TEXT   PRIMARY KEY,
    run_id           TEXT   NOT NULL,
    step_id          TEXT   NOT NULL,
    attempt_id       TEXT   NOT NULL,
    -- The installation, by its name in the Org: names are never reused
    -- there (0055), so (org, installation) names one installation forever.
    org              TEXT   NOT NULL,
    installation     TEXT   NOT NULL,
    action           TEXT   NOT NULL,
    -- The action's inputs, interpolated when the Attempt started.
    inputs           JSONB  NOT NULL DEFAULT '{}',
    poll_interval_ms BIGINT NOT NULL CHECK (poll_interval_ms >= 0),
    created_at       BIGINT NOT NULL,
    next_offer_at    BIGINT NOT NULL,
    lease_until      BIGINT,
    label            TEXT,
    last_answer_at   BIGINT,
    UNIQUE (run_id, step_id, attempt_id),
    FOREIGN KEY (run_id, step_id, attempt_id)
        REFERENCES attempts (run_id, step_id, attempt_id) ON DELETE CASCADE
);

-- The long poll: an installation's invocations, soonest offer first.
CREATE INDEX plugin_invocations_offer_idx
    ON plugin_invocations (org, installation, next_offer_at);
