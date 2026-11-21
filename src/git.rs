use crate::dbg_log;
use crate::debug;
use anyhow::{anyhow, bail, Context, Result};
use git2::{BranchType, ErrorCode, Oid, Repository};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Merged,
    Squashed,
    Contained,
    Superseded,
    Gone,
    Abandoned,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Merged,
        Category::Squashed,
        Category::Contained,
        Category::Superseded,
        Category::Gone,
        Category::Abandoned,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Merged => "merged",
            Category::Squashed => "squashed",
            Category::Contained => "contained",
            Category::Superseded => "superseded",
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

/// The built-in protected names plus `git config cleanup.protect`
/// entries (multi-valued and comma-separated both work) and any
/// explicit extras from the command line.
pub fn protected_set(repo: &Repository, extra: &[String]) -> HashSet<String> {
    let mut set: HashSet<String> = PROTECTED.iter().map(|s| s.to_string()).collect();
    if let Ok(config) = repo.config() {
        if let Ok(entries) = config.multivar("cleanup.protect", None) {
            entries
                .for_each(|entry| {
                    if let Some(value) = entry.value() {
                        for name in value.split(',') {
                            let name = name.trim();
                            if !name.is_empty() {
                                set.insert(name.to_string());
                            }
                        }
                    }
                })
                .ok();
        }
    }
    for name in extra {
        let name = name.trim();
        if !name.is_empty() {
            set.insert(name.to_string());
        }
    }
    set
}

/// A branch that was scanned but not offered for deletion.
#[derive(Debug, Clone)]
pub struct Kept {
    pub name: String,
    pub reason: String,
}

pub struct Scan {
    pub candidates: Vec<Candidate>,
    pub kept: Vec<Kept>,
}

#[derive(Debug, Clone, Copy)]
pub struct Filter {
    pub merged: bool,
    pub squashed: bool,
    pub contained: bool,
    pub superseded: Option<SupersededCfg>,
    pub gone: bool,
    pub abandoned: bool,
    pub abandoned_days: u32,
    pub squash_scan_limit: u32,
}

/// Thresholds for the heuristic `superseded` category.
#[derive(Debug, Clone, Copy)]
pub struct SupersededCfg {
    /// Minimum share (percent) of the branch's changed lines found in main.
    pub min_pct: u8,
    /// Minimum age of the tip commit, so live work is never offered.
    pub min_days: u32,
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

/// Quiet `git fetch --prune` in `dir`, for running many repositories in
/// parallel; the error carries git's stderr.
pub fn fetch_prune_in(dir: &Path, remote: &str) -> Result<()> {
    let out = Command::new("git")
        .args(["fetch", "--prune", "--quiet", remote])
        .current_dir(dir)
        // Never hang the whole run on a credential prompt.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("failed to spawn `git fetch`")?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!(
            "`git fetch --prune {remote}` failed{}",
            if msg.is_empty() { String::new() } else { format!(": {msg}") }
        );
    }
    Ok(())
}

/// Non-bare repositories under `root` (a directory holding a `.git`
/// directory), at most `max_depth` levels down. A found repository is not
/// descended into; `.git` files (linked worktrees, submodules) are skipped
/// since their main repository covers them. Symlinks, hidden directories
/// and `node_modules` are never followed.
pub fn discover_repos(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
        match std::fs::symlink_metadata(dir.join(".git")) {
            Ok(m) if m.is_dir() => {
                out.push(dir.to_path_buf());
                return;
            }
            Ok(_) => return,
            Err(_) => {}
        }
        if depth >= max_depth {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut subdirs: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                !name.starts_with('.') && name != "node_modules"
            })
            .map(|e| e.path())
            .collect();
        subdirs.sort();
        for sub in subdirs {
            walk(&sub, depth + 1, max_depth, out);
        }
    }
    let mut out = Vec::new();
    walk(root, 0, max_depth, &mut out);
    out
}

/// The ref the content checks compare against: the remote-tracking main
/// when it exists and contains local main (it is fresh after a fetch,
/// while local main may lag), otherwise local main.
fn content_base(repo: &Repository, main_name: &str, remote: &str, main_oid: Oid) -> (String, Oid) {
    let remote_ref = format!("refs/remotes/{remote}/{main_name}");
    if let Some(oid) = repo.find_reference(&remote_ref).ok().and_then(|r| r.target()) {
        if oid == main_oid || repo.graph_descendant_of(oid, main_oid).unwrap_or(false) {
            return (format!("{remote}/{main_name}"), oid);
        }
    }
    (main_name.to_string(), main_oid)
}

