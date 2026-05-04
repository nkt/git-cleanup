mod cli;
mod git;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::Args;
use git::Filter;
use owo_colors::OwoColorize;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{}: {e:#}", "error".red().bold());
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode> {
    let args = Args::parse();
    let repo = git::open()?;

    if !args.no_fetch {
        if args.verbose {
            eprintln!("{} git fetch --prune {}", "·".dimmed(), args.remote);
        }
        if let Err(e) = git::fetch_prune(&args.remote) {
            eprintln!("{} {e}", "warning:".yellow().bold());
        }
    }

    let main_name = git::detect_main(&repo, &args.remote, args.main.as_deref())?;

    let filter = Filter {
        merged: !args.no_merged,
        squashed: !args.no_squashed,
        gone: !args.no_gone,
    };

    if !filter.merged && !filter.squashed && !filter.gone {
        eprintln!("{}: all detection categories disabled; nothing to do.", "note".yellow());
        return Ok(ExitCode::SUCCESS);
    }

    let candidates = git::categorize(&repo, &main_name, filter)?;

    let dry_run = !args.force;
    ui::print_summary(&candidates, &main_name, dry_run);

    if candidates.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    let to_delete: Vec<String> = if args.force {
        candidates.iter().map(|c| c.name.clone()).collect()
    } else if args.yes {
        if !ui::confirm_delete(candidates.len())? {
            println!("{}", "aborted.".dimmed());
            return Ok(ExitCode::SUCCESS);
        }
        candidates.iter().map(|c| c.name.clone()).collect()
    } else {
        if !ui::is_interactive() {
            eprintln!(
                "\n{}: not running on a TTY. Pass {} to delete all, or {} to skip the picker.",
                "note".yellow(),
                "--force".bold(),
                "--yes".bold()
            );
            return Ok(ExitCode::from(1));
        }
        println!();
        let picked = ui::select(&candidates)?;
        if picked.is_empty() {
            println!("{}", "nothing selected.".dimmed());
            return Ok(ExitCode::SUCCESS);
        }
        let chosen: Vec<String> =
            picked.iter().map(|&i| candidates[i].name.clone()).collect();
        if !ui::confirm_delete(chosen.len())? {
            println!("{}", "aborted.".dimmed());
            return Ok(ExitCode::SUCCESS);
        }
        chosen
    };

    println!();
    let results = git::delete_branches(&repo, &to_delete);
    ui::report_deletions(&results);

    let any_failed = results.iter().any(|(_, err)| err.is_some());
    Ok(if any_failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}
