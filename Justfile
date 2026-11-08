# Scarab dev harness. `just` lists every recipe; the long notes sit next to
# each recipe in this file and in the modules under just/.
#
#   just stack up     # kind + Postgres + MinIO + Forgejo + the chart, server built on the host
#   just stack demo   # submit a pipeline and watch it complete
#   just stack down   # tear it all down
#   just test         # the Rust suite and the UI no-DOM suite
#
# Requires: docker, kind, kubectl, helm, cargo (+ cargo-zigbuild and zig on
# macOS), and python3 for the demo.
#
# Requires just 1.43.0 or later: the modules use the `[default]` attribute
# (added in 1.43.0) and `set working-directory` (added in 1.33.0). An older
# just fails to parse just/*.just.

[private]
[doc('List every recipe, modules included')]
default:
    @just --justfile {{quote(justfile())}} --list --list-submodules --unsorted

# The kind dev stack, the public demo and the Depot: just/stack.just.
[doc('The local stack: up, down, reload, logs, status, demo')]
mod stack 'just/stack.just'

# The test tiers, from the workspace suite to the live and fault tiers:
# just/test.just.
[doc('Test suites: Rust + UI by default; rust, ui, e2e, chaos, coverage')]
mod test 'just/test.just'

# What CI checks besides the tests: just/check.just.
[doc('cargo check; `all` adds the CI gates; `conformance` runs the CEL cases')]
mod check 'just/check.just'

# `just ui` runs the web UI dev server (Vite on http://localhost:5173, proxies
# /v1 to the server). Env (incl. SCARAB_API_URL, which vite refuses to guess)
# comes from the one dev env file, deploy/kind/.env, not wired inline here.
# Point elsewhere by editing that file.
#
# `just ui mock` runs it against a built-in fixture (no server, no DB), the
# fastest way to look at UI changes. It serves a fixed "acme" org
# (dashboard/run-detail/env/secrets) with the dark theme forced. See
# ui/scarab-web-ui/src/mock.ts.
[doc('Web UI dev server against the kind stack; `mock` uses a built-in fixture')]
ui what="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{what}}" in
      "")
        npm --prefix ui/scarab-web-ui install
        [ -f deploy/kind/.env ] || { echo "==> creating deploy/kind/.env from .env.example"; cp deploy/kind/.env.example deploy/kind/.env; }
        set -a && source deploy/kind/.env && set +a
        npm --prefix ui/scarab-web-ui run dev
        ;;
      mock)
        npm --prefix ui/scarab-web-ui install
        VITE_SCARAB_MOCK=1 npm --prefix ui/scarab-web-ui run dev
        ;;
      *) echo "usage: just ui [mock]" >&2; exit 2 ;;
    esac

# `just docs` runs the docs site dev server (Astro Starlight; ADR-0040).
# `just docs build` builds it once, the way docs.yml does (npm ci + astro build
# into ui/scarab-docs-ui/dist). The prebuild step syncs CONTEXT.md and docs/adr.
#
# `just docs screenshots` recaptures every committed screenshot (the README's
# and the docs site's) from the mock UI with Playwright
# (ui/scarab-web-ui/scripts/screenshots.mjs). Look at the images before you
# commit them. `just docs screenshots check` only compares a fresh capture with
# the committed files, as `just release` does; diffs go to
# target/release-screenshots/.
[doc('Docs site dev server; `build` builds it the way docs.yml does; `screenshots [check]` recaptures the UI screenshots')]
docs what="" *args:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{what}}" in
      "")
        npm --prefix ui/scarab-docs-ui install
        npm --prefix ui/scarab-docs-ui run dev
        ;;
      build)
        npm --prefix ui/scarab-docs-ui ci
        npm --prefix ui/scarab-docs-ui run build
        ;;
      screenshots)
        case "{{args}}" in
          ""|check) ;;
          *) echo "usage: just docs screenshots [check]" >&2; exit 2 ;;
        esac
        npm --prefix ui/scarab-web-ui install --no-audit --no-fund
        npx --prefix ui/scarab-web-ui playwright install chromium
        node ui/scarab-web-ui/scripts/screenshots.mjs {{args}}
        ;;
      *) echo "usage: just docs [build | screenshots [check]]" >&2; exit 2 ;;
    esac

