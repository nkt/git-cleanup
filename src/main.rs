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

    if !args.delete.is_empty() {
        let main_name = git::detect_main(&repo, &args.remote, args.main.as_deref())?;
        let mut results: Vec<(String, Option<String>)> = Vec::new();
        let mut to_delete: Vec<String> = Vec::new();
        for name in &args.delete {
            if name == &main_name || git::PROTECTED.contains(&name.as_str()) {
                results.push((name.clone(), Some("protected branch".into())));
            } else {
                to_delete.push(name.clone());
            }
        }
        results.extend(git::delete_branches(&repo, &to_delete));
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

    if args.json {
        ui::print_json(&candidates, &main_name);
        return Ok(ExitCode::SUCCESS);
    }
    ui::print_summary(&candidates, &main_name, read_only);
    if read_only || candidates.is_empty() {
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
    let t = Instant::now();
    let results = git::delete_branches(&repo, &to_delete);
    dbg_log!(
        "delete_branches: {} attempted, {} failed, {:?}",
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
