//! Single-writer Merge Queue & Integrator engine.
//!
//! Serializes all updates to `main`, runs server-side 3-way merge, neutralizes
//! hot-file collisions (e.g. `pending_*.sql` -> `NNNN_*.sql`), flags sensitive paths, runs scoped
//! verify gates in a warm build cache, and atomically flips `main`.
//!
//! Fully automated: there is no human approval state. Sensitive paths (migrations, auth, infra,
//! contracts) are held to a stricter *automated* bar: the verify gate must actually execute.
//! Any internal error marks only that changeset `failed`; it never blocks the queue.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use dockside_core::hotfiles::process_migrations_on_land;
use dockside_core::merge::{MergeOutcome, three_way_merge_trees};
use dockside_core::model::{
    ChangesetRecord, ChangesetStatus, FileEntry, VerifyReport, VerifyStepResult,
};
use dockside_core::tree::{apply_ops, diff_trees_to_ops, read_tree_flat, write_tree};
use tracing::{error, info, warn};

use crate::AppState;
use crate::error::ServerResult;

/// Process a single changeset through the single-writer integrator pipeline.
/// Also drains any queued stacked children if this changeset lands on `main`.
pub async fn integrate_changeset(state: &AppState, cs_id: &str) -> ServerResult<ChangesetRecord> {
    let result = {
        let _guard = state.merge_lock.lock().await;
        integrate_one_guarded(state, cs_id).await?
    };

    // If this changeset landed, opportunistically drain other queued changesets (e.g. stacked children).
    if result.status == ChangesetStatus::Landed {
        let _ = drain_queue(state).await;
    }

    Ok(result)
}

/// Drain all currently queued changesets in FIFO order.
pub async fn drain_queue(state: &AppState) -> ServerResult<Vec<ChangesetRecord>> {
    let _guard = state.merge_lock.lock().await;
    let mut processed = Vec::new();
    // Bounded passes to allow multi-level stacks to unwind cleanly without infinite loops.
    for _pass in 0..8 {
        let queued = state.db.list_queued_cs_ids()?;
        if queued.is_empty() {
            break;
        }
        let mut landed_any = false;
        for id in queued {
            let rec = integrate_one_guarded(state, &id).await?;
            if rec.status == ChangesetStatus::Landed {
                landed_any = true;
            }
            processed.push(rec);
        }
        if !landed_any {
            break;
        }
    }
    Ok(processed)
}

/// Re-run changesets interrupted by a crash/restart (`merging`/`verifying`), then drain the queue.
pub async fn recover_and_drain(state: &AppState) -> ServerResult<usize> {
    let requeued = state.db.requeue_interrupted()?;
    let processed = drain_queue(state).await?;
    if requeued > 0 || !processed.is_empty() {
        info!(
            requeued,
            processed = processed.len(),
            "recovered queue after restart"
        );
    }
    Ok(requeued)
}

/// Run one changeset; an internal error fails only that changeset (never wedges the queue).
async fn integrate_one_guarded(state: &AppState, cs_id: &str) -> ServerResult<ChangesetRecord> {
    match integrate_one_locked(state, cs_id).await {
        Ok(rec) => Ok(rec),
        Err(e) => {
            error!(cs_id, error = %e, "integrator error; failing changeset");
            let msg = format!("integrator error: {e}");
            // If the changeset cannot even be updated (e.g. unknown id), surface the original error.
            match state.db.update_changeset_state(
                cs_id,
                ChangesetStatus::Failed,
                None,
                None,
                Some(&msg),
                None,
                None,
            ) {
                Ok(rec) => Ok(rec),
                Err(_) => Err(e),
            }
        }
    }
}

