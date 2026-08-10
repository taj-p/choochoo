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

## Command reference

| Command | What it does |
|---|---|
| `choo init <name> [--base main]` | Create a new (empty) train. |
| `choo list` | List every train in this repo. |
| `choo show [<name>]` | Show a train's branches and PRs. |
| `choo switch <name>` | Set the active train. |
| `choo add [<branch>] [-t <train>]` | Append a branch (default: current) to a train. |
| `choo remove <branch> [-t <train>]` | Drop a branch from a train (does not delete the git branch). |
| `choo move <branch> --before <other>` | Move a branch within a train. Use `--after` for the other direction. |
| `choo checkout <branch> [-t <train>]` | Check out a branch in the train. |
| `choo rebase [-t <train>]` | Restack the whole train onto the current base. |
| `choo rebase --continue` | Resume after resolving conflicts and running `git rebase --continue`. |
| `choo rebase --abort` | Cancel an in-progress rebase. |
| `choo push [-t <train>] [--without-lease] [--no-force-with-lease] [--remote origin]` | Push every branch with `--set-upstream` so each branch tracks its remote. Default: `git push --force-with-lease`. `--without-lease` uses `git push --force` (no lease check). `--no-force-with-lease` uses plain `git push` (no force at all). |
| `choo pr [-t <train>] [--draft]` | Create or update one PR per branch and sync the train table on every PR. |
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

## Mental model

A **train** is just a name + a base branch + an ordered list of git branches
stored in `.git/choochoo/state.json`. choochoo never invents commits or
moves branches behind your back — it always uses your local `git` for git
operations and your local `gh` for GitHub operations.

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

## Testing

```bash
cargo test
```

Tests are hermetic — no network, no `gh` auth required:

* **Unit tests** in every module exercise pure logic (state validation,
  table rendering, list manipulation, rebase planning, the TUI app state
  machine).
* **Integration tests** under `tests/` run the real `choo` binary inside
  temporary `git init`'d repos. The `pr` command is exercised against an
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

## License

MIT OR Apache-2.0
