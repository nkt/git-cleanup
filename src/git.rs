use crate::dbg_log;
use crate::debug;
use anyhow::{anyhow, bail, Context, Result};
use git2::{BranchType, ErrorCode, Oid, Repository};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Command, Stdio};
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
    pub squash_scan_limit: u32,
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

    dbg_log!("categorize: main={} main_oid={}", main_name, short_oid(main_oid));

    // Phase 1: per-branch facts + the cheap categories (merged, gone).
    let phase_t = Instant::now();
    let merged_set = if filter.merged {
        let set = batch_merged(main_name);
        match &set {
            Some(s) => dbg_log!(
                "merged-set: {} branches via `git branch --merged` ({:?})",
                s.len(),
                phase_t.elapsed()
            ),
            None => dbg_log!("merged-set: unavailable, falling back to per-branch ancestry"),
        }
        set
    } else {
        None
    };

    let mut infos: Vec<BranchInfo> = Vec::new();
    for entry in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = entry?;
        let name = match branch.name()? {
            Some(n) => n.to_string(),
            None => continue,
        };

        if name == main_name {
            continue;
        }
        if protected.contains(name.as_str()) {
            dbg_log!("branch {name}: skip (protected)");
            continue;
        }
        if current.as_deref() == Some(name.as_str()) {
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

        let (upstream, upstream_display) = upstream_status(repo, &branch);
        let age_days = commit_age_days(repo, oid).unwrap_or(0);

        let mut category: Option<(Category, String)> = None;

        if filter.merged {
            let t = Instant::now();
            let is_merged = match &merged_set {
                Some(set) => set.contains(&name),
                None => {
                    oid == main_oid
                        || repo.graph_descendant_of(main_oid, oid).unwrap_or(false)
                }
            };
            dbg_log!("branch {name}: merged={is_merged} ({:?})", t.elapsed());
            if is_merged {
                category = Some((Category::Merged, format!("merged into {main_name}")));
            }
        }

        if category.is_none() && filter.gone && upstream == UpstreamState::Gone {
            let display = upstream_display.as_deref().unwrap_or("upstream");
            dbg_log!("branch {name}: upstream {display} is gone");
            category = Some((Category::Gone, format!("upstream {display} is gone")));
        }

        let fallback = if category.is_none() && filter.squashed {
            fallback_tip(repo, &branch, remote)
        } else {
            None
        };

        infos.push(BranchInfo {
            name,
            oid,
            upstream,
            age_days,
            fallback,
            category,
        });
    }
    dbg_log!("phase 1 (merged/gone): {} branches in {:?}", infos.len(), phase_t.elapsed());

    // Phase 2: squashed detection over whatever is still uncategorized.
    if filter.squashed {
        squashed_phase(repo, main_name, main_oid, &mut infos, filter.squash_scan_limit)?;
    }

    // Phase 3: abandoned + candidate assembly.
    let mut candidates = Vec::new();
    for info in infos {
        let mut category = info.category;

        if category.is_none() && filter.abandoned {
            if info.age_days >= i64::from(filter.abandoned_days) {
                category = Some((
                    Category::Abandoned,
                    format!("untouched for {} days", info.age_days),
                ));
            } else {
                dbg_log!(
                    "branch {}: tip is {} day(s) old, under threshold {}",
                    info.name,
                    info.age_days,
                    filter.abandoned_days
                );
            }
        }

        let Some((cat, mut detail)) = category else {
            dbg_log!("branch {}: kept (no category)", info.name);
            continue;
        };

        let ahead = match cat {
            Category::Merged => Some(0),
            Category::Squashed => None,
            Category::Gone | Category::Abandoned => {
                let t = Instant::now();
                let a = repo
                    .graph_ahead_behind(info.oid, main_oid)
                    .ok()
                    .map(|(ahead, _)| ahead);
                dbg_log!("branch {}: ahead={a:?} ({:?})", info.name, t.elapsed());
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
        dbg_log!("branch {}: -> {} ({detail})", info.name, cat.label());
        candidates.push(Candidate {
            worktree: worktrees.get(&info.name).cloned(),
            name: info.name,
            category: cat,
            detail,
            tip: info.oid,
            age_days: info.age_days,
            ahead,
            upstream: info.upstream,
        });
    }

    dbg_log!("categorize: {} candidate(s)", candidates.len());
    Ok(candidates)
}

struct BranchInfo {
    name: String,
    oid: Oid,
    upstream: UpstreamState,
    age_days: i64,
    /// Remote/upstream tip to retry the squashed check against.
    fallback: Option<Oid>,
    category: Option<(Category, String)>,
}

/// One `git branch --merged` call replaces a per-branch ancestry check;
/// git resolves the whole set in a single commit-graph traversal.
fn batch_merged(main_name: &str) -> Option<HashSet<String>> {
    let out = Command::new("git")
        .args([
            "branch",
            "--format=%(refname:short)",
            "--merged",
            main_name,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

fn squashed_phase(
    repo: &Repository,
    main_name: &str,
    main_oid: Oid,
    infos: &mut [BranchInfo],
    scan_limit: u32,
) -> Result<()> {
    let phase_t = Instant::now();

    // Gather check jobs: (info index, tip label, tip, merge-base).
    let mut jobs: Vec<(usize, &'static str, Oid, Oid)> = Vec::new();
    for (i, info) in infos.iter().enumerate() {
        if info.category.is_some() {
            continue;
        }
        let mut tips: Vec<(&'static str, Oid)> = vec![("local tip", info.oid)];
        if let Some(fb) = info.fallback {
            if fb != info.oid && repo.graph_descendant_of(fb, info.oid).unwrap_or(false) {
                dbg_log!(
                    "branch {}: local ({}) is ancestor of remote tip ({}); will also try remote tip",
                    info.name,
                    short_oid(info.oid),
                    short_oid(fb)
                );
                tips.push(("remote tip", fb));
            }
        }
        for (label, tip) in tips {
            let mb = match repo.merge_base(main_oid, tip) {
                Ok(o) => o,
                Err(_) => {
                    dbg_log!("branch {}: squashed ({label}): no merge-base", info.name);
                    continue;
                }
            };
            if mb == tip {
                dbg_log!("branch {}: squashed ({label}): merge-base == tip", info.name);
                continue;
            }
            let base_tree = repo.find_commit(mb)?.tree()?;
            let tip_tree = repo.find_commit(tip)?.tree()?;
            if base_tree.id() == tip_tree.id() {
                dbg_log!("branch {}: squashed ({label}): empty diff vs merge-base", info.name);
                continue;
            }
            jobs.push((i, label, tip, mb));
        }
    }
    if jobs.is_empty() {
        dbg_log!("phase 2 (squashed): no checkable branches ({:?})", phase_t.elapsed());
        return Ok(());
    }

    // One pass over main covers every job: patch-ids for oldest_mb..main.
    let oldest_mb = jobs
        .iter()
        .map(|&(_, _, _, mb)| mb)
        .min_by_key(|&mb| commit_time(repo, mb))
        .expect("jobs is non-empty");

    match cli_main_patchids(oldest_mb, main_oid, scan_limit) {
        Some(main_pids) => {
            for &(i, label, tip, mb) in &jobs {
                if infos[i].category.is_some() {
                    continue; // matched via an earlier tip
                }
                let t = Instant::now();
                let Some(pid) = cli_diff_patchid(mb, tip) else {
                    dbg_log!(
                        "branch {}: squashed ({label}): patch-id via git CLI failed",
                        infos[i].name
                    );
                    continue;
                };
                match main_pids.get(&pid) {
                    Some(&commit) => {
                        dbg_log!(
                            "branch {}: squashed ({label}): MATCH commit {} ({:?})",
                            infos[i].name,
                            short_oid(commit),
                            t.elapsed()
                        );
                        let detail = if label == "local tip" {
                            format!("squashed into {main_name} (commit {})", short_oid(commit))
                        } else {
                            format!(
                                "squashed into {main_name} via remote tip (commit {})",
                                short_oid(commit)
                            )
                        };
                        infos[i].category = Some((Category::Squashed, detail));
                    }
                    None => {
                        dbg_log!(
                            "branch {}: squashed ({label}): no match ({:?})",
                            infos[i].name,
                            t.elapsed()
                        );
                    }
                }
            }
        }
        None => {
            dbg_log!("phase 2: git CLI patch-ids unavailable, using libgit2 walk");
            let mut memo: HashMap<Oid, Option<Oid>> = HashMap::new();
            for &(i, label, tip, _) in &jobs {
                if infos[i].category.is_some() {
                    continue;
                }
                let t = Instant::now();
                match check_squashed(repo, main_oid, tip, &mut memo)? {
                    SquashedCheck::Match { patch_id, walked, cache_hits } => {
                        dbg_log!(
                            "branch {}: squashed ({label}): MATCH patch-id={} after {walked} commits ({cache_hits} cache hits) ({:?})",
                            infos[i].name,
                            short_oid(patch_id),
                            t.elapsed()
                        );
                        let detail = if label == "local tip" {
                            format!("squashed into {main_name} (patch-id {})", short_oid(patch_id))
                        } else {
                            format!(
                                "squashed into {main_name} via remote tip (patch-id {})",
                                short_oid(patch_id)
                            )
                        };
                        infos[i].category = Some((Category::Squashed, detail));
                    }
                    SquashedCheck::NoMatch { walked, cache_hits, .. } => {
                        dbg_log!(
                            "branch {}: squashed ({label}): no match (walked {walked}, {cache_hits} cache hits) ({:?})",
                            infos[i].name,
                            t.elapsed()
                        );
                    }
                    SquashedCheck::Skip(reason) => {
                        dbg_log!("branch {}: squashed ({label}): skip ({reason})", infos[i].name);
                    }
                }
            }
        }
    }

    dbg_log!("phase 2 (squashed): {} job(s) in {:?}", jobs.len(), phase_t.elapsed());
    Ok(())
}

fn commit_time(repo: &Repository, oid: Oid) -> i64 {
    repo.find_commit(oid)
        .map(|c| c.time().seconds())
        .unwrap_or(i64::MAX)
}

/// Patch-id -> commit map for the first-parent range `oldest_mb..main`,
/// via `git log -p | git patch-id --stable` (streams, ~8x faster than
/// per-commit tree diffs through libgit2). `scan_limit` caps the number
/// of commits inspected, newest first; 0 means no cap.
fn cli_main_patchids(
    oldest_mb: Oid,
    main_oid: Oid,
    scan_limit: u32,
) -> Option<HashMap<Oid, Oid>> {
    let t = Instant::now();
    let range = format!("{oldest_mb}..{main_oid}");
    let mut log_cmd = Command::new("git");
    log_cmd.args(["log", "--first-parent", "--format=commit %H", "-p", &range]);
    if scan_limit > 0 {
        log_cmd.arg(format!("-n{scan_limit}"));
    }
    let mut log_child = log_cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let log_out = log_child.stdout.take()?;
    let patch_out = Command::new("git")
        .args(["patch-id", "--stable"])
        .stdin(Stdio::from(log_out))
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let _ = log_child.wait();
    if !patch_out.status.success() {
        return None;
    }
    let mut map: HashMap<Oid, Oid> = HashMap::new();
    for line in String::from_utf8_lossy(&patch_out.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(cid)) = (fields.next(), fields.next()) else {
            continue;
        };
        let (Ok(pid), Ok(cid)) = (Oid::from_str(pid), Oid::from_str(cid)) else {
            continue;
        };
        map.entry(pid).or_insert(cid);
    }
    dbg_log!(
        "main patch-ids via git CLI: {} commits in {:?} (range {range}, limit {scan_limit})",
        map.len(),
        t.elapsed()
    );
    Some(map)
}

/// Patch-id of the whole branch diff (merge-base..tip as one patch),
/// via `git diff-tree -p | git patch-id --stable` so it hashes
/// identically to the main-side ids.
fn cli_diff_patchid(mb: Oid, tip: Oid) -> Option<Oid> {
    let diff = Command::new("git")
        .args(["diff-tree", "-p", &mb.to_string(), &tip.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !diff.status.success() {
        return None;
    }
    let mut child = Command::new("git")
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(&diff.stdout).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let pid = text.lines().next()?.split_whitespace().next()?;
    Oid::from_str(pid).ok()
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
    NoMatch { walked: usize, cache_hits: usize },
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
    Ok(SquashedCheck::NoMatch { walked, cache_hits })
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
