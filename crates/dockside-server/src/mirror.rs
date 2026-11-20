//! Optional 1-way Git Mirror exporter (Disaster Recovery & transition safety net).
//!
//! Every landed `rev_id` on `main` can be mirrored as a clean single commit inside
//! a dedicated backup Git repository. Neither humans nor agents ever push/merge to it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use dockside_core::cas::CasStore;
use dockside_core::hash::hash_bytes;
use dockside_core::model::{FileEntry, RevisionRecord};
use dockside_core::tree::read_tree_flat;
use tracing::{info, warn};

use crate::error::{ServerError, ServerResult};

#[derive(Clone)]
pub struct GitMirror {
    dir: PathBuf,
}

impl GitMirror {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub async fn sync_revision(&self, cas: &CasStore, rev: &RevisionRecord) -> ServerResult<()> {
        let files = read_tree_flat(cas, &rev.tree_hash).await?;
        let dir = self.dir.clone();

        // Ensure standalone git repo exists (check `.git/HEAD`, not just `.git/`).
        std::fs::create_dir_all(&dir)?;
        let git_dir = dir.join(".git");
        if !git_dir.join("HEAD").is_file() {
            run_git_init(&dir)?;
            let _ = run_git_isolated(&dir, &["config", "user.name", "dockside-mirror"]);
            let _ = run_git_isolated(&dir, &["config", "user.email", "dockside-mirror@localhost"]);
        }

        // Remove stale files outside `.git/` safely (top-down walk so `.git/` is never entered!).
        clean_working_tree(&dir, &files)?;

        // Write only changed/missing files from CAS (fast delta sync on subsequent lands).
        for (rel_path, entry) in &files {
            let full = dir.join(rel_path);
            if full.is_file()
                && let Ok(existing) = std::fs::read(&full)
                && hash_bytes(&existing) == entry.hash
            {
                continue;
            }
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let bytes = cas.get_bytes(&entry.hash).await?;
            std::fs::write(&full, bytes)?;
        }

        run_git_isolated(&dir, &["add", "-A"])?;
        let cs_id = rev.cs_id.as_deref().unwrap_or("import");
        let msg = format!(
            "r{}: {} [cs:{}] [author:{}]",
            rev.rev_id, rev.summary, cs_id, rev.author
        );
        run_git_isolated(&dir, &["commit", "--allow-empty", "-m", &msg])?;
        info!(rev_id = rev.rev_id, dir = %dir.display(), "mirrored landed revision to 1-way git backup");
        Ok(())
    }
}

fn clean_working_tree(dir: &Path, expected: &BTreeMap<String, FileEntry>) -> ServerResult<()> {
    let mut stale_files = Vec::new();
    let mut candidate_dirs = Vec::new();

    // IMPORTANT: Do NOT use `contents_first(true)` with `filter_entry`, because
    // `walkdir` visits children before filtering the parent when `contents_first` is enabled.
    for entry in walkdir::WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| e.file_name().to_str() != Some(".git"))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("mirror walk warning: {e}");
                continue;
            }
        };
        let rel = match entry.path().strip_prefix(dir) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if entry.file_type().is_file() {
            if !expected.contains_key(&rel) {
                stale_files.push(entry.path().to_path_buf());
            }
        } else if entry.file_type().is_dir() {
            candidate_dirs.push(entry.path().to_path_buf());
        }
    }

    for f in stale_files {
        let _ = std::fs::remove_file(f);
    }
    // Remove empty directories deepest-first.
    candidate_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for d in candidate_dirs {
        let _ = std::fs::remove_dir(d);
    }
    Ok(())
}

fn run_git_init(dir: &Path) -> ServerResult<()> {
    let out = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(ServerError::Internal(format!(
            "git init failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

/// Run git strictly pinned to `dir/.git` and `dir` so it can NEVER walk up into a parent repo.
fn run_git_isolated(dir: &Path, args: &[&str]) -> ServerResult<()> {
    let git_dir = dir.join(".git");
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_DIR", &git_dir)
        .env("GIT_WORK_TREE", dir)
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(ServerError::Internal(format!(
            "git {:?} failed: {}",
            args,
            stderr.trim()
        )));
    }
    Ok(())
}
