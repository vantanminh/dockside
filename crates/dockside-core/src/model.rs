use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub hash: String,
    pub size: u64,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ChangesetOp {
    Put {
        path: String,
        hash: String,
        size: u64,
        #[serde(default)]
        executable: bool,
    },
    Delete {
        path: String,
    },
}

impl ChangesetOp {
    pub fn path(&self) -> &str {
        match self {
            ChangesetOp::Put { path, .. } | ChangesetOp::Delete { path } => path,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangesetStatus {
    Open,
    Queued,
    Merging,
    Verifying,
    Landed,
    Conflict,
    Failed,
    Abandoned,
}

impl ChangesetStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Queued => "queued",
            Self::Merging => "merging",
            Self::Verifying => "verifying",
            Self::Landed => "landed",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "queued" => Some(Self::Queued),
            "merging" => Some(Self::Merging),
            "verifying" => Some(Self::Verifying),
            // Legacy states from the former human-approval design: re-run automatically / treat as abandoned.
            "awaiting_approval" => Some(Self::Queued),
            "rejected" => Some(Self::Abandoned),
            "landed" => Some(Self::Landed),
            "conflict" => Some(Self::Conflict),
            "failed" => Some(Self::Failed),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionRecord {
    pub rev_id: i64,
    pub parent_rev: Option<i64>,
    pub tree_hash: String,
    pub cs_id: Option<String>,
    pub author: String,
    pub story_ids: Vec<String>,
    pub summary: String,
    pub touched_paths: Vec<String>,
    pub verify_report: Option<VerifyReport>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesetRecord {
    pub cs_id: String,
    pub base_rev: i64,
    pub stack_parent_cs_id: Option<String>,
    pub author: String,
    pub title: String,
    pub story_ids: Vec<String>,
    pub scope_prefixes: Vec<String>,
    pub status: ChangesetStatus,
    /// Touched paths matching `policy.sensitive_globs` (migrations, auth, infra, contracts...).
    /// Informational + stricter automated checks; never a human gate.
    #[serde(alias = "approval_triggers")]
    pub sensitive_paths: Vec<String>,
    pub landed_rev: Option<i64>,
    pub error_message: Option<String>,
    pub ops: Vec<ChangesetOp>,
    pub conflicts: Vec<ConflictFile>,
    pub verify_report: Option<VerifyReport>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub snap_id: i64,
    pub cs_id: String,
    pub message: String,
    pub ops: Vec<ChangesetOp>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    Soft,
    Exclusive,
}

impl LeaseMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Exclusive => "exclusive",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "soft" => Some(Self::Soft),
            "exclusive" => Some(Self::Exclusive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub path_prefix: String,
    pub cs_id: String,
    pub author: String,
    pub mode: LeaseMode,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    Content,
    ModifyDelete,
    Binary,
    MigrationRule,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictFile {
    pub path: String,
    pub kind: ConflictKind,
    pub base_hash: Option<String>,
    pub ours_hash: Option<String>,
    pub theirs_hash: Option<String>,
    pub base_text: Option<String>,
    pub ours_text: Option<String>,
    pub theirs_text: Option<String>,
    pub merged_markers: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyStepResult {
    pub target: String,
    pub command: String,
    pub passed: bool,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub output_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyReport {
    /// True when no verify command was executed (verify disabled, or no affected target).
    /// A skipped report is never a pass: `passed` is only true when commands ran and all succeeded.
    pub skipped: bool,
    #[serde(default)]
    pub skip_reason: Option<String>,
    pub passed: bool,
    pub affected_targets: Vec<String>,
    pub steps: Vec<VerifyStepResult>,
}

impl VerifyReport {
    /// A real failure: commands ran and at least one failed.
    pub fn failed(&self) -> bool {
        !self.skipped && !self.passed
    }

    /// Commands actually ran and all passed.
    pub fn executed_and_passed(&self) -> bool {
        !self.skipped && self.passed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentRecord {
    pub id: i64,
    pub target: String,
    pub env: String,
    pub rev_id: i64,
    pub artifact_ref: String,
    pub status: String,
    pub deployed_by: String,
    pub notes: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleasePlan {
    pub target: String,
    pub env: String,
    pub rev_id: i64,
    pub kind: String,
    pub export_dir: String,
    pub artifact_tag: String,
    pub commands: Vec<String>,
    pub ready_url: Option<String>,
    pub executed: bool,
    pub command_outputs: Vec<String>,
    /// Post-deploy runtime check (only when executed): `Some(true)` verified, `Some(false)` failed.
    #[serde(default)]
    pub health_verified: Option<bool>,
    #[serde(default)]
    pub health_detail: Option<String>,
    /// Final deployment status recorded (`deployed`, `deploy_unhealthy`, `failed`, ...).
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationRename {
    pub from: String,
    pub to: String,
}

// ---- API Requests / Responses ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenChangesetReq {
    pub author: String,
    pub title: String,
    #[serde(default)]
    pub story_ids: Vec<String>,
    #[serde(default)]
    pub scope_prefixes: Vec<String>,
    #[serde(default)]
    pub stack_parent_cs_id: Option<String>,
    #[serde(default = "default_lease_mode")]
    pub lease_mode: LeaseMode,
    #[serde(default = "default_lease_ttl_secs")]
    pub lease_ttl_secs: i64,
}

fn default_lease_mode() -> LeaseMode {
    LeaseMode::Soft
}

fn default_lease_ttl_secs() -> i64 {
    14_400
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenChangesetResp {
    pub changeset: ChangesetRecord,
    pub lease_warnings: Vec<LeaseRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotReq {
    pub message: String,
    pub ops: Vec<ChangesetOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitChangesetReq {
    pub ops: Vec<ChangesetOp>,
    #[serde(default)]
    pub base_rev: Option<i64>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRevisionReq {
    pub author: String,
    pub summary: String,
    pub ops: Vec<ChangesetOp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseReq {
    pub target: String,
    pub env: String,
    pub rev_id: i64,
    pub actor: String,
    #[serde(default)]
    pub execute: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackReq {
    pub target: String,
    pub env: String,
    pub to_rev_id: i64,
    pub actor: String,
    #[serde(default)]
    pub execute: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceNextResp {
    pub kind: String,
    pub value: i64,
    pub formatted: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeFlatResp {
    pub rev_id: i64,
    pub tree_hash: String,
    pub files: std::collections::BTreeMap<String, FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobsExistReq {
    pub hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobsExistResp {
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResp {
    pub status: String,
    #[serde(default)]
    pub version: String,
    pub repo_name: String,
    pub main_rev: i64,
    pub r2_enabled: bool,
    pub git_mirror_enabled: bool,
    pub verify_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorBody {
    pub error: String,
    pub code: String,
}