pub fn categorize(
    repo: &Repository,
    main_name: &str,
    remote: &str,
    filter: Filter,
    protected: &HashSet<String>,
) -> Result<Scan> {
    let main_branch = repo.find_branch(main_name, BranchType::Local)?;
    let main_oid = main_branch
        .get()
        .target()
        .ok_or_else(|| anyhow!("main branch '{main_name}' has no target"))?;

    let current = current_branch(repo)?;
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
        if protected.contains(&name) {
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
            content_note: None,
        });
    }
    dbg_log!("phase 1 (merged/gone): {} branches in {:?}", infos.len(), phase_t.elapsed());

    // Phase 2: squashed detection over whatever is still uncategorized.
    if filter.squashed {
        squashed_phase(repo, main_name, main_oid, &mut infos, filter.squash_scan_limit)?;
    }

    // Phase 2b: content checks (squash-merged, then edited on main).
    if filter.contained || filter.superseded.is_some() {
        let phase_t = Instant::now();
        let (base_name, base_oid) = content_base(repo, main_name, remote, main_oid);
        let mut probe = ContentProbe::new(repo, base_name, base_oid)?;
        for info in infos.iter_mut().filter(|i| i.category.is_none()) {
            match probe.judge(info.oid, info.age_days, filter) {
                ContentVerdict::Hit(cat, detail) => {
                    dbg_log!("branch {}: {} ({detail})", info.name, cat.label());
                    info.category = Some((cat, detail));
                }
                ContentVerdict::Miss(note) => {
                    dbg_log!("branch {}: content: {note}", info.name);
                    info.content_note = Some(note);
                }
            }
        }
        dbg_log!("phase 2b (content): {:?}", phase_t.elapsed());
    }

    // Phase 3: abandoned + candidate assembly.
    let mut candidates = Vec::new();
    let mut kept = Vec::new();
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
            let upstream = match info.upstream {
                UpstreamState::Tracked => "upstream tracked",
                UpstreamState::Gone => "upstream gone",
                UpstreamState::None => "no upstream",
            };
            let mut reason = format!(
                "not merged or squashed, {upstream}, last commit {} day{} ago",
                info.age_days,
                if info.age_days == 1 { "" } else { "s" }
            );
            if let Some(note) = &info.content_note {
                reason.push_str(&format!(", {note}"));
            }
            dbg_log!("branch {}: kept ({reason})", info.name);
            kept.push(Kept {
                name: info.name,
                reason,
            });
            continue;
        };

        let ahead = match cat {
            Category::Merged => Some(0),
            Category::Squashed => None,
            Category::Contained | Category::Superseded | Category::Gone | Category::Abandoned => {
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

    dbg_log!(
        "categorize: {} candidate(s), {} kept",
        candidates.len(),
        kept.len()
    );
    Ok(Scan { candidates, kept })
}

struct BranchInfo {
    name: String,
    oid: Oid,
    upstream: UpstreamState,
    age_days: i64,
    /// Remote/upstream tip to retry the squashed check against.
    fallback: Option<Oid>,
    category: Option<(Category, String)>,
    /// Why the content checks did not match, for the verbose kept list.
    content_note: Option<String>,
}

pub enum ContentVerdict {
    Hit(Category, String),
    Miss(String),
}

/// Lock files are derived from the manifests, so they are left out of the
/// line counts.
const DERIVED_FILES: [&str; 7] = [
    "yarn.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "bun.lockb",
    "Cargo.lock",
    "Podfile.lock",
    "Gemfile.lock",
];

/// Shorter lines (braces, `});`, blank-ish) match anywhere and prove nothing.
const MIN_LINE_LEN: usize = 4;

/// Content checks against one base commit, for branches that were
/// squash-merged and then edited on main, so neither ancestry nor
/// patch-id finds them.
pub struct ContentProbe<'r> {
    repo: &'r Repository,
    base_name: String,
    base_oid: Oid,
    base_tree: git2::Tree<'r>,
    /// Trimmed lines of each base file; None when the base lacks it.
    lines: HashMap<PathBuf, Option<HashSet<String>>>,
    merge_tree_ok: bool,
}

