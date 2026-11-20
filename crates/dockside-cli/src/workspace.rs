use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use dockside_core::hash::{hash_bytes, normalize_path};
use dockside_core::merge::unified_diff;
use dockside_core::model::{ChangesetOp, ChangesetRecord, FileEntry};
use dockside_core::policy::PolicyEngine;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::client::{ApiClient, CliError, CliResult};

pub const WORKSPACE_META_FILE: &str = ".dockside-workspace.json";
pub const CONFLICT_SIDECAR_DIR: &str = ".dockside-conflicts";

const IGNORED_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".wrangler",
    ".react-router",
    ".expo",
    ".harness",
    "artifacts",
    "test-results",
    "playwright-report",
    "__pycache__",
    ".dockside-conflicts",
];

const IGNORED_FILE_SUFFIXES: &[&str] = &[
    ".tsbuildinfo",
    ".log",
    ".sqlite",
    ".sqlite-wal",
    ".sqlite-shm",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub cs_id: String,
    pub base_rev: i64,
    pub stack_parent_cs_id: Option<String>,
    pub scope_prefixes: Vec<String>,
    pub base_files: BTreeMap<String, FileEntry>,
}

#[derive(Debug, Default, Serialize)]
pub struct ScanReport {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    #[serde(skip_serializing)]
    pub ops: Vec<ChangesetOp>,
    #[serde(skip_serializing)]
    pub blobs_to_upload: BTreeMap<String, Vec<u8>>,
}

pub fn load_workspace_meta(ws_dir: &Path) -> CliResult<WorkspaceMeta> {
    let p = ws_dir.join(WORKSPACE_META_FILE);
    if !p.is_file() {
        return Err(CliError::Message(format!(
            "not a dockside workspace (missing {} in {})",
            WORKSPACE_META_FILE,
            ws_dir.display()
        )));
    }
    let text = std::fs::read_to_string(&p)?;
    Ok(serde_json::from_str(&text)?)
}

pub fn save_workspace_meta(ws_dir: &Path, meta: &WorkspaceMeta) -> CliResult<()> {
    std::fs::create_dir_all(ws_dir)?;
    let p = ws_dir.join(WORKSPACE_META_FILE);
    let json = serde_json::to_string_pretty(meta)?;
    std::fs::write(p, json)?;
    Ok(())
}

fn should_skip_dir(name: &str) -> bool {
    IGNORED_DIR_NAMES.contains(&name)
}

