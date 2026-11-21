//! Server-side 3-way merge engine + unified diff utilities.
//!
//! Combines `diffy` line-level 3-way merge with a safe sorted-addition auto-merger
//! for configured registry hot-files (`routes/mod.rs`, `app/routes.ts`).

use std::collections::{BTreeMap, BTreeSet};

use crate::cas::CasStore;
use crate::error::CoreResult;
use crate::model::{ConflictFile, ConflictKind, FileEntry};
use crate::policy::PolicyEngine;

#[derive(Debug)]
pub enum MergeOutcome {
    Clean {
        tree: BTreeMap<String, FileEntry>,
        auto_merged_paths: Vec<String>,
    },
    Conflicts(Vec<ConflictFile>),
}

/// Produce a standard unified diff string between two UTF-8 texts.
pub fn unified_diff(path: &str, old_text: &str, new_text: &str) -> String {
    diffy::DiffOptions::new()
        .set_original_filename(format!("a/{path}"))
        .set_modified_filename(format!("b/{path}"))
        .create_patch(old_text, new_text)
        .to_string()
}

/// Perform a 3-way merge of `ours` (agent changeset tree) onto `theirs` (`main` head tree)
/// relative to their common ancestor `base`.
pub async fn three_way_merge_trees(
    base: &BTreeMap<String, FileEntry>,
    theirs: &BTreeMap<String, FileEntry>,
    ours: &BTreeMap<String, FileEntry>,
    cas: &CasStore,
    policy: &PolicyEngine,
) -> CoreResult<MergeOutcome> {
    let mut all_paths: BTreeSet<&String> = BTreeSet::new();
    all_paths.extend(base.keys());
    all_paths.extend(theirs.keys());
    all_paths.extend(ours.keys());

    let mut merged = BTreeMap::new();
    let mut auto_merged_paths = Vec::new();
    let mut conflicts = Vec::new();

    for path in all_paths {
        let b = base.get(path);
        let t = theirs.get(path);
        let o = ours.get(path);

        // Fast paths (identical hashes / single-side edit / convergent edit)
        if o == b {
            if let Some(entry) = t {
                merged.insert(path.clone(), entry.clone());
            }
            continue;
        }
        if t == b {
            if let Some(entry) = o {
                merged.insert(path.clone(), entry.clone());
            }
            continue;
        }
        if o == t {
            if let Some(entry) = o {
                merged.insert(path.clone(), entry.clone());
            }
            continue;
        }

        // Both sides diverged on `path`
        match (b, o, t) {
            (Some(be), Some(oe), None) => {
                conflicts.push(
                    build_conflict(
                        path,
                        ConflictKind::ModifyDelete,
                        Some(be),
                        Some(oe),
                        None,
                        None,
                        "Agent modified file that was deleted on main".to_string(),
                        cas,
                    )
                    .await?,
                );
            }
            (Some(be), None, Some(te)) => {
                conflicts.push(
                    build_conflict(
                        path,
                        ConflictKind::ModifyDelete,
                        Some(be),
                        None,
                        Some(te),
                        None,
                        "Agent deleted file that was modified on main".to_string(),
                        cas,
                    )
                    .await?,
                );
            }
            (base_opt, Some(oe), Some(te)) => {
                let base_bytes = match base_opt {
                    Some(be) => cas.get_bytes(&be.hash).await?,
                    None => Vec::new(),
                };
                let ours_bytes = cas.get_bytes(&oe.hash).await?;
                let theirs_bytes = cas.get_bytes(&te.hash).await?;

                if is_binary(&base_bytes) || is_binary(&ours_bytes) || is_binary(&theirs_bytes) {
                    conflicts.push(ConflictFile {
                        path: path.clone(),
                        kind: ConflictKind::Binary,
                        base_hash: base_opt.map(|e| e.hash.clone()),
                        ours_hash: Some(oe.hash.clone()),
                        theirs_hash: Some(te.hash.clone()),
                        base_text: None,
                        ours_text: None,
                        theirs_text: None,
                        merged_markers: None,
                        detail: "Binary file diverged on both main and agent changeset".to_string(),
                    });
                    continue;
                }

                let base_str = String::from_utf8_lossy(&base_bytes).replace("\r\n", "\n");
                let ours_str = String::from_utf8_lossy(&ours_bytes).replace("\r\n", "\n");
                let theirs_str = String::from_utf8_lossy(&theirs_bytes).replace("\r\n", "\n");

                match diffy::merge(&base_str, &ours_str, &theirs_str) {
                    Ok(merged_text) => {
                        let raw = merged_text.into_bytes();
                        let size = raw.len() as u64;
                        let hash = cas.put_bytes(&raw).await?;
                        merged.insert(
                            path.clone(),
                            FileEntry {
                                hash,
                                size,
                                executable: oe.executable || te.executable,
                            },
                        );
                        auto_merged_paths.push(path.clone());
                    }
                    Err(markers) => {
                        // Try safe pure-insertion merge on configured hot registry files
                        if policy.is_sorted_merge_candidate(path)
                            && let Some(clean_text) =
                                try_pure_addition_merge(&base_str, &ours_str, &theirs_str)
                        {
                            let raw = clean_text.into_bytes();
                            let size = raw.len() as u64;
                            let hash = cas.put_bytes(&raw).await?;
                            merged.insert(
                                path.clone(),
                                FileEntry {
                                    hash,
                                    size,
                                    executable: oe.executable || te.executable,
                                },
                            );
                            auto_merged_paths.push(path.clone());
                            continue;
                        }

                        conflicts.push(ConflictFile {
                            path: path.clone(),
                            kind: ConflictKind::Content,
                            base_hash: base_opt.map(|e| e.hash.clone()),
                            ours_hash: Some(oe.hash.clone()),
                            theirs_hash: Some(te.hash.clone()),
                            base_text: Some(base_str),
                            ours_text: Some(ours_str),
                            theirs_text: Some(theirs_str),
                            merged_markers: Some(markers),
                            detail: "Overlapping text edits require conflict resolution"
                                .to_string(),
                        });
                    }
                }
            }
            (None, None, None)
            | (Some(_), None, None)
            | (None, Some(_), None)
            | (None, None, Some(_)) => {}
        }
    }

    if conflicts.is_empty() {
        Ok(MergeOutcome::Clean {
            tree: merged,
            auto_merged_paths,
        })
    } else {
        Ok(MergeOutcome::Conflicts(conflicts))
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0) || std::str::from_utf8(bytes).is_err()
}