# `just brand` regenerates the beetle mark's SVGs in ui/brand from the three
# traced masters in ui/brand/mark (generate.py, python3, standard library only).
# `just brand check` fails if the SVGs on disk differ from what the generator
# produces. `just brand png` re-renders the 37 PNG icons in a browser: it opens
# a tab that draws each SVG at every size, serve_png.py saves the files and
# exits when the page reports it is done, then favicon.ico is rebuilt from the
# favicon PNGs. Re-tracing the masters from the artwork (mark/trace.py) needs
# numpy, opencv and pillow plus potrace, so it is not a recipe step.
[doc('Regenerate the beetle mark SVGs; `check` compares them; `png` re-renders the PNG icons')]
brand what="":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{what}}" in
      "") python3 ui/brand/mark/generate.py ;;
      check) python3 ui/brand/mark/generate.py --check ;;
      png)
        python3 ui/brand/mark/serve_png.py
        python3 ui/brand/mark/generate.py --ico
        ;;
      *) echo "usage: just brand [check | png]" >&2; exit 2 ;;
    esac

# `just release <version>` checks a release before you tag v<version>
# (docs/RELEASING.md): Cargo.toml and Chart.yaml name <version>, libs/clone.yaml
# pins an earlier scarab-clone build by digest, CHANGELOG.md has the version's
# section (it prints the Release text release.yml will write), every Features
# entry links a docs page, and a fresh capture of the mock UI matches every
# committed screenshot (`just docs screenshots` recaptures them). It runs every
# check and ends with a PASS/FAIL list. See scripts/release_check.sh.
[doc('Check a release before tagging v<version>')]
release version:
    bash scripts/release_check.sh {{quote(version)}}

# Merge gate: are PR <n>'s REQUIRED checks green? Run this before merging.
# Branch protection and rulesets are both unavailable on this repo (private,
# free plan → 403), so this is the enforcement, not a convenience: the required
# set lives in .github/required-checks.txt. Exits 8 while checks still run.
# Fails while the PR has the ci:skip label (README: "CI labels on pull requests").
[doc("Merge gate: are PR <n>'s required checks green?")]
pr n:
    python3 scripts/pr_gate.py {{n}}

# Reclaim build-cache disk. Cargo NEVER garbage-collects `target/`: every
# fingerprint change (branch switch, dep bump, feature flag) writes a new
# artifact and keeps the old one forever. This checkout reached 100 GB — 610k
# files in target/debug/deps, 88k of them stale `scarab_server-*` variants —
# purely from two weeks of branch-hopping.
#
# `just clean` prints where the disk goes (free space, target/ here and in each
# git worktree), prunes artifacts untouched for <days> (default 7) across this
# checkout AND any git worktrees, and prints the same report again. That keeps
# target/ low WITHOUT the full rebuild that `cargo clean` costs. Run it when
# disk gets tight, or on a cron.
#
# Needs cargo-sweep: cargo install cargo-sweep
[doc('Prune build artifacts older than <days> here and in every worktree')]
clean days="7":
    #!/usr/bin/env bash
    set -euo pipefail
    report() {
      echo "== free =="; df -h /System/Volumes/Data 2>/dev/null | tail -1 || df -h .
      echo "== target/ in this checkout =="
      du -sh target 2>/dev/null || echo "  (none)"
      du -sh target/debug/deps target/debug/incremental 2>/dev/null || true
      echo "== target/ in git worktrees =="
      git worktree list --porcelain | awk '/^worktree /{print $2}' | while read -r w; do
        [ -d "$w/target" ] && du -sh "$w/target" 2>/dev/null || true
      done
    }
    echo "==> before"; report
    if ! command -v cargo-sweep >/dev/null 2>&1; then
      echo "error: cargo-sweep not installed — run: cargo install cargo-sweep" >&2
      exit 1
    fi
    # --recursive so sibling worktrees under .workspaces/ and .claude/worktrees/
    # are swept too; they are the reason the total multiplies.
    cargo sweep --recursive --time {{days}} .
    echo "==> after"; report
