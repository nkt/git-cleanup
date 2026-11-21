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

    print_groups(candidates, worktrees, kept);
    println!(
        "{} {}",
        found_line(candidates.len(), worktrees.len()).bold(),
        "found.".dimmed()
    );
}

/// Header line for one repository in a recursive scan.
pub fn print_repo_header(display: &str, main: &str) {
    println!("{} {}", display.bold(), format!("(main: {main})").dimmed());
}

/// Candidate groups, worktrees and (optionally) kept branches.
pub fn print_groups(
    candidates: &[Candidate],
    worktrees: &[WorktreeCandidate],
    kept: Option<&[Kept]>,
) {
    let name_width = candidates.iter().map(|c| c.name.len()).max().unwrap_or(0);

    for cat in Category::ALL {
        let group: Vec<&Candidate> =
            candidates.iter().filter(|c| c.category == cat).collect();
        if group.is_empty() {
            continue;
        }
        let header = format!("{} ({})", cat.label(), group.len());
        println!("  {}", color_category(cat, &header));
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
}

fn color_category(cat: Category, s: &str) -> String {
    match cat {
        Category::Merged => s.green().to_string(),
        Category::Squashed => s.yellow().to_string(),
        Category::Contained => s.yellow().to_string(),
        Category::Superseded => s.bright_yellow().to_string(),
        Category::Gone => s.red().to_string(),
        Category::Abandoned => s.magenta().to_string(),
    }
}

/// "3 stale branches and 2 stale worktrees".
pub fn found_line(branches: usize, worktrees: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if branches > 0 {
        parts.push(format!(
            "{branches} stale {}",
            if branches == 1 { "branch" } else { "branches" }
        ));
    }
    if worktrees > 0 {
        parts.push(format!(
            "{worktrees} stale {}",
            if worktrees == 1 { "worktree" } else { "worktrees" }
        ));
    }
    parts.join(" and ")
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

pub fn candidates_json(candidates: &[Candidate]) -> Vec<serde_json::Value> {
    candidates
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
        .collect()
}

pub fn worktrees_json(worktrees: &[WorktreeCandidate]) -> Vec<serde_json::Value> {
    worktrees
        .iter()
        .map(|wt| {
            serde_json::json!({
                "path": wt.path,
                "branch": wt.branch,
                "head": wt.head.map(|h| h.to_string()),
                "category": wt.category.map(|c| c.label()),
                "detail": wt.detail,
                "prunable": wt.prunable,
                "blocked": wt.blocked,
            })
        })
        .collect()
}

pub fn print_json(candidates: &[Candidate], worktrees: &[WorktreeCandidate], main: &str) {
    let out = serde_json::json!({
        "main": main,
        "candidates": candidates_json(candidates),
        "worktrees": worktrees_json(worktrees),
    });
    println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
}

pub fn select(candidates: &[&Candidate]) -> Result<Vec<usize>> {
    let items: Vec<String> = candidates
        .iter()
        .map(|c| format!("[{}] {} — {}", c.category.label(), c.name, c.detail))
        .collect();
    select_labels("Select branches to delete (space toggles, enter confirms)", &items)
}

pub fn select_worktrees(worktrees: &[&WorktreeCandidate]) -> Result<Vec<usize>> {
    let items: Vec<String> = worktrees
        .iter()
        .map(|wt| format!("{} — {}", wt.path, wt.detail))
        .collect();
    select_labels("Select worktrees to remove (space toggles, enter confirms)", &items)
}

/// Multi-select over prepared labels, everything preselected.
pub fn select_labels(prompt: &str, items: &[String]) -> Result<Vec<usize>> {
    let theme = ColorfulTheme::default();
    let defaults: Vec<bool> = vec![true; items.len()];
    Ok(MultiSelect::with_theme(&theme)
        .with_prompt(prompt)
        .items(items)
        .defaults(&defaults)
        .interact()?)
}

/// `repos` > 1 adds "across N repositories" to the prompt.
pub fn confirm_cleanup(branches: usize, worktrees: usize, repos: usize) -> Result<bool> {
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
    if repos > 1 {
        prompt.push_str(&format!(" across {repos} repositories"));
    }
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
