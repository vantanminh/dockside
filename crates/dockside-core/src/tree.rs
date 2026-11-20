//! Immutable Merkle directory trees stored inside CAS.
//!
//! Unchanged subtrees share identical BLAKE3 hashes across revisions, and
//! sparse scope checkouts only traverse directories matching requested prefixes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cas::CasStore;
use crate::error::{CoreError, CoreResult};
use crate::hash::{normalize_path, normalize_scope_prefix};
use crate::model::{ChangesetOp, FileEntry};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeNode {
    pub dirs: BTreeMap<String, String>,
    pub files: BTreeMap<String, FileEntry>,
}

#[derive(Default)]
struct BuildNode {
    dirs: BTreeMap<String, BuildNode>,
    files: BTreeMap<String, FileEntry>,
}

impl BuildNode {
    fn insert(&mut self, segments: &[&str], entry: FileEntry) -> CoreResult<()> {
        let Some((first, rest)) = segments.split_first() else {
            return Err(CoreError::InvalidPath {
                path: String::new(),
                reason: "empty path segments".to_string(),
            });
        };
        if rest.is_empty() {
            self.files.insert((*first).to_string(), entry);
        } else {
            let child = self.dirs.entry((*first).to_string()).or_default();
            child.insert(rest, entry)?;
        }
        Ok(())
    }

    fn persist<'a>(
        &'a self,
        cas: &'a CasStore,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CoreResult<String>> + Send + 'a>> {
        Box::pin(async move {
            let mut node = TreeNode {
                dirs: BTreeMap::new(),
                files: self.files.clone(),
            };
            for (name, child) in &self.dirs {
                let child_hash = child.persist(cas).await?;
                node.dirs.insert(name.clone(), child_hash);
            }
            cas.put_json(&node).await
        })
    }
}

/// Persist a flat map of canonical paths into an immutable Merkle tree and return root hash.
pub async fn write_tree(cas: &CasStore, files: &BTreeMap<String, FileEntry>) -> CoreResult<String> {
    let mut root = BuildNode::default();
    for (raw_path, entry) in files {
        let norm = normalize_path(raw_path)?;
        let segs: Vec<&str> = norm.split('/').collect();
        root.insert(&segs, entry.clone())?;
    }
    root.persist(cas).await
}

/// Flatten an entire Merkle tree into a sorted map of POSIX relative paths.
pub async fn read_tree_flat(
    cas: &CasStore,
    root_hash: &str,
) -> CoreResult<BTreeMap<String, FileEntry>> {
    read_scoped_tree_flat(cas, root_hash, &[]).await
}

/// Flatten only paths matching at least one scope prefix (empty `scopes` reads the full tree).
pub async fn read_scoped_tree_flat(
    cas: &CasStore,
    root_hash: &str,
    scopes: &[String],
) -> CoreResult<BTreeMap<String, FileEntry>> {
    let mut norm_scopes = Vec::with_capacity(scopes.len());
    for s in scopes {
        let n = normalize_scope_prefix(s)?;
        if n.is_empty() {
            norm_scopes.clear();
            break;
        }
        norm_scopes.push(n);
    }

    let mut out = BTreeMap::new();
    walk_tree(cas, root_hash, String::new(), &norm_scopes, &mut out).await?;
    Ok(out)
}

fn prefix_matches_dir(dir_path: &str, scopes: &[String]) -> bool {
    if scopes.is_empty() || dir_path.is_empty() {
        return true;
    }
    scopes.iter().any(|scope| {
        dir_path == scope
            || dir_path.starts_with(&format!("{scope}/"))
            || scope.starts_with(&format!("{dir_path}/"))
    })
}

fn prefix_matches_file(file_path: &str, scopes: &[String]) -> bool {
    if scopes.is_empty() {
        return true;
    }
    scopes
        .iter()
        .any(|scope| file_path == scope || file_path.starts_with(&format!("{scope}/")))
}

fn walk_tree<'a>(
    cas: &'a CasStore,
    node_hash: &'a str,
    prefix: String,
    scopes: &'a [String],
    out: &'a mut BTreeMap<String, FileEntry>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = CoreResult<()>> + Send + 'a>> {
    Box::pin(async move {
        let node: TreeNode = cas.get_json(node_hash).await?;
        for (name, entry) in node.files {
            let full = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if prefix_matches_file(&full, scopes) {
                out.insert(full, entry);
            }
        }
        for (dir_name, child_hash) in node.dirs {
            let child_prefix = if prefix.is_empty() {
                dir_name
            } else {
                format!("{prefix}/{dir_name}")
            };
            if prefix_matches_dir(&child_prefix, scopes) {
                walk_tree(cas, &child_hash, child_prefix, scopes, out).await?;
            }
        }
        Ok(())
    })
}

/// Apply changeset operations onto a base flat tree, returning the new flat tree.
pub fn apply_ops(
    base: &BTreeMap<String, FileEntry>,
    ops: &[ChangesetOp],
) -> CoreResult<BTreeMap<String, FileEntry>> {
    let mut next = base.clone();
    for op in ops {
        match op {
            ChangesetOp::Put {
                path,
                hash,
                size,
                executable,
            } => {
                let norm = normalize_path(path)?;
                next.insert(
                    norm,
                    FileEntry {
                        hash: hash.clone(),
                        size: *size,
                        executable: *executable,
                    },
                );
            }
            ChangesetOp::Delete { path } => {
                let norm = normalize_path(path)?;
                next.remove(&norm);
            }
        }
    }
    Ok(next)
}

/// Compute minimal ops required to transform `old_tree` into `new_tree`.
pub fn diff_trees_to_ops(
    old_tree: &BTreeMap<String, FileEntry>,
    new_tree: &BTreeMap<String, FileEntry>,
) -> Vec<ChangesetOp> {
    let mut ops = Vec::new();
    for (path, new_entry) in new_tree {
        if old_tree.get(path) != Some(new_entry) {
            ops.push(ChangesetOp::Put {
                path: path.clone(),
                hash: new_entry.hash.clone(),
                size: new_entry.size,
                executable: new_entry.executable,
            });
        }
    }
    for path in old_tree.keys() {
        if !new_tree.contains_key(path) {
            ops.push(ChangesetOp::Delete { path: path.clone() });
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merkle_tree_roundtrip_and_sparse_scope() -> CoreResult<()> {
        let dir = tempfile::tempdir()?;
        let cas = CasStore::open(dir.path(), None)?;
        let h1 = cas.put_bytes(b"api").await?;
        let h2 = cas.put_bytes(b"web").await?;
        let h3 = cas.put_bytes(b"dash").await?;

        let mut files = BTreeMap::new();
        files.insert(
            "services/api/src/main.rs".to_string(),
            FileEntry {
                hash: h1,
                size: 3,
                executable: false,
            },
        );
        files.insert(
            "apps/web/app/root.tsx".to_string(),
            FileEntry {
                hash: h2,
                size: 3,
                executable: false,
            },
        );
        files.insert(
            "apps/dashboard/app/routes.ts".to_string(),
            FileEntry {
                hash: h3,
                size: 4,
                executable: false,
            },
        );

        let root = write_tree(&cas, &files).await?;
        let full = read_tree_flat(&cas, &root).await?;
        assert_eq!(full, files);

        let scoped = read_scoped_tree_flat(&cas, &root, &["apps/web".to_string()]).await?;
        assert_eq!(scoped.len(), 1);
        assert!(scoped.contains_key("apps/web/app/root.tsx"));
        Ok(())
    }
}
