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

impl AppState {
    pub async fn init(opts: ServerOptions) -> ServerResult<Self> {
        std::fs::create_dir_all(&opts.data_dir)?;
        let cas = CasStore::open(opts.data_dir.join("cas"), opts.r2)?;
        let empty_tree_hash = write_tree(&cas, &BTreeMap::new()).await?;
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
