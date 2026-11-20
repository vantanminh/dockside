use std::process::Command;

use dockside_core::config::DocksideConfig;
use dockside_server::{AppState, ServerOptions, build_router};
use serde_json::Value;

fn run_dockside(url: &str, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let bin = env!("CARGO_BIN_EXE_dockside");
    let output = Command::new(bin)
        .arg("--url")
        .arg(url)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "dockside {:?} failed (exit {:?}):\nstdout: {}\nstderr: {}",
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_full_agent_workflow_import_open_snap_undo_submit_release()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let state = AppState::init(ServerOptions {
        data_dir: tmp.path().join("server-data"),
        config: DocksideConfig::default_pilot(),
        r2: None,
        auth_token: None,
        git_mirror_dir: None,
        verify_workdir: None,
        run_verify: false,
    })
    .await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });
    let url = format!("http://{addr}");

    // 1. Health check
    let health_out = run_dockside(&url, &["health", "--json"])?;
    let health: Value = serde_json::from_str(&health_out)?;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["main_rev"], 0);

    // 2. Prepare fake pilot repos (including a secret .env that MUST be skipped!)
    let src_api = tmp.path().join("src-api");
    let src_web = tmp.path().join("src-web");
    std::fs::create_dir_all(src_api.join("src"))?;
    std::fs::create_dir_all(src_web.join("app"))?;
    std::fs::write(src_api.join("src/main.rs"), "fn main() {}\n")?;
    std::fs::write(src_api.join(".env"), "DATABASE_URL=secret\n")?;
    std::fs::write(src_api.join(".env.example"), "DATABASE_URL=\n")?;
    std::fs::write(
        src_web.join("app/home.tsx"),
        "export function Home() { return <h1>Acme</h1>; }\n",
    )?;

    let map_api = format!("services/api={}", src_api.display());
    let map_web = format!("apps/web={}", src_web.display());
    let imp_out = run_dockside(
        &url,
        &[
            "import",
            "--map",
            &map_api,
            "--map",
            &map_web,
            "--summary",
            "CLI import test",
            "--json",
        ],
    )?;
    let imp: Value = serde_json::from_str(&imp_out)?;
    assert_eq!(imp["rev"]["rev_id"], 1);
    assert_eq!(imp["total_files"], 3);
    assert_eq!(imp["skipped_secrets"].as_array().map(|a| a.len()), Some(1));

    // 3. Open sparse workspace scoped to apps/web only
    let ws_dir = tmp.path().join("ws-agent-1");
    let ws_str = ws_dir.to_string_lossy().into_owned();
    let open_out = run_dockside(
        &url,
        &[
            "open",
            "--title",
            "Update Home heading",
            "--scope",
            "apps/web",
            "--story",
            "US-999",
            "--author",
            "agent-1",
            "--dir",
            &ws_str,
            "--json",
        ],
    )?;
    let open: Value = serde_json::from_str(&open_out)?;
    assert_eq!(open["files_materialized"], 1);
    assert!(ws_dir.join("apps/web/app/home.tsx").is_file());
    assert!(!ws_dir.join("services/api/src/main.rs").exists());

    // 4. Edit -> Snap -> Bad edit -> Undo -> Verify restored!
    let home_path = ws_dir.join("apps/web/app/home.tsx");
    std::fs::write(
        &home_path,
        "export function Home() { return <h1>Acme v2</h1>; }\n",
    )?;
    let _ = run_dockside(
        &url,
        &["snap", "-m", "good heading", "--dir", &ws_str, "--json"],
    )?;
    std::fs::write(&home_path, "BROKEN CODE\n")?;
    let _ = run_dockside(&url, &["undo", "--dir", &ws_str, "--json"])?;
    let restored = std::fs::read_to_string(&home_path)?;
    assert!(restored.contains("Acme v2"));

    // 5. Status & Diff & Submit -> auto-land as r2 and clean workspace
    let status_out = run_dockside(&url, &["status", "--dir", &ws_str, "--json"])?;
    let st: Value = serde_json::from_str(&status_out)?;
    assert_eq!(st["modified"].as_array().map(|a| a.len()), Some(1));

    let diff_out = run_dockside(&url, &["diff", "--dir", &ws_str])?;
    assert!(diff_out.contains("+export function Home() { return <h1>Acme v2</h1>; }"));

    let sub_out = run_dockside(&url, &["submit", "--dir", &ws_str, "--json"])?;
    let sub: Value = serde_json::from_str(&sub_out)?;
    assert_eq!(sub["changeset"]["status"], "landed");
    assert_eq!(sub["changeset"]["landed_rev"], 2);
    assert_eq!(sub["workspace_cleaned"], true);
    assert!(!ws_dir.exists());

    // 6. Sequence & Dry-run Release plans for both Rudder image (api) and CF Worker (web)
    let seq_out = run_dockside(&url, &["seq", "intake", "--json"])?;
    let seq: Value = serde_json::from_str(&seq_out)?;
    assert_eq!(seq["formatted"], "IN-001");

    let rel_api_out = run_dockside(
        &url,
        &[
            "release", "--target", "api", "--env", "prod", "--rev", "2", "--json",
        ],
    )?;
    let rel_api: Value = serde_json::from_str(&rel_api_out)?;
    assert_eq!(rel_api["executed"], false);
    assert_eq!(rel_api["kind"], "rudder-image");
    assert_eq!(rel_api["artifact_tag"], "api-prod-r2");

    let rel_web_out = run_dockside(
        &url,
        &[
            "release", "--target", "web", "--env", "prod", "--rev", "2", "--json",
        ],
    )?;
    let rel_web: Value = serde_json::from_str(&rel_web_out)?;
    assert_eq!(rel_web["executed"], false);
    assert_eq!(rel_web["kind"], "cloudflare-worker");

    server.abort();
    Ok(())
}
