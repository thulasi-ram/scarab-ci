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
--   `closed`. Every append locks it, under the writer's lease for that
--   stream held FOR SHARE, in `scarab_append_log_row`.
-- * `log_units` + `log_unit_values` are the value ledger: the secret values
--   each launch of a unit mounts, sealed under the master key, one
--   generation per launch that added a value, and when the minted forge
--   tokens of each generation were revoked. A unit's log is stored only
--   while every generation is present and not purged. A unit has
--   `stream_count` streams (its main container and one per sidecar
--   service); its ledger is purged after the last of them is terminal.
--
-- Expand step (ADR-0003). The four checks on `log_chunks` are NOT VALID, so
-- adding them scans no row: rows stored before this migration stay as they
-- are (kind `output`, the new columns NULL), and a server built before it
-- can no longer insert a log row (it writes no `writer_term`), so it stores
-- no output it cannot redact. The `log_chunks` ALTER is the last statement:
-- the scans of `attempts` and `run_services` below run before it takes its
-- lock.
--
-- Rollback: a server built before this migration does not start on a
-- database that ran it (the migrator finds version 99, which it does not
-- know, and stops). To go back to 0.6.x: let Runs finish and stop new ones,
-- scale the server to zero, back up the database, run
-- `crates/scarab-db-postgres/rollback/0099_revert.sql` (it reverts 0100
-- too), then start 0.6.x. The revert deletes the marker rows and the value
-- ledgers; output rows stay readable. Upgrading again runs 0099 again.

SET LOCAL lock_timeout = '5s';

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
  -- The unit's main stream and one per sidecar service of its step.
  stream_count        INT     NOT NULL DEFAULT 1,
  -- Of two units with one handle (a Pod name hash collision), the newer.
  created_at          BIGINT  NOT NULL DEFAULT 0,
  started_at          BIGINT,
  settled_at          BIGINT,
  revoke_attempted_at BIGINT,
  -- The ledger sweep's backoff after a failed revocation: 1 min, doubling
  -- to 1 h.
  revoke_failures     INT     NOT NULL DEFAULT 0,
  revoke_retry_at     BIGINT,
  purged              BOOLEAN NOT NULL DEFAULT false,
  PRIMARY KEY (run_id, step_id, attempt_id)
);
CREATE INDEX log_units_handle ON log_units (handle, created_at);
CREATE INDEX log_units_pending ON log_units (settled_at) WHERE NOT purged;

CREATE TABLE log_unit_values (
  run_id     TEXT   NOT NULL,
  step_id    TEXT   NOT NULL,
  attempt_id TEXT   NOT NULL,
  gen        BIGINT NOT NULL,
  sealed     BYTEA  NOT NULL,
  created_at BIGINT NOT NULL,
  -- When every minted forge token in this generation was revoked.
  revoked_at BIGINT,
  PRIMARY KEY (run_id, step_id, attempt_id, gen),
  FOREIGN KEY (run_id, step_id, attempt_id) REFERENCES log_units ON DELETE CASCADE
);
CREATE INDEX log_unit_values_age ON log_unit_values (created_at);

-- Units running when this migration ran, in a Run that had not ended:
-- launched by an earlier version, so they have no ledger. Once they end, a
-- server closes their streams with a `not_stored` marker (or nothing, if
-- they never started) without reading them, even if no tail of a 0.7
-- server saw them run; the row is deleted then.
CREATE TABLE log_legacy_units (
  run_id     TEXT NOT NULL,
  step_id    TEXT NOT NULL,
  attempt_id TEXT NOT NULL,
  PRIMARY KEY (run_id, step_id, attempt_id)
);
INSERT INTO log_legacy_units (run_id, step_id, attempt_id)
  SELECT a.run_id, a.step_id, a.attempt_id FROM attempts a
  WHERE (a.outcome = 'running' OR (a.outcome IS NULL AND a.failure IS NULL))
    AND EXISTS (SELECT 1 FROM runs r WHERE r.id = a.run_id
                  AND r.status NOT IN ('succeeded', 'failed', 'cancelled', 'dead_lettered'));
INSERT INTO log_legacy_units (run_id, step_id, attempt_id)
  SELECT s.run_id, 'service:' || s.name, 't' || s.take FROM run_services s
  WHERE s.status NOT IN ('torn-down', 'failed')
    AND EXISTS (SELECT 1 FROM runs r WHERE r.id = s.run_id
                  AND r.status NOT IN ('succeeded', 'failed', 'cancelled', 'dead_lettered'))
  ON CONFLICT DO NOTHING;

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

