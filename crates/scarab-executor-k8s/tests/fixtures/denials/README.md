# Admission denial fixtures

**To be re-recorded.** These files are hand-written from the message formats
in the Kubernetes source, not yet recorded from a cluster; `recorded_from.json`
says so. Re-record them, review the diff, and commit it.

Each case is the `Status` body the API server answers a Pod create with
(`<case>.json`) and a dry-run create of the same Pod (`<case>.dry_run.json`).
The executor's `k8s_overlay` classifier tests in `src/lib.rs`
(`recorded_builtin_denials_are_classified`) read them, one row per case with
the classification expected.

| Case | Denied by | Code | The classifier |
|---|---|---|---|
| `vap_forbidden` | a ValidatingAdmissionPolicy with `reason: Forbidden` | 403 | may be the overlay's |
| `vap_invalid` | the same policy with no reason | 422 | may be the overlay's |
| `resource_quota` | a ResourceQuota (`exceeded quota: `) | 403 | a launch error, retried |
| `limit_range` | a LimitRange's max per Container | 403 | may be the overlay's |
| `pod_security` | Pod Security `restricted` | 403 | may be the overlay's |

"May be the overlay's" means the step fails with the API server's reason when
the Pod without its `k8s_overlay` passes a dry run; the test asks the classifier
with that answer. Re-record with:

```sh
just stack up                 # if the stack is not running
just test record-denials      # deploy/kind/record-denials.sh
```

The script applies each policy alone in the throwaway namespace
`scarab-record-denials`, sends the create and the dry-run create through
`kubectl proxy`, writes the answers here, and deletes the namespace and the
policy. If a test fails after a recording, the recording shows how Kubernetes
answers; fix the classifier, not the fixture.

Kyverno and Gatekeeper denials are not recorded (git-bug 1d983fd asks for
them): each needs its controller's images pulled and installed into the
cluster. Their messages are still hand-written in the tests in `src/lib.rs`.
