# git-cleanup

Prune merged, squashed, and gone branches from a git repository.

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
| `--no-fetch` | don't run `git fetch --prune` first |
| `-f`, `--force` | delete every candidate without prompting |
| `-y`, `--yes` | accept the full list (skip the picker, still confirms) |
| `--main <BRANCH>` | override main-branch detection |
| `--remote <NAME>` | remote to fetch from (default `origin`) |
| `-v`, `--verbose` | per-branch reasoning |

The currently checked-out branch and the resolved main branch are always
protected.

## How it decides

- **merged** — branch tip is an ancestor of the main branch.
- **squashed** — patch-id of `merge_base..branch` matches a patch-id of
  some commit in main.
- **gone** — branch has an upstream configured, but the upstream ref no
  longer exists.

Main is detected from `refs/remotes/<remote>/HEAD`, falling back to
`main`, `master`, `trunk`, `develop`.

## License

MIT