-- One fenced append (2.1 of the design). The writer's lease is the one of
-- kind `p_lease` (`tail` or `append`) for this stream: its resource is
-- derived here from the stream id (the same strings as
-- `scarab_engine::LogLease::resource`), so a writer holding another
-- stream's lease is refused. That lease row is held FOR SHARE; the stream
-- row is locked, or created from the rows already stored when missing
-- (unless the run's log was pruned); state and generation checked; the row
-- inserted at the next seq (a conflict raises); the stream updated. Returns
-- the outcome and, on `ok`, the row's seq and byte offset. Callers bound
-- the transaction (lock, statement and idle timeouts) before calling it.
CREATE FUNCTION scarab_append_log_row(
  p_run TEXT, p_step TEXT, p_attempt TEXT,
  p_lease TEXT, p_owner TEXT, p_term BIGINT,
  p_kind TEXT, p_reason TEXT, p_object_key TEXT, p_len BIGINT, p_digest BYTEA,
  p_values_gen BIGINT, p_then TEXT, p_now BIGINT
) RETURNS TABLE (outcome TEXT, out_seq BIGINT, out_offset BIGINT, out_gen BIGINT)
LANGUAGE plpgsql AS $$
DECLARE
  s log_streams%ROWTYPE;
  u log_units%ROWTYPE;
  -- A sidecar stream `<step>::service-<i>` belongs to its step's unit; the
  -- split is at the last `::service-`.
  v_cut INT := strpos(reverse(p_step), reverse('::service-'));
  v_unit_step TEXT := CASE WHEN v_cut = 0 THEN p_step
                           ELSE left(p_step, length(p_step) - v_cut - 9) END;
  v_resource TEXT;
BEGIN
  v_resource := CASE p_lease
    WHEN 'tail' THEN
      CASE WHEN v_cut = 0 THEN 'tail2:' || p_run || ':' || p_step || ':' || p_attempt
           ELSE 'tail2:' || p_run || ':' || v_unit_step || ':sidecar-'
                || right(p_step, v_cut - 1) || ':' || p_attempt END
    WHEN 'append' THEN 'append:' || p_run || ':' || p_step || ':' || p_attempt
  END;
  PERFORM 1 FROM leases
    WHERE resource = v_resource AND owner = p_owner AND term = p_term FOR SHARE;
  IF NOT FOUND THEN
    RETURN QUERY SELECT 'not_writer'::TEXT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT;
    RETURN;
  END IF;

  -- The stream row first: the rows stored before it existed are read only
  -- when it is missing.
  SELECT * INTO s FROM log_streams
    WHERE run_id = p_run AND step_id = p_step AND attempt_id = p_attempt FOR UPDATE;
  IF NOT FOUND THEN
    -- A run whose log was pruned takes no new stream.
    IF EXISTS (SELECT 1 FROM log_prune_pending WHERE run_id = p_run)
       OR EXISTS (SELECT 1 FROM log_streams WHERE run_id = p_run AND pruned) THEN
      RETURN QUERY SELECT 'closed'::TEXT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT;
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
  END IF;

  IF s.state <> 'open' THEN
    RETURN QUERY SELECT 'closed'::TEXT, NULL::BIGINT, NULL::BIGINT, NULL::BIGINT;
    RETURN;
  END IF;

  IF p_kind = 'output' THEN
    SELECT * INTO u FROM log_units
      WHERE run_id = p_run AND step_id = v_unit_step AND attempt_id = p_attempt FOR SHARE;
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

-- Last: the columns and checks on `log_chunks`. Adding a column with a
-- constant default rewrites nothing, and NOT VALID checks scan nothing, so
-- the lock is held for catalog changes only. `log_chunks_kind`,
-- `log_chunks_reason` and `log_chunks_body` can be validated in a later
-- release, once no 0.6.x server can run; `log_chunks_fenced` never is (rows
-- from before this migration have no `writer_term`).
ALTER TABLE log_chunks
  ADD COLUMN kind TEXT NOT NULL DEFAULT 'output',
  ADD COLUMN reason TEXT,
  ADD COLUMN writer_term BIGINT,
  ADD COLUMN values_gen BIGINT,
  ADD COLUMN digest BYTEA,
  ALTER COLUMN object_key DROP NOT NULL,
  ADD CONSTRAINT log_chunks_kind
    CHECK (kind IN ('output', 'source_restarted', 'not_stored', 'incomplete')) NOT VALID,
  ADD CONSTRAINT log_chunks_reason
    CHECK (reason IN ('no_ledger', 'ledger_purged')) NOT VALID,
  ADD CONSTRAINT log_chunks_body CHECK ((kind = 'output') = (object_key IS NOT NULL)) NOT VALID,
  ADD CONSTRAINT log_chunks_fenced CHECK (writer_term IS NOT NULL) NOT VALID;