#[derive(Default)]
struct FileChange {
    added: Vec<String>,
    removed: Vec<String>,
    binary: bool,
    /// Blob at the branch tip; None when the branch deleted the file.
    tip_blob: Option<Oid>,
}

struct Coverage {
    changed: usize,
    in_base: usize,
    /// (path, lines not in base), largest first.
    gaps: Vec<(String, usize)>,
    /// Files the branch has that the base lacks.
    absent: Vec<String>,
}

impl<'r> ContentProbe<'r> {
    pub fn new(repo: &'r Repository, base_name: String, base_oid: Oid) -> Result<Self> {
        let base_tree = repo.find_commit(base_oid)?.tree()?;
        Ok(Self {
            repo,
            base_name,
            base_oid,
            base_tree,
            lines: HashMap::new(),
            merge_tree_ok: true,
        })
    }

    pub fn judge(&mut self, tip: Oid, age_days: i64, filter: Filter) -> ContentVerdict {
        if filter.merged
            && (tip == self.base_oid
                || self.repo.graph_descendant_of(self.base_oid, tip).unwrap_or(false))
        {
            return ContentVerdict::Hit(
                Category::Merged,
                format!("merged into {}", self.base_name),
            );
        }
        if filter.contained && self.contained(tip) == Some(true) {
            return ContentVerdict::Hit(
                Category::Contained,
                format!("every change already in {}", self.base_name),
            );
        }
        let Some(cfg) = filter.superseded else {
            return ContentVerdict::Miss(format!("changes not all in {}", self.base_name));
        };
        let cov = match self.coverage(tip) {
            Ok(c) => c,
            Err(e) => return ContentVerdict::Miss(format!("content check failed: {e}")),
        };
        let pct = (cov.in_base * 100).checked_div(cov.changed).unwrap_or(100);
        let summary = self.describe(&cov, pct);
        if !cov.absent.is_empty() {
            return ContentVerdict::Miss(summary);
        }
        if pct < usize::from(cfg.min_pct) {
            return ContentVerdict::Miss(summary);
        }
        if age_days < i64::from(cfg.min_days) {
            return ContentVerdict::Miss(format!("{summary}, but younger than {} days", cfg.min_days));
        }
        ContentVerdict::Hit(Category::Superseded, summary)
    }

    fn describe(&self, cov: &Coverage, pct: usize) -> String {
        let missing = cov.changed - cov.in_base;
        let mut s = format!(
            "{pct}% of {} changed lines in {}",
            cov.changed, self.base_name
        );
        if !cov.absent.is_empty() {
            let shown: Vec<&str> = cov.absent.iter().take(3).map(String::as_str).collect();
            s.push_str(&format!(
                ", {} file{} not in {}: {}{}",
                cov.absent.len(),
                if cov.absent.len() == 1 { "" } else { "s" },
                self.base_name,
                shown.join(", "),
                if cov.absent.len() > 3 { ", …" } else { "" }
            ));
        } else if missing > 0 {
            let shown: Vec<String> = cov
                .gaps
                .iter()
                .take(2)
                .map(|(f, n)| format!("{f} {n}"))
                .collect();
            s.push_str(&format!(
                ", {missing} differ (most in {}{})",
                shown.join(", "),
                if cov.gaps.len() > 2 { ", …" } else { "" }
            ));
        }
        s
    }

    /// True when merging the tip into the base changes nothing: the merge
    /// result is the base tree. None when `git merge-tree --write-tree`
    /// (git 2.38+) is unavailable.
    fn contained(&mut self, tip: Oid) -> Option<bool> {
        if !self.merge_tree_ok {
            return None;
        }
        let out = Command::new("git")
            .args(["merge-tree", "--write-tree", &self.base_oid.to_string(), &tip.to_string()])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        match out.status.code() {
            Some(0) => {
                let tree = String::from_utf8_lossy(&out.stdout);
                let tree = tree.lines().next().unwrap_or("").trim();
                Some(tree == self.base_tree.id().to_string())
            }
            Some(1) => Some(false), // conflicts
            _ => {
                dbg_log!("git merge-tree --write-tree unavailable; contained check off");
                self.merge_tree_ok = false;
                None
            }
        }
    }

