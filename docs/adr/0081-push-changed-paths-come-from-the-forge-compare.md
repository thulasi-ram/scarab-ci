# 0081. A push's changed paths come from the forge's compare API

- **Status:** Proposed
- **Date:** 2026-11-22
- **Deciders:** thulasi.ram (architect), pending review
- **Builds on:** [0010](0010-forge-integration.md) (forge integration),
  [0046](0046-forge-auth-and-multi-adapter.md) (GitHub and Forgejo adapters)
- **Tickets:** git-bug 63dbeee (Forgejo forced push), 0354b5a (merge-commit
  resolutions); `event.changed_paths` itself is 418f55e, built in f9772ad7

> **2026-11-22, after the red team.** Revised: an incomplete forge answer is never a known
> list (§2, §3); the compare body and the pushed commit count are bounded (§3); the
> fast-forward test on Forgejo looks at every parent position (§2); a 404 has its own reason
> (§4); every reader of a push's paths is named (§5); the two owner decisions are recorded
> (§6, Consequences); the time spent inside a webhook delivery is checked against the
> forges' limits (§7).

## Context

A `when:` can read `event.changed_paths`. For a push, f9772ad7 builds the list in the
adapter from the webhook payload: the union of every listed commit's `added`, `modified`
and `removed` (`push_changed_paths`, `ChangedPaths::of_commits` in `scarab-forge`). The
list is unknown for a new branch, a deleted branch, a payload flagged `forced` (GitHub
only), no commits, or a cut-short commit list (GitHub: 2,048 or more listed; Forgejo: fewer
listed than its `total_commits`). An unknown list makes a `when:` that needs it count as
true; the Run starts and its activity log says why (`record_path_filter_note`). That is the
owner's decision in f9772ad7, and this ADR keeps it (§6).

Two gaps remain:

- **Forgejo forced push (63dbeee).** Forgejo's push payload has no `forced` flag and its
  commits carry no parents. A forced push yields a known list of the new commits' files and
  misses the files the dropped commits changed.
