-- Log storage and secret redaction (git-bug 2d78464, 511a769, 93b7733,
-- 772c34f, 85b29e0).
--
-- * `log_chunks` rows get a `kind`: `output` (a body in the object store)
--   or a marker the server writes (`source_restarted`, `not_stored`,
--   `incomplete`) with no body. Whether a row is a marker is never read
--   from a body. Output rows carry the SHA-256 of their body (`digest`), the
--   tail lease term that wrote them (`writer_term`) and the ledger
--   generation they were redacted with (`values_gen`).
-- * `log_streams` holds one row per stored log: the next seq and byte
--   offset, where the current part starts (after the newest
--   `source_restarted` row), and whether the stream is `open`, `complete` or
--   `closed`. Every append locks it, under the tail lease held FOR SHARE, in
--   `scarab_append_log_row`.
-- * `log_units` + `log_unit_values` are the value ledger: the secret values
--   each launch of a unit mounts, sealed under the master key, one
--   generation per launch that added a value. A unit's log is stored only
--   while every generation is present and not purged.
--
-- Expand step (ADR-0003) with two NOT VALID checks: rows stored before
-- this migration stay as they are (kind `output`, the new columns NULL),
-- and a server built before it can no longer insert a log row (it writes no
-- `writer_term`), so it stores no output it cannot redact. Roll back only
-- with no step running (docs: upgrading).

SET LOCAL lock_timeout = '5s';

ALTER TABLE log_chunks
  ADD COLUMN kind TEXT NOT NULL DEFAULT 'output'
    CHECK (kind IN ('output', 'source_restarted', 'not_stored', 'incomplete')),
  ADD COLUMN reason TEXT CHECK (reason IN ('no_ledger', 'ledger_purged')),
  ADD COLUMN writer_term BIGINT,
  ADD COLUMN values_gen BIGINT,
  ADD COLUMN digest BYTEA,
  ALTER COLUMN object_key DROP NOT NULL,
  ADD CONSTRAINT log_chunks_body CHECK ((kind = 'output') = (object_key IS NOT NULL)) NOT VALID,
  ADD CONSTRAINT log_chunks_fenced CHECK (writer_term IS NOT NULL) NOT VALID;

CREATE TABLE log_streams (
  run_id         TEXT    NOT NULL,
  step_id        TEXT    NOT NULL,
  attempt_id     TEXT    NOT NULL,
  next_seq       BIGINT  NOT NULL,
  byte_len       BIGINT  NOT NULL,
  part_start_seq BIGINT  NOT NULL DEFAULT 0,
  state          TEXT    NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'complete', 'closed')),
  pruned         BOOLEAN NOT NULL DEFAULT false,
  final_since    BIGINT,
  updated_at     BIGINT  NOT NULL,
  PRIMARY KEY (run_id, step_id, attempt_id)
);
CREATE INDEX log_streams_open ON log_streams (run_id, attempt_id) WHERE state = 'open';

CREATE TABLE log_units (
  run_id              TEXT    NOT NULL,
  step_id             TEXT    NOT NULL,
  attempt_id          TEXT    NOT NULL,
  gen                 BIGINT  NOT NULL DEFAULT 0,
  handle              TEXT,
  started_at          BIGINT,
  settled_at          BIGINT,
  revoke_attempted_at BIGINT,
  purged              BOOLEAN NOT NULL DEFAULT false,
  PRIMARY KEY (run_id, step_id, attempt_id)
);
CREATE INDEX log_units_handle ON log_units (handle);
CREATE INDEX log_units_pending ON log_units (settled_at) WHERE NOT purged;

CREATE TABLE log_unit_values (
  run_id     TEXT   NOT NULL,
  step_id    TEXT   NOT NULL,
  attempt_id TEXT   NOT NULL,
  gen        BIGINT NOT NULL,
  sealed     BYTEA  NOT NULL,
  created_at BIGINT NOT NULL,
  PRIMARY KEY (run_id, step_id, attempt_id, gen),
  FOREIGN KEY (run_id, step_id, attempt_id) REFERENCES log_units ON DELETE CASCADE
);
CREATE INDEX log_unit_values_age ON log_unit_values (created_at);

-- Step attempts running when this migration ran: launched by an earlier
-- version, so they have no ledger. Once they end, a server reads their
-- streams once more and closes them with a `not_stored` marker (or nothing,
-- if they never started), even if no tail of a 0.7 server saw them run.
CREATE TABLE log_legacy_units (
  run_id     TEXT NOT NULL,
  step_id    TEXT NOT NULL,
  attempt_id TEXT NOT NULL,
  PRIMARY KEY (run_id, step_id, attempt_id)
);
INSERT INTO log_legacy_units (run_id, step_id, attempt_id)
  SELECT run_id, step_id, attempt_id FROM attempts
  WHERE outcome = 'running' OR (outcome IS NULL AND failure IS NULL);