    /// How much of merge-base..tip the base already has: an added line
    /// counts when the base's version of the file contains it, a removed
    /// line when the base's version no longer does.
    fn coverage(&mut self, tip: Oid) -> Result<Coverage> {
        let repo = self.repo;
        let mb = repo.merge_base(self.base_oid, tip)?;
        let from = repo.find_commit(mb)?.tree()?;
        let to = repo.find_commit(tip)?.tree()?;
        let mut opts = git2::DiffOptions::new();
        opts.context_lines(0);
        let diff = repo.diff_tree_to_tree(Some(&from), Some(&to), Some(&mut opts))?;

        let mut files: HashMap<PathBuf, FileChange> = HashMap::new();
        diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            let deleted = delta.status() == git2::Delta::Deleted;
            let file = if deleted { delta.old_file() } else { delta.new_file() };
            let Some(path) = file.path() else { return true };
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| DERIVED_FILES.contains(&n))
            {
                return true;
            }
            let entry = files.entry(path.to_path_buf()).or_insert_with(|| FileChange {
                tip_blob: (!deleted).then(|| delta.new_file().id()),
                ..Default::default()
            });
            let text = || String::from_utf8_lossy(line.content()).trim().to_string();
            match line.origin_value() {
                git2::DiffLineType::Addition => {
                    let t = text();
                    if t.len() >= MIN_LINE_LEN {
                        entry.added.push(t);
                    }
                }
                git2::DiffLineType::Deletion => {
                    let t = text();
                    if t.len() >= MIN_LINE_LEN {
                        entry.removed.push(t);
                    }
                }
                git2::DiffLineType::Binary => entry.binary = true,
                _ => {}
            }
            true
        })?;

        let mut cov = Coverage { changed: 0, in_base: 0, gaps: Vec::new(), absent: Vec::new() };
        for (path, fc) in files {
            let display = path.to_string_lossy().to_string();
            let base_blob = self.base_tree.get_path(&path).ok().map(|e| e.id());
            let weight = (fc.added.len() + fc.removed.len()).max(usize::from(fc.binary));
            if weight == 0 {
                continue;
            }
            cov.changed += weight;
            // Same final content, or deleted on both sides: fully in base.
            if fc.tip_blob == base_blob {
                cov.in_base += weight;
                continue;
            }
            if fc.tip_blob.is_some() && base_blob.is_none() {
                cov.absent.push(display.clone());
                cov.gaps.push((display, weight));
                continue;
            }
            if fc.binary {
                cov.gaps.push((display, weight));
                continue;
            }
            let base_lines = self.base_lines(&path, base_blob);
            let hits = match base_lines {
                Some(set) => {
                    fc.added.iter().filter(|l| set.contains(*l)).count()
                        + fc.removed.iter().filter(|l| !set.contains(*l)).count()
                }
                // Base deleted the file too: the branch's removals are in.
                None => fc.removed.len(),
            };
            cov.in_base += hits;
            if hits < weight {
                cov.gaps.push((display, weight - hits));
            }
        }
        cov.gaps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        cov.absent.sort();
        Ok(cov)
    }

    fn base_lines(&mut self, path: &Path, blob: Option<Oid>) -> Option<&HashSet<String>> {
        let repo = self.repo;
        self.lines
            .entry(path.to_path_buf())
            .or_insert_with(|| {
                let blob = repo.find_blob(blob?).ok()?;
                Some(
                    String::from_utf8_lossy(blob.content())
                        .lines()
                        .map(|l| l.trim().to_string())
                        .collect(),
                )
            })
            .as_ref()
    }
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

        let tip = branch_tip(repo, name);
        let outcome = delete_one(repo, name);
        match outcome {
            Ok(()) => {
                dbg_log!("  ok ({:?})", t.elapsed());
                results.push((with_tip(name, tip), None));
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

fn branch_tip(repo: &Repository, name: &str) -> Option<Oid> {
    repo.find_branch(name, BranchType::Local).ok()?.get().target()
}

/// Deletion label with the tip, so `git branch <name> <sha>` can undo it.
fn with_tip(label: &str, tip: Option<Oid>) -> String {
    match tip {
        Some(oid) => format!("{label} (was {})", short_oid(oid)),
        None => label.to_string(),
    }
}

fn delete_one(repo: &Repository, name: &str) -> std::result::Result<(), git2::Error> {
    let mut b = repo.find_branch(name, BranchType::Local)?;
    b.delete()?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<Oid>,
    pub locked: bool,
    pub prunable: bool,
    pub is_main: bool,
}

fn list_worktrees() -> Vec<WorktreeInfo> {
    let out = match Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut result: Vec<WorktreeInfo> = Vec::new();
    let mut cur: Option<WorktreeInfo> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(wt) = cur.take() {
                result.push(wt);
            }
            cur = Some(WorktreeInfo {
                path: p.to_string(),
                branch: None,
                head: None,
                locked: false,
                prunable: false,
                is_main: result.is_empty(),
            });
        } else if let Some(wt) = cur.as_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                wt.head = Oid::from_str(h).ok();
            } else if let Some(b) = line.strip_prefix("branch ") {
                wt.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            } else if line == "locked" || line.starts_with("locked ") {
                wt.locked = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                wt.prunable = true;
            }
        }
    }
    if let Some(wt) = cur.take() {
        result.push(wt);
    }
    result
}

