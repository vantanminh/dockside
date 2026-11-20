use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use dockside_core::config::DocksideConfig;
use dockside_core::hash::hash_bytes;
use dockside_core::model::{
    ChangesetOp, ChangesetRecord, ChangesetStatus, ImportRevisionReq, LeaseMode, OpenChangesetReq,
    OpenChangesetResp, ReleasePlan, ReleaseReq, RevisionRecord, SequenceNextResp,
    SubmitChangesetReq, TreeFlatResp,
};
use dockside_server::{AppState, ServerOptions, build_router};
use http_body_util::BodyExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tower::ServiceExt;

/// Merge-logic tests run with verify disabled, so allow sensitive paths without an executed gate.
/// (The strict default is covered in `tests/autonomy.rs`.)
fn merge_test_config() -> DocksideConfig {
    let mut cfg = DocksideConfig::default_pilot();
    cfg.policy.allow_unverified_sensitive = true;
    cfg
}

async fn setup_app(
    auth_token: Option<String>,
) -> Result<(tempfile::TempDir, axum::Router), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let state = AppState::init(ServerOptions {
        data_dir: dir.path().to_path_buf(),
        config: merge_test_config(),
        r2: None,
        auth_token,
        git_mirror_dir: None,
        verify_workdir: None,
        run_verify: false,
    })
    .await?;
    Ok((dir, build_router(state)))
}

async fn put_blob(
    app: &axum::Router,
    data: &[u8],
    token: Option<&str>,
) -> Result<(String, u64), Box<dyn std::error::Error>> {
    let hash = hash_bytes(data);
    let mut builder = Request::builder()
        .method(Method::PUT)
        .uri(format!("/v1/blobs/{hash}"));
    if let Some(t) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = builder.body(Body::from(data.to_vec()))?;
    let resp = app.clone().oneshot(req).await?;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    Ok((hash, data.len() as u64))
}

async fn post_json<Req: Serialize, Resp: DeserializeOwned>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
    token: Option<&str>,
) -> Result<(StatusCode, Option<Resp>), Box<dyn std::error::Error>> {
    let bytes = serde_json::to_vec(body)?;
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = builder.body(Body::from(bytes))?;
    let resp = app.clone().oneshot(req).await?;
    let status = resp.status();
    let raw = resp.into_body().collect().await?.to_bytes();
    if status.is_success() {
        let parsed: Resp = serde_json::from_slice(&raw)?;
        Ok((status, Some(parsed)))
    } else {
        Ok((status, None))
    }
}

async fn get_json<Resp: DeserializeOwned>(
    app: &axum::Router,
    uri: &str,
    token: Option<&str>,
) -> Result<(StatusCode, Option<Resp>), Box<dyn std::error::Error>> {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    if let Some(t) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = builder.body(Body::empty())?;
    let resp = app.clone().oneshot(req).await?;
    let status = resp.status();
    let raw = resp.into_body().collect().await?.to_bytes();
    if status.is_success() {
        let parsed: Resp = serde_json::from_slice(&raw)?;
        Ok((status, Some(parsed)))
    } else {
        Ok((status, None))
    }
}

