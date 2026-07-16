-- ADR-0047: the durable "launch happened" marker. An attempt whose handle is
-- recorded and whose backend object is later missing is Lost (assertion-gated
-- retry on a new fence) — never blindly relaunched under the same fence.
ALTER TABLE attempts ADD COLUMN handle TEXT;
