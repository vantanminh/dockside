pub mod db;
pub mod error;
pub mod integrator;
pub mod mirror;
pub mod release;
pub mod routes;
pub mod ui;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use dockside_core::cas::{CasStore, R2Config};
use dockside_core::config::DocksideConfig;
use dockside_core::policy::PolicyEngine;
use dockside_core::tree::write_tree;
use tokio::sync::Mutex;

use crate::db::Db;
use crate::error::ServerResult;
use crate::mirror::GitMirror;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<DocksideConfig>,
    pub policy: Arc<PolicyEngine>,
    pub cas: CasStore,
    pub db: Db,
    pub merge_lock: Arc<Mutex<()>>,
    pub auth_token: Option<String>,
    pub git_mirror: Option<GitMirror>,
    pub verify_workdir: PathBuf,
    pub exports_dir: PathBuf,
    pub run_verify: bool,
}

pub struct ServerOptions {
    pub data_dir: PathBuf,
    pub config: DocksideConfig,
    pub r2: Option<R2Config>,
    pub auth_token: Option<String>,
    pub git_mirror_dir: Option<PathBuf>,
    pub verify_workdir: Option<PathBuf>,
    pub run_verify: bool,
}

/// Data directories created before the project was renamed hold `dsrc.sqlite`; adopt it (and its
/// WAL/SHM side files) so an upgrade never starts from an empty history.
fn migrate_legacy_db(dir: &std::path::Path) -> ServerResult<()> {
    let new = dir.join("dockside.sqlite");
    let legacy = dir.join("dsrc.sqlite");
    if new.exists() || !legacy.exists() {
        return Ok(());
    }
    for suffix in ["", "-wal", "-shm"] {
        let from = dir.join(format!("dsrc.sqlite{suffix}"));
        if from.exists() {
            std::fs::rename(&from, dir.join(format!("dockside.sqlite{suffix}")))?;
        }
    }
    Ok(())
}

impl AppState {
    pub async fn init(opts: ServerOptions) -> ServerResult<Self> {
        std::fs::create_dir_all(&opts.data_dir)?;
        let cas = CasStore::open(opts.data_dir.join("cas"), opts.r2)?;
        let empty_tree_hash = write_tree(&cas, &BTreeMap::new()).await?;
        migrate_legacy_db(&opts.data_dir)?;
        let db = Db::open(opts.data_dir.join("dockside.sqlite"), &empty_tree_hash)?;
        let policy = PolicyEngine::from_config(&opts.config)?;
        let verify_workdir = opts
            .verify_workdir
            .unwrap_or_else(|| opts.data_dir.join("verify-work"));
        let exports_dir = opts.data_dir.join("exports");
        std::fs::create_dir_all(&exports_dir)?;

        Ok(Self {
            config: Arc::new(opts.config),
            policy: Arc::new(policy),
            cas,
            db,
            merge_lock: Arc::new(Mutex::new(())),
            auth_token: opts.auth_token,
            git_mirror: opts.git_mirror_dir.map(GitMirror::new),
            verify_workdir,
            exports_dir,
            run_verify: opts.run_verify,
        })
    }
}

pub fn build_router(state: AppState) -> Router {
    routes::router(state)
}

#[cfg(test)]
mod legacy_tests {
    use super::migrate_legacy_db;

    #[test]
    fn adopts_legacy_db_files_once() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let p = dir.path();
        std::fs::write(p.join("dsrc.sqlite"), b"db")?;
        std::fs::write(p.join("dsrc.sqlite-wal"), b"wal")?;
        migrate_legacy_db(p)?;
        assert!(!p.join("dsrc.sqlite").exists());
        assert_eq!(std::fs::read(p.join("dockside.sqlite"))?, b"db");
        assert_eq!(std::fs::read(p.join("dockside.sqlite-wal"))?, b"wal");
        // A present new DB is never overwritten.
        std::fs::write(p.join("dsrc.sqlite"), b"other")?;
        migrate_legacy_db(p)?;
        assert_eq!(std::fs::read(p.join("dockside.sqlite"))?, b"db");
        Ok(())
    }
}
