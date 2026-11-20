//! Behaviour of the fully automated (no human approval) pipeline.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use dockside_core::config::DocksideConfig;
use dockside_core::hash::hash_bytes;
use dockside_core::model::{
    ChangesetOp, ChangesetRecord, ChangesetStatus, ImportRevisionReq, LeaseMode, OpenChangesetReq,
    OpenChangesetResp, RevisionRecord, SubmitChangesetReq,
};
use dockside_server::{AppState, ServerOptions, build_router};
use http_body_util::BodyExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tower::ServiceExt;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Harness {
    _dir: tempfile::TempDir,
    state: AppState,
    app: axum::Router,
    verify_dir: std::path::PathBuf,
}

async fn harness(
    cfg: DocksideConfig,
    run_verify: bool,
) -> Result<Harness, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let verify_dir = dir.path().join("verify");
    let state = AppState::init(ServerOptions {
        data_dir: dir.path().join("data"),
        config: cfg,
        r2: None,
        auth_token: None,
        git_mirror_dir: None,
        verify_workdir: Some(verify_dir.clone()),
        run_verify,
    })
    .await?;
    let app = build_router(state.clone());
    Ok(Harness {
        _dir: dir,
        state,
        app,
        verify_dir,
    })
}

/// Default pilot config, but `api` verifies with a trivial shell command (`exit N`) that works under
/// both PowerShell and sh.
fn cfg_with_api_verify(cmds: &[&str]) -> DocksideConfig {
    let mut cfg = DocksideConfig::default_pilot();
    for t in &mut cfg.targets {
        if t.name == "api" {
            t.verify_commands = cmds.iter().map(|s| (*s).to_string()).collect();
        }
    }
    cfg
}

async fn post<Req: Serialize, Resp: DeserializeOwned>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
) -> Result<(StatusCode, Option<Resp>), Box<dyn std::error::Error>> {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body)?))?;
    let resp = app.clone().oneshot(req).await?;
    let status = resp.status();
    let raw = resp.into_body().collect().await?.to_bytes();
    if status.is_success() {
        Ok((status, Some(serde_json::from_slice(&raw)?)))
    } else {
        Ok((status, None))
    }
}

async fn blob(
    app: &axum::Router,
    data: &[u8],
) -> Result<(String, u64), Box<dyn std::error::Error>> {
    let hash = hash_bytes(data);
    let req = Request::builder()
        .method(Method::PUT)
        .uri(format!("/v1/blobs/{hash}"))
        .body(Body::from(data.to_vec()))?;
    let resp = app.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok((hash, data.len() as u64))
}

async fn put_op(
    app: &axum::Router,
    path: &str,
    data: &[u8],
) -> Result<ChangesetOp, Box<dyn std::error::Error>> {
    let (hash, size) = blob(app, data).await?;
    Ok(ChangesetOp::Put {
        path: path.into(),
        hash,
        size,
        executable: false,
    })
}

