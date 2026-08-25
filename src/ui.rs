use crate::git::{Candidate, Category, Kept, WorktreeCandidate};
use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Confirm, MultiSelect};
use owo_colors::OwoColorize;
use std::io::IsTerminal;

pub fn print_summary(
    candidates: &[Candidate],
    worktrees: &[WorktreeCandidate],
    kept: Option<&[Kept]>,
    main: &str,
    read_only: bool,
) {
    if read_only {
        println!(
            "{} {} (main: {})\n",
            "git-cleanup".bold(),
            "— read-only scan".dimmed(),
            main.cyan()
        );
    } else {
        println!("{} (main: {})\n", "git-cleanup".bold(), main.cyan());
    }

    if candidates.is_empty() && worktrees.is_empty() {
        print_kept_section(kept);
        println!("  {}", "nothing to clean up.".green());
        return;
    }

    let name_width = candidates.iter().map(|c| c.name.len()).max().unwrap_or(0);

    for cat in [
        Category::Merged,
        Category::Squashed,
        Category::Gone,
        Category::Abandoned,
    ] {
        let group: Vec<&Candidate> =
            candidates.iter().filter(|c| c.category == cat).collect();
        if group.is_empty() {
            continue;
        }
        let header = format!("{} ({})", cat.label(), group.len());
        let colored_header = match cat {
            Category::Merged => header.green().to_string(),
            Category::Squashed => header.yellow().to_string(),
            Category::Gone => header.red().to_string(),
            Category::Abandoned => header.magenta().to_string(),
        };
        println!("  {colored_header}");
        for c in group {
            let detail = match &c.worktree {
                Some(path) => format!("{} — in worktree {path}", c.detail),
                None => c.detail.clone(),
            };
            println!(
                "    {} {:<width$}  {}",
                "•".dimmed(),
                c.name,
                detail.dimmed(),
                width = name_width
            );
        }
        println!();
    }

    if !worktrees.is_empty() {
        println!("  {}", format!("worktrees ({})", worktrees.len()).blue());
        for wt in worktrees {
            let note = match &wt.blocked {
                Some(reason) => format!("{} — skipped: {reason}", wt.detail),
                None => wt.detail.clone(),
            };
            println!("    {} {}  {}", "•".dimmed(), wt.path, note.dimmed());
        }
        println!();
    }

    print_kept_section(kept);

    let mut parts: Vec<String> = Vec::new();
    if !candidates.is_empty() {
        parts.push(format!(
            "{} stale {}",
            candidates.len(),
            if candidates.len() == 1 { "branch" } else { "branches" }
        ));
    }
    if !worktrees.is_empty() {
        parts.push(format!(
            "{} stale {}",
            worktrees.len(),
            if worktrees.len() == 1 { "worktree" } else { "worktrees" }
        ));
    }
    println!("{} {}", parts.join(" and ").bold(), "found.".dimmed());
}

fn print_kept_section(kept: Option<&[Kept]>) {
    let Some(kept) = kept else { return };
    if kept.is_empty() {
        return;
    }
    println!("  {}", format!("kept ({})", kept.len()).dimmed());
    for k in kept {
        println!("    {} {}  {}", "•".dimmed(), k.name, k.reason.dimmed());
    }
    println!();
}

pub fn print_json(candidates: &[Candidate], worktrees: &[WorktreeCandidate], main: &str) {
    let arr: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "category": c.category.label(),
                "detail": c.detail,
                "tip": c.tip.to_string(),
                "age_days": c.age_days,
                "ahead": c.ahead,
                "upstream": c.upstream.label(),
                "worktree": c.worktree,
            })
        })
        .collect();
    let wt_arr: Vec<serde_json::Value> = worktrees
        .iter()
        .map(|wt| {
            serde_json::json!({
                "path": wt.path,
                "branch": wt.branch,
                "category": wt.category.map(|c| c.label()),
                "detail": wt.detail,
                "prunable": wt.prunable,
                "blocked": wt.blocked,
            })
        })
        .collect();
    let out = serde_json::json!({
        "main": main,
        "candidates": arr,
        "worktrees": wt_arr,
    });
    println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
}

pub fn select(candidates: &[&Candidate]) -> Result<Vec<usize>> {
    let theme = ColorfulTheme::default();
    let items: Vec<String> = candidates
        .iter()
        .map(|c| format!("[{}] {} — {}", c.category.label(), c.name, c.detail))
        .collect();
    let defaults: Vec<bool> = vec![true; candidates.len()];

    let chosen = MultiSelect::with_theme(&theme)
        .with_prompt("Select branches to delete (space toggles, enter confirms)")
        .items(&items)
        .defaults(&defaults)
        .interact()?;

    Ok(chosen)
}

pub fn select_worktrees(worktrees: &[&WorktreeCandidate]) -> Result<Vec<usize>> {
    let theme = ColorfulTheme::default();
    let items: Vec<String> = worktrees
        .iter()
        .map(|wt| format!("{} — {}", wt.path, wt.detail))
        .collect();
    let defaults: Vec<bool> = vec![true; worktrees.len()];

    let chosen = MultiSelect::with_theme(&theme)
        .with_prompt("Select worktrees to remove (space toggles, enter confirms)")
        .items(&items)
        .defaults(&defaults)
        .interact()?;

    Ok(chosen)
}

pub fn confirm_cleanup(branches: usize, worktrees: usize) -> Result<bool> {
    let theme = ColorfulTheme::default();
    let mut parts: Vec<String> = Vec::new();
    if branches > 0 {
        parts.push(format!(
            "delete {branches} {}",
            if branches == 1 { "branch" } else { "branches" }
        ));
    }
    if worktrees > 0 {
        parts.push(format!(
            "remove {worktrees} {}",
            if worktrees == 1 { "worktree" } else { "worktrees" }
        ));
    }
    let mut prompt = parts.join(" and ");
    if let Some(first) = prompt.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    Ok(Confirm::with_theme(&theme)
        .with_prompt(format!("{prompt}?"))
        .default(false)
        .interact()?)
}

pub fn report_deletions(results: &[(String, Option<String>)]) {
    for (name, err) in results {
        match err {
            None => println!("  {} {}", "✓".green(), name),
            Some(msg) => println!("  {} {} — {}", "✗".red(), name, msg.red()),
        }
    }
}

pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
