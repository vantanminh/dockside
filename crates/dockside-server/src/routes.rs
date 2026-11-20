use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use dockside_core::hash::{hash_bytes, is_valid_hash, normalize_path, normalize_scope_prefix};
use dockside_core::merge::unified_diff;
use dockside_core::model::{
    BlobsExistReq, BlobsExistResp, ChangesetOp, ChangesetRecord, ChangesetStatus, DeploymentRecord,
    HealthResp, ImportRevisionReq, LeaseRecord, OpenChangesetReq, OpenChangesetResp, ReleasePlan,
    ReleaseReq, RevisionRecord, RollbackReq, SequenceNextResp, SnapshotRecord, SnapshotReq,
    SubmitChangesetReq, TreeFlatResp,
};
use dockside_core::tree::{apply_ops, read_scoped_tree_flat, read_tree_flat, write_tree};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::error::{ServerError, ServerResult};
use crate::integrator::{drain_queue, integrate_changeset};
use crate::release::{handle_release, handle_rollback};
use crate::ui::DASHBOARD_HTML;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/blobs/missing", post(blobs_missing))
        .route("/blobs/{hash}", put(put_blob).get(get_blob))
        .route("/revisions", get(list_revisions))
        .route("/revisions/{rev_id}", get(get_revision))
        .route("/revisions/{rev_id}/tree", get(get_revision_tree))
        .route("/import", post(import_revision))
        .route("/changesets", get(list_changesets).post(open_changeset))
        .route("/changesets/{cs_id}", get(get_changeset))
        .route(
            "/changesets/{cs_id}/snapshots",
            get(list_snapshots).post(create_snapshot),
        )
        .route("/changesets/{cs_id}/submit", post(submit_changeset))
        .route("/changesets/{cs_id}/abandon", post(abandon_changeset))
        .route("/changesets/{cs_id}/diff", get(diff_changeset))
        .route("/queue/drain", post(drain_merge_queue))
        .route("/leases", get(list_leases))
        .route("/sequences/{kind}/next", post(next_sequence))
        .route("/deployments", get(list_deployments))
        .route("/releases", post(trigger_release))
        .route("/rollbacks", post(trigger_rollback))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth));

    Router::new()
        .route("/", get(index_ui))
        .route("/v1/health", get(health))
        .nest("/v1", api)
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state)
}

async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ServerError> {
    if let Some(ref expected) = state.auth_token {
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .unwrap_or("");
        let ok = provided.len() == expected.len()
            && bool::from(provided.as_bytes().ct_eq(expected.as_bytes()));
        if !ok {
            return Err(ServerError::Unauthorized);
        }
    }
    Ok(next.run(req).await)
}

async fn index_ui() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn health(State(state): State<AppState>) -> ServerResult<Json<HealthResp>> {
    let main_rev = state.db.main_rev_id()?;
    Ok(Json(HealthResp {
        status: "ok".to_string(),
        repo_name: state.config.repo.name.clone(),
        main_rev,
        r2_enabled: state.cas.is_r2_enabled(),
        git_mirror_enabled: state.git_mirror.is_some(),
        verify_enabled: state.run_verify,
    }))
}

async fn blobs_missing(
    State(state): State<AppState>,
    Json(req): Json<BlobsExistReq>,
) -> ServerResult<Json<BlobsExistResp>> {
    let mut missing = Vec::new();
    for h in req.hashes {
        if !is_valid_hash(&h) {
            return Err(ServerError::BadRequest(format!("invalid hash '{h}'")));
        }
        if !state.cas.exists(&h).await? {
            missing.push(h);
        }
    }
    Ok(Json(BlobsExistResp { missing }))
}