#[tokio::test]
async fn parallel_agents_auto_land_and_auto_merge_routes() -> Result<(), Box<dyn std::error::Error>>
{
    let (_tmp, app) = setup_app(None).await?;

    // 1. Seed r1 with base files
    let routes_base = b"export default [\n  route(\"home\", \"routes/home.tsx\"),\n];\n";
    let (h_routes, s_routes) = put_blob(&app, routes_base, None).await?;
    let (h_note, s_note) = put_blob(&app, b"pub fn note() {}\n", None).await?;

    let (st, rev1) = post_json::<_, RevisionRecord>(
        &app,
        "/v1/import",
        &ImportRevisionReq {
            author: "agent".into(),
            summary: "Initial pilot import".into(),
            ops: vec![
                ChangesetOp::Put {
                    path: "apps/web/app/routes.ts".into(),
                    hash: h_routes,
                    size: s_routes,
                    executable: false,
                },
                ChangesetOp::Put {
                    path: "services/api/src/notes.rs".into(),
                    hash: h_note,
                    size: s_note,
                    executable: false,
                },
            ],
        },
        None,
    )
    .await?;
    assert_eq!(st, StatusCode::OK);
    let Some(r1) = rev1 else {
        panic!("expected r1");
    };
    assert_eq!(r1.rev_id, 1);

    // 2. Two agents open concurrently on r1, both editing apps/web/app/routes.ts
    let (_, open_a) = post_json::<_, OpenChangesetResp>(
        &app,
        "/v1/changesets",
        &OpenChangesetReq {
            author: "agent-a".into(),
            title: "Add billing route".into(),
            story_ids: vec!["US-001".into()],
            scope_prefixes: vec!["apps/web".into()],
            stack_parent_cs_id: None,
            lease_mode: LeaseMode::Soft,
            lease_ttl_secs: 3600,
        },
        None,
    )
    .await?;
    let Some(open_a) = open_a else {
        panic!("open_a failed");
    };

    let (_, open_b) = post_json::<_, OpenChangesetResp>(
        &app,
        "/v1/changesets",
        &OpenChangesetReq {
            author: "agent-b".into(),
            title: "Add cdn route".into(),
            story_ids: vec!["US-002".into()],
            scope_prefixes: vec!["apps/web".into()],
            stack_parent_cs_id: None,
            lease_mode: LeaseMode::Soft,
            lease_ttl_secs: 3600,
        },
        None,
    )
    .await?;
    let Some(open_b) = open_b else {
        panic!("open_b failed");
    };
    // Soft lease overlap should warn Agent B!
    assert_eq!(open_b.lease_warnings.len(), 1);
    assert_eq!(open_b.lease_warnings[0].cs_id, open_a.changeset.cs_id);

    // 3. Agent A submits -> auto-lands as r2
    let routes_a =
        b"export default [\n  route(\"home\", \"routes/home.tsx\"),\n  route(\"billing\", \"routes/billing.tsx\"),\n];\n";
    let (ha, sa) = put_blob(&app, routes_a, None).await?;
    let (_, sub_a) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{}/submit", open_a.changeset.cs_id),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "apps/web/app/routes.ts".into(),
                hash: ha,
                size: sa,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    let Some(sub_a) = sub_a else {
        panic!("sub_a failed");
    };
    assert_eq!(sub_a.status, ChangesetStatus::Landed);
    assert_eq!(sub_a.landed_rev, Some(2));

    // 4. Agent B (still based on r1!) submits -> auto-merges hot registry routes.ts -> lands as r3!
    let routes_b =
        b"export default [\n  route(\"home\", \"routes/home.tsx\"),\n  route(\"cdn\", \"routes/cdn.tsx\"),\n];\n";
    let (hb, sb) = put_blob(&app, routes_b, None).await?;
    let (_, sub_b) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{}/submit", open_b.changeset.cs_id),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "apps/web/app/routes.ts".into(),
                hash: hb,
                size: sb,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    let Some(sub_b) = sub_b else {
        panic!("sub_b failed");
    };
    assert_eq!(sub_b.status, ChangesetStatus::Landed);
    assert_eq!(sub_b.landed_rev, Some(3));

    Ok(())
}

