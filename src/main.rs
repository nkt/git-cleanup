mod cli;
mod debug;
mod git;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::Args;
use git::Filter;
use owo_colors::OwoColorize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
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

    let launch_dir = std::env::current_dir().ok();
    if args.recursive {
        return run_recursive(&args, launch_dir);
    }

    let t = Instant::now();
    let repo = git::open()?;
    dbg_log!("repo discovered at {:?} ({:?})", repo.path(), t.elapsed());

    let protected = git::protected_set(&repo, &args.protect);

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
                if name == &main_name || protected.contains(name.as_str()) {
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

    let filter = build_filter(&args);
    if !any_category(&filter) {
        eprintln!("{}: all detection categories disabled; nothing to do.", "note".yellow());
        return Ok(ExitCode::SUCCESS);
    }

    let t = Instant::now();
    let scan = git::categorize(&repo, &main_name, &args.remote, filter, &protected)?;
    let candidates = scan.candidates;
    dbg_log!(
        "categorize produced {} candidate(s) in {:?}",
        candidates.len(),
        t.elapsed()
    );

    let ctx = git::WorktreeScanCtx {
        launch_dir: launch_dir.as_deref(),
        main_name: &main_name,
        remote: &args.remote,
        filter,
    };
    let worktree_cands = git::scan_worktrees(&repo, &candidates, &ctx);

    if args.json {
        ui::print_json(&candidates, &worktree_cands, &main_name);
        return Ok(ExitCode::SUCCESS);
    }
    let kept = args.verbose.then_some(scan.kept.as_slice());
    ui::print_summary(&candidates, &worktree_cands, kept, &main_name, read_only);
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
            if !ui::confirm_cleanup(free.len(), removable.len(), 1)? {
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
            if !ui::confirm_cleanup(chosen.len(), wt_chosen.len(), 1)? {
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

fn build_filter(args: &Args) -> Filter {
    Filter {
        merged: !args.no_merged,
        squashed: !args.no_squashed,
        contained: !args.no_contained,
        superseded: args.superseded.then_some(git::SupersededCfg {
            min_pct: args.superseded_pct,
            min_days: args.superseded_days,
        }),
        gone: !args.no_gone,
        abandoned: args.abandoned,
        abandoned_days: args.abandoned_days,
        squash_scan_limit: args.squash_scan_limit,
    }
}

fn any_category(f: &Filter) -> bool {
    f.merged || f.squashed || f.contained || f.superseded.is_some() || f.gone || f.abandoned
}

/// One repository's scan in a recursive run.
struct RepoScan {
    dir: PathBuf,
    display: String,
    repo: git2::Repository,
    main_name: String,
    candidates: Vec<git::Candidate>,
    worktrees: Vec<git::WorktreeCandidate>,
    kept: Vec<git::Kept>,
}

impl RepoScan {
    fn is_empty(&self) -> bool {
        self.candidates.is_empty() && self.worktrees.is_empty()
    }
}

/// A picked deletion: (repo index, what).
enum Pick {
    Branch(String),
    Worktree(usize),
}

fn display_path(root: &Path, p: &Path) -> String {
    match p.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().to_string(),
        Err(_) => p.to_string_lossy().to_string(),
    }
}

/// Fetch every repository in parallel (git does the network wait, not us).
fn fetch_all(root: &Path, repos: &[PathBuf], remote: &str) {
    eprintln!(
        "{} git fetch --prune {remote} in {} repositories…",
        "·".dimmed(),
        repos.len()
    );
    let t = Instant::now();
    let next = AtomicUsize::new(0);
    let errors: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..repos.len().min(8) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(dir) = repos.get(i) else { break };
                if let Err(e) = git::fetch_prune_in(dir, remote) {
                    errors
                        .lock()
                        .expect("fetch error list")
                        .push((display_path(root, dir), e.to_string()));
                }
            });
        }
    });
    let mut errors = errors.into_inner().expect("fetch error list");
    errors.sort();
    for (repo, e) in errors {
        eprintln!("{} {repo}: {e}", "warning:".yellow().bold());
    }
    dbg_log!("fetch_all: {} repos in {:?}", repos.len(), t.elapsed());
}

