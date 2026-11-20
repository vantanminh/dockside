//! Hot-file conflict neutralizers for monorepos:
//! 1. Atomic SQL migration numbering (`pending_<slug>.sql` -> `NNNN_<slug>.sql` on land).
//! 2. Applied migration immutability guard (applied migrations are never rewritten).
//! 3. Duplicate migration sequence detection.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{CoreError, CoreResult};
use crate::model::{ConflictFile, ConflictKind, FileEntry, MigrationRename};

/// Matches `NNNN_slug.sql` where `NNNN` is 4 ASCII digits and `slug` is `[a-z0-9_]+`.
pub fn parse_numbered_migration(file_name: &str) -> Option<(u32, &str)> {
    let stem = file_name.strip_suffix(".sql")?;
    let (num_str, slug) = stem.split_once('_')?;
    if num_str.len() != 4 || !num_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !is_valid_slug(slug) {
        return None;
    }
    let num = num_str.parse::<u32>().ok()?;
    Some((num, slug))
}

/// Matches `pending_<slug>.sql` where `slug` is `[a-z0-9_]+`.
pub fn parse_pending_migration(file_name: &str) -> Option<&str> {
    let stem = file_name.strip_suffix(".sql")?;
    let slug = stem.strip_prefix("pending_")?;
    if is_valid_slug(slug) {
        Some(slug)
    } else {
        None
    }
}

pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

pub struct MigrationHotfileOutcome {
    pub renames: Vec<MigrationRename>,
    pub conflicts: Vec<ConflictFile>,
}