async fn put_blob(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    body: Bytes,
) -> ServerResult<StatusCode> {
    if !is_valid_hash(&hash) {
        return Err(ServerError::BadRequest(format!("invalid hash '{hash}'")));
    }
    let actual = hash_bytes(&body);
    if actual != hash {
        return Err(ServerError::Core(dockside_core::CoreError::HashMismatch {
            expected: hash,
            actual,
        }));
    }
    let _ = state.cas.put_bytes(&body).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_blob(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> ServerResult<impl IntoResponse> {
    let raw = state.cas.get_bytes(&hash).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    Ok((StatusCode::OK, headers, raw))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

async fn list_revisions(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> ServerResult<Json<Vec<RevisionRecord>>> {
    let items = state.db.list_revisions(q.limit.unwrap_or(50))?;
    Ok(Json(items))
}

async fn get_revision(
    State(state): State<AppState>,
    Path(rev_id): Path<i64>,
) -> ServerResult<Json<RevisionRecord>> {
    Ok(Json(state.db.get_revision(rev_id)?))
}

#[derive(Deserialize)]
struct TreeQuery {
    scope: Option<String>,
}

async fn get_revision_tree(
    State(state): State<AppState>,
    Path(rev_id): Path<i64>,
    Query(q): Query<TreeQuery>,
) -> ServerResult<Json<TreeFlatResp>> {
    let rev = state.db.get_revision(rev_id)?;
    let scopes: Vec<String> = q
        .scope
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let files = read_scoped_tree_flat(&state.cas, &rev.tree_hash, &scopes).await?;
    Ok(Json(TreeFlatResp {
        rev_id: rev.rev_id,
        tree_hash: rev.tree_hash,
        files,
    }))
}

async fn import_revision(
    State(state): State<AppState>,
    Json(req): Json<ImportRevisionReq>,
) -> ServerResult<Json<RevisionRecord>> {
    // Run detached: a large import can outlast a proxy's request timeout (Cloudflare cuts at ~100 s),
    // and dropping the request future would otherwise abort it half way. The client can then look the
    // revision up afterwards.
    tokio::spawn(import_revision_inner(state, req))
        .await
        .map_err(|e| ServerError::Internal(format!("import task failed: {e}")))?
}

async fn import_revision_inner(
    state: AppState,
    req: ImportRevisionReq,
) -> ServerResult<Json<RevisionRecord>> {
    let _guard = state.merge_lock.lock().await;
    let mut normalized_ops = Vec::with_capacity(req.ops.len());
    for op in &req.ops {
        let p = normalize_path(op.path())?;
        state.policy.ensure_no_secrets([p.as_str()])?;
        match op {
            ChangesetOp::Put {
                hash,
                size,
                executable,
                ..
            } => {
                if !state.cas.exists(hash).await? {
                    return Err(ServerError::BadRequest(format!(
                        "missing blob {hash} for path {p}; upload blobs first"
                    )));
                }
                normalized_ops.push(ChangesetOp::Put {
                    path: p,
                    hash: hash.clone(),
                    size: *size,
                    executable: *executable,
                });
            }
            ChangesetOp::Delete { .. } => {
                normalized_ops.push(ChangesetOp::Delete { path: p });
            }
        }
    }

    let head_id = state.db.main_rev_id()?;
    let head_rev = state.db.get_revision(head_id)?;
    let head_tree = read_tree_flat(&state.cas, &head_rev.tree_hash).await?;
    let next_tree = apply_ops(&head_tree, &normalized_ops)?;
    let new_tree_hash = write_tree(&state.cas, &next_tree).await?;

    let mut touched: Vec<String> = normalized_ops
        .iter()
        .map(|o| o.path().to_string())
        .collect();
    touched.sort();
    touched.dedup();

    let rev = state.db.insert_revision(
        head_id,
        &new_tree_hash,
        None,
        &req.author,
        &[],
        &req.summary,
        &touched,
        None,
    )?;

    if let Some(ref mirror) = state.git_mirror {
        let _ = mirror.sync_revision(&state.cas, &rev).await;
    }

    Ok(Json(rev))
}

#[derive(Deserialize)]
struct ChangesetsQuery {
    status: Option<String>,
    limit: Option<i64>,
}

async fn list_changesets(
    State(state): State<AppState>,
    Query(q): Query<ChangesetsQuery>,
) -> ServerResult<Json<Vec<ChangesetRecord>>> {
    let filter = match q.status.as_deref() {
        Some(s) if !s.is_empty() => Some(
            ChangesetStatus::parse(s)
                .ok_or_else(|| ServerError::BadRequest(format!("unknown status '{s}'")))?,
        ),
        _ => None,
    };
    let items = state.db.list_changesets(filter, q.limit.unwrap_or(100))?;
    Ok(Json(items))
}

async fn open_changeset(
    State(state): State<AppState>,
    Json(req): Json<OpenChangesetReq>,
) -> ServerResult<Json<OpenChangesetResp>> {
    let mut scopes = Vec::with_capacity(req.scope_prefixes.len());
    for s in &req.scope_prefixes {
        let norm = normalize_scope_prefix(s)?;
        if !norm.is_empty() {
            scopes.push(norm);
        }
    }

    let base_rev = if let Some(ref parent_id) = req.stack_parent_cs_id {
        let parent = state.db.get_changeset(parent_id)?;
        parent.base_rev
    } else {
        state.db.main_rev_id()?
    };

    let seq = state.db.next_sequence("changeset")?;
    let (changeset, lease_warnings) = state.db.create_changeset(
        &seq.formatted,
        base_rev,
        req.stack_parent_cs_id.as_deref(),
        &req.author,
        &req.title,
        &req.story_ids,
        &scopes,
        req.lease_mode,
        req.lease_ttl_secs,
    )?;

    Ok(Json(OpenChangesetResp {
        changeset,
        lease_warnings,
    }))
}

async fn get_changeset(
    State(state): State<AppState>,
    Path(cs_id): Path<String>,
) -> ServerResult<Json<ChangesetRecord>> {
    Ok(Json(state.db.get_changeset(&cs_id)?))
}

async fn create_snapshot(
    State(state): State<AppState>,
    Path(cs_id): Path<String>,
    Json(req): Json<SnapshotReq>,
) -> ServerResult<Json<SnapshotRecord>> {
    for op in &req.ops {
        let norm = normalize_path(op.path())?;
        state.policy.ensure_no_secrets([norm.as_str()])?;
    }
    let snap = state.db.add_snapshot(&cs_id, &req.message, &req.ops)?;
    Ok(Json(snap))
}

async fn list_snapshots(
    State(state): State<AppState>,
    Path(cs_id): Path<String>,
) -> ServerResult<Json<Vec<SnapshotRecord>>> {
    Ok(Json(state.db.list_snapshots(&cs_id)?))
}

#[derive(Deserialize)]
struct SubmitQuery {
    wait: Option<bool>,
}

async fn submit_changeset(
    State(state): State<AppState>,
    Path(cs_id): Path<String>,
    Query(q): Query<SubmitQuery>,
    Json(req): Json<SubmitChangesetReq>,
) -> ServerResult<Json<ChangesetRecord>> {
    let mut norm_ops = Vec::with_capacity(req.ops.len());
    for op in &req.ops {
        let norm = normalize_path(op.path())?;
        state.policy.ensure_no_secrets([norm.as_str()])?;
        match op {
            ChangesetOp::Put {
                hash,
                size,
                executable,
                ..
            } => {
                if !state.cas.exists(hash).await? {
                    return Err(ServerError::BadRequest(format!(
                        "missing blob {hash} for path {norm}; upload blobs before submit"
                    )));
                }
                norm_ops.push(ChangesetOp::Put {
                    path: norm,
                    hash: hash.clone(),
                    size: *size,
                    executable: *executable,
                });
            }
            ChangesetOp::Delete { .. } => {
                norm_ops.push(ChangesetOp::Delete { path: norm });
            }
        }
    }

    let queued = state.db.queue_changeset_submission(
        &cs_id,
        &norm_ops,
        req.base_rev,
        req.title.as_deref(),
    )?;

    if q.wait.unwrap_or(true) {
        let final_rec = integrate_changeset(&state, &cs_id).await?;
        Ok(Json(final_rec))
    } else {
        let state_bg = state.clone();
        let id_bg = cs_id.clone();
        tokio::spawn(async move {
            let _ = integrate_changeset(&state_bg, &id_bg).await;
        });
        Ok(Json(queued))
    }
}

async fn abandon_changeset(
    State(state): State<AppState>,
    Path(cs_id): Path<String>,
) -> ServerResult<Json<ChangesetRecord>> {
    let cs = state.db.get_changeset(&cs_id)?;
    if cs.status == ChangesetStatus::Landed {
        return Err(ServerError::BadRequest(format!(
            "cannot abandon landed changeset {cs_id}"
        )));
    }
    let rec = state.db.update_changeset_state(
        &cs_id,
        ChangesetStatus::Abandoned,
        None,
        None,
        Some("Abandoned"),
        None,
        None,
    )?;
    Ok(Json(rec))
}

async fn diff_changeset(
    State(state): State<AppState>,
    Path(cs_id): Path<String>,
) -> ServerResult<String> {
    let cs = state.db.get_changeset(&cs_id)?;
    let base_rev = state.db.get_revision(cs.base_rev)?;
    let base_tree = read_tree_flat(&state.cas, &base_rev.tree_hash).await?;

    let mut out = String::new();
    for op in &cs.ops {
        match op {
            ChangesetOp::Put { path, hash, .. } => {
                let old_bytes = match base_tree.get(path) {
                    Some(e) => state.cas.get_bytes(&e.hash).await?,
                    None => Vec::new(),
                };
                let new_bytes = state.cas.get_bytes(hash).await?;
                if old_bytes.contains(&0)
                    || new_bytes.contains(&0)
                    || std::str::from_utf8(&old_bytes).is_err()
                    || std::str::from_utf8(&new_bytes).is_err()
                {
                    out.push_str(&format!(
                        "Binary file changed: {path} ({} -> {} bytes)\n",
                        old_bytes.len(),
                        new_bytes.len()
                    ));
                } else {
                    let old_str = String::from_utf8_lossy(&old_bytes);
                    let new_str = String::from_utf8_lossy(&new_bytes);
                    out.push_str(&unified_diff(path, &old_str, &new_str));
                    out.push('\n');
                }
            }
            ChangesetOp::Delete { path } => {
                let old_bytes = match base_tree.get(path) {
                    Some(e) => state.cas.get_bytes(&e.hash).await?,
                    None => Vec::new(),
                };
                let old_str = String::from_utf8_lossy(&old_bytes);
                out.push_str(&unified_diff(path, &old_str, ""));
                out.push('\n');
            }
        }
    }
    Ok(out)
}

async fn drain_merge_queue(
    State(state): State<AppState>,
) -> ServerResult<Json<Vec<ChangesetRecord>>> {
    Ok(Json(drain_queue(&state).await?))
}

async fn list_leases(State(state): State<AppState>) -> ServerResult<Json<Vec<LeaseRecord>>> {
    Ok(Json(state.db.list_active_leases()?))
}

async fn next_sequence(
    State(state): State<AppState>,
    Path(kind): Path<String>,
) -> ServerResult<Json<SequenceNextResp>> {
    Ok(Json(state.db.next_sequence(&kind)?))
}

async fn list_deployments(
    State(state): State<AppState>,
    Query(q): Query<LimitQuery>,
) -> ServerResult<Json<Vec<DeploymentRecord>>> {
    Ok(Json(state.db.list_deployments(q.limit.unwrap_or(50))?))
}

async fn trigger_release(
    State(state): State<AppState>,
    Json(req): Json<ReleaseReq>,
) -> ServerResult<Json<ReleasePlan>> {
    Ok(Json(handle_release(&state, &req).await?))
}

async fn trigger_rollback(
    State(state): State<AppState>,
    Json(req): Json<RollbackReq>,
) -> ServerResult<Json<ReleasePlan>> {
    Ok(Json(handle_rollback(&state, &req).await?))
}