fn list_worktree_branches() -> HashMap<String, String> {
    list_worktrees()
        .into_iter()
        .filter(|wt| !wt.is_main)
        .filter_map(|wt| wt.branch.map(|b| (b, wt.path)))
        .collect()
}

#[derive(Debug, Clone)]
pub struct WorktreeCandidate {
    pub path: String,
    pub branch: Option<String>,
    /// None means prunable (the directory is gone), not tied to a branch category.
    pub category: Option<Category>,
    pub detail: String,
    /// Reason this worktree is listed but not removable (dirty, locked, ...).
    pub blocked: Option<String>,
    pub prunable: bool,
    /// Checked-out commit, printed on removal so it can be recovered.
    pub head: Option<Oid>,
}

/// What `scan_worktrees` needs beyond the branch candidates.
pub struct WorktreeScanCtx<'a> {
    /// Directory git-cleanup was launched from; a worktree containing it
    /// is never removed.
    pub launch_dir: Option<&'a Path>,
    pub main_name: &'a str,
    pub remote: &'a str,
    pub filter: Filter,
}

/// Find stale worktrees: prunable ones, worktrees whose checked-out
/// branch is itself a deletion candidate, and detached worktrees whose
/// HEAD is merged into main or whose content main already has.
pub fn scan_worktrees(
    repo: &Repository,
    candidates: &[Candidate],
    ctx: &WorktreeScanCtx<'_>,
) -> Vec<WorktreeCandidate> {
    let by_branch: HashMap<&str, &Candidate> =
        candidates.iter().map(|c| (c.name.as_str(), c)).collect();
    let cwd = ctx.launch_dir.and_then(|p| p.canonicalize().ok());
    let blocked_reason = |wt: &WorktreeInfo| -> Option<String> {
        if wt.locked {
            Some("locked".to_string())
        } else if cwd
            .as_ref()
            .zip(Path::new(&wt.path).canonicalize().ok())
            .is_some_and(|(c, w)| c.starts_with(w))
        {
            Some("current directory".to_string())
        } else if worktree_is_dirty(&wt.path) {
            Some("uncommitted changes".to_string())
        } else {
            None
        }
    };
    let mut probe: Option<ContentProbe<'_>> = None;

    let mut result = Vec::new();
    for wt in list_worktrees() {
        if wt.is_main {
            continue;
        }

        if wt.branch.is_none() && !wt.prunable {
            let Some(head) = wt.head else { continue };
            if probe.is_none() {
                let main_oid = repo
                    .find_branch(ctx.main_name, BranchType::Local)
                    .ok()
                    .and_then(|b| b.get().target());
                if let Some(main_oid) = main_oid {
                    let (name, oid) = content_base(repo, ctx.main_name, ctx.remote, main_oid);
                    probe = ContentProbe::new(repo, name, oid).ok();
                }
            }
            let Some(p) = probe.as_mut() else { continue };
            let age = commit_age_days(repo, head).unwrap_or(0);
            let detail_head = format!("detached at {}", short_oid(head));
            match p.judge(head, age, ctx.filter) {
                ContentVerdict::Hit(cat, detail) => {
                    let blocked = blocked_reason(&wt);
                    dbg_log!("worktree {}: detached, {} (blocked: {blocked:?})", wt.path, cat.label());
                    result.push(WorktreeCandidate {
                        path: wt.path,
                        branch: None,
                        category: Some(cat),
                        detail: format!("{detail_head}: {detail}"),
                        blocked,
                        prunable: false,
                        head: Some(head),
                    });
                }
                ContentVerdict::Miss(note) => {
                    dbg_log!("worktree {}: {detail_head}, kept ({note})", wt.path);
                }
            }
            continue;
        }

        if wt.prunable {
            dbg_log!("worktree {}: prunable", wt.path);
            // If the checked-out branch is itself a candidate, pruning
            // frees it for deletion too.
            let category = wt
                .branch
                .as_deref()
                .and_then(|b| by_branch.get(b))
                .map(|c| c.category);
            result.push(WorktreeCandidate {
                path: wt.path,
                branch: wt.branch,
                category,
                detail: "prunable (directory is gone)".to_string(),
                blocked: wt.locked.then(|| "locked".to_string()),
                prunable: true,
                head: wt.head,
            });
            continue;
        }

        let Some(cand) = wt.branch.as_deref().and_then(|b| by_branch.get(b)) else {
            dbg_log!("worktree {}: branch not a candidate, kept", wt.path);
            continue;
        };

        let blocked = blocked_reason(&wt);
        dbg_log!(
            "worktree {}: branch {} is {} (blocked: {:?})",
            wt.path,
            cand.name,
            cand.category.label(),
            blocked
        );
        result.push(WorktreeCandidate {
            path: wt.path,
            branch: wt.branch,
            category: Some(cand.category),
            detail: format!("branch {}: {}", cand.name, cand.detail),
            blocked,
            prunable: false,
            head: wt.head,
        });
    }
    result
}

