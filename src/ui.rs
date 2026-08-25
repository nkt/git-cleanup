use crate::git::{Candidate, Category};
use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Confirm, MultiSelect};
use owo_colors::OwoColorize;
use std::io::IsTerminal;

pub fn print_summary(candidates: &[Candidate], main: &str, read_only: bool) {
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

    if candidates.is_empty() {
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

    println!(
        "{} stale {} {}",
        candidates.len().bold(),
        if candidates.len() == 1 {
            "branch"
        } else {
            "branches"
        },
        "found.".dimmed()
    );
}

pub fn print_json(candidates: &[Candidate], main: &str) {
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
    let out = serde_json::json!({ "main": main, "candidates": arr });
    println!("{}", serde_json::to_string_pretty(&out).expect("serialize"));
}

pub fn select(candidates: &[Candidate]) -> Result<Vec<usize>> {
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

pub fn confirm_delete(count: usize) -> Result<bool> {
    let theme = ColorfulTheme::default();
    let prompt = format!(
        "Delete {count} {}?",
        if count == 1 { "branch" } else { "branches" }
    );
    Ok(Confirm::with_theme(&theme)
        .with_prompt(prompt)
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
