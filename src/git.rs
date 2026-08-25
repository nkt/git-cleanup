use crate::dbg_log;
use crate::debug;
use anyhow::{anyhow, bail, Context, Result};
use git2::{BranchType, ErrorCode, Oid, Repository};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Merged,
    Squashed,
    Gone,
    Abandoned,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Merged => "merged",
            Category::Squashed => "squashed",
            Category::Gone => "gone",
            Category::Abandoned => "abandoned",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamState {
    Tracked,
    Gone,
    None,
}

impl UpstreamState {
    pub fn label(self) -> &'static str {
        match self {
            UpstreamState::Tracked => "tracked",
            UpstreamState::Gone => "gone",
            UpstreamState::None => "none",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub category: Category,
    pub detail: String,
    pub tip: Oid,
    pub age_days: i64,
    /// Commits on the branch that are not ancestors of main.
    /// Some(0) means safe by ancestry; None means not computed (squashed —
    /// content equivalence already proven via patch-id).
    pub ahead: Option<usize>,
    pub upstream: UpstreamState,
    pub worktree: Option<String>,
}

/// Branch names that are never offered for deletion.
pub const PROTECTED: [&str; 5] = ["main", "master", "develop", "trunk", "HEAD"];

#[derive(Debug, Clone, Copy)]
pub struct Filter {
    pub merged: bool,
    pub squashed: bool,
    pub gone: bool,
    pub abandoned: bool,
    pub abandoned_days: u32,
}

pub fn open() -> Result<Repository> {
    Repository::discover(".").context("not inside a git repository")
}

pub fn detect_main(
    repo: &Repository,
    remote: &str,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(name) = explicit {
        repo.find_branch(name, BranchType::Local)
            .with_context(|| format!("local branch '{name}' not found"))?;
        return Ok(name.to_string());
    }

    let head_ref = format!("refs/remotes/{remote}/HEAD");
    if let Ok(reference) = repo.find_reference(&head_ref) {
        if let Some(target) = reference.symbolic_target() {
            let prefix = format!("refs/remotes/{remote}/");
            if let Some(stripped) = target.strip_prefix(&prefix) {
                if repo.find_branch(stripped, BranchType::Local).is_ok() {
                    return Ok(stripped.to_string());
                }
            }
        }
    }

    for fallback in ["main", "master", "trunk", "develop"] {
        if repo.find_branch(fallback, BranchType::Local).is_ok() {
            return Ok(fallback.to_string());
        }
    }

    bail!("could not detect main branch; pass --main <BRANCH>")
}

pub fn current_branch(repo: &Repository) -> Result<Option<String>> {
    let head = match repo.head() {
        Ok(h) => h,
        Err(e)
            if e.code() == ErrorCode::UnbornBranch || e.code() == ErrorCode::NotFound =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    if !head.is_branch() {
        return Ok(None);
    }
    Ok(head.shorthand().map(|s| s.to_string()))
}

pub fn fetch_prune(remote: &str) -> Result<()> {
    let status = Command::new("git")
        .args(["fetch", "--prune", remote])
        .status()
        .context("failed to spawn `git fetch`")?;
    if !status.success() {
        bail!("`git fetch --prune {remote}` failed");
    }
    Ok(())
}

pub fn categorize(
    repo: &Repository,
    main_name: &str,
    remote: &str,
    filter: Filter,
) -> Result<Vec<Candidate>> {
    let main_branch = repo.find_branch(main_name, BranchType::Local)?;
    let main_oid = main_branch
        .get()
        .target()
        .ok_or_else(|| anyhow!("main branch '{main_name}' has no target"))?;

    let current = current_branch(repo)?;
    let protected: HashSet<&str> = PROTECTED.into_iter().collect();
    let worktrees = list_worktree_branches();

    let mut candidates = Vec::new();
    let mut patchid_memo: HashMap<Oid, Option<Oid>> = HashMap::new();

    let mut total_local: usize = 0;
    let mut skipped_protected: usize = 0;
    let mut skipped_current: usize = 0;
    let mut merged_checks: u128 = 0;
    let mut gone_checks: u128 = 0;
    let mut squashed_walks: usize = 0;
    let mut squashed_commits_walked: usize = 0;
    let mut squashed_cache_hits: usize = 0;
    let mut squashed_total: u128 = 0;

    dbg_log!("categorize: main={} main_oid={}", main_name, short_oid(main_oid));

    for entry in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = entry?;
        let name = match branch.name()? {
            Some(n) => n.to_string(),
            None => continue,
        };
        total_local += 1;

        if name == main_name {
            continue;
        }
        if protected.contains(name.as_str()) {
            skipped_protected += 1;
            dbg_log!("branch {name}: skip (protected)");
            continue;
        }
        if current.as_deref() == Some(name.as_str()) {
            skipped_current += 1;
            dbg_log!("branch {name}: skip (current HEAD)");
            continue;
        }

        let oid = match branch.get().target() {
            Some(o) => o,
            None => {
                dbg_log!("branch {name}: skip (no target oid)");
                continue;
            }
        };

        let branch_t = Instant::now();
        dbg_log!("branch {name}: oid={}", short_oid(oid));

        let (upstream, upstream_display) = upstream_status(repo, &branch);
        let age_days = commit_age_days(repo, oid).unwrap_or(0);

        let mut category: Option<(Category, String)> = None;

        if filter.merged {
            let t = Instant::now();
            let is_merged = oid == main_oid
                || repo.graph_descendant_of(main_oid, oid).unwrap_or(false);
            let elapsed = t.elapsed();
            merged_checks += elapsed.as_micros();
            dbg_log!("  merged-check: {is_merged} ({elapsed:?})");
            if is_merged {
                category = Some((Category::Merged, format!("merged into {main_name}")));
            }
        }

        if category.is_none() && filter.gone {
            let t = Instant::now();
            if upstream == UpstreamState::Gone {
                let display = upstream_display.as_deref().unwrap_or("upstream");
                dbg_log!("  gone-check: matched ({display}) ({:?})", t.elapsed());
                category =
                    Some((Category::Gone, format!("upstream {display} is gone")));
            } else {
                dbg_log!(
                    "  gone-check: upstream state = {} ({:?})",
                    upstream.label(),
                    t.elapsed()
                );
            }
            gone_checks += t.elapsed().as_micros();
        }

        if category.is_none() && filter.squashed {
            let mut tips_to_try: Vec<(&'static str, Oid)> = vec![("local tip", oid)];
            if let Some(fallback) = fallback_tip(repo, &branch, remote) {
                if fallback != oid {
                    let local_in_fallback = repo
                        .graph_descendant_of(fallback, oid)
                        .unwrap_or(false);
                    if local_in_fallback {
                        dbg_log!(
                            "  squashed-fallback: local ({}) is ancestor of remote tip ({}); will retry on no-match",
                            short_oid(oid),
                            short_oid(fallback)
                        );
                        tips_to_try.push(("remote tip", fallback));
                    } else {
                        dbg_log!(
                            "  squashed-fallback: local has commits not in remote tip ({}); skipping fallback",
                            short_oid(fallback)
                        );
                    }
                }
            }

            for (label, tip) in tips_to_try {
                let t = Instant::now();
                let outcome = check_squashed(repo, main_oid, tip, &mut patchid_memo)?;
                let elapsed = t.elapsed();
                squashed_total += elapsed.as_micros();
                match outcome {
                    SquashedCheck::Match { patch_id, walked, cache_hits } => {
                        squashed_walks += 1;
                        squashed_commits_walked += walked;
                        squashed_cache_hits += cache_hits;
                        dbg_log!(
                            "  squashed-check ({label}): MATCH patch-id={} after {walked} main commits ({cache_hits} cache hits) ({elapsed:?})",
                            short_oid(patch_id)
                        );
                        let detail = if label == "local tip" {
                            format!("squashed into {main_name} (patch-id {})", short_oid(patch_id))
                        } else {
                            format!(
                                "squashed into {main_name} via remote tip (patch-id {})",
                                short_oid(patch_id)
                            )
                        };
                        category = Some((Category::Squashed, detail));
                        break;
                    }
                    SquashedCheck::NoMatch { patch_id, walked, cache_hits } => {
                        squashed_walks += 1;
                        squashed_commits_walked += walked;
                        squashed_cache_hits += cache_hits;
                        dbg_log!(
                            "  squashed-check ({label}): no match (patch-id={}, walked {walked} main commits, {cache_hits} cache hits) ({elapsed:?})",
                            short_oid(patch_id)
                        );
                    }
                    SquashedCheck::Skip(reason) => {
                        dbg_log!("  squashed-check ({label}): skip ({reason}) ({elapsed:?})");
                    }
                }
            }
        }

        if category.is_none() && filter.abandoned {
            dbg_log!(
                "  abandoned-check: tip is {age_days} day(s) old (threshold {})",
                filter.abandoned_days
            );
            if age_days >= i64::from(filter.abandoned_days) {
                category = Some((
                    Category::Abandoned,
                    format!("untouched for {age_days} days"),
                ));
            }
        }

        match category {
            Some((cat, mut detail)) => {
                let ahead = match cat {
                    Category::Merged => Some(0),
                    Category::Squashed => None,
                    Category::Gone | Category::Abandoned => {
                        let t = Instant::now();
                        let a = repo
                            .graph_ahead_behind(oid, main_oid)
                            .ok()
                            .map(|(ahead, _)| ahead);
                        dbg_log!("  ahead-check: {a:?} ({:?})", t.elapsed());
                        a
                    }
                };
                if let (Some(a), Category::Gone | Category::Abandoned) = (ahead, cat) {
                    if a > 0 {
                        detail.push_str(&format!(
                            ", {a} unmerged commit{}",
                            if a == 1 { "" } else { "s" }
                        ));
                    }
                }
                dbg_log!("  -> {} ({:?})", cat.label(), branch_t.elapsed());
                candidates.push(Candidate {
                    worktree: worktrees.get(&name).cloned(),
                    name,
                    category: cat,
                    detail,
                    tip: oid,
                    age_days,
                    ahead,
                    upstream,
                });
            }
            None => {
                dbg_log!("  -> kept (no category) ({:?})", branch_t.elapsed());
            }
        }
    }

    dbg_log!(
        "categorize totals: local={} skipped_protected={} skipped_current={} candidates={} merged_check_total={}us gone_check_total={}us squashed_walks={} squashed_main_commits_walked={} squashed_cache_hits={} patchid_memo_size={} squashed_total={}us",
        total_local,
        skipped_protected,
        skipped_current,
        candidates.len(),
        merged_checks,
        gone_checks,
        squashed_walks,
        squashed_commits_walked,
        squashed_cache_hits,
        patchid_memo.len(),
        squashed_total,
    );

    Ok(candidates)
}

fn short_oid(oid: Oid) -> String {
    oid.to_string().chars().take(7).collect()
}

fn upstream_status(
    repo: &Repository,
    branch: &git2::Branch<'_>,
) -> (UpstreamState, Option<String>) {
    let refname = match branch.get().name() {
        Some(r) => r,
        None => return (UpstreamState::None, None),
    };
    let buf = match repo.branch_upstream_name(refname) {
        Ok(b) => b,
        Err(_) => return (UpstreamState::None, None),
    };
    let up = match buf.as_str() {
        Some(u) => u,
        None => return (UpstreamState::None, None),
    };
    let display = up
        .strip_prefix("refs/remotes/")
        .or_else(|| up.strip_prefix("refs/heads/"))
        .unwrap_or(up)
        .to_string();
    if repo.find_reference(up).is_ok() {
        (UpstreamState::Tracked, Some(display))
    } else {
        (UpstreamState::Gone, Some(display))
    }
}

fn fallback_tip(repo: &Repository, branch: &git2::Branch<'_>, remote: &str) -> Option<Oid> {
    if let Ok(up) = branch.upstream() {
        if let Some(o) = up.get().target() {
            return Some(o);
        }
    }
    let name = branch.name().ok().flatten()?;
    let ref_path = format!("refs/remotes/{remote}/{name}");
    repo.find_reference(&ref_path).ok()?.target()
}

fn commit_age_days(repo: &Repository, oid: Oid) -> Result<i64> {
    let commit = repo.find_commit(oid)?;
    let committed = commit.time().seconds();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(committed);
    Ok(((now - committed).max(0)) / 86400)
}

enum SquashedCheck {
    Match { patch_id: Oid, walked: usize, cache_hits: usize },
    NoMatch { patch_id: Oid, walked: usize, cache_hits: usize },
    Skip(&'static str),
}

fn check_squashed(
    repo: &Repository,
    main_oid: Oid,
    branch_oid: Oid,
    memo: &mut HashMap<Oid, Option<Oid>>,
) -> Result<SquashedCheck> {
    let mb = match repo.merge_base(main_oid, branch_oid) {
        Ok(o) => o,
        Err(_) => return Ok(SquashedCheck::Skip("no merge-base")),
    };
    if mb == branch_oid {
        return Ok(SquashedCheck::Skip("merge-base == branch tip"));
    }
    let base_tree = repo.find_commit(mb)?.tree()?;
    let branch_tree = repo.find_commit(branch_oid)?.tree()?;
    if base_tree.id() == branch_tree.id() {
        return Ok(SquashedCheck::Skip("empty diff vs merge-base"));
    }
    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&branch_tree), None)?;
    let branch_pid = match diff.patchid(None) {
        Ok(p) => p,
        Err(_) => return Ok(SquashedCheck::Skip("patch-id computation failed")),
    };

    let mut walk = repo.revwalk()?;
    walk.push(main_oid)?;
    walk.hide(mb)?;
    let mut walked = 0usize;
    let mut cache_hits = 0usize;
    for oid in walk {
        let oid = oid?;
        walked += 1;
        let pid_opt = if let Some(cached) = memo.get(&oid) {
            cache_hits += 1;
            *cached
        } else {
            let commit = repo.find_commit(oid)?;
            let pid = if commit.parent_count() == 0 {
                None
            } else {
                let parent = commit.parent(0)?;
                let d = repo.diff_tree_to_tree(
                    Some(&parent.tree()?),
                    Some(&commit.tree()?),
                    None,
                )?;
                d.patchid(None).ok()
            };
            memo.insert(oid, pid);
            pid
        };
        if pid_opt == Some(branch_pid) {
            return Ok(SquashedCheck::Match {
                patch_id: branch_pid,
                walked,
                cache_hits,
            });
        }
    }
    Ok(SquashedCheck::NoMatch {
        patch_id: branch_pid,
        walked,
        cache_hits,
    })
}

pub fn delete_branches(
    repo: &Repository,
    names: &[String],
) -> Vec<(String, Option<String>)> {
    let worktree_branches = list_worktree_branches();
    if debug::enabled() && !worktree_branches.is_empty() {
        dbg_log!("worktree-tracked branches:");
        for (b, p) in &worktree_branches {
            dbg_log!("  {b} -> {p}");
        }
    }

    let head_branch = current_branch(repo).ok().flatten();
    dbg_log!("delete_branches: current HEAD branch = {:?}", head_branch);

    let mut results = Vec::with_capacity(names.len());
    for name in names {
        let t = Instant::now();
        dbg_log!("delete {name}");

        if let Some(path) = worktree_branches.get(name) {
            dbg_log!("  refused: checked out in worktree {path}");
            results.push((
                name.clone(),
                Some(format!("checked out in worktree {path}")),
            ));
            continue;
        }
        if head_branch.as_deref() == Some(name.as_str()) {
            dbg_log!("  refused: branch is current HEAD");
            results.push((name.clone(), Some("currently checked out (HEAD)".into())));
            continue;
        }

        let outcome = delete_one(repo, name);
        match outcome {
            Ok(()) => {
                dbg_log!("  ok ({:?})", t.elapsed());
                results.push((name.clone(), None));
            }
            Err(e) => {
                dbg_log!(
                    "  FAILED: code={:?} class={:?} msg={} ({:?})",
                    e.code(),
                    e.class(),
                    e.message(),
                    t.elapsed()
                );
                let ref_still = repo.find_branch(name, BranchType::Local).is_ok();
                dbg_log!("  post-check: local ref still exists = {ref_still}");
                if ref_still {
                    let head_now = current_branch(repo).ok().flatten();
                    dbg_log!("  post-check: HEAD now = {:?}", head_now);
                }
                results.push((name.clone(), Some(e.message().to_string())));
            }
        }
    }
    results
}

fn delete_one(repo: &Repository, name: &str) -> std::result::Result<(), git2::Error> {
    let mut b = repo.find_branch(name, BranchType::Local)?;
    b.delete()?;
    Ok(())
}

fn list_worktree_branches() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let out = match Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return map,
    };
    if !out.status.success() {
        return map;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut cur_path: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            cur_path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            let b = b.strip_prefix("refs/heads/").unwrap_or(b);
            if let Some(p) = &cur_path {
                map.insert(b.to_string(), p.clone());
            }
        } else if line.is_empty() {
            cur_path = None;
        }
    }
    map
}