async fn integrate_one_locked(state: &AppState, cs_id: &str) -> ServerResult<ChangesetRecord> {
    let cs = state.db.get_changeset(cs_id)?;

    if cs.status != ChangesetStatus::Queued {
        return Ok(cs);
    }

    // 1. Respect stacked parent ordering.
    let mut stacked_parent: Option<ChangesetRecord> = None;
    if let Some(ref parent_id) = cs.stack_parent_cs_id {
        let parent = state.db.get_changeset(parent_id)?;
        match parent.status {
            ChangesetStatus::Landed => {
                stacked_parent = Some(parent);
            }
            ChangesetStatus::Abandoned | ChangesetStatus::Failed => {
                return state.db.update_changeset_state(
                    cs_id,
                    ChangesetStatus::Failed,
                    None,
                    None,
                    Some(&format!(
                        "stack parent {parent_id} was {}",
                        parent.status.as_str()
                    )),
                    None,
                    None,
                );
            }
            _ => {
                // Keep queued until parent lands.
                return Ok(cs);
            }
        }
    }

    // 2. Defense-in-depth secret guard before touching anything.
    state
        .policy
        .ensure_no_secrets(cs.ops.iter().map(|op| op.path()))?;

    // 3. Load main head, base tree, and construct ours tree.
    let head_rev_id = state.db.main_rev_id()?;
    let head_rev = state.db.get_revision(head_rev_id)?;
    let head_tree = read_tree_flat(&state.cas, &head_rev.tree_hash).await?;

    let base_rev = state.db.get_revision(cs.base_rev)?;
    let raw_base_tree = read_tree_flat(&state.cas, &base_rev.tree_hash).await?;
    let effective_base_tree = if let Some(ref parent) = stacked_parent
        && parent.base_rev == cs.base_rev
    {
        apply_ops(&raw_base_tree, &parent.ops)?
    } else {
        raw_base_tree
    };
    let ours_tree = apply_ops(&effective_base_tree, &cs.ops)?;

    let _ = state.db.update_changeset_state(
        cs_id,
        ChangesetStatus::Merging,
        None,
        None,
        None,
        None,
        None,
    )?;

    // 4. Server-side 3-way merge onto latest main head.
    let merge_outcome = three_way_merge_trees(
        &effective_base_tree,
        &head_tree,
        &ours_tree,
        &state.cas,
        &state.policy,
    )
    .await?;

    let mut merged_tree = match merge_outcome {
        MergeOutcome::Clean { tree, .. } => tree,
        MergeOutcome::Conflicts(conflicts) => {
            warn!(
                cs_id,
                count = conflicts.len(),
                "changeset hit 3-way merge conflict"
            );
            return state.db.update_changeset_state(
                cs_id,
                ChangesetStatus::Conflict,
                None,
                None,
                Some("3-way merge conflict against latest main"),
                Some(&conflicts),
                None,
            );
        }
    };

    // 5. Hot-file neutralizer: auto-number pending_*.sql migrations & enforce immutability.
    let mig_outcome = process_migrations_on_land(
        &head_tree,
        &mut merged_tree,
        &state.config.hotfiles.migration_dirs,
    )?;
    if !mig_outcome.conflicts.is_empty() {
        warn!(cs_id, "changeset violated migration rules");
        return state.db.update_changeset_state(
            cs_id,
            ChangesetStatus::Conflict,
            None,
            None,
            Some("SQL migration hot-file rule violation"),
            Some(&mig_outcome.conflicts),
            None,
        );
    }

    // 6. Compute exact touched paths relative to current main head.
    let final_ops = diff_trees_to_ops(&head_tree, &merged_tree);
    let mut touched_paths: Vec<String> = final_ops.iter().map(|o| o.path().to_string()).collect();
    touched_paths.sort();
    touched_paths.dedup();

    // 7. Flag sensitive paths (migrations/infra/auth/contracts...). No human gate: they get a
    //    stricter automated bar below instead.
    let mut sensitive_paths = state.policy.sensitive_paths(&touched_paths);
    // Also check original op paths (e.g. pending_*.sql before rename)
    let orig_paths: Vec<String> = cs.ops.iter().map(|o| o.path().to_string()).collect();
    for t in state.policy.sensitive_paths(&orig_paths) {
        if !sensitive_paths.contains(&t) {
            sensitive_paths.push(t);
        }
    }
    sensitive_paths.sort();

    // 8. Scoped verify gate (warm cache).
    let _ = state.db.update_changeset_state(
        cs_id,
        ChangesetStatus::Verifying,
        Some(&sensitive_paths),
        None,
        None,
        None,
        None,
    )?;

    let verify_report = run_verify_gate(state, &merged_tree, &touched_paths).await;
    if verify_report.failed() {
        warn!(cs_id, "verify gate failed");
        return state.db.update_changeset_state(
            cs_id,
            ChangesetStatus::Failed,
            Some(&sensitive_paths),
            None,
            Some("verify gate failed; see verify_report for details"),
            None,
            Some(&verify_report),
        );
    }

    // 9. Sensitive changes must be backed by a verify gate that really ran when any affected
    //    target defines verify commands. "Skipped" is never treated as "passed".
    if !sensitive_paths.is_empty()
        && verify_report.skipped
        && !state.policy.allow_unverified_sensitive()
        && state
            .policy
            .affected_targets(&touched_paths)
            .iter()
            .any(|t| !t.verify_commands.is_empty())
    {
        warn!(
            cs_id,
            ?sensitive_paths,
            "sensitive changeset cannot land without an executed verify gate"
        );
        return state.db.update_changeset_state(
            cs_id,
            ChangesetStatus::Failed,
            Some(&sensitive_paths),
            None,
            Some(
                "sensitive paths touched but the verify gate did not run (enable DOCKSIDE_RUN_VERIFY); \
                 resubmit once verification is available",
            ),
            None,
            Some(&verify_report),
        );
    }

    // 10. Atomic land onto `main`.
    let new_tree_hash = write_tree(&state.cas, &merged_tree).await?;
    let mut summary = cs.title.clone();
    if !mig_outcome.renames.is_empty() {
        let renamed_desc: Vec<String> = mig_outcome
            .renames
            .iter()
            .map(|r| format!("{} -> {}", r.from, r.to))
            .collect();
        summary.push_str(&format!(" [migrations: {}]", renamed_desc.join(", ")));
    }

    let landed_rev = state.db.insert_revision(
        head_rev_id,
        &new_tree_hash,
        Some(cs_id),
        &cs.author,
        &cs.story_ids,
        &summary,
        &touched_paths,
        Some(&verify_report),
    )?;

    let updated = state.db.update_changeset_state(
        cs_id,
        ChangesetStatus::Landed,
        Some(&sensitive_paths),
        Some(landed_rev.rev_id),
        None,
        Some(&[]),
        Some(&verify_report),
    )?;

    info!(
        cs_id,
        rev_id = landed_rev.rev_id,
        files = touched_paths.len(),
        "landed changeset onto main"
    );

    // 11. Optional 1-way Git mirror backup (non-blocking error handling).
    if let Some(ref mirror) = state.git_mirror
        && let Err(e) = mirror.sync_revision(&state.cas, &landed_rev).await
    {
        warn!(rev_id = landed_rev.rev_id, error = %e, "git mirror sync failed (primary state unaffected)");
    }

    Ok(updated)
}

