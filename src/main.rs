mod cli;
mod debug;
mod git;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::Args;
use git::Filter;
use owo_colors::OwoColorize;
use std::process::ExitCode;
use std::time::Instant;

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
    let total = Instant::now();
    let args = Args::parse();
    debug::set(args.debug);
    dbg_log!(
        "args: remote={} main={:?} no_fetch={} no_merged={} no_squashed={} no_gone={} abandoned={} list={} json={} delete={:?} force={} yes={}",
        args.remote,
        args.main,
        args.no_fetch,
        args.no_merged,
        args.no_squashed,
        args.no_gone,
        args.abandoned,
        args.list,
        args.json,
        args.delete,
        args.force,
        args.yes,
    );

    let t = Instant::now();
    let repo = git::open()?;
    dbg_log!("repo discovered at {:?} ({:?})", repo.path(), t.elapsed());

    if !args.delete.is_empty() || !args.remove_worktree.is_empty() {
        let mut results: Vec<(String, Option<String>)> = Vec::new();
        // Worktrees first: a branch checked out in a removed worktree
        // becomes deletable in the same run.
        for path in &args.remove_worktree {
            results.push(git::remove_worktree_path(path));
        }
        if !args.delete.is_empty() {
            let main_name =
                git::detect_main(&repo, &args.remote, args.main.as_deref())?;
            let mut to_delete: Vec<String> = Vec::new();
            for name in &args.delete {
                if name == &main_name || git::PROTECTED.contains(&name.as_str()) {
                    results.push((name.clone(), Some("protected branch".into())));
                } else {
                    to_delete.push(name.clone());
                }
            }
            results.extend(git::delete_branches(&repo, &to_delete));
        }
        ui::report_deletions(&results);
        let any_failed = results.iter().any(|(_, err)| err.is_some());
        return Ok(if any_failed {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        });
    }

    let read_only = args.list || args.json;

    if !args.no_fetch {
        if args.verbose {
            eprintln!("{} git fetch --prune {}", "·".dimmed(), args.remote);
        }
        let t = Instant::now();
        if let Err(e) = git::fetch_prune(&args.remote) {
            eprintln!("{} {e}", "warning:".yellow().bold());
        }
        dbg_log!("fetch --prune {} took {:?}", args.remote, t.elapsed());
    }

    let t = Instant::now();
    let main_name = git::detect_main(&repo, &args.remote, args.main.as_deref())?;
    dbg_log!("detect_main resolved to {} ({:?})", main_name, t.elapsed());

    let filter = Filter {
        merged: !args.no_merged,
        squashed: !args.no_squashed,
        gone: !args.no_gone,
        abandoned: args.abandoned,
        abandoned_days: args.abandoned_days,
        squash_scan_limit: args.squash_scan_limit,
    };

    if !filter.merged && !filter.squashed && !filter.gone && !filter.abandoned {
        eprintln!("{}: all detection categories disabled; nothing to do.", "note".yellow());
        return Ok(ExitCode::SUCCESS);
    }

    let t = Instant::now();
    let candidates = git::categorize(&repo, &main_name, &args.remote, filter)?;
    dbg_log!(
        "categorize produced {} candidate(s) in {:?}",
        candidates.len(),
        t.elapsed()
    );

    let worktree_cands = git::scan_worktrees(&candidates);

    if args.json {
        ui::print_json(&candidates, &worktree_cands, &main_name);
        return Ok(ExitCode::SUCCESS);
    }
    ui::print_summary(&candidates, &worktree_cands, &main_name, read_only);
    if read_only || (candidates.is_empty() && worktree_cands.is_empty()) {
        return Ok(ExitCode::SUCCESS);
    }

    // Branches checked out in a worktree are handled through their worktree;
    // blocked worktrees (dirty, locked, current dir) are shown but kept.
    let free: Vec<&git::Candidate> =
        candidates.iter().filter(|c| c.worktree.is_none()).collect();
    let removable: Vec<&git::WorktreeCandidate> = worktree_cands
        .iter()
        .filter(|w| w.blocked.is_none())
        .collect();

    let (to_delete, wt_selected): (Vec<String>, Vec<&git::WorktreeCandidate>) =
        if args.force {
            (
                free.iter().map(|c| c.name.clone()).collect(),
                removable.clone(),
            )
        } else if args.yes {
            if !ui::confirm_cleanup(free.len(), removable.len())? {
                println!("{}", "aborted.".dimmed());
                return Ok(ExitCode::SUCCESS);
            }
            (
                free.iter().map(|c| c.name.clone()).collect(),
                removable.clone(),
            )
        } else {
            if !ui::is_interactive() {
                eprintln!(
                    "\n{}: not running on a TTY. Use {} for a read-only scan ({} for machine output), {} to delete all, or {} to skip the picker.",
                    "note".yellow(),
                    "--list".bold(),
                    "--json".bold(),
                    "--force".bold(),
                    "--yes".bold()
                );
                return Ok(ExitCode::from(1));
            }
            println!();
            let chosen: Vec<String> = if free.is_empty() {
                Vec::new()
            } else {
                ui::select(&free)?
                    .into_iter()
                    .map(|i| free[i].name.clone())
                    .collect()
            };
            let wt_chosen: Vec<&git::WorktreeCandidate> = if removable.is_empty() {
                Vec::new()
            } else {
                ui::select_worktrees(&removable)?
                    .into_iter()
                    .map(|i| removable[i])
                    .collect()
            };
            if chosen.is_empty() && wt_chosen.is_empty() {
                println!("{}", "nothing selected.".dimmed());
                return Ok(ExitCode::SUCCESS);
            }
            if !ui::confirm_cleanup(chosen.len(), wt_chosen.len())? {
                println!("{}", "aborted.".dimmed());
                return Ok(ExitCode::SUCCESS);
            }
            (chosen, wt_chosen)
        };

    if to_delete.is_empty() && wt_selected.is_empty() {
        println!("{}", "nothing deletable (all candidates are blocked).".dimmed());
        return Ok(ExitCode::SUCCESS);
    }

    println!();
    let t = Instant::now();
    let mut results = git::delete_branches(&repo, &to_delete);
    results.extend(git::remove_worktrees(&repo, &wt_selected));
    dbg_log!(
        "cleanup: {} attempted, {} failed, {:?}",
        results.len(),
        results.iter().filter(|(_, e)| e.is_some()).count(),
        t.elapsed()
    );
    ui::report_deletions(&results);

    let any_failed = results.iter().any(|(_, err)| err.is_some());
    dbg_log!("total runtime: {:?}", total.elapsed());
    Ok(if any_failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}
