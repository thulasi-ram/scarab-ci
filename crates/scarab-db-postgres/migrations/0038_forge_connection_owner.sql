-- ADR-0060 part D: a connection has exactly ONE owner — the declarative
-- `connections:` config, or the DB (API/UI, or the GitHub installation
-- webhook) — never both.
--
-- The marker has to be durable: only a persisted flag lets a later boot tell
-- "the row I provisioned from config last time" (safe to overwrite) from "a row
-- a human created" (a collision that must refuse the boot). Inferring it would
-- reintroduce exactly the config-vs-DB ambiguity the decision removes.
--
-- Defaults FALSE so every pre-existing connection stays DB-owned and editable.
ALTER TABLE forge_connections
    ADD COLUMN owned_by_config BOOLEAN NOT NULL DEFAULT FALSE;
