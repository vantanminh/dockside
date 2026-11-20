//! Unified release & rollback orchestrator bridging `dockside` revisions (`rN`) to:
//! - Rudder Images (`api`, `cdn`, `doc-quest`)
//! - Cloudflare Workers (`web`, `dashboard`)
//! - Expo EAS (`mobile`)
//!
//! Operated by AI agents; there is no human approval step. Safety comes from automation:
//! - `execute` must be passed explicitly by the caller (default is a dry-run plan, so a stray
//!   call cannot deploy by accident).
//! - A deploy is recorded as `deployed` only after the target's `ready_url` is verified
//!   (HTTP 2xx and, for Rudder images, the body carrying the expected `rN` GIT_SHA).
//! - If the runtime check fails, the release is recorded `deploy_unhealthy` and the previous
//!   healthy deployment is restored automatically.

use std::path::{Path, PathBuf};
use std::time::Duration;

use dockside_core::config::{TargetConfig, TargetKind};
use dockside_core::model::{ReleasePlan, ReleaseReq, RollbackReq};
use dockside_core::tree::read_scoped_tree_flat;
use tracing::{error, info, warn};

use crate::AppState;
use crate::error::{ServerError, ServerResult};

const READY_ATTEMPTS: u32 = 24;
const READY_DELAY: Duration = Duration::from_secs(5);

pub async fn handle_release(state: &AppState, req: &ReleaseReq) -> ServerResult<ReleasePlan> {
    let target = state
        .config
        .target_by_name(&req.target)
        .ok_or_else(|| ServerError::NotFound(format!("target '{}'", req.target)))?
        .clone();

    let rev = state.db.get_revision(req.rev_id)?;
    // Automated gate: a revision whose own verify gate failed must never ship.
    if let Some(report) = &rev.verify_report
        && report.failed()
    {
        return Err(ServerError::BadRequest(format!(
            "r{} failed its verify gate and cannot be released",
            rev.rev_id
        )));
    }

    let export_dir = export_target_snapshot(state, &target, rev.rev_id, &rev.tree_hash).await?;
    let artifact_tag = format!("{}-{}-r{}", target.name, req.env, rev.rev_id);
    let commands =
        build_release_commands(&target, &req.env, rev.rev_id, &artifact_tag, &export_dir);

    let mut plan = plan_of(
        &target,
        &req.env,
        rev.rev_id,
        &export_dir,
        &artifact_tag,
        commands,
    );
    if !req.execute {
        return Ok(plan);
    }

    plan.executed = true;
    let (ok, outputs) = run_all(&export_dir, &plan.commands).await?;
    plan.command_outputs = outputs;
    if !ok {
        plan.status = Some("failed".to_string());
        let notes = plan.command_outputs.last().cloned();
        state.db.insert_deployment(
            &target.name,
            &req.env,
            rev.rev_id,
            &artifact_tag,
            "failed",
            &req.actor,
            notes.as_deref(),
        )?;
        return Err(ServerError::Internal(format!(
            "release execution failed for {}@{} r{}",
            target.name, req.env, rev.rev_id
        )));
    }

    let (health, detail) = verify_runtime(&target, rev.rev_id).await;
    plan.health_verified = health;
    plan.health_detail = Some(detail.clone());
    let status = match health {
        Some(true) => "deployed",
        Some(false) => "deploy_unhealthy",
        None => "deployed_unverified",
    };
    plan.status = Some(status.to_string());
    state.db.insert_deployment(
        &target.name,
        &req.env,
        rev.rev_id,
        &artifact_tag,
        status,
        &req.actor,
        Some(&detail),
    )?;

    if health == Some(false) {
        let restored = auto_rollback(state, &target, &req.env, rev.rev_id, &req.actor).await;
        return Err(ServerError::Internal(format!(
            "release r{} of {}@{} failed the runtime check ({detail}); {restored}",
            rev.rev_id, target.name, req.env
        )));
    }
    Ok(plan)
}

pub async fn handle_rollback(state: &AppState, req: &RollbackReq) -> ServerResult<ReleasePlan> {
    let target = state
        .config
        .target_by_name(&req.target)
        .ok_or_else(|| ServerError::NotFound(format!("target '{}'", req.target)))?
        .clone();
    rollback_to(
        state,
        &target,
        &req.env,
        req.to_rev_id,
        &req.actor,
        req.execute,
    )
    .await
}