async fn run_verify_gate(
    state: &AppState,
    merged_tree: &BTreeMap<String, FileEntry>,
    touched_paths: &[String],
) -> VerifyReport {
    let affected = state.policy.affected_targets(touched_paths);
    let affected_names: Vec<String> = affected.iter().map(|t| t.name.clone()).collect();

    let skip = |reason: &str| VerifyReport {
        skipped: true,
        skip_reason: Some(reason.to_string()),
        passed: false,
        affected_targets: affected_names.clone(),
        steps: Vec::new(),
    };

    if affected.is_empty() {
        return skip("no target affected by touched paths");
    }
    if !affected.iter().any(|t| !t.verify_commands.is_empty()) {
        return skip("affected targets define no verify commands");
    }
    if !state.run_verify {
        return skip("verify disabled (set DOCKSIDE_RUN_VERIFY=true)");
    }

    // Materialize the exact revision into the warm verify directory (keeps `target/`, `node_modules/`).
    let mut steps = Vec::new();
    if let Err(e) = sync_warm_verify_dir(&state.verify_workdir, merged_tree, state).await {
        steps.push(VerifyStepResult {
            target: "-".to_string(),
            command: "sync verify workdir".to_string(),
            passed: false,
            exit_code: -1,
            duration_ms: 0,
            output_tail: format!("failed to materialize snapshot: {e}"),
        });
        return VerifyReport {
            skipped: false,
            skip_reason: None,
            passed: false,
            affected_targets: affected_names,
            steps,
        };
    }

    let mut all_passed = true;
    'targets: for target in &affected {
        for cmd in &target.verify_commands {
            let started = Instant::now();
            let step =
                match exec_shell_command(&state.verify_workdir, &target.name, cmd, started).await {
                    Ok(step) => step,
                    Err(e) => VerifyStepResult {
                        target: target.name.clone(),
                        command: cmd.clone(),
                        passed: false,
                        exit_code: -1,
                        duration_ms: started.elapsed().as_millis() as u64,
                        output_tail: format!("could not run command: {e}"),
                    },
                };
            let ok = step.passed;
            steps.push(step);
            if !ok {
                all_passed = false;
                break 'targets;
            }
        }
    }

    VerifyReport {
        skipped: false,
        skip_reason: None,
        passed: all_passed,
        affected_targets: affected_names,
        steps,
    }
}

