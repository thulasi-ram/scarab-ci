# Sourced by `just test` and `just test coverage` (just/test.just): make sure
# the compose Postgres and MinIO in this directory are up, and export the
# SCARAB_TEST_* variables that point the workspace suite at them.
# Run from the repository root.
# shellcheck shell=bash

# Reuse whatever already serves 55432 (an earlier run's container, or an
# ad-hoc dev one); otherwise bring up just the compose Postgres.
if (exec 3<>/dev/tcp/127.0.0.1/55432) 2>/dev/null; then
  echo "==> reusing the Postgres already listening on 127.0.0.1:55432"
else
  docker compose -f deploy/test-services/compose.yaml up -d --wait postgres
fi
export SCARAB_TEST_DATABASE_URL=postgres://scarab:scarab@127.0.0.1:55432/scarab
# The live-S3 test (crates/scarab-storage-s3/tests/minio.rs) wants the
# compose MinIO too — same reuse-or-start dance as Postgres above.
if (exec 3<>/dev/tcp/127.0.0.1/9000) 2>/dev/null; then
  # An earlier run already ran createbuckets; a foreign MinIO with
  # different credentials would wedge it, so do not re-run it here. (A
  # MinIO from before the scarab-test bucket existed needs one manual
  # `docker compose -f deploy/test-services/compose.yaml run --rm createbuckets`.)
  echo "==> reusing the MinIO already listening on 127.0.0.1:9000"
else
  docker compose -f deploy/test-services/compose.yaml up -d --wait minio
  docker compose -f deploy/test-services/compose.yaml run --rm createbuckets
fi
export SCARAB_TEST_S3_ENDPOINT=http://127.0.0.1:9000
# scarab-test: the suite's own bucket (A11), so a suite prune or fixture
# collision cannot eat another stack's state.
export SCARAB_TEST_S3_BUCKET=scarab-test
export SCARAB_TEST_S3_ACCESS_KEY=scarab
export SCARAB_TEST_S3_SECRET_KEY=scarabsecret
