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
| `--main <BRANCH>` | override main-branch detection |
| `--remote <NAME>` | remote to fetch from (default `origin`) |
| `-v`, `--verbose` | per-branch reasoning |
| `--debug` | structured timing + diagnostic logs on stderr |

The currently checked-out branch, the resolved main branch, and
`main`/`master`/`develop`/`trunk` are always protected.

## Worktrees

The scan also covers linked worktrees: prunable ones (directory gone)
and worktrees whose checked-out branch is itself a candidate. Removing
such a worktree also deletes its freed branch. Worktrees that are dirty,
locked, or contain the current directory are shown but never removed.

## How it decides

- **merged** — branch tip is an ancestor of the main branch.
- **squashed** — patch-id of `merge_base..branch` matches a patch-id of
  some commit in main.
- **gone** — branch has an upstream configured, but the upstream ref no
  longer exists.
- **abandoned** (only with `--abandoned`) — branch tip is older than the
  threshold. The summary shows how many unmerged commits it carries.

Main is detected from `refs/remotes/<remote>/HEAD`, falling back to
`main`, `master`, `trunk`, `develop`.

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
worktree: `path`, `branch`, `category` (the branch's category, `null`
for a plain prunable entry), `detail`, `prunable`, and `blocked` (`null`
when removable, otherwise the reason it is kept).

Exit codes: `0` success (including a clean `--list`), `1` some deletions
failed or interactive mode was blocked by a missing TTY, `2` fatal error.

## License

MIT