async fn import(app: &axum::Router, ops: Vec<ChangesetOp>) -> TestResult {
    let (status, _) = post::<_, RevisionRecord>(
        app,
        "/v1/import",
        &ImportRevisionReq {
            author: "agent".into(),
            summary: "seed".into(),
            ops,
        },
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

/// Open + submit in one go (as an agent would) and return the resulting record.
async fn land(
    app: &axum::Router,
    title: &str,
    ops: Vec<ChangesetOp>,
) -> Result<ChangesetRecord, Box<dyn std::error::Error>> {
    let (_, open) = post::<_, OpenChangesetResp>(
        app,
        "/v1/changesets",
        &OpenChangesetReq {
            author: "agent".into(),
            title: title.into(),
            story_ids: vec![],
            scope_prefixes: vec![],
            stack_parent_cs_id: None,
            lease_mode: LeaseMode::Soft,
            lease_ttl_secs: 3600,
        },
    )
    .await?;
    let Some(open) = open else {
        return Err("open failed".into());
    };
    let (status, rec) = post::<_, ChangesetRecord>(
        app,
        &format!("/v1/changesets/{}/submit", open.changeset.cs_id),
        &SubmitChangesetReq {
            ops,
            base_rev: None,
            title: None,
        },
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    rec.ok_or_else(|| "submit returned no record".into())
}

const MIGRATION: &str = "services/api/src/db/migrations/pending_add_tags.sql";
const AUTH_FILE: &str = "services/api/rust/crates/myapp-api/src/modules/auth/login.rs";

#[tokio::test]
async fn sensitive_changeset_passes_checks_and_lands_without_any_approval() -> TestResult {
    let h = harness(cfg_with_api_verify(&["exit 0"]), true).await?;
    let op = put_op(&h.app, MIGRATION, b"CREATE TABLE tags (id int);\n").await?;
    let rec = land(&h.app, "add tags", vec![op]).await?;

    assert_eq!(
        rec.status,
        ChangesetStatus::Landed,
        "{:?}",
        rec.error_message
    );
    assert!(
        !rec.sensitive_paths.is_empty(),
        "sensitive paths are still flagged"
    );
    let report = rec.verify_report.ok_or("missing verify report")?;
    assert!(
        report.executed_and_passed(),
        "checks really ran: {report:?}"
    );
    assert_eq!(report.steps.len(), 1);
    Ok(())
}

#[tokio::test]
async fn failing_checks_block_sensitive_land_but_never_the_queue() -> TestResult {
    let h = harness(cfg_with_api_verify(&["exit 1"]), true).await?;
    let bad = put_op(&h.app, AUTH_FILE, b"pub fn login() {}\n").await?;
    let rec = land(&h.app, "auth tweak", vec![bad]).await?;
    assert_eq!(rec.status, ChangesetStatus::Failed);
    assert!(rec.landed_rev.is_none());
    let report = rec.verify_report.ok_or("missing verify report")?;
    assert!(report.failed());
    assert!(!report.skipped);

    // A change that affects no target (docs only) is not blocked by the failed one.
    let doc = put_op(&h.app, "docs/notes.md", b"hello\n").await?;
    let ok = land(&h.app, "docs", vec![doc]).await?;
    assert_eq!(ok.status, ChangesetStatus::Landed);
    assert_eq!(h.state.db.main_rev_id()?, 1);
    Ok(())
}

#[tokio::test]
async fn skipped_verify_is_never_reported_as_passed() -> TestResult {
    let h = harness(DocksideConfig::default_pilot(), false).await?;
    // Normal (non-sensitive) code still lands, but the report says it was skipped, not passed.
    let op = put_op(&h.app, "services/api/rust/src/lib.rs", b"pub fn a() {}\n").await?;
    let rec = land(&h.app, "plain change", vec![op]).await?;
    assert_eq!(rec.status, ChangesetStatus::Landed);
    let report = rec.verify_report.ok_or("missing verify report")?;
    assert!(report.skipped);
    assert!(!report.passed, "skipped must not look like a pass");
    assert!(!report.executed_and_passed());
    assert!(report.skip_reason.is_some());
    Ok(())
}

#[tokio::test]
async fn sensitive_change_without_an_executed_gate_is_failed_not_landed() -> TestResult {
    let h = harness(DocksideConfig::default_pilot(), false).await?;
    let op = put_op(&h.app, MIGRATION, b"CREATE TABLE tags (id int);\n").await?;
    let rec = land(&h.app, "add tags", vec![op]).await?;
    assert_eq!(rec.status, ChangesetStatus::Failed);
    assert!(rec.landed_rev.is_none());
    assert!(
        rec.error_message
            .as_deref()
            .is_some_and(|m| m.contains("verify gate did not run"))
    );
    assert_eq!(h.state.db.main_rev_id()?, 0);
    Ok(())
}

#[tokio::test]
async fn approve_and_reject_endpoints_no_longer_exist() -> TestResult {
    let h = harness(DocksideConfig::default_pilot(), false).await?;
    for action in ["approve", "reject"] {
        let (status, _) = post::<_, serde_json::Value>(
            &h.app,
            &format!("/v1/changesets/CS-0001/{action}"),
            &serde_json::json!({"approved_by": "agent"}),
        )
        .await?;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
            "{action} answered {status}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn verify_workdir_drops_files_removed_from_the_revision() -> TestResult {
    let h = harness(cfg_with_api_verify(&["exit 0"]), true).await?;
    let path = "services/api/rust/src/old_module.rs";
    let add = put_op(&h.app, path, b"pub fn old() {}\n").await?;
    assert_eq!(
        land(&h.app, "add", vec![add]).await?.status,
        ChangesetStatus::Landed
    );
    assert!(h.verify_dir.join(path).is_file());

    // Build output living next to synced sources must survive the sync.
    let target_dir = h.verify_dir.join("services/api/rust/target");
    std::fs::create_dir_all(&target_dir)?;
    std::fs::write(target_dir.join("keep.bin"), b"warm")?;

    let del = ChangesetOp::Delete { path: path.into() };
    assert_eq!(
        land(&h.app, "remove", vec![del]).await?.status,
        ChangesetStatus::Landed
    );
    assert!(
        !h.verify_dir.join(path).exists(),
        "deleted source must not linger and be verified"
    );
    assert!(target_dir.join("keep.bin").is_file());
    Ok(())
}

#[tokio::test]
async fn interrupted_and_legacy_changesets_are_requeued_and_landed_after_restart() -> TestResult {
    let h = harness(DocksideConfig::default_pilot(), false).await?;
    import(&h.app, vec![put_op(&h.app, "docs/a.md", b"a\n").await?]).await?;

    let (_, open) = post::<_, OpenChangesetResp>(
        &h.app,
        "/v1/changesets",
        &OpenChangesetReq {
            author: "agent".into(),
            title: "interrupted".into(),
            story_ids: vec![],
            scope_prefixes: vec![],
            stack_parent_cs_id: None,
            lease_mode: LeaseMode::Soft,
            lease_ttl_secs: 3600,
        },
    )
    .await?;
    let Some(open) = open else {
        return Err("open failed".into());
    };
    let cs_id = open.changeset.cs_id;
    let op = put_op(&h.app, "docs/b.md", b"b\n").await?;
    h.state
        .db
        .queue_changeset_submission(&cs_id, &[op], None, None)?;
    // Simulate a crash mid-verify.
    h.state.db.update_changeset_state(
        &cs_id,
        ChangesetStatus::Verifying,
        None,
        None,
        None,
        None,
        None,
    )?;

    let requeued = dockside_server::integrator::recover_and_drain(&h.state).await?;
    assert_eq!(requeued, 1);
    let rec = h.state.db.get_changeset(&cs_id)?;
    assert_eq!(rec.status, ChangesetStatus::Landed);
    Ok(())
}

#[tokio::test]
async fn legacy_status_names_map_to_automated_states() {
    assert_eq!(
        ChangesetStatus::parse("awaiting_approval"),
        Some(ChangesetStatus::Queued)
    );
    assert_eq!(
        ChangesetStatus::parse("rejected"),
        Some(ChangesetStatus::Abandoned)
    );
}
