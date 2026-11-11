-- The login of the account a connection serves (a GitHub installation's
-- `account.login`), shown beside its account id while the connection waits
-- for an organization (git-bug bb8d644). Display only: the org check keeps
-- using the immutable `account_id`, since a login can be renamed and then
-- taken by someone else. Existing rows fill on their next installation
-- delivery.
ALTER TABLE forge_connections ADD COLUMN IF NOT EXISTS account_login TEXT;