- **Merge resolutions (0354b5a).** The list is built per commit. On GitHub the payload's
  per-commit lists are not a tree diff, so a file changed only in a merge commit's conflict
  resolution can be missing. Forgejo 16.0.1 computes each commit's files with `git log
  --name-status -m --first-parent --no-renames` (`GetCommitFileStatus`), so a merge commit
  lists its diff against its first parent, resolutions included; but a merge whose
  first-parent line does not pass through the branch's old head (a `git pull` merge pushed
  back) can drop the old head's change to a file the other side did not touch, and no
  listed commit shows it.

What the forges offer, checked against their API specs and Forgejo's v16.0.1 source:

| | GitHub `GET /repos/{o}/{r}/compare/{before}...{after}` | Forgejo `GET /repos/{o}/{r}/compare/{before}...{after}` |
|---|---|---|
| Ancestry | `status`: `ahead`, `identical`, `behind`, `diverged`; `ahead_by` | none; each commit has `parents` |
| Files | `files`: the tree diff merge-base..after, renames with `previous_filename`, at most 300, no paging for files | each listed commit's own `files`; no tree diff |
| Commits | paged (`per_page`); files only on page 1 | every commit in merge-base..after, and `total_commits` |

Forgejo has no API for the tree diff of two arbitrary commits. The repository names no
Forgejo version other than the 16.0.1 that `just test e2e` runs (`deploy/kind/forgejo.yaml`);
the docs and CHANGELOG state no minimum. A tag push is an `Event::Tag` and has no list; that
does not change.

**Where the trigger runs.** `github_webhook` and `forgejo_webhook` record the delivery id
(`delivery_is_fresh`), then `ingest_event` (`scarab-server/src/lib.rs`, about line 8473)
reads the Pipeline files, compiles them and creates the Runs (`trigger_runs_from_files`)
before it answers. A pull request's files request already waits inside that answer, for at
most `PULL_REQUEST_FILES_TIMEOUT_SECS` (5 seconds, half of GitHub's 10-second delivery
limit, by that constant's own comment).

## Decision

### 1. Asked lazily, once per delivery

A push's paths start as **not fetched**, as a pull request's do today. The adapter still
reads these facts from the payload without a call, and each stays unknown with its reason
and makes no request:

- `after` all zeros or `deleted`: `Deleted`;
- `before` all zeros or `created`: `NewBranch`;
- `forced: true` (GitHub): `Forced`;
- `before` or `after` not 40 or 64 hex digits: `BadPayload` (the ids go into a URL path;
  the payload is HMAC-checked, and the adapter checks them anyway; all zeros of either
  length is recognised by `is_zero_sha`);
- Forgejo `total_commits` over 1,000: `TooManyCommits` (§3);
- `before` equal to `after`: a known empty list.

Otherwise the server asks the forge only when a candidate Pipeline's matching trigger, or
one of its run-scope Step `when:`s, reads `event.changed_paths` (`reads_changed_paths`): the
same rule and the same `DeliveryTrigger::decide` path the pull request files request uses,
with the same 5-second limit. One delivery makes at most one compare request. A repository
whose Pipelines do not read the field makes none. `Event::Push` gains `before` (serde
default) so the server can ask.

The answer replaces the payload's list. The payload's per-commit lists are no longer used
for a push's paths, so the adapters stop producing `Truncated` and `NoCommits` for pushes: a
push of 2,048 or more commits on GitHub, or a Forgejo push that lists fewer commits than its
`total_commits`, now gets a list (up to the limits in §3).

### 2. `ForgePort::push_changed_paths(repo, before, after)`

A new port method returns `ChangedPaths`. The default implementation returns
`Unknown(ForgeError)`. Both adapters follow one rule: **a known list comes only from an
answer that is complete by its own fields**. Anything else is unknown, never a partial list.

**GitHub.** One request: `compare/{before}...{after}?per_page=1` (one commit per page; the
file list is on page 1 and covers the whole comparison). The commit list is not read, so
`total_commits` is not compared with it.

- `identical`: a known empty list.
- `behind` or `diverged`: `Unknown(Forced)`.
- `ahead` with `ahead_by` above 0 and a non-empty `files` array of fewer than 300 entries:
  the list is each entry's `filename` and, for a rename, `previous_filename`.
- `ahead` with no `files` key, or with `files` empty: `Unknown(Incomplete)`. GitHub can
  answer an empty `files` for a comparison it did not compute. The cost: a push of only
  empty commits, or of a commit and its revert, runs its path-filtered Pipelines.
- 300 entries in `files`: `Unknown(TooMany)`, because GitHub stops there without saying so.
- Any other `status`, or no `ahead_by`: `Unknown(ForgeError)`.

**Forgejo.** One request: `compare/{before}...{after}?verification=false` (signature checks
are the expensive part and are not needed). The listed commits are merge-base..after. In
order:

1. **Complete?** `total_commits` missing or different from the number of listed commits,
   `after` not among them, or any listed commit with no `files` key: `Unknown(Incomplete)`.
2. **Fast-forward?** The push is a fast-forward exactly when some listed commit has `before`
   among its parents, in any position: then `before` is an ancestor of `after`, the
   merge-base is `before`, and the list holds every commit between them. If no listed
   commit has `before` as a parent: `Unknown(Forced)`.
3. **First-parent line.** From `after`, follow each commit's first parent through the listed
   commits. If the walk reaches `before`, the list is the union of the files of the commits
   on that line. Each one's files are its diff against its first parent, so the union holds
   every path that differs between `before` and `after`, merge resolutions included. It can
   also hold a path that changed and changed back. If the walk leaves the listed commits
   without reaching `before`: `Unknown(MergedHead)`.

This handles a merge `M(p1 = X, p2 = c1)` where `c1`'s first parent is `before`: it is a
fast-forward (step 2), `before` is on no first-parent line from `after`, and the reason is
`MergedHead`, not `Forced`.

3,000 or more paths is `Unknown(TooMany)` (`CHANGED_PATHS_MAX`), as today.

**Oldest supported Forgejo: 16.0.1**, the version the live tier runs. A Forgejo or Gitea
whose compare answer has no `total_commits` or no per-commit `files` gets
`Unknown(Incomplete)` on every push: its path-filtered Pipelines run, each with the note.
The docs state the version.

### 3. Bounds

- **Time:** the 5-second limit of §7 covers the whole call, the GitHub adapter's
  `Retry-After` waits included. The constant is renamed `CHANGED_FILES_TIMEOUT_SECS`, since
  it now covers both requests.
- **Bytes:** each adapter reads the compare body as a stream and stops at 16 MiB:
  `Unknown(TooLarge)`. The body is never parsed from a partial read.
- **Commits (Forgejo):** a payload whose `total_commits` is over 1,000 gets
  `Unknown(TooManyCommits)` and no request, so a push that merges a long-lived branch does
  not make Forgejo run one `git log --name-status` per commit for Scarab. GitHub needs no
  such limit: `per_page=1` returns one commit, and its `files` stop at 300.

### 4. Failure is unknown, never the payload list

A forge error, a rate limit that outlasts the adapter's retries, a malformed answer, the
5-second limit, a 404, an incomplete answer or an over-limit answer gives an unknown list.
The `when:` counts as true and the Run's activity log names the reason. Falling back to the
payload list would bring back both bugs without a note.

`PathsUnknown` reasons after this ADR (new or changed text):

| Reason | Text in the Run's activity log |
|---|---|
| `NotFetched` | the changed files were not asked for |
| `NotFound` (new) | the forge answered 404: it no longer has the earlier head (removed after a forced push), or Scarab can no longer read the repository |
| `MergedHead` (new) | the branch's earlier head is not on the new head's first-parent line |
| `Incomplete` (new) | the forge's answer did not list every pushed commit or file |
| `TooMany` | 300 or more files changed (the most a GitHub compare lists), or 3,000 or more elsewhere; the text names the limit that was reached |
| `TooManyCommits` (new) | more than 1,000 commits were pushed, so the changed files were not asked for |
| `TooLarge` (new) | the forge's changed files answer was over 16 MiB |
| `BadPayload` (new) | the push payload's commit ids are not commit ids |
| `ForgeError` | the forge did not answer the changed files request |
| `TimedOut` | the forge did not list the changed files within 5 seconds |

`NewBranch`, `Forced`, `Deleted` and `NotGiven` keep their texts.

### 5. Every reader of a push's changed paths

- **`trigger_runs_from_files`** (`lib.rs`, about line 5329) builds `ctx` from
  `event.when_context()` and `paths` from `event.changed_paths()`, both `NotFetched` for a
  push that needs a request. Each compiled Pipeline goes through `DeliveryTrigger::decide`
  (about line 6237), which today asks only for `Event::PullRequest` (about line 6255). It
  gains the `Event::Push` arm: on a match that may rest on the unknown paths, it calls
  `push_changed_paths`, applies the answer to `ctx` and decides again. Run-scope Step
  `when:`s and `record_path_filter_note` (about line 5489) then read that same `ctx` and
  `paths`.
- **`refuse_uncompiled_delivery`** decides every trigger through the same `decide` with the
  same `ctx` and `paths`, so a delivery with a broken file makes the same one request.
- **Manual and API dispatch** (`lib.rs`, about line 7421) and a **cron fire**
  (`schedules.rs`, about line 328) call `record_path_filter_note` with
  `event.changed_paths()`, but their events are `Event::Manual`, `Event::Api` and
  `Event::Cron`, never `Event::Push`, so the paths are `NotGiven` there. No change.
- **`schedules::index_push`** reads a push's ref and `after`, not its paths. No change.
- **Policy test triggers** (`policies.rs` `test_event`, about line 1320) build an
  `Event::Push` with `changed_paths` at its default, `NotGiven`, and call no forge. No
  change: a policy test cannot exercise a path filter, as today.

A `NotFetched` list that reaches `record_path_filter_note` without a request means no
`when:` read the field, and `changed_paths_unapplied` then finds no place, so no note is
written. `Event::changed_paths()`'s doc changes from "a push carries its own" to "a push's
come from the forge's compare API".

### 6. Unknown counts as true; a path filter is not a deploy guard

Owner decision, 2026-11-22: an unknown list keeps counting as true. This ADR adds causes of
unknown that happen in normal operation: a forge outage, a slow answer, a rate limit, 300 or
more files on GitHub, a pull merge pushed back on Forgejo. A Pipeline written as `when:
event.branch == 'main' && event.changed_paths.exists(p, p.startsWith('infra/'))` that runs
`terraform apply` runs on each of those. Someone who can push 300 files to that branch can
also touch `infra/`, so this is no escalation; the cost is deploys nobody planned during a
forge incident.

The Pipeline docs (`usage/pipeline-syntax.mdx`, Changed files) say so: a `changed_paths`
filter decides when a Pipeline may skip work; it is not a guard on a deploy, because an
unknown list runs the Pipeline. A deploy is guarded by an Environment's protection and
approvals.

Later work, not decided here: expose whether the list is known to `when:` (for example
`event.changed_paths_known`, or the field left out so `has(event.changed_paths)` is false),
so an author can write a deploy trigger that skips on unknown.

### 7. Time inside the webhook delivery

The compare runs inside the webhook request (§Context), after the config reads, library
prefetch and compile, and before admission. Its limit stays 5 seconds, as for a pull
request:

- **GitHub** marks a delivery failed after 10 seconds. 5 seconds of compare leaves 5 for
  the rest, as the pull request path assumes today.
- **Forgejo**'s limit is `[webhook] DELIVER_TIMEOUT`, 5 seconds in Forgejo's default
  configuration (from its configuration docs, not re-checked against 16.0.1; the kind
  stack sets 30). On a stock Forgejo a compare that uses most of its 5 seconds makes
  Forgejo mark the delivery failed. The pull request files request has the same exposure
  today.

A delivery the forge gave up on must still create its Runs. `delivery_is_fresh` records the
delivery id before the trigger runs, so a redelivery is answered "duplicate delivery" and
cannot repair a delivery whose handler was dropped. The build adds a server test that
closes the client connection while a FakeForge compare is pending and checks that the Runs
are created. If they are not (the HTTP server drops the handler with the connection), the
work after `delivery_is_fresh` moves into a spawned task that the handler waits on, so a
closed connection drops only the wait. That change covers the pull request files request
too and gets its own ticket.

### 8. Cost

At most one request per push delivery, only in repositories that filter on paths, beside
the config reads each push already makes. On GitHub it counts against the installation's
hourly limit (5,000 or more). On Forgejo the answer grows with the number of commits pushed
(every commit, with its files), bounded by the 1,000-commit limit and the 16 MiB cap.

## Consequences

- A Forgejo forced push no longer yields a partial list; it is unknown, as on GitHub. The
  docs line "forced-push detection is GitHub only" goes.
- Merge resolutions are in the list on both forges. A Forgejo push whose first-parent line
  does not pass through the old head is unknown, so its path-filtered Pipelines run.
- Pushes that list too many commits for the payload now get a list, up to 1,000 commits on
  Forgejo.
- **GitHub pushes of 300 to 2,999 changed files lose their list** (owner decision,
  2026-11-22: accepted). Today the payload gives a known list up to 3,000 paths; after this
  ADR such a push is `Unknown(TooMany)` and every path-filtered Pipeline runs, deploys
  included. In a monorepo that is a large merge or a dependency bump. The `TooMany` text
  names 300 for a GitHub push.
- A forge outage makes path-filtered Pipelines run on every push, each with a note.
- Forgejo older than 16.0.1, or Gitea without per-commit `files`, gets no push list.
- Docs tasks (`usage/pipeline-syntax.mdx`, Changed files): the compare source, the 300-file
  GitHub limit, the oldest Forgejo, the new reasons, and the §6 sentence that a path filter
  is not a deploy guard. The CHANGELOG names the 300-file change.

## Rejected options

- **Payload list as a fallback when the compare fails.** Silently wrong in exactly the cases
  this ADR fixes.
- **Ask on every push.** A network call per delivery for repositories that never filter on
  paths.
- **A list for forced pushes** (union of `merge-base...before` and `merge-base...after`).
  Two requests, and GitHub's 300-file cap on each. Forced pushes stay unknown for now.
- **New branch compared with the default branch.** That answers "what does this branch
  change", a pull request question, against a ref that moves. A new branch stays unknown.
- **GitHub: page the commits and union each commit's files for 300 to 2,999 files.** More
  requests per push, and the per-commit union misses merge resolutions, the bug this ADR
  fixes (owner decision, 2026-11-22).
- **Forgejo: union of every listed commit when the old head is off the first-parent line.**
  Misses a merge that drops the old head's change to a file the other side did not touch
  (§Context).
- **Forgejo per-commit `git/commits/{sha}.diff`.** One request per commit, and it is still
  a first-parent diff.
- **A per-trigger setting for unknown (run, skip, or refuse with a failed check).** A new
  decision rule in the server; the later work in §6 gives authors the same choice in
  `when:`.

## Test plan

No network in tests. Fixtures are recorded once, from GitHub and from the Forgejo 16.0.1 in
`deploy/kind/forgejo.yaml`, and committed with a note of where each came from. The
`diverged`, 300-file, empty-`files` and no-`files` GitHub fixtures may be edited from one
real `ahead` recording; the note beside each says which were edited.

- **Adapter parse functions** (pure, like `normalize`), in each adapter's
  `tests/changed_paths.rs`:
  - GitHub `ahead` with a merge commit whose resolution file is in `files` and in no
    commit of the matching `push.json`: the path is listed. A rename lists both names.
  - GitHub `diverged`: `Unknown(Forced)`. 300 files: `Unknown(TooMany)`. No `files`, and
    empty `files` with `ahead_by` 1: `Unknown(Incomplete)`. `identical`: known empty.
  - Forgejo fast-forward with a merge commit on the first-parent line: the resolution file
    is listed. Forced (old head unreachable): `Unknown(Forced)`. Old head as a merge's
    second parent: `Unknown(MergedHead)`. Old head as the first parent of a merge's second
    parent, one level down: `Unknown(MergedHead)`, not `Forced`. 3,000 paths:
    `Unknown(TooMany)`.
  - Forgejo incomplete answers, each `Unknown(Incomplete)`: a commit with no `files` key;
    `after` not listed; `total_commits` one more than the listed commits; no
    `total_commits`.
  - A body over 16 MiB: `Unknown(TooLarge)`. A 404: `Unknown(NotFound)`.
  - The request URL each adapter builds (`per_page=1`; `verification=false`), and no
    request for a `before` that is not hex, which is `Unknown(BadPayload)`.
- **Server** (`crates/scarab-server/tests/changed_paths.rs`, FakeForge counting calls): a
  push whose payload misses the filtered directory but whose compare answer has it starts the
  Run; no Pipeline reading the field makes zero calls; two Pipelines reading it make one; a
  delivery with a broken file makes at most one; a compare error and a slow compare each
  start the Run with the note naming the reason; new branch, deleted branch, a GitHub
  `forced: true` payload and a Forgejo payload with `total_commits` 1,001 make no call; a
  client connection closed during a slow compare still creates the Runs (§7).
- **Live** (`just test e2e changed_paths`, the Forgejo in the kind cluster): a merge with a
  conflict resolved in a filtered directory starts the Pipeline; a force push starts it with
  the "forced push" note.
