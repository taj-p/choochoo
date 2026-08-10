# choochoo

`choochoo` (binary `choo`) is a small CLI + TUI for managing **PR trains** on
GitHub: a stacked sequence of git branches where each branch's PR targets the
prior branch, so a large change set can be reviewed and merged a piece at a
time.

```
main ── feat/part-1 ── feat/part-2 ── feat/part-3
          PR #100        PR #101        PR #102
        (base: main)   (base: part-1) (base: part-2)
```

Every PR's description gets the same train table, with a "this PR" marker on
its own row, so reviewers always see where the change fits in the overall
plan.

Optionally, a train can also have an **aggregate ("combined") branch**: one
extra branch holding *all* of the train's changes, with its own draft PR
against the base branch, for reviewers who want to see the whole change at
once (and for CI that only runs against the default branch).

```
main ── feat/part-1 ── feat/part-2 ── feat/part-3
  │       PR #100        PR #101        PR #102
  └────────────────────────────────── choo/my-feature/combined
                                        draft PR #103 (base: main)
```

## Install

```bash
cargo install --path .
# or, from the repo root:
cargo build --release
./target/release/choo --help
```

`choo` requires `git` and `gh` on `PATH`. GitHub authentication is reused
from your existing `gh auth login`.

## Quickstart

```bash
# Create a new train based off `main`.
choo init my-feature

# Build a stack of branches.
git checkout -b feat/part-1 main
# ... commit ...
choo add                       # adds the current branch to the active train

git checkout -b feat/part-2 feat/part-1
# ... commit ...
choo add

git checkout -b feat/part-3 feat/part-2
# ... commit ...
choo add

# Open one PR per branch (idempotent: re-run after each push).
choo pr

# Rebase the whole train when `main` advances.
git fetch origin && git checkout main && git pull
choo rebase

# Push the entire stack.
choo push

# Browse trains interactively.
choo tui
```

### With a combined branch

```bash
# At creation time...
choo init my-feature --aggregate

# ...or on an existing train.
choo aggregate enable                    # branch: choo/my-feature/combined
choo aggregate enable --branch all-of-it  # or pick your own name

# From here nothing changes: push and pr handle the combined branch too.
choo push                # pushes the train, then the combined branch
choo pr                  # opens/updates the PRs, plus a draft combined PR
choo rebase              # restacks the train, then re-points the combined branch

choo aggregate sync      # or re-point it explicitly, without pushing
choo aggregate disable   # stop managing it (branch and PR are left alone)
```

## Command reference

| Command | What it does |
|---|---|
| `choo init <name> [--base main] [--aggregate] [--aggregate-branch <b>]` | Create a new (empty) train. `--aggregate` also manages a combined branch (default name `choo/<name>/combined`). |
| `choo list` | List every train in this repo. |
| `choo show [<name>]` | Show a train's branches and PRs. |
| `choo switch <name>` | Set the active train. |
| `choo add [<branch>] [-t <train>]` | Append a branch (default: current) to a train. |
| `choo remove <branch> [-t <train>]` | Drop a branch from a train (does not delete the git branch). |
| `choo move <branch> --before <other>` | Move a branch within a train. Use `--after` for the other direction. |
| `choo checkout <branch> [-t <train>]` | Check out a branch in the train. |
| `choo rebase [-t <train>]` | Restack the whole train onto the current base, then re-point the combined branch (if enabled). |
| `choo rebase --continue` | Resume after resolving conflicts and running `git rebase --continue`. |
| `choo rebase --abort` | Cancel an in-progress rebase. |
| `choo push [-t <train>] [--without-lease] [--no-force-with-lease] [--remote origin]` | Push every branch with `--set-upstream` so each branch tracks its remote. Default: `git push --force-with-lease`. `--without-lease` uses `git push --force` (no lease check). `--no-force-with-lease` uses plain `git push` (no force at all). |
| `choo pr [-t <train>] [--draft]` | Create or update one PR per branch and sync the train table on every PR. Also opens/updates the combined branch's draft PR (if enabled). |
| `choo aggregate enable [--branch <b>] [-t <train>]` | Start managing a combined branch for the train, and sync it now. |
| `choo aggregate disable [-t <train>]` | Stop managing it. The git branch and its PR are left untouched. |
| `choo aggregate sync [-t <train>]` | Re-point the combined branch at the train's current tip. |
| `choo tui` | Launch the interactive UI. |

`-t/--train` defaults to the **active** train (set by `choo init` for the
first train, or `choo switch <name>`).

## TUI keys

| Key | Action |
|---|---|
| `j` / `k` or arrows | Move selection |
| `Enter` / `l` | Drill into a train |
| `Esc` / `h` | Back to the trains list |
| `J` / `K` | Reorder a branch within a train |
| `o` | `git checkout` the selected branch |
| `R` | Rebase the selected train |
| `P` | Push the selected train |
| `O` | Create/update PRs for the selected train |
| `q` or `Ctrl-C` | Quit |

