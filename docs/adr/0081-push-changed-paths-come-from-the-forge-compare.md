# 0081. A push's changed paths come from the forge's compare API

- **Status:** Proposed
- **Date:** 2026-11-22
- **Deciders:** thulasi.ram (architect), pending review
- **Builds on:** [0010](0010-forge-integration.md) (forge integration),
  [0046](0046-forge-auth-and-multi-adapter.md) (GitHub and Forgejo adapters)
- **Tickets:** git-bug 63dbeee (Forgejo forced push), 0354b5a (merge-commit
  resolutions); `event.changed_paths` itself is 418f55e, built in f9772ad7

## Context

A `when:` can read `event.changed_paths`. For a push, f9772ad7 builds the list in the
adapter from the webhook payload: the union of every listed commit's `added`, `modified`
and `removed` (`push_changed_paths`, `ChangedPaths::of_commits` in `scarab-forge`). The
list is unknown for a new branch, a deleted branch, a payload flagged `forced` (GitHub
only), no commits, or a cut-short commit list. An unknown list makes a `when:` that needs
it count as true; the Run starts and its activity log says why (`record_path_filter_note`).
That is the owner's decision in f9772ad7, and this ADR keeps it.

Two gaps remain:

- **Forgejo forced push (63dbeee).** Forgejo's push payload has no `forced` flag and its
  commits carry no parents. A forced push yields a known list of the new commits' files and
  misses the files the dropped commits changed.