/// Enforce migration safety and assign monotonic 4-digit sequence numbers to
/// any `pending_<slug>.sql` migrations at land time (inside the single-writer lock).
pub fn process_migrations_on_land(
    main_head: &BTreeMap<String, FileEntry>,
    merged: &mut BTreeMap<String, FileEntry>,
    migration_dirs: &[String],
) -> CoreResult<MigrationHotfileOutcome> {
    let mut renames = Vec::new();
    let mut conflicts = Vec::new();

    for dir in migration_dirs {
        let dir_prefix = format!("{}/", dir.trim_matches('/'));

        // 1. Immutability guard: existing NNNN_*.sql in main_head must never be modified or deleted.
        for (path, head_entry) in main_head {
            if let Some(name) = path.strip_prefix(&dir_prefix)
                && !name.contains('/')
                && parse_numbered_migration(name).is_some()
            {
                match merged.get(path) {
                    None => conflicts.push(ConflictFile {
                        path: path.clone(),
                        kind: ConflictKind::MigrationRule,
                        base_hash: Some(head_entry.hash.clone()),
                        ours_hash: None,
                        theirs_hash: Some(head_entry.hash.clone()),
                        base_text: None,
                        ours_text: None,
                        theirs_text: None,
                        merged_markers: None,
                        detail: "Applied SQL migrations cannot be deleted".to_string(),
                    }),
                    Some(cur) if cur.hash != head_entry.hash => conflicts.push(ConflictFile {
                        path: path.clone(),
                        kind: ConflictKind::MigrationRule,
                        base_hash: Some(head_entry.hash.clone()),
                        ours_hash: Some(cur.hash.clone()),
                        theirs_hash: Some(head_entry.hash.clone()),
                        base_text: None,
                        ours_text: None,
                        theirs_text: None,
                        merged_markers: None,
                        detail: "Applied SQL migrations are immutable (checksum mismatch risk)"
                            .to_string(),
                    }),
                    _ => {}
                }
            }
        }

        // 2. Scan all existing numbered migrations and collect pending_*.sql files in merged tree.
        let mut max_num: u32 = 0;
        let mut seen_nums: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        let mut pending_paths: Vec<(String, String)> = Vec::new(); // (full_path, slug)

        for path in merged.keys() {
            if let Some(name) = path.strip_prefix(&dir_prefix)
                && !name.contains('/')
            {
                if let Some((num, _)) = parse_numbered_migration(name) {
                    if num > max_num {
                        max_num = num;
                    }
                    seen_nums.entry(num).or_default().push(path.clone());
                } else if let Some(slug) = parse_pending_migration(name) {
                    pending_paths.push((path.clone(), slug.to_string()));
                }
            }
        }

        // 3. Reject duplicate manual NNNN_ prefixes (e.g. two agents both typed 0087_*).
        let mut already_flagged: BTreeSet<String> = BTreeSet::new();
        for (num, paths) in &seen_nums {
            if paths.len() > 1 {
                for p in paths {
                    if already_flagged.insert(p.clone()) {
                        conflicts.push(ConflictFile {
                            path: p.clone(),
                            kind: ConflictKind::MigrationRule,
                            base_hash: None,
                            ours_hash: merged.get(p).map(|e| e.hash.clone()),
                            theirs_hash: main_head.get(p).map(|e| e.hash.clone()),
                            base_text: None,
                            ours_text: None,
                            theirs_text: None,
                            merged_markers: None,
                            detail: format!(
                                "Duplicate migration number {num:04} ({}). Use pending_<slug>.sql instead.",
                                paths.join(", ")
                            ),
                        });
                    }
                }
            }
        }

        if !conflicts.is_empty() {
            continue;
        }

        // 4. Deterministically assign sequential 4-digit numbers to pending_*.sql files.
        pending_paths.sort_by(|a, b| a.1.cmp(&b.1));
        for (old_path, slug) in pending_paths {
            max_num = max_num
                .checked_add(1)
                .ok_or_else(|| CoreError::MigrationViolation("migration number overflow".into()))?;
            if max_num > 9999 {
                return Err(CoreError::MigrationViolation(
                    "migration number exceeded 9999".into(),
                ));
            }
            let new_path = format!("{dir_prefix}{max_num:04}_{slug}.sql");
            if let Some(entry) = merged.remove(&old_path) {
                merged.insert(new_path.clone(), entry);
                renames.push(MigrationRename {
                    from: old_path,
                    to: new_path,
                });
            }
        }
    }

    Ok(MigrationHotfileOutcome { renames, conflicts })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(h: &str) -> FileEntry {
        FileEntry {
            hash: h.to_string(),
            size: 10,
            executable: false,
        }
    }

    #[test]
    fn auto_numbers_pending_migrations_sequentially() -> CoreResult<()> {
        let mut head = BTreeMap::new();
        head.insert(
            "services/api/src/db/migrations/0086_platform_maintenance.sql".to_string(),
            entry("aaa"),
        );

        let mut merged = head.clone();
        merged.insert(
            "services/api/src/db/migrations/pending_add_tags.sql".to_string(),
            entry("bbb"),
        );
        merged.insert(
            "services/api/src/db/migrations/pending_alpha_index.sql".to_string(),
            entry("ccc"),
        );

        let dirs = vec!["services/api/src/db/migrations".to_string()];
        let out = process_migrations_on_land(&head, &mut merged, &dirs)?;
        assert!(out.conflicts.is_empty());
        assert_eq!(out.renames.len(), 2);
        assert!(merged.contains_key("services/api/src/db/migrations/0087_add_tags.sql"));
        assert!(merged.contains_key("services/api/src/db/migrations/0088_alpha_index.sql"));
        assert!(!merged.contains_key("services/api/src/db/migrations/pending_add_tags.sql"));
        Ok(())
    }

    #[test]
    fn blocks_editing_applied_migration() -> CoreResult<()> {
        let mut head = BTreeMap::new();
        head.insert(
            "services/api/src/db/migrations/0086_platform_maintenance.sql".to_string(),
            entry("aaa"),
        );
        let mut merged = BTreeMap::new();
        merged.insert(
            "services/api/src/db/migrations/0086_platform_maintenance.sql".to_string(),
            entry("modified"),
        );
        let dirs = vec!["services/api/src/db/migrations".to_string()];
        let out = process_migrations_on_land(&head, &mut merged, &dirs)?;
        assert_eq!(out.conflicts.len(), 1);
        assert_eq!(out.conflicts[0].kind, ConflictKind::MigrationRule);
        Ok(())
    }
}
