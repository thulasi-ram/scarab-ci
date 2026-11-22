# Forgejo fixtures

| File | Source |
|---|---|
| `push.json`, `push_new_branch.json` | push webhook payloads in the shape Forgejo sends (git-bug 418f55e) |
| `compare_ff_merge.json`, `compare_forced.json`, `compare_pull_merge.json` | answers of `GET /repos/{o}/{r}/compare/{before}...{after}?verification=false` (ADR-0081) |
| `compare_ids.json` | the `before` and `after` of each compare, and where the answers came from |

The compare answers are recorded from the Forgejo of the kind stack
(`deploy/kind/forgejo.yaml`); `recorded_from` in `compare_ids.json` names the
version and the date. The top-level `files` of an answer lists every commit's
files one after another, duplicates included: it is not a tree diff, and the
adapter does not read it. Re-record with:

```sh
just stack up                 # if the stack is not running
just test record-compare      # deploy/kind/record-compare.sh
```

The script builds the three pushes with git, asks the compare API as the
adapter does, and rewrites the four `compare_*` files. The tests read
`before` and `after` from `compare_ids.json` and find commits by their message,
so a recording changes only ids and timestamps. If a test fails after a
recording, the recording shows how Forgejo answers; fix the adapter, not the
fixture.
