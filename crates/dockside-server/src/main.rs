use std::net::SocketAddr;
use std::path::PathBuf;

use dockside_core::cas::R2Config;
use dockside_core::config::DocksideConfig;
use dockside_server::{AppState, ServerOptions, build_router};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let bind_raw =
        std::env::var("DOCKSIDE_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:7420".to_string());
    let addr: SocketAddr = bind_raw.parse()?;

    let data_dir =
        PathBuf::from(std::env::var("DOCKSIDE_DATA_DIR").unwrap_or_else(|_| "./data".to_string()));

    let config = match std::env::var("DOCKSIDE_CONFIG") {
        Ok(p) if !p.trim().is_empty() && PathBuf::from(&p).is_file() => {
            info!(path = %p, "loaded dockside config file");
            DocksideConfig::from_file(PathBuf::from(p).as_path())?
        }
        _ => {
            let default_path = PathBuf::from("dockside.toml");
            if default_path.is_file() {
                info!("loaded ./dockside.toml");
                DocksideConfig::from_file(&default_path)?
            } else {
                info!("using built-in pilot config (api + web + dashboard)");
                DocksideConfig::default_pilot()
            }
        }
    };

    let auth_token = std::env::var("DOCKSIDE_AUTH_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty());

    if !addr.ip().is_loopback() && auth_token.is_none() {
        return Err(
            "Refusing to bind to non-loopback address without DOCKSIDE_AUTH_TOKEN set".into(),
        );
    }

    let r2 = load_r2_from_env();
    let git_mirror_dir = std::env::var("DOCKSIDE_GIT_MIRROR_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);
    let verify_workdir = std::env::var("DOCKSIDE_VERIFY_WORKDIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);
    let run_verify = matches!(
        std::env::var("DOCKSIDE_RUN_VERIFY")
            .unwrap_or_else(|_| "false".to_string())
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    );

    let state = AppState::init(ServerOptions {
        data_dir,
        config,
        r2,
        auth_token,
        git_mirror_dir,
        verify_workdir,
        run_verify,
    })
    .await?;

    let main_rev = state.db.main_rev_id()?;
    info!(
        %addr,
        main_rev,
        r2 = state.cas.is_r2_enabled(),
        mirror = state.git_mirror.is_some(),
        verify = state.run_verify,
        "dockside-server server starting"
    );

    // Crash recovery: re-run changesets interrupted mid-merge/verify, then drain the queue.
    if let Err(e) = dockside_server::integrator::recover_and_drain(&state).await {
        warn!(error = %e, "queue recovery on startup failed");
    }

    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn load_r2_from_env() -> Option<R2Config> {
    let endpoint = std::env::var("DOCKSIDE_R2_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())?;
    let bucket = std::env::var("DOCKSIDE_R2_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())?;
    let access_key_id = std::env::var("DOCKSIDE_R2_ACCESS_KEY_ID")
        .ok()
        .filter(|s| !s.is_empty())?;
    let secret_access_key = std::env::var("DOCKSIDE_R2_SECRET_ACCESS_KEY")
        .ok()
        .filter(|s| !s.is_empty())?;
    let region = std::env::var("DOCKSIDE_R2_REGION").unwrap_or_else(|_| "auto".to_string());
    let prefix = std::env::var("DOCKSIDE_R2_PREFIX").unwrap_or_else(|_| "cas/v1".to_string());
    Some(R2Config {
        endpoint,
        region,
        bucket,
        prefix,
        access_key_id,
        secret_access_key,
    })
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            warn!("failed to listen for ctrl+c: {e}");
        }
    };
    ctrl_c.await;
    info!("shutdown signal received");
}
