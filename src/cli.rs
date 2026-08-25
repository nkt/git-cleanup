use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "git-cleanup",
    version,
    about = "Prune merged, squashed, and gone branches from a git repository"
)]
pub struct Args {
    /// Skip merged-branch detection
    #[arg(long)]
    pub no_merged: bool,

    /// Skip squashed-branch detection
    #[arg(long)]
    pub no_squashed: bool,

    /// Skip gone-branch detection
    #[arg(long)]
    pub no_gone: bool,

    /// Also detect abandoned branches (tip older than --abandoned-days).
    /// Off by default: these may hold unmerged work
    #[arg(long)]
    pub abandoned: bool,

    /// Age threshold in days for the abandoned category
    #[arg(long, value_name = "DAYS", default_value_t = 90)]
    pub abandoned_days: u32,

    /// Max main-history commits (newest first) scanned for squash matches; 0 = no cap
    #[arg(long, value_name = "COMMITS", default_value_t = 10_000)]
    pub squash_scan_limit: u32,

    /// Read-only scan: print candidates and exit 0, never prompt or delete
    #[arg(long, conflicts_with_all = ["force", "yes"])]
    pub list: bool,

    /// Machine-readable JSON output (implies --list)
    #[arg(long, conflicts_with_all = ["force", "yes"])]
    pub json: bool,

    /// Delete the named branches (no scan, no prompt) and exit
    #[arg(
        long,
        value_name = "BRANCH",
        num_args = 1..,
        conflicts_with_all = ["list", "json", "force", "yes"]
    )]
    pub delete: Vec<String>,

    /// Remove the named worktrees (git worktree remove; refuses dirty or
    /// locked ones) and exit; combines with --delete
    #[arg(
        long,
        value_name = "PATH",
        num_args = 1..,
        conflicts_with_all = ["list", "json", "force", "yes"]
    )]
    pub remove_worktree: Vec<String>,

    /// Don't run `git fetch --prune` before scanning
    #[arg(long)]
    pub no_fetch: bool,

    /// Delete every candidate without any prompt
    #[arg(short, long)]
    pub force: bool,

    /// Accept the full candidate list (skip multi-select, still confirms)
    #[arg(short, long)]
    pub yes: bool,

    /// Override main-branch detection
    #[arg(long, value_name = "BRANCH")]
    pub main: Option<String>,

    /// Remote name for fetch and gone detection
    #[arg(long, value_name = "NAME", default_value = "origin")]
    pub remote: String,

    /// Per-branch reasoning
    #[arg(short, long)]
    pub verbose: bool,

    /// Print structured timing + diagnostic logs to stderr
    #[arg(long)]
    pub debug: bool,
}