fn worktree_is_dirty(path: &str) -> bool {
    match Command::new("git")
        .args(["-C", path, "status", "--porcelain"])
        .output()
    {
        Ok(o) if o.status.success() => !o.stdout.is_empty(),
        // If status can't run, err on the side of caution.
        _ => true,
    }
}

/// Remove one worktree via `git worktree remove` (no --force: git itself
/// refuses dirty or locked worktrees as a second line of defense).
pub fn remove_worktree_path(path: &str) -> (String, Option<String>) {
    let out = Command::new("git")
        .args(["worktree", "remove", path])
        .output();
    match out {
        Ok(o) if o.status.success() => (format!("worktree {path}"), None),
        Ok(o) => {
            let msg = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let msg = if msg.is_empty() { "git worktree remove failed".to_string() } else { msg };
            (format!("worktree {path}"), Some(msg))
        }
        Err(e) => (format!("worktree {path}"), Some(e.to_string())),
    }
}

/// Remove the selected worktrees; prunable ones go through
/// `git worktree prune`, live ones through `git worktree remove` followed
/// by deleting their (now free) branch.
pub fn remove_worktrees(
    repo: &Repository,
    selected: &[&WorktreeCandidate],
) -> Vec<(String, Option<String>)> {
    let mut results = Vec::new();
    let mut pruned = false;
    for wt in selected {
        let removed_ok = if wt.prunable {
            let err = if pruned {
                None
            } else {
                pruned = true;
                let out = Command::new("git").args(["worktree", "prune"]).output();
                match out {
                    Ok(o) if o.status.success() => None,
                    Ok(o) => Some(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                    Err(e) => Some(e.to_string()),
                }
            };
            let ok = err.is_none();
            results.push((format!("worktree {} (pruned)", wt.path), err));
            ok
        } else {
            let (label, err) = remove_worktree_path(&wt.path);
            let ok = err.is_none();
            let label = if ok && wt.branch.is_none() { with_tip(&label, wt.head) } else { label };
            results.push((label, err));
            ok
        };
        if !removed_ok {
            continue;
        }
        // Delete the freed branch only when it is itself a candidate.
        if let (Some(branch), Some(_)) = (&wt.branch, wt.category) {
            let tip = branch_tip(repo, branch);
            match delete_one(repo, branch) {
                Ok(()) => results.push((with_tip(&format!("branch {branch}"), tip), None)),
                Err(e) => {
                    results.push((format!("branch {branch}"), Some(e.message().to_string())))
                }
            }
        }
    }
    results
}