async fn rollback_to(
    state: &AppState,
    target: &TargetConfig,
    env: &str,
    to_rev_id: i64,
    actor: &str,
    execute: bool,
) -> ServerResult<ReleasePlan> {
    let rev = state.db.get_revision(to_rev_id)?;
    let export_dir = export_target_snapshot(state, target, rev.rev_id, &rev.tree_hash).await?;
    let artifact_tag = format!("{}-{}-r{}", target.name, env, rev.rev_id);

    let commands = rollback_commands(state, target, env, rev.rev_id, &artifact_tag, &export_dir)?;
    let mut plan = plan_of(
        target,
        env,
        rev.rev_id,
        &export_dir,
        &artifact_tag,
        commands,
    );
    if !execute {
        return Ok(plan);
    }

    plan.executed = true;
    let (ok, outputs) = run_all(&export_dir, &plan.commands).await?;
    plan.command_outputs = outputs;
    if !ok {
        plan.status = Some("rollback_failed".to_string());
        state.db.insert_deployment(
            &target.name,
            env,
            rev.rev_id,
            &artifact_tag,
            "rollback_failed",
            actor,
            Some(&format!("rollback to r{}", rev.rev_id)),
        )?;
        return Err(ServerError::Internal(format!(
            "rollback failed for {}@{env} r{}",
            target.name, rev.rev_id
        )));
    }

    let (health, detail) = verify_runtime(target, rev.rev_id).await;
    plan.health_verified = health;
    plan.health_detail = Some(detail.clone());
    let status = match health {
        Some(true) => "rolled_back",
        Some(false) => "rollback_unhealthy",
        None => "rolled_back_unverified",
    };
    plan.status = Some(status.to_string());
    state.db.insert_deployment(
        &target.name,
        env,
        rev.rev_id,
        &artifact_tag,
        status,
        actor,
        Some(&format!("rollback to r{}: {detail}", rev.rev_id)),
    )?;
    if health == Some(false) {
        return Err(ServerError::Internal(format!(
            "rollback of {}@{env} to r{} failed the runtime check ({detail})",
            target.name, rev.rev_id
        )));
    }
    Ok(plan)
}

/// Restore the previous healthy deployment after a failed runtime check. Returns a short report.
async fn auto_rollback(
    state: &AppState,
    target: &TargetConfig,
    env: &str,
    failed_rev: i64,
    actor: &str,
) -> String {
    let previous = match state
        .db
        .last_healthy_deployment(&target.name, env, Some(failed_rev))
    {
        Ok(Some(prev)) => prev,
        Ok(None) => return "no previous healthy deployment to restore".to_string(),
        Err(e) => return format!("could not look up previous deployment: {e}"),
    };
    warn!(
        target = %target.name,
        failed_rev,
        restore_rev = previous.rev_id,
        "release unhealthy; auto-rolling back"
    );
    let who = format!("{actor}(auto-rollback)");
    match rollback_to(state, target, env, previous.rev_id, &who, true).await {
        Ok(_) => format!("auto-rolled back to r{}", previous.rev_id),
        Err(e) => {
            error!(error = %e, "auto-rollback failed");
            format!("auto-rollback to r{} FAILED: {e}", previous.rev_id)
        }
    }
}

fn plan_of(
    target: &TargetConfig,
    env: &str,
    rev_id: i64,
    export_dir: &Path,
    artifact_tag: &str,
    commands: Vec<String>,
) -> ReleasePlan {
    ReleasePlan {
        target: target.name.clone(),
        env: env.to_string(),
        rev_id,
        kind: kind_label(target.kind).to_string(),
        export_dir: export_dir.display().to_string(),
        artifact_tag: artifact_tag.to_string(),
        commands,
        ready_url: target.ready_url.clone(),
        executed: false,
        command_outputs: Vec::new(),
        health_verified: None,
        health_detail: None,
        status: None,
    }
}

async fn run_all(workdir: &Path, commands: &[String]) -> ServerResult<(bool, Vec<String>)> {
    let mut outputs = Vec::new();
    for cmd in commands {
        let (ok, out) = run_cmd(workdir, cmd).await?;
        outputs.push(format!("$ {cmd}\n{out}"));
        if !ok {
            return Ok((false, outputs));
        }
    }
    Ok((true, outputs))
}