-- When this migration ran. A step attempt claimed after it with no ledger
-- either never launched or was launched by a deposed earlier-version leader.
CREATE TABLE log_store_epoch (
  id INT PRIMARY KEY CHECK (id = 1),
  at BIGINT NOT NULL
);
INSERT INTO log_store_epoch (id, at) VALUES (1, (extract(epoch from now()) * 1000)::bigint);

-- Runs whose log index was pruned: their bodies are deleted by prefix once
-- more on a later retention pass.
CREATE TABLE log_prune_pending (
  run_id    TEXT   PRIMARY KEY,
  pruned_at BIGINT NOT NULL
);

-- One fenced append (2.1 of the design): lease held FOR SHARE, stream row
-- created if missing and locked, state and generation checked, the row
-- inserted at the next seq (a conflict raises), the stream updated. Returns
-- the outcome and, on `ok`, the row's seq and byte offset. Callers bound the
-- transaction (lock, statement and idle timeouts) before calling it.
CREATE FUNCTION scarab_append_log_row(
  p_run TEXT, p_step TEXT, p_attempt TEXT,
  p_unit_step TEXT,
  p_resource TEXT, p_owner TEXT, p_term BIGINT,
  p_kind TEXT, p_reason TEXT, p_object_key TEXT, p_len BIGINT, p_digest BYTEA,
  p_values_gen BIGINT, p_then TEXT, p_now BIGINT
) RETURNS TABLE (outcome TEXT, out_seq BIGINT, out_offset BIGINT, out_gen BIGINT)
LANGUAGE plpgsql AS $$
DECLARE
  s log_streams%ROWTYPE;
  u log_units%ROWTYPE;
BEGIN
  PERFORM 1 FROM leases
    WHERE resource = p_resource AND owner = p_owner AND term = p_term FOR SHARE;
  IF NOT FOUND THEN
    RETURN QUERY SELECT 'not_writer'::TEXT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT;
    RETURN;
  END IF;

  INSERT INTO log_streams (run_id, step_id, attempt_id, next_seq, byte_len, updated_at)
    SELECT p_run, p_step, p_attempt,
           COALESCE(max(c.seq) + 1, 0), COALESCE(max(c.byte_offset + c.len), 0), p_now
    FROM log_chunks c
    WHERE c.run_id = p_run AND c.step_id = p_step AND c.attempt_id = p_attempt
  ON CONFLICT (run_id, step_id, attempt_id) DO NOTHING;
  SELECT * INTO s FROM log_streams
    WHERE run_id = p_run AND step_id = p_step AND attempt_id = p_attempt FOR UPDATE;

  IF s.state <> 'open' THEN
    RETURN QUERY SELECT 'closed'::TEXT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT;
    RETURN;
  END IF;

  IF p_kind = 'output' THEN
    SELECT * INTO u FROM log_units
      WHERE run_id = p_run AND step_id = p_unit_step AND attempt_id = p_attempt FOR SHARE;
    IF NOT FOUND THEN
      RETURN QUERY SELECT 'no_ledger'::TEXT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT;
      RETURN;
    END IF;
    IF u.purged THEN
      RETURN QUERY SELECT 'ledger_purged'::TEXT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT;
      RETURN;
    END IF;
    IF p_values_gen IS DISTINCT FROM u.gen THEN
      RETURN QUERY SELECT 'values_changed'::TEXT, NULL::BIGINT, NULL::BIGINT, u.gen;
      RETURN;
    END IF;
  END IF;

  INSERT INTO log_chunks
    (run_id, step_id, attempt_id, seq, byte_offset, len, object_key, at,
     kind, reason, writer_term, values_gen, digest)
  VALUES
    (p_run, p_step, p_attempt, s.next_seq, s.byte_len, p_len, p_object_key, p_now,
     p_kind, p_reason, p_term, p_values_gen, p_digest);

  UPDATE log_streams SET
    next_seq = s.next_seq + 1,
    byte_len = s.byte_len + p_len,
    part_start_seq = CASE WHEN p_kind = 'source_restarted' THEN s.next_seq + 1
                          ELSE s.part_start_seq END,
    state = COALESCE(p_then, s.state),
    updated_at = p_now
  WHERE run_id = p_run AND step_id = p_step AND attempt_id = p_attempt;

  RETURN QUERY SELECT 'ok'::TEXT, s.next_seq, s.byte_len, NULL::BIGINT;
END;
$$;