#[tokio::test]
async fn sensitive_migrations_land_automatically_and_auto_number_sequentially()
-> Result<(), Box<dyn std::error::Error>> {
    let (_tmp, app) = setup_app(None).await?;

    let (h86, s86) = put_blob(&app, b"CREATE TABLE maint (id int);\n", None).await?;
    let _ = post_json::<_, RevisionRecord>(
        &app,
        "/v1/import",
        &ImportRevisionReq {
            author: "agent".into(),
            summary: "Seed migrations".into(),
            ops: vec![ChangesetOp::Put {
                path: "services/api/src/db/migrations/0086_platform_maintenance.sql".into(),
                hash: h86,
                size: s86,
                executable: false,
            }],
        },
        None,
    )
    .await?;

    // Two parallel agents both create pending_*.sql based on r1
    let (_, open1) = post_json::<_, OpenChangesetResp>(
        &app,
        "/v1/changesets",
        &OpenChangesetReq {
            author: "agent-1".into(),
            title: "Add tags table".into(),
            story_ids: vec![],
            scope_prefixes: vec!["services/api/src/db/migrations".into()],
            stack_parent_cs_id: None,
            lease_mode: LeaseMode::Soft,
            lease_ttl_secs: 3600,
        },
        None,
    )
    .await?;
    let Some(open1) = open1 else { panic!("open1") };

    let (_, open2) = post_json::<_, OpenChangesetResp>(
        &app,
        "/v1/changesets",
        &OpenChangesetReq {
            author: "agent-2".into(),
            title: "Add badges table".into(),
            story_ids: vec![],
            scope_prefixes: vec!["services/api/src/db/migrations".into()],
            stack_parent_cs_id: None,
            lease_mode: LeaseMode::Soft,
            lease_ttl_secs: 3600,
        },
        None,
    )
    .await?;
    let Some(open2) = open2 else { panic!("open2") };

    let (hm1, sm1) = put_blob(&app, b"CREATE TABLE tags (id int);\n", None).await?;
    let (hm2, sm2) = put_blob(&app, b"CREATE TABLE badges (id int);\n", None).await?;

    let (_, sub1) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{}/submit", open1.changeset.cs_id),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "services/api/src/db/migrations/pending_tags.sql".into(),
                hash: hm1,
                size: sm1,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    let Some(sub1) = sub1 else { panic!("sub1") };
    // No human gate: the sensitive changeset lands by itself and is only flagged.
    assert_eq!(sub1.status, ChangesetStatus::Landed);
    assert_eq!(sub1.landed_rev, Some(2));
    assert!(!sub1.sensitive_paths.is_empty());

    let (_, sub2) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{}/submit", open2.changeset.cs_id),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "services/api/src/db/migrations/pending_badges.sql".into(),
                hash: hm2,
                size: sm2,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    let Some(sub2) = sub2 else { panic!("sub2") };
    // Second agent was based on r1; the server rebases it onto r2 and renumbers its migration.
    assert_eq!(sub2.status, ChangesetStatus::Landed);
    assert_eq!(sub2.landed_rev, Some(3));

    let (_, tree3) = get_json::<TreeFlatResp>(&app, "/v1/revisions/3/tree", None).await?;
    let Some(tree3) = tree3 else { panic!("tree3") };
    assert!(
        tree3
            .files
            .contains_key("services/api/src/db/migrations/0087_tags.sql")
    );
    assert!(
        tree3
            .files
            .contains_key("services/api/src/db/migrations/0088_badges.sql")
    );
    Ok(())
}

#[tokio::test]
async fn conflict_does_not_block_other_agents_and_secrets_are_blocked()
-> Result<(), Box<dyn std::error::Error>> {
    let (_tmp, app) = setup_app(Some("secret-bearer-token".into())).await?;
    let tok = Some("secret-bearer-token");

    // Unauthenticated request must fail with 401
    let (unauth_status, _) = get_json::<Vec<RevisionRecord>>(&app, "/v1/revisions", None).await?;
    assert_eq!(unauth_status, StatusCode::UNAUTHORIZED);

    // Seed r1
    let (hb, sb) = put_blob(&app, b"title = 'base'\n", tok).await?;
    let _ = post_json::<_, RevisionRecord>(
        &app,
        "/v1/import",
        &ImportRevisionReq {
            author: "agent".into(),
            summary: "init".into(),
            ops: vec![ChangesetOp::Put {
                path: "apps/web/app/root.tsx".into(),
                hash: hb,
                size: sb,
                executable: false,
            }],
        },
        tok,
    )
    .await?;

    // Secret file must be rejected on import/submit
    let (hs, ss) = put_blob(&app, b"OPENAI_API_KEY=sk-fake\n", tok).await?;
    let (sec_status, _) = post_json::<_, RevisionRecord>(
        &app,
        "/v1/import",
        &ImportRevisionReq {
            author: "agent".into(),
            summary: "bad".into(),
            ops: vec![ChangesetOp::Put {
                path: "services/api/.env".into(),
                hash: hs,
                size: ss,
                executable: false,
            }],
        },
        tok,
    )
    .await?;
    assert_eq!(sec_status, StatusCode::BAD_REQUEST);

    // Sequence allocator prevents IN-015 collisions
    let (_, s1) =
        post_json::<_, SequenceNextResp>(&app, "/v1/sequences/intake/next", &(), tok).await?;
    let (_, s2) =
        post_json::<_, SequenceNextResp>(&app, "/v1/sequences/intake/next", &(), tok).await?;
    assert_eq!(s1.map(|x| x.formatted).as_deref(), Some("IN-001"));
    assert_eq!(s2.map(|x| x.formatted).as_deref(), Some("IN-002"));

    // Release dry-run plan for both Rudder image (api) and Cloudflare Worker (web)
    let (rel_st, plan) = post_json::<_, ReleasePlan>(
        &app,
        "/v1/releases",
        &ReleaseReq {
            target: "web".into(),
            env: "prod".into(),
            rev_id: 1,
            actor: "agent".into(),
            execute: false,
        },
        tok,
    )
    .await?;
    assert_eq!(rel_st, StatusCode::OK);
    let Some(plan) = plan else { panic!("plan") };
    assert!(!plan.executed);
    assert_eq!(plan.kind, "cloudflare-worker");
    assert!(plan.commands.iter().any(|c| c.contains("api:check-domain")));
    Ok(())
}

