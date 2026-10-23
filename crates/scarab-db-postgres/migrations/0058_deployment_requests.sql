-- Slice b, criterion 1 (ADR-0070 §7): a Deploy Run Step's DeploymentRequests.
--
-- 1. The Deployment record's input log gains a request's EVIDENCE — what the
--    Step offered for the change (image digests, typically), read from its own
--    results. Carried and shown, never folded (the fold ignores it). NULL for a
--    report, and for a request appended before this column existed (read back
--    as "no evidence").
ALTER TABLE deployment_inputs ADD COLUMN evidence TEXT[];
ALTER TABLE deployment_inputs
    ADD CONSTRAINT deployment_inputs_evidence_request_only
    CHECK (kind = 'request' OR evidence IS NULL);

-- 2. Each Run's DeploymentRequests, resolved once at Run creation: the static
--    DeploymentTarget (its `${{ inputs.* }}` parts bound to the frozen launch
--    parameters) and the names of the results the version and the evidence are
--    read from when the Step succeeds. The Step success then appends one
--    `deployment_inputs` request row per row here, in the same transaction as
--    the success transition.
--
--    `gate_steps` is the DeploymentGates in this Run that wait on the request
--    (the per-target concurrency hold and the gate oracle, later sub-slices);
--    created here, empty until then, so those slices need no migration of
--    their own.
--
--    `ordinal` is the request's position in the Step's `deployment_requests`.
--    The (run, step, target) UNIQUE spells out "at most one request per
--    DeploymentTarget per Step", which pipeline validation enforces first.
CREATE TABLE run_deployment_requests (
    run_id           TEXT    NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    step_id          TEXT    NOT NULL,
    ordinal          INT     NOT NULL,
    org              TEXT    NOT NULL,
    environment      TEXT    NOT NULL,
    target_source    TEXT    NOT NULL,
    target_id        TEXT    NOT NULL,
    version_result   TEXT    NOT NULL,
    evidence_results TEXT[]  NOT NULL DEFAULT '{}',
    gate_steps       TEXT[]  NOT NULL DEFAULT '{}',
    PRIMARY KEY (run_id, step_id, ordinal),
    UNIQUE (run_id, step_id, target_source, target_id)
);

-- "Which Runs request this DeploymentTarget" (the concurrency hold) reads by
-- target within one Environment.
CREATE INDEX run_deployment_requests_target_idx
    ON run_deployment_requests (org, environment, target_source, target_id);

-- 3. Run creation refuses a request whose DeploymentTarget is homed in another
--    Environment; with no current routing the home is the Environment of the
--    target's newest input, read by target across the Org's log.
CREATE INDEX deployment_inputs_target_idx
    ON deployment_inputs (org, target_source, target_id, seq);
