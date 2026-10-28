-- ADR-0074 §2: the deploy marker. A pipeline or a Step marks itself
-- `deploy: true`; a Run is a deploy Run when its pipeline or any of its Steps
-- is marked. Decided once at Run creation from the IR and stamped here, so the
-- Runs list can filter on it in the query. Independent of `deploy_environment`:
-- an Environment-governed run may be a smoke test or a backup.
--
-- `deploy_steps` holds the ids of the marked Steps (empty when only the
-- pipeline is marked), so the Run page can show which Step deploys.
--
-- Forward-only, additive (expand-contract, ADR-0003): an old binary never
-- names the columns, and every existing run is unmarked.
ALTER TABLE runs ADD COLUMN is_deploy_run BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE runs ADD COLUMN deploy_steps TEXT[] NOT NULL DEFAULT '{}';