/// Post-deploy runtime verification. `None` = target has no `ready_url` (cannot be verified).
async fn verify_runtime(target: &TargetConfig, rev_id: i64) -> (Option<bool>, String) {
    let Some(url) = target.ready_url.as_deref() else {
        return (
            None,
            "no ready_url configured; runtime not verified".to_string(),
        );
    };
    let marker = expected_marker(target, rev_id);
    let (ok, detail) = check_ready(url, marker.as_deref(), READY_ATTEMPTS, READY_DELAY).await;
    info!(target = %target.name, rev_id, ok, %detail, "runtime check");
    (Some(ok), detail)
}

/// Text the ready endpoint must contain for this revision, if any.
pub fn expected_marker(target: &TargetConfig, rev_id: i64) -> Option<String> {
    let rev = format!("r{rev_id}");
    match &target.ready_marker {
        Some(tpl) => Some(tpl.replace("{rev}", &rev)),
        None if target.kind == TargetKind::RudderImage => Some(rev),
        None => None,
    }
}

/// Poll `url` until it answers 2xx (and contains `marker`, when given) or attempts run out.
pub async fn check_ready(
    url: &str,
    marker: Option<&str>,
    attempts: u32,
    delay: Duration,
) -> (bool, String) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return (false, format!("http client error: {e}")),
    };
    let mut last = String::from("no attempt made");
    for attempt in 1..=attempts.max(1) {
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    last = format!("{url} answered HTTP {status}");
                } else if let Some(m) = marker
                    && !body.contains(m)
                {
                    last = format!(
                        "{url} is up but does not report '{m}' (still serving another revision?)"
                    );
                } else {
                    return (
                        true,
                        match marker {
                            Some(m) => format!("{url} OK, reports '{m}'"),
                            None => format!("{url} OK (HTTP {status})"),
                        },
                    );
                }
            }
            Err(e) => last = format!("{url} unreachable: {e}"),
        }
        if attempt < attempts {
            tokio::time::sleep(delay).await;
        }
    }
    (false, last)
}

fn rollback_commands(
    state: &AppState,
    target: &TargetConfig,
    env: &str,
    rev_id: i64,
    artifact_tag: &str,
    export_dir: &Path,
) -> ServerResult<Vec<String>> {
    match target.kind {
        TargetKind::RudderImage => {
            // Fast path only when this exact image was already built and pushed; otherwise rebuild.
            if state.db.has_healthy_artifact(&target.name, artifact_tag)? {
                let svc = target
                    .rudder_service
                    .as_deref()
                    .unwrap_or("myproject/prod/api");
                Ok(vec![format!("rudder deploy {svc} --image {artifact_tag}")])
            } else {
                Ok(build_release_commands(
                    target,
                    env,
                    rev_id,
                    artifact_tag,
                    export_dir,
                ))
            }
        }
        // Worker/Expo sources live under `target.path` inside the export (e.g. `apps/web`), so the
        // rollback must use the same `--dir`/`--cwd` commands as a release, not run at the export root.
        TargetKind::CloudflareWorker | TargetKind::ExpoEas => Ok(build_release_commands(
            target,
            env,
            rev_id,
            artifact_tag,
            export_dir,
        )),
    }
}