/// Scan the repository the process is currently in.
fn scan_repo(
    args: &Args,
    filter: Filter,
    launch_dir: Option<&Path>,
    dir: &Path,
    display: String,
) -> Result<RepoScan> {
    let repo = git::open()?;
    let protected = git::protected_set(&repo, &args.protect);
    let main_name = git::detect_main(&repo, &args.remote, None)?;
    let scan = git::categorize(&repo, &main_name, &args.remote, filter, &protected)?;
    let ctx = git::WorktreeScanCtx {
        launch_dir,
        main_name: &main_name,
        remote: &args.remote,
        filter,
    };
    let worktrees = git::scan_worktrees(&repo, &scan.candidates, &ctx);
    Ok(RepoScan {
        dir: dir.to_path_buf(),
        display,
        main_name: main_name.clone(),
        candidates: scan.candidates,
        worktrees,
        kept: scan.kept,
        repo,
    })
}

fn run_recursive(args: &Args, launch_dir: Option<PathBuf>) -> Result<ExitCode> {
    let root = launch_dir
        .clone()
        .ok_or_else(|| anyhow::anyhow!("cannot read the current directory"))?;
    let filter = build_filter(args);
    if !any_category(&filter) {
        eprintln!("{}: all detection categories disabled; nothing to do.", "note".yellow());
        return Ok(ExitCode::SUCCESS);
    }
    let read_only = args.list || args.json;

    let dirs = git::discover_repos(&root, args.max_depth);
    dbg_log!("recursive: {} repositories under {:?}", dirs.len(), root);
    if dirs.is_empty() {
        eprintln!(
            "{}: no git repositories within {} levels of {}",
            "note".yellow(),
            args.max_depth,
            root.display()
        );
        return Ok(ExitCode::SUCCESS);
    }

    if !args.no_fetch {
        fetch_all(&root, &dirs, &args.remote);
    }

    // Every git call in the scan is cwd-relative, so step into each repo.
    let mut scans: Vec<RepoScan> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    for dir in &dirs {
        let display = display_path(&root, dir);
        let t = Instant::now();
        let res = std::env::set_current_dir(dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| scan_repo(args, filter, launch_dir.as_deref(), dir, display.clone()));
        match res {
            Ok(s) => scans.push(s),
            Err(e) => failed.push((display.clone(), format!("{e:#}"))),
        }
        dbg_log!("recursive: scanned {display} in {:?}", t.elapsed());
    }
    std::env::set_current_dir(&root).ok();
    let scan_code = if failed.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) };

    if args.json {
        let mut repos: Vec<serde_json::Value> = scans
            .iter()
            .map(|s| {
                serde_json::json!({
                    "path": s.dir.to_string_lossy(),
                    "main": s.main_name,
                    "candidates": ui::candidates_json(&s.candidates),
                    "worktrees": ui::worktrees_json(&s.worktrees),
                })
            })
            .collect();
        repos.extend(failed.iter().map(|(d, e)| {
            serde_json::json!({ "path": root.join(d).to_string_lossy(), "error": e })
        }));
        let out = serde_json::json!({ "root": root.to_string_lossy(), "repos": repos });
        println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
        return Ok(scan_code);
    }

    println!(
        "{} {} {} {}\n",
        "git-cleanup".bold(),
        if read_only { "— read-only recursive scan of" } else { "— recursive scan of" }.dimmed(),
        root.display().cyan(),
        format!("({} repositories)", dirs.len()).dimmed()
    );
    for s in &scans {
        if s.is_empty() && !(args.verbose && !s.kept.is_empty()) {
            continue;
        }
        ui::print_repo_header(&s.display, &s.main_name);
        let kept = args.verbose.then_some(s.kept.as_slice());
        ui::print_groups(&s.candidates, &s.worktrees, kept);
    }
    for (d, e) in &failed {
        eprintln!("{} {d}: {e}", "warning:".yellow().bold());
    }

    let n_branches: usize = scans.iter().map(|s| s.candidates.len()).sum();
    let n_worktrees: usize = scans.iter().map(|s| s.worktrees.len()).sum();
    let n_repos = scans.iter().filter(|s| !s.is_empty()).count();
    if n_branches + n_worktrees == 0 {
        println!("  {}", "nothing to clean up.".green());
        return Ok(scan_code);
    }
    println!(
        "{} {}",
        format!(
            "{} in {n_repos} {}",
            ui::found_line(n_branches, n_worktrees),
            if n_repos == 1 { "repository" } else { "repositories" }
        )
        .bold(),
        "found.".dimmed()
    );
    if read_only {
        return Ok(scan_code);
    }

    // Same rules as a single repo: branches in a worktree go through it,
    // blocked worktrees are shown but never offered.
    let mut picks: Vec<(usize, Pick)> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    for (ri, s) in scans.iter().enumerate() {
        for c in s.candidates.iter().filter(|c| c.worktree.is_none()) {
            labels.push(format!(
                "{}: [{}] {} — {}",
                s.display,
                c.category.label(),
                c.name,
                c.detail
            ));
            picks.push((ri, Pick::Branch(c.name.clone())));
        }
        for (wi, wt) in s.worktrees.iter().enumerate() {
            if wt.blocked.is_some() {
                continue;
            }
            labels.push(format!(
                "{}: worktree {} — {}",
                s.display,
                display_path(&s.dir, Path::new(&wt.path)),
                wt.detail
            ));
            picks.push((ri, Pick::Worktree(wi)));
        }
    }
    if picks.is_empty() {
        println!("{}", "nothing deletable (all candidates are blocked).".dimmed());
        return Ok(scan_code);
    }

    let count = |chosen: &[usize]| {
        let b = chosen.iter().filter(|&&i| matches!(picks[i].1, Pick::Branch(_))).count();
        let mut repos: Vec<usize> = chosen.iter().map(|&i| picks[i].0).collect();
        repos.dedup();
        (b, chosen.len() - b, repos.len())
    };
    let all: Vec<usize> = (0..picks.len()).collect();
    let chosen: Vec<usize> = if args.force {
        all
    } else if args.yes {
        let (b, w, r) = count(&all);
        if !ui::confirm_cleanup(b, w, r)? {
            println!("{}", "aborted.".dimmed());
            return Ok(scan_code);
        }
        all
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
        let chosen = ui::select_labels(
            "Select what to clean up (space toggles, enter confirms)",
            &labels,
        )?;
        if chosen.is_empty() {
            println!("{}", "nothing selected.".dimmed());
            return Ok(scan_code);
        }
        let (b, w, r) = count(&chosen);
        if !ui::confirm_cleanup(b, w, r)? {
            println!("{}", "aborted.".dimmed());
            return Ok(scan_code);
        }
        chosen
    };

    println!();
    let mut any_failed = false;
    for (ri, s) in scans.iter().enumerate() {
        let mine: Vec<&Pick> = chosen
            .iter()
            .filter(|&&i| picks[i].0 == ri)
            .map(|&i| &picks[i].1)
            .collect();
        if mine.is_empty() {
            continue;
        }
        let branches: Vec<String> = mine
            .iter()
            .filter_map(|p| match p {
                Pick::Branch(n) => Some(n.clone()),
                Pick::Worktree(_) => None,
            })
            .collect();
        let wts: Vec<&git::WorktreeCandidate> = mine
            .iter()
            .filter_map(|p| match p {
                Pick::Worktree(i) => Some(&s.worktrees[*i]),
                Pick::Branch(_) => None,
            })
            .collect();
        ui::print_repo_header(&s.display, &s.main_name);
        let results = match std::env::set_current_dir(&s.dir) {
            Ok(()) => {
                let mut r = git::delete_branches(&s.repo, &branches);
                r.extend(git::remove_worktrees(&s.repo, &wts));
                r
            }
            Err(e) => vec![(s.display.clone(), Some(format!("cannot enter repository: {e}")))],
        };
        ui::report_deletions(&results);
        any_failed |= results.iter().any(|(_, e)| e.is_some());
    }
    std::env::set_current_dir(&root).ok();

    Ok(if any_failed || !failed.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}