const SYNC_MANIFEST: &str = ".dockside-synced.json";

/// Make the warm workdir equal to `tree` for every dockside-managed file: write changed files, and delete
/// files that a previous sync wrote but are no longer in the revision. Build output
/// (`target/`, `node_modules/`) is untouched because only previously synced paths are removed.
async fn sync_warm_verify_dir(
    workdir: &Path,
    tree: &BTreeMap<String, FileEntry>,
    state: &AppState,
) -> ServerResult<()> {
    std::fs::create_dir_all(workdir)?;
    let manifest_path = workdir.join(SYNC_MANIFEST);
    let previous: BTreeSet<String> = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    for stale in previous.iter().filter(|p| !tree.contains_key(p.as_str())) {
        let dst = workdir.join(stale);
        if dst.is_file() {
            std::fs::remove_file(&dst)?;
        }
    }

    for (rel_path, entry) in tree {
        let dst = workdir.join(rel_path);
        // Skip rewriting if existing file matches hash (keeps cargo/tsc mtimes warm!).
        if dst.is_file()
            && let Ok(existing) = std::fs::read(&dst)
            && dockside_core::hash::hash_bytes(&existing) == entry.hash
        {
            continue;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = state.cas.get_bytes(&entry.hash).await?;
        std::fs::write(&dst, bytes)?;
    }

    let current: BTreeSet<&String> = tree.keys().collect();
    std::fs::write(&manifest_path, serde_json::to_vec(&current)?)?;
    Ok(())
}

async fn exec_shell_command(
    workdir: &Path,
    target_name: &str,
    command: &str,
    started: Instant,
) -> ServerResult<VerifyStepResult> {
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
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let exit_code = output.status.code().unwrap_or(-1);
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        combined.push_str("\n--- stderr ---\n");
        combined.push_str(&stderr);
    }
    let tail = if combined.len() > 4000 {
        combined[combined.len() - 4000..].to_string()
    } else {
        combined
    };
    Ok(VerifyStepResult {
        target: target_name.to_string(),
        command: command.to_string(),
        passed: output.status.success(),
        exit_code,
        duration_ms: elapsed_ms,
        output_tail: tail,
    })
}