- **Merge resolutions (0354b5a).** The list is built per commit. On GitHub the payload's
  per-commit lists are not a tree diff, so a file changed only in a merge commit's conflict
  resolution can be missing. Forgejo 16.0.1 computes each commit's files with `git log
  --name-status -m --first-parent --no-renames` (`GetCommitFileStatus`), so a merge commit
  lists its diff against its first parent, resolutions included; but a merge whose
  **second** parent is the branch's old head (a `git pull` merge pushed back) can drop the
  old head's change to a file the other side did not touch, and no listed commit shows it.

What the forges offer, checked against their API specs and Forgejo's v16.0.1 source:

| | GitHub `GET /repos/{o}/{r}/compare/{before}...{after}` | Forgejo `GET /repos/{o}/{r}/compare/{before}...{after}` |
|---|---|---|
| Ancestry | `status`: `ahead`, `identical`, `behind`, `diverged` | none; each commit has `parents` |
| Files | `files`: the tree diff merge-base..after, renames with `previous_filename`, at most 300, no paging for files | `files`: the concatenation of each listed commit's own files; no tree diff |
| Commits | paged (`per_page`); files only on page 1 | every commit in merge-base..after, no paging |

Forgejo has no API for the tree diff of two arbitrary commits. A tag push
is an `Event::Tag` and has no list; that does not change.

## Decision

### 1. Asked lazily, once per delivery

A push's paths start as **not fetched**, as a pull request's do today. The adapter still
reads three facts from the payload without a call: `after` all zeros (deleted), `before`
all zeros (new branch), `forced: true` (GitHub). Each stays unknown with its current
reason, and no request is made.

Otherwise the server asks the forge only when a candidate Pipeline's matching trigger, or
one of its run-scope Step `when:`s, reads `event.changed_paths`: the same rule, the same
`DeliveryTrigger::decide` path and the same 5-second limit
(`PULL_REQUEST_FILES_TIMEOUT_SECS`) the pull request files request uses. One delivery makes
at most one compare request. A repository whose Pipelines do not read the field makes
none. `Event::Push` gains `before` (serde default) so the server can ask.

The answer replaces the payload's list. The payload's per-commit lists are no longer used
for a push's paths, so `Truncated` and `NoCommits` stop occurring for pushes: a push of
more than 2048 commits on GitHub, or more than the few Forgejo lists, now gets a list.

### 2. `ForgePort::push_changed_paths(repo, before, after)`

A new port method returns `ChangedPaths`. The default implementation returns
`Unknown(ForgeError)`.

**GitHub.** One request: `compare/{before}...{after}?per_page=1` (one commit per page; the
file list is on page 1). `status` `ahead` is a fast-forward and the list is `files`, each
entry's `filename` and, for a rename, `previous_filename`. `identical` is a known empty
list. `behind` and `diverged` are `Unknown(Forced)`. 300 entries in `files` is
`Unknown(TooMany)`, because GitHub stops there without saying so. A response with no
`files` array is `Unknown(ForgeError)`.

**Forgejo.** One request: `compare/{before}...{after}?verification=false` (signature checks
are the expensive part and are not needed). From `after`, follow each commit's first parent
through the listed commits:

- The walk reaches `before`: a fast-forward with `before` on the first-parent chain. The
  list is the union of the files of the commits on that chain. Each such commit's files are
  its diff against its first parent, so the union contains every path that differs between
  `before` and `after`, merge resolutions included. It can also hold a path that changed
  and changed back.
- The walk leaves the listed commits without reaching `before`, and some listed commit has
  `before` as a later parent: `Unknown(MergedHead)`, reason "the push merged the branch's
  earlier head as a second parent".
- Otherwise: `Unknown(Forced)`.

3,000 or more paths is `Unknown(TooMany)` (`CHANGED_PATHS_MAX`), as today.

### 3. Failure is unknown, never the payload list

A forge error, a rate limit that outlasts the adapter's retries, a 404 (an object the forge
no longer has), a malformed response or the 5-second limit gives `Unknown(ForgeError)` or
`Unknown(TimedOut)`. The `when:` counts as true and the Run's activity log names the reason.
Falling back to the payload list would bring back both bugs without a note.

The texts of `ForgeError` and `TimedOut` become forge-neutral, since they now cover two
requests: "the forge did not answer the changed files request" and "the forge did not list
the changed files within 5 seconds".

### 4. Cost

At most one request per push delivery, only in repositories that filter on paths, beside
the config reads each push already makes. On GitHub it counts against the installation's
hourly limit (5,000 or more). On Forgejo the response grows with the number of commits
pushed (every commit, with its files); a push large enough to take more than 5 seconds is
`TimedOut`, and its Runs start.

## Consequences

- A Forgejo forced push no longer yields a partial list; it is unknown, as on GitHub. The
  docs line "forced-push detection is GitHub only" goes.
- Merge resolutions are in the list on both forges. A Forgejo push that merges the old head
  as a second parent is unknown, so its path-filtered Pipelines run.
- Pushes that list too many commits for the payload now get a list.
- A forge outage makes path-filtered Pipelines run on every push, each with a note.
- Pipeline docs (`usage/pipeline-syntax.mdx`, Changed files) and the CHANGELOG change.

## Rejected options

- **Payload list as a fallback when the compare fails.** Silently wrong in exactly the cases
  this ADR fixes.
- **Ask on every push.** A network call per delivery for repositories that never filter on
  paths.
- **A list for forced pushes** (union of `merge-base...before` and `merge-base...after`).
  Two requests, and GitHub's 300-file cap on each. Forced pushes stay unknown for now.
- **New branch compared with the default branch.** That answers "what does this branch
  change", a pull request question, against a ref that moves. A new branch stays unknown.
- **Forgejo: union of every listed commit for a second-parent merge.** Misses a merge that
  drops the old head's change to a file the other side did not touch (§Context).
- **Forgejo per-commit `git/commits/{sha}.diff`.** One request per commit, and it is still
  a first-parent diff.

## Test plan

No network in tests. Fixtures are recorded once, from GitHub and from the Forgejo 16.0.1 in
`deploy/kind/forgejo.yaml`, and committed with a note of where each came from.

- **Adapter parse functions** (pure, like `normalize`), in each adapter's
  `tests/changed_paths.rs`:
  - GitHub `ahead` with a merge commit whose resolution file is in `files` and in no
    commit of the matching `push.json`: the path is listed. A rename lists both names.
  - GitHub `diverged`: `Unknown(Forced)`. 300 files: `Unknown(TooMany)`. No `files`:
    `Unknown(ForgeError)`.
  - Forgejo fast-forward with a merge commit on the first-parent chain: the resolution file
    is listed. Forced (old head unreachable): `Unknown(Forced)`. Old head as a merge's
    second parent: `Unknown(MergedHead)`. 3,000 paths: `Unknown(TooMany)`.
  - The request URL each adapter builds (`per_page=1`; `verification=false`).
- **Server** (`crates/scarab-server/tests/changed_paths.rs`, FakeForge counting calls): a
  push whose payload misses the filtered directory but whose compare answer has it starts the
  Run; no Pipeline reading the field makes zero calls; two Pipelines reading it make one; a
  compare error and a slow compare each start the Run with the note naming the reason; new
  branch, deleted branch and a GitHub `forced: true` payload make no call.
- **Live** (`just test e2e changed_paths`, the Forgejo in the kind cluster): a merge with a
  conflict resolved in a filtered directory starts the Pipeline; a force push starts it with
  the "forced push" note.
