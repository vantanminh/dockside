//! SQLite WAL metadata store for `dockside-server`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use dockside_core::model::{
    ChangesetOp, ChangesetRecord, ChangesetStatus, ConflictFile, DeploymentRecord, LeaseMode,
    LeaseRecord, RevisionRecord, SequenceNextResp, SnapshotRecord, VerifyReport,
};
use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{ServerError, ServerResult};

/// Flush WAL frames before a stopped-service volume copy. Never creates a database.
pub fn checkpoint(path: &Path) -> ServerResult<()> {
    checkpoint_with_timeout(path, std::time::Duration::from_secs(30))
}

fn checkpoint_with_timeout(path: &Path, timeout: std::time::Duration) -> ServerResult<()> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(timeout)?;
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(ServerError::Conflict(
            "SQLite checkpoint is busy; backup must not proceed".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn flushes_wal_and_rejects_missing_or_busy_databases() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("dockside.sqlite");
        assert!(checkpoint(&path).is_err());
        assert!(!path.exists());
        let writer = Connection::open(&path)?;
        writer.pragma_update(None, "journal_mode", "WAL")?;
        writer
            .execute_batch("CREATE TABLE items (value INTEGER); INSERT INTO items VALUES (1);")?;
        checkpoint(&path)?;
        assert_eq!(
            std::fs::metadata(dir.path().join("dockside.sqlite-wal"))?.len(),
            0
        );
        let reader = Connection::open(&path)?;
        reader.execute_batch("BEGIN; SELECT * FROM items;")?;
        writer.execute("INSERT INTO items VALUES (2)", [])?;
        assert!(matches!(
            checkpoint_with_timeout(&path, std::time::Duration::from_millis(5)),
            Err(ServerError::Conflict(_))
        ));
        reader.execute_batch("ROLLBACK")?;
        checkpoint(&path)?;
        let count: i64 = writer.query_row("SELECT count(*) FROM items", [], |row| row.get(0))?;
        assert_eq!(count, 2);
        std::fs::write(dir.path().join("invalid.sqlite"), b"not a database")?;
        assert!(checkpoint(&dir.path().join("invalid.sqlite")).is_err());
        Ok(())
    }
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn future_rfc3339(secs: i64) -> String {
    let dt = OffsetDateTime::now_utc() + time::Duration::seconds(secs.max(60));
    dt.format(&Rfc3339)
        .unwrap_or_else(|_| "2099-01-01T00:00:00Z".to_string())
}

impl Db {
    pub fn open(path: impl AsRef<Path>, empty_tree_hash: &str) -> ServerResult<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_schema(&conn, empty_tree_hash)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> ServerResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| ServerError::Internal("sqlite mutex poisoned".to_string()))
    }

    pub fn main_rev_id(&self) -> ServerResult<i64> {
        let conn = self.lock()?;
        let rev: i64 =
            conn.query_row("SELECT COALESCE(MAX(rev_id), 0) FROM revisions", [], |r| {
                r.get(0)
            })?;
        Ok(rev)
    }

    pub fn get_revision(&self, rev_id: i64) -> ServerResult<RevisionRecord> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT rev_id, parent_rev, tree_hash, cs_id, author, story_ids_json, summary,
                        touched_paths_json, verify_json, created_at
                 FROM revisions WHERE rev_id = ?1",
                params![rev_id],
                row_to_revision,
            )
            .optional()?;
        row.ok_or_else(|| ServerError::NotFound(format!("revision r{rev_id}")))
    }

    pub fn list_revisions(&self, limit: i64) -> ServerResult<Vec<RevisionRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT rev_id, parent_rev, tree_hash, cs_id, author, story_ids_json, summary,
                    touched_paths_json, verify_json, created_at
             FROM revisions ORDER BY rev_id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit.clamp(1, 500)], row_to_revision)?;
        let mut out = Vec::new();
        for item in rows {
            out.push(item?);
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_revision(
        &self,
        parent_rev: i64,
        tree_hash: &str,
        cs_id: Option<&str>,
        author: &str,
        story_ids: &[String],
        summary: &str,
        touched_paths: &[String],
        verify_report: Option<&VerifyReport>,
    ) -> ServerResult<RevisionRecord> {
        let conn = self.lock()?;
        // Strict compare-and-swap check: ensure parent_rev is truly the latest main revision.
        let current_head: i64 =
            conn.query_row("SELECT COALESCE(MAX(rev_id), 0) FROM revisions", [], |r| {
                r.get(0)
            })?;
        if current_head != parent_rev {
            return Err(ServerError::Conflict(format!(
                "CAS mismatch: expected main head r{parent_rev}, actual r{current_head}"
            )));
        }

        let new_rev = parent_rev + 1;
        let created_at = now_rfc3339();
        let story_json = serde_json::to_string(story_ids)?;
        let touched_json = serde_json::to_string(touched_paths)?;
        let verify_json = verify_report.map(serde_json::to_string).transpose()?;

        conn.execute(
            "INSERT INTO revisions (
                rev_id, parent_rev, tree_hash, cs_id, author, story_ids_json, summary,
                touched_paths_json, verify_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                new_rev,
                parent_rev,
                tree_hash,
                cs_id,
                author,
                story_json,
                summary,
                touched_json,
                verify_json,
                created_at
            ],
        )?;

        Ok(RevisionRecord {
            rev_id: new_rev,
            parent_rev: Some(parent_rev),
            tree_hash: tree_hash.to_string(),
            cs_id: cs_id.map(str::to_string),
            author: author.to_string(),
            story_ids: story_ids.to_vec(),
            summary: summary.to_string(),
            touched_paths: touched_paths.to_vec(),
            verify_report: verify_report.cloned(),
            created_at,
        })
    }

    pub fn next_sequence(&self, kind: &str) -> ServerResult<SequenceNextResp> {
        let clean_kind = kind.trim().to_ascii_lowercase();
        if clean_kind.is_empty()
            || !clean_kind
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(ServerError::BadRequest(format!(
                "invalid sequence kind '{kind}'"
            )));
        }
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sequences (kind, next_val) VALUES (?1, 1)
             ON CONFLICT(kind) DO UPDATE SET next_val = next_val + 1",
            params![clean_kind],
        )?;
        let val: i64 = conn.query_row(
            "SELECT next_val FROM sequences WHERE kind = ?1",
            params![clean_kind],
            |r| r.get(0),
        )?;
        let formatted = match clean_kind.as_str() {
            "changeset" => format!("CS-{val:04}"),
            "intake" => format!("IN-{val:03}"),
            "story" => format!("US-{val:03}"),
            "decision" => format!("DEC-{val:03}"),
            "report" => format!("RP-{val:03}"),
            other => format!("{}-{val:04}", other.to_ascii_uppercase()),
        };
        Ok(SequenceNextResp {
            kind: clean_kind,
            value: val,
            formatted,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_changeset(
        &self,
        cs_id: &str,
        base_rev: i64,
        stack_parent_cs_id: Option<&str>,
        author: &str,
        title: &str,
        story_ids: &[String],
        scope_prefixes: &[String],
        lease_mode: LeaseMode,
        lease_ttl_secs: i64,
    ) -> ServerResult<(ChangesetRecord, Vec<LeaseRecord>)> {
        let conn = self.lock()?;
        let now = now_rfc3339();
        let expires_at = future_rfc3339(lease_ttl_secs);

        // Clean up expired leases first
        conn.execute("DELETE FROM leases WHERE expires_at <= ?1", params![now])?;

        // Check overlapping active leases
        let active = load_active_leases(&conn)?;
        let mut warnings = Vec::new();
        for lease in active {
            if scope_prefixes
                .iter()
                .any(|s| prefixes_overlap(s, &lease.path_prefix))
            {
                if lease.mode == LeaseMode::Exclusive || lease_mode == LeaseMode::Exclusive {
                    return Err(ServerError::Conflict(format!(
                        "exclusive path lease conflict on '{}' held by {} ({})",
                        lease.path_prefix, lease.cs_id, lease.author
                    )));
                }
                warnings.push(lease);
            }
        }

        let story_json = serde_json::to_string(story_ids)?;
        let scope_json = serde_json::to_string(scope_prefixes)?;

        conn.execute(
            "INSERT INTO changesets (
                cs_id, base_rev, stack_parent_cs_id, author, title, story_ids_json, scope_json,
                status, approval_triggers_json, approved_by, landed_rev, error_message,
                ops_json, conflicts_json, verify_json, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', '[]', NULL, NULL, NULL, '[]', '[]', NULL, ?8, ?8)",
            params![
                cs_id,
                base_rev,
                stack_parent_cs_id,
                author,
                title,
                story_json,
                scope_json,
                now
            ],
        )?;

        for prefix in scope_prefixes {
            conn.execute(
                "INSERT OR REPLACE INTO leases (path_prefix, cs_id, author, mode, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![prefix, cs_id, author, lease_mode.as_str(), now, expires_at],
            )?;
        }

        let rec = ChangesetRecord {
            cs_id: cs_id.to_string(),
            base_rev,
            stack_parent_cs_id: stack_parent_cs_id.map(str::to_string),
            author: author.to_string(),
            title: title.to_string(),
            story_ids: story_ids.to_vec(),
            scope_prefixes: scope_prefixes.to_vec(),
            status: ChangesetStatus::Open,
            sensitive_paths: Vec::new(),
            landed_rev: None,
            error_message: None,
            ops: Vec::new(),
            conflicts: Vec::new(),
            verify_report: None,
            created_at: now.clone(),
            updated_at: now,
        };
        Ok((rec, warnings))
    }

    pub fn get_changeset(&self, cs_id: &str) -> ServerResult<ChangesetRecord> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT cs_id, base_rev, stack_parent_cs_id, author, title, story_ids_json, scope_json,
                        status, approval_triggers_json, approved_by, landed_rev, error_message,
                        ops_json, conflicts_json, verify_json, created_at, updated_at
                 FROM changesets WHERE cs_id = ?1",
                params![cs_id],
                row_to_changeset,
            )
            .optional()?;
        row.ok_or_else(|| ServerError::NotFound(format!("changeset {cs_id}")))
    }

    pub fn list_changesets(
        &self,
        status_filter: Option<ChangesetStatus>,
        limit: i64,
    ) -> ServerResult<Vec<ChangesetRecord>> {
        let conn = self.lock()?;
        let mut out = Vec::new();
        if let Some(st) = status_filter {
            let mut stmt = conn.prepare(
                "SELECT cs_id, base_rev, stack_parent_cs_id, author, title, story_ids_json, scope_json,
                        status, approval_triggers_json, approved_by, landed_rev, error_message,
                        ops_json, conflicts_json, verify_json, created_at, updated_at
                 FROM changesets WHERE status = ?1 ORDER BY updated_at DESC LIMIT ?2",
            )?;
            let rows =
                stmt.query_map(params![st.as_str(), limit.clamp(1, 500)], row_to_changeset)?;
            for r in rows {
                out.push(r?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT cs_id, base_rev, stack_parent_cs_id, author, title, story_ids_json, scope_json,
                        status, approval_triggers_json, approved_by, landed_rev, error_message,
                        ops_json, conflicts_json, verify_json, created_at, updated_at
                 FROM changesets ORDER BY updated_at DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit.clamp(1, 500)], row_to_changeset)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn list_queued_cs_ids(&self) -> ServerResult<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT cs_id FROM changesets WHERE status = 'queued' ORDER BY updated_at ASC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn queue_changeset_submission(
        &self,
        cs_id: &str,
        ops: &[ChangesetOp],
        new_base_rev: Option<i64>,
        new_title: Option<&str>,
    ) -> ServerResult<ChangesetRecord> {
        let conn = self.lock()?;
        let existing = conn
            .query_row(
                "SELECT status, base_rev, title FROM changesets WHERE cs_id = ?1",
                params![cs_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| ServerError::NotFound(format!("changeset {cs_id}")))?;

        let status = ChangesetStatus::parse(&existing.0).unwrap_or(ChangesetStatus::Open);
        if matches!(status, ChangesetStatus::Landed | ChangesetStatus::Abandoned) {
            return Err(ServerError::BadRequest(format!(
                "cannot submit changeset {cs_id} in state {}",
                status.as_str()
            )));
        }

        let base_rev = new_base_rev.unwrap_or(existing.1);
        let title = new_title.unwrap_or(&existing.2);
        let ops_json = serde_json::to_string(ops)?;
        let now = now_rfc3339();

        conn.execute(
            "UPDATE changesets
             SET base_rev = ?1, title = ?2, ops_json = ?3, status = 'queued',
                 conflicts_json = '[]', error_message = NULL, approved_by = NULL, updated_at = ?4
             WHERE cs_id = ?5",
            params![base_rev, title, ops_json, now, cs_id],
        )?;
        drop(conn);
        self.get_changeset(cs_id)
    }

    /// After a crash/restart, changesets stuck in `merging`/`verifying` (and legacy
    /// `awaiting_approval`) go back to `queued` so the integrator re-runs them automatically.
    pub fn requeue_interrupted(&self) -> ServerResult<usize> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE changesets SET status = 'queued', updated_at = ?1
             WHERE status IN ('merging', 'verifying', 'awaiting_approval')",
            params![now_rfc3339()],
        )?;
        conn.execute(
            "UPDATE changesets SET status = 'abandoned', updated_at = ?1 WHERE status = 'rejected'",
            params![now_rfc3339()],
        )?;
        Ok(n)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_changeset_state(
        &self,
        cs_id: &str,
        status: ChangesetStatus,
        sensitive_paths: Option<&[String]>,
        landed_rev: Option<i64>,
        error_message: Option<&str>,
        conflicts: Option<&[ConflictFile]>,
        verify_report: Option<&VerifyReport>,
    ) -> ServerResult<ChangesetRecord> {
        let conn = self.lock()?;
        let now = now_rfc3339();
        let triggers_json = sensitive_paths.map(serde_json::to_string).transpose()?;
        let conflicts_json = conflicts.map(serde_json::to_string).transpose()?;
        let verify_json = verify_report.map(serde_json::to_string).transpose()?;

        conn.execute(
            "UPDATE changesets
             SET status = ?1,
                 approval_triggers_json = COALESCE(?2, approval_triggers_json),
                 landed_rev = COALESCE(?3, landed_rev),
                 error_message = ?4,
                 conflicts_json = COALESCE(?5, conflicts_json),
                 verify_json = COALESCE(?6, verify_json),
                 updated_at = ?7
             WHERE cs_id = ?8",
            params![
                status.as_str(),
                triggers_json,
                landed_rev,
                error_message,
                conflicts_json,
                verify_json,
                now,
                cs_id
            ],
        )?;

        if matches!(status, ChangesetStatus::Landed | ChangesetStatus::Abandoned) {
            conn.execute("DELETE FROM leases WHERE cs_id = ?1", params![cs_id])?;
        }

        drop(conn);
        self.get_changeset(cs_id)
    }

    pub fn add_snapshot(
        &self,
        cs_id: &str,
        message: &str,
        ops: &[ChangesetOp],
    ) -> ServerResult<SnapshotRecord> {
        let _ = self.get_changeset(cs_id)?;
        let conn = self.lock()?;
        let ops_json = serde_json::to_string(ops)?;
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO snapshots (cs_id, message, ops_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![cs_id, message, ops_json, now],
        )?;
        let snap_id = conn.last_insert_rowid();
        Ok(SnapshotRecord {
            snap_id,
            cs_id: cs_id.to_string(),
            message: message.to_string(),
            ops: ops.to_vec(),
            created_at: now,
        })
    }

    pub fn list_snapshots(&self, cs_id: &str) -> ServerResult<Vec<SnapshotRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT snap_id, cs_id, message, ops_json, created_at
             FROM snapshots WHERE cs_id = ?1 ORDER BY snap_id DESC",
        )?;
        let rows = stmt.query_map(params![cs_id], |r| {
            let ops_str: String = r.get(3)?;
            let ops: Vec<ChangesetOp> = serde_json::from_str(&ops_str).unwrap_or_default();
            Ok(SnapshotRecord {
                snap_id: r.get(0)?,
                cs_id: r.get(1)?,
                message: r.get(2)?,
                ops,
                created_at: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for item in rows {
            out.push(item?);
        }
        Ok(out)
    }

    pub fn list_active_leases(&self) -> ServerResult<Vec<LeaseRecord>> {
        let conn = self.lock()?;
        let now = now_rfc3339();
        conn.execute("DELETE FROM leases WHERE expires_at <= ?1", params![now])?;
        load_active_leases(&conn)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_deployment(
        &self,
        target: &str,
        env: &str,
        rev_id: i64,
        artifact_ref: &str,
        status: &str,
        deployed_by: &str,
        notes: Option<&str>,
    ) -> ServerResult<DeploymentRecord> {
        let conn = self.lock()?;
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO deployments (target, env, rev_id, artifact_ref, status, deployed_by, notes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![target, env, rev_id, artifact_ref, status, deployed_by, notes, now],
        )?;
        let id = conn.last_insert_rowid();
        Ok(DeploymentRecord {
            id,
            target: target.to_string(),
            env: env.to_string(),
            rev_id,
            artifact_ref: artifact_ref.to_string(),
            status: status.to_string(),
            deployed_by: deployed_by.to_string(),
            notes: notes.map(str::to_string),
            created_at: now,
        })
    }

    /// Most recent healthy deployment (`deployed` / `rolled_back`) of a target+env, optionally
    /// excluding one revision (used to pick an automatic rollback target).
    pub fn last_healthy_deployment(
        &self,
        target: &str,
        env: &str,
        exclude_rev: Option<i64>,
    ) -> ServerResult<Option<DeploymentRecord>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT id, target, env, rev_id, artifact_ref, status, deployed_by, notes, created_at
                 FROM deployments
                 WHERE target = ?1 AND env = ?2 AND status IN ('deployed', 'rolled_back')
                   AND (?3 IS NULL OR rev_id != ?3)
                 ORDER BY id DESC LIMIT 1",
                params![target, env, exclude_rev],
                |r| {
                    Ok(DeploymentRecord {
                        id: r.get(0)?,
                        target: r.get(1)?,
                        env: r.get(2)?,
                        rev_id: r.get(3)?,
                        artifact_ref: r.get(4)?,
                        status: r.get(5)?,
                        deployed_by: r.get(6)?,
                        notes: r.get(7)?,
                        created_at: r.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Whether this exact artifact tag was ever successfully built+pushed for a target.
    pub fn has_healthy_artifact(&self, target: &str, artifact_ref: &str) -> ServerResult<bool> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM deployments
             WHERE target = ?1 AND artifact_ref = ?2 AND status IN ('deployed', 'rolled_back')",
            params![target, artifact_ref],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn list_deployments(&self, limit: i64) -> ServerResult<Vec<DeploymentRecord>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, target, env, rev_id, artifact_ref, status, deployed_by, notes, created_at
             FROM deployments ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit.clamp(1, 200)], |r| {
            Ok(DeploymentRecord {
                id: r.get(0)?,
                target: r.get(1)?,
                env: r.get(2)?,
                rev_id: r.get(3)?,
                artifact_ref: r.get(4)?,
                status: r.get(5)?,
                deployed_by: r.get(6)?,
                notes: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for item in rows {
            out.push(item?);
        }
        Ok(out)
    }
}

fn load_active_leases(conn: &Connection) -> ServerResult<Vec<LeaseRecord>> {
    let mut stmt = conn.prepare(
        "SELECT path_prefix, cs_id, author, mode, created_at, expires_at
         FROM leases ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let mode_str: String = r.get(3)?;
        Ok(LeaseRecord {
            path_prefix: r.get(0)?,
            cs_id: r.get(1)?,
            author: r.get(2)?,
            mode: LeaseMode::parse(&mode_str).unwrap_or(LeaseMode::Soft),
            created_at: r.get(4)?,
            expires_at: r.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for item in rows {
        out.push(item?);
    }
    Ok(out)
}

fn prefixes_overlap(a: &str, b: &str) -> bool {
    let a = a.trim_matches('/');
    let b = b.trim_matches('/');
    if a.is_empty() || b.is_empty() {
        return true;
    }
    a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/"))
}

fn row_to_revision(r: &rusqlite::Row<'_>) -> rusqlite::Result<RevisionRecord> {
    let story_str: String = r.get(5)?;
    let touched_str: String = r.get(7)?;
    let verify_str: Option<String> = r.get(8)?;
    Ok(RevisionRecord {
        rev_id: r.get(0)?,
        parent_rev: r.get(1)?,
        tree_hash: r.get(2)?,
        cs_id: r.get(3)?,
        author: r.get(4)?,
        story_ids: serde_json::from_str(&story_str).unwrap_or_default(),
        summary: r.get(6)?,
        touched_paths: serde_json::from_str(&touched_str).unwrap_or_default(),
        verify_report: verify_str.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get(9)?,
    })
}

fn row_to_changeset(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChangesetRecord> {
    let story_str: String = r.get(5)?;
    let scope_str: String = r.get(6)?;
    let status_str: String = r.get(7)?;
    let triggers_str: String = r.get(8)?;
    let ops_str: String = r.get(12)?;
    let conflicts_str: String = r.get(13)?;
    let verify_str: Option<String> = r.get(14)?;
    Ok(ChangesetRecord {
        cs_id: r.get(0)?,
        base_rev: r.get(1)?,
        stack_parent_cs_id: r.get(2)?,
        author: r.get(3)?,
        title: r.get(4)?,
        story_ids: serde_json::from_str(&story_str).unwrap_or_default(),
        scope_prefixes: serde_json::from_str(&scope_str).unwrap_or_default(),
        status: ChangesetStatus::parse(&status_str).unwrap_or(ChangesetStatus::Open),
        sensitive_paths: serde_json::from_str(&triggers_str).unwrap_or_default(),
        landed_rev: r.get(10)?,
        error_message: r.get(11)?,
        ops: serde_json::from_str(&ops_str).unwrap_or_default(),
        conflicts: serde_json::from_str(&conflicts_str).unwrap_or_default(),
        verify_report: verify_str.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
    })
}

fn init_schema(conn: &Connection, empty_tree_hash: &str) -> ServerResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS revisions (
            rev_id             INTEGER PRIMARY KEY,
            parent_rev         INTEGER,
            tree_hash          TEXT NOT NULL,
            cs_id              TEXT,
            author             TEXT NOT NULL,
            story_ids_json     TEXT NOT NULL,
            summary            TEXT NOT NULL,
            touched_paths_json TEXT NOT NULL,
            verify_json        TEXT,
            created_at         TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS changesets (
            cs_id                  TEXT PRIMARY KEY,
            base_rev               INTEGER NOT NULL,
            stack_parent_cs_id     TEXT,
            author                 TEXT NOT NULL,
            title                  TEXT NOT NULL,
            story_ids_json         TEXT NOT NULL,
            scope_json             TEXT NOT NULL,
            status                 TEXT NOT NULL,
            approval_triggers_json TEXT NOT NULL,
            approved_by            TEXT,
            landed_rev             INTEGER,
            error_message          TEXT,
            ops_json               TEXT NOT NULL,
            conflicts_json         TEXT NOT NULL,
            verify_json            TEXT,
            created_at             TEXT NOT NULL,
            updated_at             TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS snapshots (
            snap_id    INTEGER PRIMARY KEY AUTOINCREMENT,
            cs_id      TEXT NOT NULL REFERENCES changesets(cs_id) ON DELETE CASCADE,
            message    TEXT NOT NULL,
            ops_json   TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS leases (
            path_prefix TEXT NOT NULL,
            cs_id       TEXT NOT NULL,
            author      TEXT NOT NULL,
            mode        TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            expires_at  TEXT NOT NULL,
            PRIMARY KEY (path_prefix, cs_id)
        );

        CREATE TABLE IF NOT EXISTS sequences (
            kind     TEXT PRIMARY KEY,
            next_val INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS deployments (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            target       TEXT NOT NULL,
            env          TEXT NOT NULL,
            rev_id       INTEGER NOT NULL,
            artifact_ref TEXT NOT NULL,
            status       TEXT NOT NULL,
            deployed_by  TEXT NOT NULL,
            notes        TEXT,
            created_at   TEXT NOT NULL
        );
        ",
    )?;

    // Seed initial empty revision r0 if table is empty.
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM revisions", [], |r| r.get(0))?;
    if count == 0 {
        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO revisions (
                rev_id, parent_rev, tree_hash, cs_id, author, story_ids_json, summary,
                touched_paths_json, verify_json, created_at
             ) VALUES (0, NULL, ?1, NULL, 'system', '[]', 'Initial empty repository', '[]', NULL, ?2)",
            params![empty_tree_hash, now],
        )?;
    }

    Ok(())
}