fn should_skip_file(name: &str) -> bool {
    if name == WORKSPACE_META_FILE {
        return true;
    }
    IGNORED_FILE_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Collect files from an existing local repository into a monorepo prefix for `dockside import`.
pub fn collect_import_dir(
    prefix: &str,
    src_dir: &Path,
    policy: &PolicyEngine,
    ops: &mut Vec<ChangesetOp>,
    blobs: &mut BTreeMap<String, Vec<u8>>,
    skipped_secrets: &mut Vec<String>,
) -> CliResult<usize> {
    if !src_dir.is_dir() {
        return Err(CliError::Message(format!(
            "import source directory does not exist: {}",
            src_dir.display()
        )));
    }
    let clean_prefix = prefix.trim_matches('/').replace('\\', "/");
    let mut count = 0usize;

    for entry in WalkDir::new(src_dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                let name = e.file_name().to_str().unwrap_or("");
                return !should_skip_dir(name);
            }
            true
        })
    {
        let entry = entry.map_err(|e| CliError::Message(format!("walk error: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_str().unwrap_or("");
        if should_skip_file(file_name) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(src_dir)
            .map_err(|e| CliError::Message(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let full_rel = if clean_prefix.is_empty() {
            rel
        } else {
            format!("{clean_prefix}/{rel}")
        };
        let norm = normalize_path(&full_rel)?;
        if policy.is_secret_blocked(&norm) {
            skipped_secrets.push(norm);
            continue;
        }

        let raw = std::fs::read(entry.path())?;
        let hash = hash_bytes(&raw);
        let size = raw.len() as u64;
        blobs.entry(hash.clone()).or_insert(raw);
        ops.push(ChangesetOp::Put {
            path: norm,
            hash,
            size,
            executable: false,
        });
        count += 1;
    }

    Ok(count)
}

/// Scan an open agent workspace against its recorded `base_files` manifest.
pub fn scan_workspace(
    ws_dir: &Path,
    meta: &WorkspaceMeta,
    policy: &PolicyEngine,
) -> CliResult<ScanReport> {
    let mut current: BTreeMap<String, (String, u64, Vec<u8>)> = BTreeMap::new();

    for entry in WalkDir::new(ws_dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                let name = e.file_name().to_str().unwrap_or("");
                return !should_skip_dir(name);
            }
            true
        })
    {
        let entry = entry.map_err(|e| CliError::Message(format!("walk error: {e}")))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let fname = entry.file_name().to_str().unwrap_or("");
        if should_skip_file(fname) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(ws_dir)
            .map_err(|e| CliError::Message(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let norm = normalize_path(&rel)?;
        if policy.is_secret_blocked(&norm) {
            return Err(CliError::Core(dockside_core::CoreError::SecretBlocked {
                path: norm,
            }));
        }
        let raw = std::fs::read(entry.path())?;
        let hash = hash_bytes(&raw);
        let size = raw.len() as u64;
        current.insert(norm, (hash, size, raw));
    }

    let mut report = ScanReport::default();

    for (path, (hash, size, raw)) in current {
        match meta.base_files.get(&path) {
            None => {
                report.added.push(path.clone());
                report.blobs_to_upload.entry(hash.clone()).or_insert(raw);
                report.ops.push(ChangesetOp::Put {
                    path,
                    hash,
                    size,
                    executable: false,
                });
            }
            Some(base_entry) if base_entry.hash != hash => {
                report.modified.push(path.clone());
                report.blobs_to_upload.entry(hash.clone()).or_insert(raw);
                report.ops.push(ChangesetOp::Put {
                    path,
                    hash,
                    size,
                    executable: base_entry.executable,
                });
            }
            Some(_) => {}
        }
    }

    for base_path in meta.base_files.keys() {
        let on_disk = ws_dir.join(base_path);
        if !on_disk.is_file() {
            report.deleted.push(base_path.clone());
            report.ops.push(ChangesetOp::Delete {
                path: base_path.clone(),
            });
        }
    }

    Ok(report)
}

/// Materialize a map of files from `dockside-server` into `dst_dir`.
pub async fn materialize_files(
    api: &ApiClient,
    dst_dir: &Path,
    files: &BTreeMap<String, FileEntry>,
) -> CliResult<usize> {
    std::fs::create_dir_all(dst_dir)?;
    let mut cache: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut written = 0usize;
    for (rel_path, entry) in files {
        let bytes = if let Some(existing) = cache.get(&entry.hash) {
            existing.clone()
        } else {
            let fetched = api.download_blob(&entry.hash).await?;
            cache.insert(entry.hash.clone(), fetched.clone());
            fetched
        };
        let target = dst_dir.join(rel_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, bytes)?;
        written += 1;
    }
    Ok(written)
}

/// Compute unified diff for all local edits inside a workspace against its base manifest.
pub async fn compute_workspace_diff(
    api: &ApiClient,
    meta: &WorkspaceMeta,
    report: &ScanReport,
) -> CliResult<String> {
    let mut out = String::new();
    for op in &report.ops {
        match op {
            ChangesetOp::Put { path, hash, .. } => {
                let old_bytes = match meta.base_files.get(path) {
                    Some(e) => api.download_blob(&e.hash).await?,
                    None => Vec::new(),
                };
                let new_bytes = report
                    .blobs_to_upload
                    .get(hash)
                    .cloned()
                    .unwrap_or_default();
                if old_bytes.contains(&0)
                    || new_bytes.contains(&0)
                    || std::str::from_utf8(&old_bytes).is_err()
                    || std::str::from_utf8(&new_bytes).is_err()
                {
                    out.push_str(&format!(
                        "Binary file changed: {path} ({} -> {} bytes)\n",
                        old_bytes.len(),
                        new_bytes.len()
                    ));
                } else {
                    let old_str = String::from_utf8_lossy(&old_bytes);
                    let new_str = String::from_utf8_lossy(&new_bytes);
                    out.push_str(&unified_diff(path, &old_str, &new_str));
                    out.push('\n');
                }
            }
            ChangesetOp::Delete { path } => {
                let old_bytes = match meta.base_files.get(path) {
                    Some(e) => api.download_blob(&e.hash).await?,
                    None => Vec::new(),
                };
                let old_str = String::from_utf8_lossy(&old_bytes);
                out.push_str(&unified_diff(path, &old_str, ""));
                out.push('\n');
            }
        }
    }
    Ok(out)
}

/// Materialize a clean conflict resolution workspace rooted at latest `main`,
/// placing conflict markers in conflicted files and `.base/.ours/.theirs` in `.dockside-conflicts/`.
pub async fn materialize_conflict_workspace(
    api: &ApiClient,
    cs: &ChangesetRecord,
    out_dir: &Path,
) -> CliResult<PathBuf> {
    let health = api.health().await?;
    let latest_rev = health.main_rev;
    let head_tree = api.get_tree(latest_rev, &cs.scope_prefixes).await?;

    if out_dir.exists() {
        std::fs::remove_dir_all(out_dir)?;
    }
    std::fs::create_dir_all(out_dir)?;

    let _ = materialize_files(api, out_dir, &head_tree.files).await?;

    // Apply non-conflicted agent ops cleanly on top of latest main
    let conflicted_paths: std::collections::BTreeSet<&str> =
        cs.conflicts.iter().map(|c| c.path.as_str()).collect();

    for op in &cs.ops {
        if conflicted_paths.contains(op.path()) {
            continue;
        }
        match op {
            ChangesetOp::Put { path, hash, .. } => {
                let bytes = api.download_blob(hash).await?;
                let dst = out_dir.join(path);
                if let Some(p) = dst.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::write(dst, bytes)?;
            }
            ChangesetOp::Delete { path } => {
                let dst = out_dir.join(path);
                if dst.is_file() {
                    let _ = std::fs::remove_file(dst);
                }
            }
        }
    }

    // Write conflict markers + sidecar 3-way files for resolver agent
    let sidecar_root = out_dir.join(CONFLICT_SIDECAR_DIR);
    std::fs::create_dir_all(&sidecar_root)?;
    for c in &cs.conflicts {
        if let Some(ref markers) = c.merged_markers {
            let dst = out_dir.join(&c.path);
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&dst, markers)?;
        }
        let safe_name = c.path.replace('/', "__");
        if let Some(ref txt) = c.base_text {
            std::fs::write(sidecar_root.join(format!("{safe_name}.base")), txt)?;
        }
        if let Some(ref txt) = c.ours_text {
            std::fs::write(sidecar_root.join(format!("{safe_name}.ours")), txt)?;
        }
        if let Some(ref txt) = c.theirs_text {
            std::fs::write(sidecar_root.join(format!("{safe_name}.theirs")), txt)?;
        }
    }

    save_workspace_meta(
        out_dir,
        &WorkspaceMeta {
            cs_id: cs.cs_id.clone(),
            base_rev: latest_rev,
            stack_parent_cs_id: cs.stack_parent_cs_id.clone(),
            scope_prefixes: cs.scope_prefixes.clone(),
            base_files: head_tree.files,
        },
    )?;

    Ok(out_dir.to_path_buf())
}
