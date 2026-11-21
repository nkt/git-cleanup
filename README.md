# git-cleanup

Prune merged, squashed, and gone branches — and the stale worktrees
holding them — from a git repository.

## Install

```sh
cargo install --path .
```

The binary lands in `~/.cargo/bin/git-cleanup`. Once that directory is on
`PATH`, git picks it up as the `cleanup` subcommand.

To enable `git cleanup --help`:

```sh
mkdir -p ~/.local/share/man/man1
cp man/git-cleanup.1 ~/.local/share/man/man1/
```

## Usage

```
git cleanup [OPTIONS]
```

Default flow: fetch with `--prune`, scan local branches, print a summary,
prompt for which to delete.

| flag | effect |
| --- | --- |
| `--no-merged` | skip merged-branch detection |
| `--no-squashed` | skip squashed-branch detection |
| `--no-gone` | skip gone-branch detection |
| `--no-contained` | skip contained detection |
| `--superseded` | also flag branches squash-merged and then edited on main (heuristic, so opt-in) |
| `--superseded-pct <PCT>` | share of changed lines that must be in main for `--superseded` (default 90) |
| `--superseded-days <DAYS>` | minimum tip age for `--superseded` (default 3) |
| `-r`, `--recursive` | scan every repository under the current directory, clean them in one pass |
| `--max-depth <LEVELS>` | how deep `--recursive` looks for repositories (default 3) |
| `--abandoned` | also flag branches untouched for `--abandoned-days` (may hold unmerged work, so opt-in) |
| `--abandoned-days <DAYS>` | age threshold for `--abandoned` (default 90) |
| `--list` | read-only scan: print candidates and exit, never prompt or delete |
| `--json` | machine-readable output (implies `--list`) |
| `--delete <BRANCH>...` | delete the named branches directly, no scan or prompt |
| `--remove-worktree <PATH>...` | remove the named worktrees directly (combines with `--delete`) |
| `--squash-scan-limit <COMMITS>` | cap on main-history commits scanned for squash matches (default 10000, 0 = no cap) |
| `--no-fetch` | don't run `git fetch --prune` first |
| `-f`, `--force` | delete every candidate without prompting |
| `-y`, `--yes` | accept the full list (skip the picker, still confirms) |
| `--main <BRANCH>` | override main-branch detection (not with `--recursive`) |
| `--protect <BRANCH>` | additional protected branches (repeatable) |
| `--remote <NAME>` | remote to fetch from (default `origin`) |
| `-v`, `--verbose` | also list kept branches with the reason |
| `--debug` | structured timing + diagnostic logs on stderr |

The currently checked-out branch, the resolved main branch, and
`main`/`master`/`develop`/`trunk` are always protected. Add your own
with `--protect` or persistently via git config (multi-valued and
comma-separated both work):

```sh
git config cleanup.protect "release, staging"
```

## Worktrees

The scan also covers linked worktrees: prunable ones (directory gone),
worktrees whose checked-out branch is itself a candidate, and detached
worktrees whose HEAD is merged, contained or (with `--superseded`)
superseded. Removing such a worktree also deletes its freed branch.
Worktrees that are dirty, locked, or contain the directory git-cleanup
was started from are shown but never removed.

Every deletion prints the commit it pointed at (`✓ feat/x (was a1b2c3d)`),
so `git branch feat/x a1b2c3d` brings it back until git garbage-collects
the commit.

## Recursive mode

```sh
cd ~/Projects/acme && git cleanup -r --list --superseded
```

`-r` finds every repository up to `--max-depth` levels below the current
directory (a directory holding a `.git` directory; linked worktrees and
submodules are reached through their main repository, hidden directories
and `node_modules` are skipped), fetches them in parallel, scans each one
and prints a section per repository with something to clean. One picker
and one confirmation cover all of them; `--yes`, `--force`, `--list` and
`--json` work as in a single repository. A repository that cannot be
scanned (no main branch, say) is reported and skipped, and makes the exit
code 1.

## How it decides

- **merged** — branch tip is an ancestor of the main branch.
- **squashed** — patch-id of `merge_base..branch` matches a patch-id of
  some commit in main.
- **contained** — merging the branch into main would change nothing:
  `git merge-tree --write-tree` gives main's own tree. Catches branches
  whose work reached main some other way (a squash of a larger PR, a
  cherry-pick) as long as main has not edited those lines since.
- **superseded** (only with `--superseded`) — squash-merged, then edited
  on main. Every file the branch adds exists in main, at least
  `--superseded-pct` of its changed lines are in main's current version
  (an added line is found, a removed line is gone; lines shorter than four
  characters and lock files are not counted), and the tip is at least
  `--superseded-days` old. The detail says how many lines differ and in
  which files. It is a heuristic: a branch whose PR was closed and redone
  elsewhere can pass it, so read the list before confirming.
- **gone** — branch has an upstream configured, but the upstream ref no
  longer exists.
- **abandoned** (only with `--abandoned`) — branch tip is older than the
  threshold. The summary shows how many unmerged commits it carries.

Main is detected from `refs/remotes/<remote>/HEAD`, falling back to
`main`, `master`, `trunk`, `develop`. The contained and superseded checks
compare against `<remote>/<main>` when it contains local main (it is the
fresh one after a fetch), otherwise against local main.

## Scripting

`git cleanup --list --json` prints one object per candidate:

```json
{
  "main": "main",
  "candidates": [
    {
      "name": "feat/export",
      "category": "squashed",
      "detail": "squashed into main (patch-id a1b2c3d)",
      "tip": "…full oid…",
      "age_days": 12,
      "ahead": null,
      "upstream": "gone",
      "worktree": null
    }
  ]
}
```

`ahead` is the number of commits not reachable from main: `0` means safe
by ancestry, `null` means not computed (`squashed` is already proven by
patch-id equivalence). `upstream` is `tracked`, `gone`, or `none`.

A `worktrees` array sits next to `candidates`, one object per stale
worktree: `path`, `branch` (`null` when detached), `head`, `category`
(the branch's category, `null` for a plain prunable entry), `detail`,
`prunable`, and `blocked` (`null` when removable, otherwise the reason it
is kept).

With `--recursive`, the output is `{"root": …, "repos": [...]}`: one
object per repository with `path`, `main`, `candidates` and `worktrees`
as above, or `path` and `error` when it could not be scanned.

Exit codes: `0` success (including a clean `--list`), `1` some deletions
failed or interactive mode was blocked by a missing TTY, `2` fatal error.

## License

MIT
