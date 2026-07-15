-- ADR-0043: a run's resolved launch parameters — the fully typed `name -> value`
-- map produced by `scarab_pipeline::params::resolve_params` at creation and
-- frozen for the life of the run (so a re-launched step re-derives byte-identical
-- interpolation — restart determinism, ADR-0027). Exposed to steps as
-- `${{ inputs.<name> }}` and as `SCARAB_PARAM_<NAME>` env.
--
-- Expand-only (ADR-0022): a single backward-compatible column with a default, so
-- old binaries and pre-existing rows (which supply no params) stay valid.
ALTER TABLE runs ADD COLUMN params JSONB NOT NULL DEFAULT '{}'::jsonb;