async fn export_target_snapshot(
    state: &AppState,
    target: &TargetConfig,
    rev_id: i64,
    tree_hash: &str,
) -> ServerResult<PathBuf> {
    let export_dir = state.exports_dir.join(format!("{}-r{rev_id}", target.name));
    if export_dir.exists() {
        std::fs::remove_dir_all(&export_dir)?;
    }
    std::fs::create_dir_all(&export_dir)?;

    let scopes = vec![target.path.clone(), "contracts".to_string()];
    let files = read_scoped_tree_flat(&state.cas, tree_hash, &scopes).await?;
    for (rel_path, entry) in files {
        let full = export_dir.join(&rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = state.cas.get_bytes(&entry.hash).await?;
        std::fs::write(&full, bytes)?;
    }
    Ok(export_dir)
}

pub fn build_release_commands(
    target: &TargetConfig,
    env: &str,
    rev_id: i64,
    artifact_tag: &str,
    _export_dir: &Path,
) -> Vec<String> {
    match target.kind {
        TargetKind::RudderImage => {
            let svc = target
                .rudder_service
                .as_deref()
                .unwrap_or("myproject/prod/api");
            let dockerfile = target.dockerfile.as_deref().unwrap_or("Dockerfile");
            let context = target.context.as_deref().unwrap_or(&target.path);
            vec![
                format!(
                    "rudder build -s {svc} -t {artifact_tag} --dockerfile {dockerfile} --context {context} --platform linux/amd64 --build-arg GIT_SHA=r{rev_id}"
                ),
                format!("rudder push {artifact_tag} -s {svc} --no-deploy"),
                format!("rudder deploy {svc} --image {artifact_tag}"),
            ]
        }
        TargetKind::CloudflareWorker => {
            let mut cmds = Vec::new();
            for pre in &target.pre_deploy_commands {
                cmds.push(pre.clone());
            }
            cmds.push(format!(
                "pnpm --dir {} install --frozen-lockfile",
                target.path
            ));
            if let Some(ref deploy_cmd) = target.deploy_command {
                cmds.push(deploy_cmd.clone());
            } else {
                cmds.push(format!("pnpm --dir {} run deploy", target.path));
            }
            cmds
        }
        TargetKind::ExpoEas => {
            vec![
                format!("bun --cwd {} install --frozen-lockfile", target.path),
                format!(
                    "bun --cwd {} x eas-cli update --channel {env} --message \"r{rev_id}\"",
                    target.path
                ),
            ]
        }
    }
}

fn kind_label(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::RudderImage => "rudder-image",
        TargetKind::CloudflareWorker => "cloudflare-worker",
        TargetKind::ExpoEas => "expo-eas",
    }
}

async fn run_cmd(workdir: &Path, command: &str) -> ServerResult<(bool, String)> {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("powershell");
        c.args(["-NoProfile", "-Command", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let output = cmd.current_dir(workdir).output().await?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        text.push('\n');
        text.push_str(&stderr);
    }
    Ok((output.status.success(), text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use dockside_core::config::DocksideConfig;

    async fn serve(body: &'static str, status: u16) -> Result<String, Box<dyn std::error::Error>> {
        let app = Router::new().route(
            "/ready",
            get(move || async move {
                (
                    axum::http::StatusCode::from_u16(status)
                        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
                    body,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(format!("http://{addr}/ready"))
    }

    #[tokio::test]
    async fn ready_check_requires_expected_revision_marker()
    -> Result<(), Box<dyn std::error::Error>> {
        let url = serve(r#"{"git_sha":"r7"}"#, 200).await?;
        let (ok, _) = check_ready(&url, Some("r7"), 2, Duration::from_millis(10)).await;
        assert!(ok);
        let (ok, detail) = check_ready(&url, Some("r8"), 2, Duration::from_millis(10)).await;
        assert!(!ok, "wrong revision must not count as deployed: {detail}");
        Ok(())
    }

    #[tokio::test]
    async fn ready_check_fails_on_http_error_and_unreachable()
    -> Result<(), Box<dyn std::error::Error>> {
        let url = serve("down", 503).await?;
        let (ok, _) = check_ready(&url, None, 2, Duration::from_millis(10)).await;
        assert!(!ok);
        let (ok, _) = check_ready(
            "http://127.0.0.1:9/ready",
            None,
            1,
            Duration::from_millis(1),
        )
        .await;
        assert!(!ok);
        Ok(())
    }

    #[test]
    fn rudder_targets_expect_git_sha_marker_workers_do_not() {
        let cfg = DocksideConfig::default_pilot();
        let api = cfg.target_by_name("api");
        let web = cfg.target_by_name("web");
        assert_eq!(api.and_then(|t| expected_marker(t, 12)), Some("r12".into()));
        assert_eq!(web.and_then(|t| expected_marker(t, 12)), None);
    }

    #[test]
    fn cloudflare_release_commands_target_the_real_subdirectory() {
        let cfg = DocksideConfig::default_pilot();
        let Some(web) = cfg.target_by_name("web") else {
            panic!("web target missing");
        };
        let cmds = build_release_commands(web, "prod", 3, "web-prod-r3", Path::new("."));
        assert!(cmds.iter().any(|c| c.contains("--dir apps/web install")));
        assert!(!cmds.iter().any(|c| c == "pnpm install --frozen-lockfile"));
    }
}