A train's combined branch (if enabled) is listed after its branches as a
dimmed `Σ` row. It isn't selectable — `R` / `P` / `O` keep it in sync for
you, and it's configured from the CLI (`choo aggregate ...`).

## Mental model

A **train** is just a name + a base branch + an ordered list of git branches
stored in `.git/choochoo/state.json`. choochoo never invents commits or
moves branches behind your back — it always uses your local `git` for git
operations and your local `gh` for GitHub operations. The one branch it does
move on its own is the opt-in [combined branch](#the-combined-branch), which
exists only to mirror the train's tip.

The rebase algorithm is the standard stacked-rebase recipe:

1. Snapshot every branch's tip SHA before starting.
2. For each `(parent, child)` pair, walk in order and run
   `git rebase --onto <new parent tip> <pre-rebase parent tip> <child>`.
3. On conflict, leave you mid-rebase, persist progress under
   `.git/choochoo/rebase-progress.json`, and instruct you to run
   `choo rebase --continue` after resolution.

`choo pr` is idempotent: it looks up existing PRs by head branch, only
creates ones that don't exist yet, then re-renders every PR description so
the train table is consistent across the stack.

choochoo owns one contiguous region of each PR body, delimited by
`<!-- choochoo:train:start ... -->` and `<!-- choochoo:train:end -->`
markers. **Anything you write outside that region (above or below it)
is preserved verbatim** across every re-run. For PRs created outside
choochoo (no markers), the managed block is appended to the bottom so
your description stays prominent at the top.

Older choochoo versions used a different marker scheme; on first sync the
new layout is migrated automatically, including any prose you'd written
above the old `<!-- choochoo:train ... -->` header.

## The combined branch

A train with an aggregate branch keeps one extra branch — by default
`choo/<train>/combined` — that choochoo force-updates to the **tip of the
train** (its last branch). Since every branch is stacked on the one before
it, the tip's diff against the base *is* the union of every change in the
train, so:

* there's no merge or squash commit to maintain, and nothing to re-resolve
  after a rebase — `choo rebase` just re-points the branch at the new tip;
* the combined PR's diff always equals "the whole train";
* CI that only triggers on PRs against the default branch gets to see the
  complete change.

The combined branch is **derived state that choochoo owns**: it is
force-moved, so don't commit to it. If it's the branch you have checked
out and it needs to move, choochoo refuses rather than touching your
working tree — check out something else and re-run.

It is refreshed by the commands that touch git — `choo push`, `choo rebase`,
`choo aggregate enable`, and `choo aggregate sync`. `choo add` / `remove` /
`move` only edit choochoo's own metadata, as they always have, so a train
whose shape you just changed gets a fresh combined branch on your next
`choo push` (or right away with `choo aggregate sync`).

Its PR is **always a draft** and **always targets the train's base**
(normally your default branch), even when the per-branch PRs aren't drafts.
It's a review-and-CI artifact: merge the individual PRs, then close it (or
let GitHub close it once the base has all the commits). Every PR's train
table gains a `Σ` row for it, with a legend explaining what it is.

`choo aggregate disable` only makes choochoo forget the branch — the git
branch and its PR are left exactly as they are, the same way `choo remove`
never deletes branches. The aggregate branch can't be added to its own
train with `choo add`.

## Testing

```bash
cargo test
```

Tests are hermetic — no network, no `gh` auth required:

* **Unit tests** in every module exercise pure logic (state validation,
  table rendering, list manipulation, rebase planning, the TUI app state
  machine).
* **Integration tests** under `tests/` run the real `choo` binary inside
  temporary `git init`'d repos — including `cli_aggregate.rs`, which checks
  the combined branch really does carry every file the train touches. The `pr` command is exercised against an
  in-process `FakeGh` selected by the `CHOOCHOO_GH_FAKE` environment
  variable, which records all calls in a JSON file the tests can read.

To accept new snapshot output:

```bash
INSTA_UPDATE=always cargo test
```

## Limitations

* **State is local-only.** `.git/choochoo/state.json` lives inside `.git/`
  and isn't shared across machines or teammates. Bumping the schema's
  `version` field is the migration path; sharing via `git notes` is a
  future option.
* **Conflicts aren't auto-resolved.** When `choo rebase` hits a conflict it
  hands off to you and waits for `choo rebase --continue`.
* **GitHub-only.** Other forges (GitLab, Bitbucket) aren't supported.
* **Single base per train.** Branching trees with multiple roots aren't
  modelled.
* **The combined branch mirrors, it doesn't squash.** Its PR shows every
  commit in the train, not one collapsed commit, and it targets the train's
  base — which is the repo default branch in the usual setup, but is
  whatever you passed to `choo init --base`.

## License

MIT OR Apache-2.0
