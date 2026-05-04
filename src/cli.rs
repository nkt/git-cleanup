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
}
