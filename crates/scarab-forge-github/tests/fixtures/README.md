# GitHub fixtures

| File | Source |
|---|---|
| `push.json`, `push_new_branch.json`, `push_forced.json` | push webhook payloads in the shape GitHub sends (git-bug 418f55e) |
| `push_merge.json` | the push payload of a merge whose conflict resolution changed `infra/main.tf`; no commit lists that file (git-bug 0354b5a) |
| `compare_ahead_merge.json` | the answer of `GET /repos/acme/app/compare/{before}...{after}?per_page=1` for the same push (ADR-0081) |

`push_merge.json` and `compare_ahead_merge.json` are **hand-written** from
GitHub's documented REST shapes, not recorded. To record them, push a merge
with a conflict resolved in a file neither side's commits list, save the push
delivery from the repository's webhook settings as `push_merge.json`, and save
the compare answer with:

```sh
gh api "repos/<owner>/<repo>/compare/<before>...<after>?per_page=1" > compare_ahead_merge.json
```

The `behind`, `diverged`, `identical`, 300-file, empty-`files` and
no-`files` cases are edits of `compare_ahead_merge.json` made inside
`tests/changed_paths.rs`, next to their assertions.
