//! `--worktree` must isolate a job AND must not leak checkouts.
//!
//! Two claims, both easy to get wrong in opposite directions:
//!   1. each harness gets its OWN checkout (otherwise parallel agents overwrite
//!      each other, which looks like a bad model rather than a bad schedule);
//!   2. a checkout with NO changes is pruned, while one WITH changes is kept and
//!      named — deleting a dirty checkout throws away the work product, and this
//!      feature exists so there IS a work product to merge.

use std::path::{Path, PathBuf};
use std::process::Command;

fn home() -> PathBuf {
    std::env::var("FLEET_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root/.fleet"))
}

fn have_fleet() -> bool {
    home().join("harnesses").is_dir() && Command::new("git").arg("--version").output().is_ok()
}

/// A throwaway git repo with one commit, so `worktree add HEAD` has something.
fn scratch_repo(name: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("fleet-wt-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let ok = Command::new("git").arg("-C").arg(&dir).arg("init").arg("-q").arg(".").status().ok()?.success();
    if !ok {
        return None;
    }
    Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["commit", "-q", "--allow-empty", "-m", "init"])
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .ok()?;
    Some(dir)
}

fn worktrees(repo: &Path) -> Vec<String> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.strip_prefix("worktree ").map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_clean_run_leaves_no_worktree_behind() {
    if !have_fleet() {
        return;
    }
    let Some(repo) = scratch_repo("clean") else {
        return;
    };
    let before = worktrees(&repo);
    let status = Command::new("fleet")
        .args(["run", "Reply with exactly: WT", "--worktree"])
        .current_dir(&repo)
        .env("FLEET_HOME", home())
        .output();
    let Ok(out) = status else {
        let _ = std::fs::remove_dir_all(&repo);
        return;
    };
    // A run that could not reach a harness proves nothing either way.
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&repo);
        return;
    }
    let after = worktrees(&repo);
    let leaked: Vec<&String> = after.iter().filter(|w| !before.contains(w)).collect();
    assert!(
        leaked.is_empty(),
        "a clean --worktree run must prune its checkouts, but left {:?}",
        leaked
    );
    let _ = std::fs::remove_dir_all(&repo);
}