#[tokio::test]
async fn conflict_isolated_from_queue_and_stacked_changesets_unwind_cleanly()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mirror_dir = dir.path().join("git-mirror");
    let state = AppState::init(ServerOptions {
        data_dir: dir.path().join("data"),
        config: merge_test_config(),
        r2: None,
        auth_token: None,
        git_mirror_dir: Some(mirror_dir.clone()),
        verify_workdir: None,
        run_verify: false,
    })
    .await?;
    let app = build_router(state);

    // Seed r1
    let (h1, s1) = put_blob(&app, b"const title = 'Base';\n", None).await?;
    let (h2, s2) = put_blob(&app, b"pub fn other() {}\n", None).await?;
    let _ = post_json::<_, RevisionRecord>(
        &app,
        "/v1/import",
        &ImportRevisionReq {
            author: "agent".into(),
            summary: "init".into(),
            ops: vec![
                ChangesetOp::Put {
                    path: "apps/web/app/root.tsx".into(),
                    hash: h1,
                    size: s1,
                    executable: false,
                },
                ChangesetOp::Put {
                    path: "services/api/src/other.rs".into(),
                    hash: h2,
                    size: s2,
                    executable: false,
                },
            ],
        },
        None,
    )
    .await?;

    // Open Agent A, Agent B (will conflict on root.tsx), Agent C (independent file)
    let mut open_cs = Vec::new();
    for (author, title, scope) in [
        ("agent-a", "Edit title A", "apps/web"),
        ("agent-b", "Edit title B", "apps/web"),
        ("agent-c", "Edit other.rs", "services/api"),
    ] {
        let (_, resp) = post_json::<_, OpenChangesetResp>(
            &app,
            "/v1/changesets",
            &OpenChangesetReq {
                author: author.into(),
                title: title.into(),
                story_ids: vec![],
                scope_prefixes: vec![scope.into()],
                stack_parent_cs_id: None,
                lease_mode: LeaseMode::Soft,
                lease_ttl_secs: 3600,
            },
            None,
        )
        .await?;
        let Some(r) = resp else { panic!("open") };
        open_cs.push(r.changeset.cs_id);
    }

    // Agent A lands r2
    let (ha, sa) = put_blob(&app, b"const title = 'Acme A';\n", None).await?;
    let (_, ra) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{}/submit", open_cs[0]),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "apps/web/app/root.tsx".into(),
                hash: ha,
                size: sa,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    assert_eq!(ra.as_ref().map(|x| x.status), Some(ChangesetStatus::Landed));

    // Agent B conflicts with r2
    let (hb, sb) = put_blob(&app, b"const title = 'Acme B';\n", None).await?;
    let (_, rb) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{}/submit", open_cs[1]),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "apps/web/app/root.tsx".into(),
                hash: hb,
                size: sb,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    let Some(rb) = rb else { panic!("rb") };
    assert_eq!(rb.status, ChangesetStatus::Conflict);
    assert_eq!(rb.conflicts.len(), 1);

    // Agent C is NOT blocked by Agent B's conflict -> lands cleanly as r3!
    let (hc, sc) = put_blob(&app, b"pub fn other() { /* updated */ }\n", None).await?;
    let (_, rc) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{}/submit", open_cs[2]),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "services/api/src/other.rs".into(),
                hash: hc,
                size: sc,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    assert_eq!(rc.as_ref().map(|x| x.landed_rev), Some(Some(3)));

    // Resolver resubmits Agent B with base_rev = 3 -> lands cleanly as r4!
    let (hres, sres) = put_blob(&app, b"const title = 'Acme A+B';\n", None).await?;
    let (_, rb_fixed) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{}/submit", open_cs[1]),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "apps/web/app/root.tsx".into(),
                hash: hres,
                size: sres,
                executable: false,
            }],
            base_rev: Some(3),
            title: None,
        },
        None,
    )
    .await?;
    assert_eq!(rb_fixed.as_ref().map(|x| x.landed_rev), Some(Some(4)));

    // Stacked pair: CS-parent (sensitive, auto-lands) and CS-child (stacked on CS-parent)
    let (_, p_open) = post_json::<_, OpenChangesetResp>(
        &app,
        "/v1/changesets",
        &OpenChangesetReq {
            author: "agent-p".into(),
            title: "Auth helper v1".into(),
            story_ids: vec![],
            scope_prefixes: vec!["services/api/auth".into()],
            stack_parent_cs_id: None,
            lease_mode: LeaseMode::Soft,
            lease_ttl_secs: 3600,
        },
        None,
    )
    .await?;
    let Some(p_open) = p_open else {
        panic!("p_open")
    };
    let parent_id = p_open.changeset.cs_id.clone();

    // Child stacked on parent edits normal file -> stays queued until parent lands!
    let (_, c_open) = post_json::<_, OpenChangesetResp>(
        &app,
        "/v1/changesets",
        &OpenChangesetReq {
            author: "agent-c2".into(),
            title: "Use auth helper".into(),
            story_ids: vec![],
            scope_prefixes: vec!["services/api/src".into()],
            stack_parent_cs_id: Some(parent_id.clone()),
            lease_mode: LeaseMode::Soft,
            lease_ttl_secs: 3600,
        },
        None,
    )
    .await?;
    let Some(c_open) = c_open else {
        panic!("c_open")
    };
    let child_id = c_open.changeset.cs_id.clone();

    let (hc2, sc2) = put_blob(&app, b"pub fn use_guard() {}\n", None).await?;
    let (_, c_sub) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{child_id}/submit"),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "services/api/src/use_guard.rs".into(),
                hash: hc2,
                size: sc2,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    assert_eq!(
        c_sub.as_ref().map(|x| x.status),
        Some(ChangesetStatus::Queued)
    );

    // The child waited in the queue because its parent was still open; now submit the parent.
    let (hp, sp) = put_blob(&app, b"pub fn check() -> bool { true }\n", None).await?;
    let (_, p_sub) = post_json::<_, ChangesetRecord>(
        &app,
        &format!("/v1/changesets/{parent_id}/submit"),
        &SubmitChangesetReq {
            ops: vec![ChangesetOp::Put {
                path: "services/api/auth/guard.rs".into(),
                hash: hp,
                size: sp,
                executable: false,
            }],
            base_rev: None,
            title: None,
        },
        None,
    )
    .await?;
    // Sensitive parent needs no approval: it lands as r5 and its queued child drains as r6.
    assert_eq!(p_sub.as_ref().map(|x| x.landed_rev), Some(Some(5)));

    let (_, c_after) =
        get_json::<ChangesetRecord>(&app, &format!("/v1/changesets/{child_id}"), None).await?;
    assert_eq!(
        c_after.as_ref().map(|x| x.status),
        Some(ChangesetStatus::Landed)
    );
    assert_eq!(c_after.as_ref().map(|x| x.landed_rev), Some(Some(6)));

    // Verify 1-way Git mirror received clean standalone commits (HEAD intact, isolated repo)!
    assert!(mirror_dir.join(".git").join("HEAD").is_file());
    assert!(mirror_dir.join("services/api/src/use_guard.rs").is_file());
    let mirror_count = std::process::Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(&mirror_dir)
        .env("GIT_DIR", mirror_dir.join(".git"))
        .output()?;
    assert!(mirror_count.status.success());
    assert_eq!(String::from_utf8_lossy(&mirror_count.stdout).trim(), "6");
    Ok(())
}
