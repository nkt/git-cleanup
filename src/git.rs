use anyhow::{anyhow, bail, Context, Result};
use git2::{BranchType, ErrorCode, Oid, Repository};
use std::collections::HashSet;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Merged,
    Squashed,
    Gone,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Merged => "merged",
            Category::Squashed => "squashed",
            Category::Gone => "gone",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub category: Category,
    pub detail: String,
}

#[derive(Debug, Clone, Copy)]
pub struct Filter {
    pub merged: bool,
    pub squashed: bool,
    pub gone: bool,
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
    filter: Filter,
) -> Result<Vec<Candidate>> {
    let main_branch = repo.find_branch(main_name, BranchType::Local)?;
    let main_oid = main_branch
        .get()
        .target()
        .ok_or_else(|| anyhow!("main branch '{main_name}' has no target"))?;

    let current = current_branch(repo)?;
    let protected: HashSet<&str> = ["main", "master", "develop", "trunk", "HEAD"]
        .into_iter()
        .collect();

    let mut squashed_cache: Option<HashSet<Oid>> = None;
    let mut candidates = Vec::new();

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
            continue;
        }
        if current.as_deref() == Some(name.as_str()) {
            continue;
        }

        let oid = match branch.get().target() {
            Some(o) => o,
            None => continue,
        };

        if filter.merged {
            let is_merged = oid == main_oid
                || repo.graph_descendant_of(main_oid, oid).unwrap_or(false);
            if is_merged {
                candidates.push(Candidate {
                    name,
                    category: Category::Merged,
                    detail: format!("merged into {main_name}"),
                });
                continue;
            }
        }

        if filter.gone {
            if let Some(refname) = branch.get().name() {
                if let Ok(buf) = repo.branch_upstream_name(refname) {
                    if let Some(up) = buf.as_str() {
                        if repo.find_reference(up).is_err() {
                            let display = up
                                .strip_prefix("refs/remotes/")
                                .or_else(|| up.strip_prefix("refs/heads/"))
                                .unwrap_or(up);
                            candidates.push(Candidate {
                                name,
                                category: Category::Gone,
                                detail: format!("upstream {display} is gone"),
                            });
                            continue;
                        }
                    }
                }
            }
        }

        if filter.squashed {
            if let Some(pid) = branch_patchid(repo, main_oid, oid)? {
                if squashed_cache.is_none() {
                    squashed_cache = Some(main_patchids(repo, main_oid)?);
                }
                let cache = squashed_cache.as_ref().unwrap();
                if cache.contains(&pid) {
                    candidates.push(Candidate {
                        name,
                        category: Category::Squashed,
                        detail: format!(
                            "squashed into {main_name} (patch-id {})",
                            short_oid(pid)
                        ),
                    });
                    continue;
                }
            }
        }
    }

    Ok(candidates)
}

fn short_oid(oid: Oid) -> String {
    oid.to_string().chars().take(7).collect()
}

fn branch_patchid(
    repo: &Repository,
    main_oid: Oid,
    branch_oid: Oid,
) -> Result<Option<Oid>> {
    let mb = match repo.merge_base(main_oid, branch_oid) {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    if mb == branch_oid {
        return Ok(None);
    }
    let base_tree = repo.find_commit(mb)?.tree()?;
    let branch_tree = repo.find_commit(branch_oid)?.tree()?;
    if base_tree.id() == branch_tree.id() {
        return Ok(None);
    }
    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&branch_tree), None)?;
    Ok(diff.patchid(None).ok())
}

fn main_patchids(repo: &Repository, main_oid: Oid) -> Result<HashSet<Oid>> {
    let mut walk = repo.revwalk()?;
    walk.push(main_oid)?;
    let mut set = HashSet::new();
    for oid in walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        if commit.parent_count() == 0 {
            continue;
        }
        let parent = commit.parent(0)?;
        let diff = repo.diff_tree_to_tree(
            Some(&parent.tree()?),
            Some(&commit.tree()?),
            None,
        )?;
        if let Ok(pid) = diff.patchid(None) {
            set.insert(pid);
        }
    }
    Ok(set)
}

pub fn delete_branches(
    repo: &Repository,
    names: &[String],
) -> Vec<(String, Option<String>)> {
    let mut results = Vec::with_capacity(names.len());
    for name in names {
        let outcome = (|| -> Result<()> {
            let mut b = repo.find_branch(name, BranchType::Local)?;
            b.delete()?;
            Ok(())
        })();
        match outcome {
            Ok(()) => results.push((name.clone(), None)),
            Err(e) => results.push((name.clone(), Some(e.to_string()))),
        }
    }
    results
}
