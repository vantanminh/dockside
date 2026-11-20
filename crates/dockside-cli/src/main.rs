mod client;
mod workspace;

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use dockside_core::config::DocksideConfig;
use dockside_core::model::{
    ChangesetStatus, ImportRevisionReq, LeaseMode, OpenChangesetReq, ReleaseReq, RollbackReq,
    SnapshotReq, SubmitChangesetReq,
};
use dockside_core::policy::PolicyEngine;
use dockside_core::tree::apply_ops;
use serde::Serialize;

use crate::client::{ApiClient, CliError, CliResult};
use crate::workspace::{
    WorkspaceMeta, collect_import_dir, compute_workspace_diff, load_workspace_meta,
    materialize_conflict_workspace, materialize_files, save_workspace_meta, scan_workspace,
};

#[derive(Parser)]
#[command(
    name = "dockside",
    version,
    about = "Dockside CLI — server-centric, agent-first version control with automated landing and release"
)]
struct Cli {
    /// Server base URL
    #[arg(
        long,
        env = "DOCKSIDE_URL",
        default_value = "http://127.0.0.1:7420",
        global = true
    )]
    url: String,

    /// Optional bearer token for remote/authenticated dockside-server
    #[arg(long, env = "DOCKSIDE_TOKEN", global = true)]
    token: Option<String>,

    /// Emit machine-readable JSON output (recommended for AI coding agents)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check dockside-server server health and current main revision
    Health,

    /// Import existing local repositories into monorepo prefixes (read-only on source dirs)
    Import {
        /// Mapping in the form `<monorepo_prefix>=<local_dir>` (repeatable)
        #[arg(long = "map", required = true)]
        maps: Vec<String>,
        #[arg(long, default_value = "Initial monorepo import")]
        summary: String,
        #[arg(long, default_value = "agent")]
        author: String,
    },

    /// Open a new ephemeral agent workspace and register path leases
    Open {
        #[arg(long)]
        title: String,
        /// Comma-separated scope prefixes (empty = full repo)
        #[arg(long, default_value = "")]
        scope: String,
        #[arg(long, default_value = "agent")]
        author: String,
        /// Optional story IDs (repeatable)
        #[arg(long = "story")]
        stories: Vec<String>,
        /// Stack this changeset on top of an existing open/queued parent changeset
        #[arg(long)]
        stack: Option<String>,
        /// Acquire an exclusive path lease instead of soft warning lease
        #[arg(long)]
        exclusive: bool,
        /// Directory to materialize the workspace into
        #[arg(long)]
        dir: Option<PathBuf>,
    },

    /// Show added, modified, and deleted files in the current workspace
    Status {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },

    /// Show unified diff of changes in the current workspace
    Diff {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },

    /// Create an undoable checkpoint snapshot on the server
    Snap {
        #[arg(short = 'm', long)]
        message: String,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },

    /// Restore workspace files to a previous snapshot (or back to base with --base)
    Undo {
        /// Restore to specific snapshot ID (defaults to latest snapshot)
        #[arg(long)]
        snap: Option<i64>,
        /// Reset workspace all the way back to base revision
        #[arg(long)]
        base: bool,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },

    /// Submit workspace changeset into the single-writer Merge Queue (never merges locally)
    Submit {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Return immediately without waiting for integrator result
        #[arg(long)]
        no_wait: bool,
        /// Keep workspace directory on disk even after landing
        #[arg(long)]
        keep_workspace: bool,
        /// Optional updated changeset title
        #[arg(long)]
        title: Option<String>,
    },

    /// Inspect conflicts on a changeset and optionally materialize a resolver workspace
    Conflicts {
        cs_id: String,
        /// Write conflict markers and .base/.ours/.theirs sidecars into this directory
        #[arg(long)]
        materialize: Option<PathBuf>,
    },

    /// Abandon an open/conflicted changeset and release its leases
    Abandon { cs_id: String },

    /// Show linear revision history of main
    Log {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },

    /// List changesets in the merge queue / review pipeline
    Queue {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },

    /// Show active path leases across running agents
    Leases,

    /// Allocate the next monotonic ID from server (e.g. intake -> IN-001, story -> US-001)
    Seq { kind: String },

    /// Export an immutable clean snapshot of a revision
    Export {
        #[arg(long)]
        rev: i64,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "")]
        scope: String,
    },

    /// Plan or execute a release for a target via Rudder / Cloudflare Workers / EAS
    Release {
        #[arg(long)]
        target: String,
        #[arg(long, default_value = "prod")]
        env: String,
        #[arg(long)]
        rev: i64,
        #[arg(long, default_value = "agent")]
        actor: String,
        /// Actually execute the deploy commands (defaults to dry-run plan)
        #[arg(long)]
        execute: bool,
    },

    /// Plan or execute a rollback to a previous revision
    Rollback {
        #[arg(long)]
        target: String,
        #[arg(long, default_value = "prod")]
        env: String,
        #[arg(long)]
        to_rev: i64,
        #[arg(long, default_value = "agent")]
        actor: String,
        /// Actually execute the rollback commands (defaults to dry-run plan)
        #[arg(long)]
        execute: bool,
    },

    /// Show deployment history
    Deployments {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

fn emit_json<T: Serialize>(v: &T) -> CliResult<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> CliResult<()> {
    let api = ApiClient::new(cli.url, cli.token);
    let policy = PolicyEngine::from_config(&DocksideConfig::default_pilot())?;

    match cli.command {
        Commands::Health => {
            let h = api.health().await?;
            if cli.json {
                emit_json(&h)?;
            } else {
                println!(
                    "OK | repo={} | main=r{} | R2={} | git_mirror={} | verify={}",
                    h.repo_name, h.main_rev, h.r2_enabled, h.git_mirror_enabled, h.verify_enabled
                );
            }
        }
        Commands::Import {
            maps,
            summary,
            author,
        } => {
            let mut ops = Vec::new();
            let mut blobs = BTreeMap::new();
            let mut skipped_secrets = Vec::new();
            let mut total_files = 0usize;

            for spec in maps {
                let Some((prefix, path_str)) = spec.split_once('=') else {
                    return Err(CliError::Message(format!(
                        "invalid --map '{spec}': expected <prefix>=<local_dir>"
                    )));
                };
                let n = collect_import_dir(
                    prefix,
                    &PathBuf::from(path_str),
                    &policy,
                    &mut ops,
                    &mut blobs,
                    &mut skipped_secrets,
                )?;
                total_files += n;
            }

            let uploaded = api.upload_missing_map(&blobs).await?;
            let rev = api
                .import_revision(&ImportRevisionReq {
                    author,
                    summary,
                    ops,
                })
                .await?;

            if cli.json {
                #[derive(Serialize)]
                struct ImportOut {
                    rev: dockside_core::model::RevisionRecord,
                    total_files: usize,
                    uploaded_blobs: usize,
                    skipped_secrets: Vec<String>,
                }
                emit_json(&ImportOut {
                    rev,
                    total_files,
                    uploaded_blobs: uploaded,
                    skipped_secrets,
                })?;
            } else {
                println!(
                    "Imported r{} ({} files, {} new blobs uploaded, {} secret files safely skipped)",
                    rev.rev_id,
                    total_files,
                    uploaded,
                    skipped_secrets.len()
                );
            }
        }
        Commands::Open {
            title,
            scope,
            author,
            stories,
            stack,
            exclusive,
            dir,
        } => {
            let scope_prefixes: Vec<String> = scope
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();

            let resp = api
                .open_changeset(&OpenChangesetReq {
                    author,
                    title,
                    story_ids: stories,
                    scope_prefixes: scope_prefixes.clone(),
                    stack_parent_cs_id: stack.clone(),
                    lease_mode: if exclusive {
                        LeaseMode::Exclusive
                    } else {
                        LeaseMode::Soft
                    },
                    lease_ttl_secs: 14_400,
                })
                .await?;

            let cs = &resp.changeset;
            let base_tree = api.get_tree(cs.base_rev, &scope_prefixes).await?;
            let effective_base = if let Some(ref parent_id) = stack {
                let parent = api.get_changeset(parent_id).await?;
                apply_ops(&base_tree.files, &parent.ops)?
            } else {
                base_tree.files
            };

            let ws_dir = dir.unwrap_or_else(|| {
                let root = std::env::var("DOCKSIDE_WORKSPACES_DIR")
                    .unwrap_or_else(|_| "./data/workspaces".to_string());
                PathBuf::from(root).join(&cs.cs_id)
            });

            let written = materialize_files(&api, &ws_dir, &effective_base).await?;
            save_workspace_meta(
                &ws_dir,
                &WorkspaceMeta {
                    cs_id: cs.cs_id.clone(),
                    base_rev: cs.base_rev,
                    stack_parent_cs_id: cs.stack_parent_cs_id.clone(),
                    scope_prefixes,
                    base_files: effective_base,
                },
            )?;

            if cli.json {
                #[derive(Serialize)]
                struct OpenOut<'a> {
                    workspace_dir: String,
                    files_materialized: usize,
                    changeset: &'a dockside_core::model::ChangesetRecord,
                    lease_warnings: &'a [dockside_core::model::LeaseRecord],
                }
                emit_json(&OpenOut {
                    workspace_dir: ws_dir.display().to_string(),
                    files_materialized: written,
                    changeset: cs,
                    lease_warnings: &resp.lease_warnings,
                })?;
            } else {
                println!(
                    "Opened {} (base r{}) at {} ({} files)",
                    cs.cs_id,
                    cs.base_rev,
                    ws_dir.display(),
                    written
                );
                for w in &resp.lease_warnings {
                    println!(
                        "  [lease warning] '{}' overlaps active {} by {} ({:?})",
                        w.path_prefix, w.cs_id, w.author, w.mode
                    );
                }
            }
        }
        Commands::Status { dir } => {
            let meta = load_workspace_meta(&dir)?;
            let report = scan_workspace(&dir, &meta, &policy)?;
            if cli.json {
                #[derive(Serialize)]
                struct StatusOut<'a> {
                    cs_id: &'a str,
                    base_rev: i64,
                    added: &'a [String],
                    modified: &'a [String],
                    deleted: &'a [String],
                }
                emit_json(&StatusOut {
                    cs_id: &meta.cs_id,
                    base_rev: meta.base_rev,
                    added: &report.added,
                    modified: &report.modified,
                    deleted: &report.deleted,
                })?;
            } else {
                println!("Changeset {} (base r{})", meta.cs_id, meta.base_rev);
                for p in &report.added {
                    println!("  A {p}");
                }
                for p in &report.modified {
                    println!("  M {p}");
                }
                for p in &report.deleted {
                    println!("  D {p}");
                }
                if report.ops.is_empty() {
                    println!("  (clean)");
                }
            }
        }
        Commands::Diff { dir } => {
            let meta = load_workspace_meta(&dir)?;
            let report = scan_workspace(&dir, &meta, &policy)?;
            let diff = compute_workspace_diff(&api, &meta, &report).await?;
            print!("{diff}");
        }
        Commands::Snap { message, dir } => {
            let meta = load_workspace_meta(&dir)?;
            let report = scan_workspace(&dir, &meta, &policy)?;
            let _ = api.upload_missing_map(&report.blobs_to_upload).await?;
            let snap = api
                .add_snapshot(
                    &meta.cs_id,
                    &SnapshotReq {
                        message,
                        ops: report.ops,
                    },
                )
                .await?;
            if cli.json {
                emit_json(&snap)?;
            } else {
                println!(
                    "Saved snapshot #{} for {} ({} ops): {}",
                    snap.snap_id,
                    snap.cs_id,
                    snap.ops.len(),
                    snap.message
                );
            }
        }
        Commands::Undo { snap, base, dir } => {
            let meta = load_workspace_meta(&dir)?;
            let target_files = if base {
                meta.base_files.clone()
            } else {
                let snaps = api.list_snapshots(&meta.cs_id).await?;
                let chosen = match snap {
                    Some(id) => snaps.into_iter().find(|s| s.snap_id == id),
                    None => snaps.into_iter().next(),
                }
                .ok_or_else(|| CliError::Message("no matching snapshot found".to_string()))?;
                apply_ops(&meta.base_files, &chosen.ops)?
            };

            // Remove files not in target_files (preserve metadata)
            let scan = scan_workspace(&dir, &meta, &policy)?;
            for added in &scan.added {
                if !target_files.contains_key(added) {
                    let _ = std::fs::remove_file(dir.join(added));
                }
            }
            let written = materialize_files(&api, &dir, &target_files).await?;
            if cli.json {
                emit_json(&serde_json::json!({
                    "cs_id": meta.cs_id,
                    "restored_files": written,
                    "to_base": base
                }))?;
            } else {
                println!("Restored {} ({} files written)", meta.cs_id, written);
            }
        }
        Commands::Submit {
            dir,
            no_wait,
            keep_workspace,
            title,
        } => {
            let meta = load_workspace_meta(&dir)?;
            let report = scan_workspace(&dir, &meta, &policy)?;
            let uploaded = api.upload_missing_map(&report.blobs_to_upload).await?;
            let rec = api
                .submit_changeset(
                    &meta.cs_id,
                    &SubmitChangesetReq {
                        ops: report.ops,
                        base_rev: Some(meta.base_rev),
                        title,
                    },
                    !no_wait,
                )
                .await?;

            let mut cleaned = false;
            if rec.status == ChangesetStatus::Landed && !keep_workspace {
                let cur = std::env::current_dir().ok();
                let is_cwd = cur
                    .as_ref()
                    .and_then(|c| c.canonicalize().ok())
                    .zip(dir.canonicalize().ok())
                    .is_some_and(|(a, b)| a == b);
                if !is_cwd && std::fs::remove_dir_all(&dir).is_ok() {
                    cleaned = true;
                }
            }

            if cli.json {
                #[derive(Serialize)]
                struct SubmitOut {
                    uploaded_blobs: usize,
                    workspace_cleaned: bool,
                    changeset: dockside_core::model::ChangesetRecord,
                }
                emit_json(&SubmitOut {
                    uploaded_blobs: uploaded,
                    workspace_cleaned: cleaned,
                    changeset: rec,
                })?;
            } else {
                println!(
                    "Submitted {} -> status: {} (uploaded {} blobs)",
                    rec.cs_id,
                    rec.status.as_str(),
                    uploaded
                );
                if let Some(rev) = rec.landed_rev {
                    println!("  Landed on main as r{rev}");
                }
                if !rec.sensitive_paths.is_empty() {
                    println!(
                        "  Sensitive paths (held to a stricter automated check, no human approval): {}",
                        rec.sensitive_paths.join(", ")
                    );
                }
                if let Some(report) = &rec.verify_report {
                    if report.skipped {
                        println!(
                            "  Checks: SKIPPED ({})",
                            report.skip_reason.as_deref().unwrap_or("not run")
                        );
                    } else if report.passed {
                        println!("  Checks: passed ({} steps)", report.steps.len());
                    } else {
                        println!("  Checks: FAILED");
                        for step in report.steps.iter().filter(|st| !st.passed) {
                            println!(
                                "    - [{}] {} (exit {})",
                                step.target, step.command, step.exit_code
                            );
                        }
                    }
                }
                if let Some(msg) = &rec.error_message
                    && matches!(rec.status, ChangesetStatus::Failed)
                {
                    println!("  Error: {msg}");
                }
                if !rec.conflicts.is_empty() {
                    println!("  Conflicts ({}):", rec.conflicts.len());
                    for c in &rec.conflicts {
                        println!("    - {} ({:?}): {}", c.path, c.kind, c.detail);
                    }
                    println!(
                        "  Run `dockside conflicts {} --materialize <dir>` to resolve.",
                        rec.cs_id
                    );
                }
            }
        }
        Commands::Conflicts { cs_id, materialize } => {
            let cs = api.get_changeset(&cs_id).await?;
            let mat_path = match materialize {
                Some(dir) => Some(materialize_conflict_workspace(&api, &cs, &dir).await?),
                None => None,
            };
            if cli.json {
                emit_json(&serde_json::json!({
                    "cs_id": cs.cs_id,
                    "status": cs.status,
                    "conflicts": cs.conflicts,
                    "materialized_dir": mat_path.map(|p| p.display().to_string())
                }))?;
            } else {
                println!(
                    "{} status={} conflicts={}",
                    cs.cs_id,
                    cs.status.as_str(),
                    cs.conflicts.len()
                );
                for c in &cs.conflicts {
                    println!("  - {} [{:?}]: {}", c.path, c.kind, c.detail);
                }
                if let Some(p) = mat_path {
                    println!(
                        "Materialized conflict workspace at {}. Fix markers then run `dockside submit --dir {}`.",
                        p.display(),
                        p.display()
                    );
                }
            }
        }
        Commands::Abandon { cs_id } => {
            let rec = api.abandon_changeset(&cs_id).await?;
            if cli.json {
                emit_json(&rec)?;
            } else {
                println!("Abandoned {} (leases released)", rec.cs_id);
            }
        }
        Commands::Log { limit } => {
            let revs = api.list_revisions(limit).await?;
            if cli.json {
                emit_json(&revs)?;
            } else {
                for r in revs {
                    println!(
                        "r{:<5} {:<9} {:<12} {} ({} files)",
                        r.rev_id,
                        r.cs_id.as_deref().unwrap_or("-"),
                        r.author,
                        r.summary,
                        r.touched_paths.len()
                    );
                }
            }
        }
        Commands::Queue { status, limit } => {
            let items = api.list_changesets(status.as_deref(), limit).await?;
            if cli.json {
                emit_json(&items)?;
            } else {
                for c in items {
                    println!(
                        "{:<8} {:<18} base=r{:<4} land={:<5} {} — {}",
                        c.cs_id,
                        c.status.as_str(),
                        c.base_rev,
                        c.landed_rev
                            .map(|r| format!("r{r}"))
                            .unwrap_or_else(|| "-".into()),
                        c.author,
                        c.title
                    );
                }
            }
        }
        Commands::Leases => {
            let leases = api.list_leases().await?;
            if cli.json {
                emit_json(&leases)?;
            } else if leases.is_empty() {
                println!("No active leases.");
            } else {
                for l in leases {
                    println!(
                        "{} | {} | {} | {:?} | expires {}",
                        l.path_prefix, l.cs_id, l.author, l.mode, l.expires_at
                    );
                }
            }
        }
        Commands::Seq { kind } => {
            let seq = api.next_sequence(&kind).await?;
            if cli.json {
                emit_json(&seq)?;
            } else {
                println!("{}", seq.formatted);
            }
        }
        Commands::Export { rev, out, scope } => {
            let scopes: Vec<String> = scope
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            let tree = api.get_tree(rev, &scopes).await?;
            let count = materialize_files(&api, &out, &tree.files).await?;
            if cli.json {
                emit_json(&serde_json::json!({
                    "rev_id": rev,
                    "out_dir": out.display().to_string(),
                    "files": count
                }))?;
            } else {
                println!("Exported r{rev} ({count} files) to {}", out.display());
            }
        }
        Commands::Release {
            target,
            env,
            rev,
            actor,
            execute,
        } => {
            let plan = api
                .trigger_release(&ReleaseReq {
                    target,
                    env,
                    rev_id: rev,
                    actor,
                    execute,
                })
                .await?;
            if cli.json {
                emit_json(&plan)?;
            } else {
                println!(
                    "Release {}@{} -> r{} [{}] (executed={})",
                    plan.target, plan.env, plan.rev_id, plan.kind, plan.executed
                );
                println!("  export: {}", plan.export_dir);
                println!("  tag:    {}", plan.artifact_tag);
                for cmd in &plan.commands {
                    println!("  $ {cmd}");
                }
            }
        }
        Commands::Rollback {
            target,
            env,
            to_rev,
            actor,
            execute,
        } => {
            let plan = api
                .trigger_rollback(&RollbackReq {
                    target,
                    env,
                    to_rev_id: to_rev,
                    actor,
                    execute,
                })
                .await?;
            if cli.json {
                emit_json(&plan)?;
            } else {
                println!(
                    "Rollback {}@{} -> r{} [{}] (executed={})",
                    plan.target, plan.env, plan.rev_id, plan.kind, plan.executed
                );
                for cmd in &plan.commands {
                    println!("  $ {cmd}");
                }
            }
        }
        Commands::Deployments { limit } => {
            let deps = api.list_deployments(limit).await?;
            if cli.json {
                emit_json(&deps)?;
            } else if deps.is_empty() {
                println!("No deployments recorded yet.");
            } else {
                for d in deps {
                    println!(
                        "#{} {}@{} r{} [{}] tag={} by={} at={}",
                        d.id,
                        d.target,
                        d.env,
                        d.rev_id,
                        d.status,
                        d.artifact_ref,
                        d.deployed_by,
                        d.created_at
                    );
                }
            }
        }
    }
    Ok(())
}