async fn load_text_entry(cas: &CasStore, opt: Option<&FileEntry>) -> CoreResult<Option<String>> {
    match opt {
        Some(e) => {
            let bytes = cas.get_bytes(&e.hash).await?;
            if is_binary(&bytes) {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
            }
        }
        None => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_conflict(
    path: &str,
    kind: ConflictKind,
    base: Option<&FileEntry>,
    ours: Option<&FileEntry>,
    theirs: Option<&FileEntry>,
    merged_markers: Option<String>,
    detail: String,
    cas: &CasStore,
) -> CoreResult<ConflictFile> {
    Ok(ConflictFile {
        path: path.to_string(),
        kind,
        base_hash: base.map(|e| e.hash.clone()),
        ours_hash: ours.map(|e| e.hash.clone()),
        theirs_hash: theirs.map(|e| e.hash.clone()),
        base_text: load_text_entry(cas, base).await?,
        ours_text: load_text_entry(cas, ours).await?,
        theirs_text: load_text_entry(cas, theirs).await?,
        merged_markers,
        detail,
    })
}

/// Safe auto-merger for registry files (`routes/mod.rs`, `app/routes.ts`):
/// If BOTH `ours` and `theirs` are strict supersets of `base` (pure line insertions,
/// zero deletions or edits of base lines), combine all insertions at each insertion gap
/// in deterministic lexicographical order (deduplicated).
fn try_pure_addition_merge(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let base_lines: Vec<&str> = base.lines().collect();
    let ours_gaps = extract_pure_insertions(&base_lines, &ours.lines().collect::<Vec<_>>())?;
    let theirs_gaps = extract_pure_insertions(&base_lines, &theirs.lines().collect::<Vec<_>>())?;

    let mut result: Vec<&str> = Vec::with_capacity(base_lines.len() + 8);
    for gap_idx in 0..=base_lines.len() {
        let mut additions: Vec<&str> = Vec::new();
        for &line in &theirs_gaps[gap_idx] {
            additions.push(line);
        }
        for &line in &ours_gaps[gap_idx] {
            if !additions.contains(&line) {
                additions.push(line);
            }
        }
        // Sort only when both sides inserted into the same gap to keep ordering deterministic.
        if !ours_gaps[gap_idx].is_empty() && !theirs_gaps[gap_idx].is_empty() {
            additions.sort_unstable();
        }
        result.extend(additions);
        if gap_idx < base_lines.len() {
            result.push(base_lines[gap_idx]);
        }
    }

    let mut out = result.join("\n");
    if base.ends_with('\n') || ours.ends_with('\n') || theirs.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// Verify `modified` preserves every line of `base` in exact order, returning the lines
/// inserted before each `base[i]` (and after the last base line at index `base.len()`).
fn extract_pure_insertions<'a>(base: &[&str], modified: &[&'a str]) -> Option<Vec<Vec<&'a str>>> {
    let mut gaps: Vec<Vec<&'a str>> = vec![Vec::new(); base.len() + 1];
    let mut b_idx = 0usize;

    for &line in modified {
        if b_idx < base.len() && line == base[b_idx] {
            b_idx += 1;
        } else {
            gaps[b_idx].push(line);
        }
    }

    if b_idx == base.len() {
        Some(gaps)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocksideConfig;

    #[test]
    fn unified_diff_has_one_file_header_pair() {
        assert_eq!(
            unified_diff("docs/guide.md", "Before\n", "After\n"),
            "--- a/docs/guide.md\n+++ b/docs/guide.md\n@@ -1 +1 @@\n-Before\n+After\n"
        );
    }

    #[tokio::test]
    async fn clean_three_way_merge_non_overlapping_hunks() -> CoreResult<()> {
        let dir = tempfile::tempdir()?;
        let cas = CasStore::open(dir.path(), None)?;
        let policy = PolicyEngine::from_config(&DocksideConfig::default_pilot())?;

        let base_txt = "line1\nline2\nline3\nline4\nline5\n";
        let theirs_txt = "line1_main\nline2\nline3\nline4\nline5\n";
        let ours_txt = "line1\nline2\nline3\nline4\nline5_agent\n";

        let hb = cas.put_bytes(base_txt.as_bytes()).await?;
        let ht = cas.put_bytes(theirs_txt.as_bytes()).await?;
        let ho = cas.put_bytes(ours_txt.as_bytes()).await?;

        let p = "apps/web/app/home.tsx".to_string();
        let base = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: hb,
                size: 1,
                executable: false,
            },
        )]);
        let theirs = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: ht,
                size: 1,
                executable: false,
            },
        )]);
        let ours = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: ho,
                size: 1,
                executable: false,
            },
        )]);

        let outcome = three_way_merge_trees(&base, &theirs, &ours, &cas, &policy).await?;
        match outcome {
            MergeOutcome::Clean {
                tree,
                auto_merged_paths,
            } => {
                assert_eq!(auto_merged_paths, vec![p.clone()]);
                let merged_bytes = cas.get_bytes(&tree[&p].hash).await?;
                assert_eq!(
                    String::from_utf8_lossy(&merged_bytes),
                    "line1_main\nline2\nline3\nline4\nline5_agent\n"
                );
            }
            MergeOutcome::Conflicts(c) => panic!("expected clean merge, got {c:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn auto_merges_parallel_route_additions_in_hot_registry_file() -> CoreResult<()> {
        let dir = tempfile::tempdir()?;
        let cas = CasStore::open(dir.path(), None)?;
        let policy = PolicyEngine::from_config(&DocksideConfig::default_pilot())?;

        let base_txt = "export default [\n  route(\"home\", \"routes/home.tsx\"),\n];\n";
        let theirs_txt = "export default [\n  route(\"home\", \"routes/home.tsx\"),\n  route(\"cdn\", \"routes/cdn.tsx\"),\n];\n";
        let ours_txt = "export default [\n  route(\"home\", \"routes/home.tsx\"),\n  route(\"billing\", \"routes/billing.tsx\"),\n];\n";

        let hb = cas.put_bytes(base_txt.as_bytes()).await?;
        let ht = cas.put_bytes(theirs_txt.as_bytes()).await?;
        let ho = cas.put_bytes(ours_txt.as_bytes()).await?;

        let p = "apps/web/app/routes.ts".to_string();
        let base = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: hb,
                size: 1,
                executable: false,
            },
        )]);
        let theirs = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: ht,
                size: 1,
                executable: false,
            },
        )]);
        let ours = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: ho,
                size: 1,
                executable: false,
            },
        )]);

        let outcome = three_way_merge_trees(&base, &theirs, &ours, &cas, &policy).await?;
        match outcome {
            MergeOutcome::Clean { tree, .. } => {
                let merged_bytes = cas.get_bytes(&tree[&p].hash).await?;
                let text = String::from_utf8_lossy(&merged_bytes);
                assert!(text.contains("routes/billing.tsx"));
                assert!(text.contains("routes/cdn.tsx"));
            }
            MergeOutcome::Conflicts(c) => panic!("expected auto-merged routes.ts, got {c:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn overlapping_edit_returns_structured_conflict_bundle() -> CoreResult<()> {
        let dir = tempfile::tempdir()?;
        let cas = CasStore::open(dir.path(), None)?;
        let policy = PolicyEngine::from_config(&DocksideConfig::default_pilot())?;

        let p = "apps/web/app/root.tsx".to_string();
        let hb = cas.put_bytes(b"const title = 'Acme';\n").await?;
        let ht = cas.put_bytes(b"const title = 'Acme Learn';\n").await?;
        let ho = cas.put_bytes(b"const title = 'Acme Studio';\n").await?;

        let base = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: hb,
                size: 1,
                executable: false,
            },
        )]);
        let theirs = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: ht,
                size: 1,
                executable: false,
            },
        )]);
        let ours = BTreeMap::from([(
            p.clone(),
            FileEntry {
                hash: ho,
                size: 1,
                executable: false,
            },
        )]);

        let outcome = three_way_merge_trees(&base, &theirs, &ours, &cas, &policy).await?;
        match outcome {
            MergeOutcome::Conflicts(c) => {
                assert_eq!(c.len(), 1);
                assert_eq!(c[0].kind, ConflictKind::Content);
                assert!(
                    c[0].merged_markers
                        .as_deref()
                        .unwrap_or("")
                        .contains("<<<<<<<")
                );
            }
            MergeOutcome::Clean { .. } => panic!("expected conflict"),
        }
        Ok(())
    }
}
