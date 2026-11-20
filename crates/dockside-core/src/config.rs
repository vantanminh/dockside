use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocksideConfig {
    #[serde(default)]
    pub repo: RepoConfig,
    #[serde(default)]
    pub policy: PolicyConfig,
    #[serde(default)]
    pub hotfiles: HotfilesConfig,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(default = "default_repo_name")]
    pub name: String,
    #[serde(default = "default_branch_name")]
    pub default_branch: String,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            name: default_repo_name(),
            default_branch: default_branch_name(),
        }
    }
}

fn default_repo_name() -> String {
    "monorepo".to_string()
}

fn default_branch_name() -> String {
    "main".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    /// Paths whose changes get stricter *automated* checks (never a human approval gate).
    #[serde(default = "default_sensitive_globs", alias = "require_approval_globs")]
    pub sensitive_globs: Vec<String>,
    /// If false (default), a sensitive changeset whose verify gate could not actually run
    /// (verify disabled while an affected target defines verify commands) is failed, not landed.
    #[serde(default)]
    pub allow_unverified_sensitive: bool,
    #[serde(default = "default_blocked_secret_globs")]
    pub blocked_secret_globs: Vec<String>,
    #[serde(default = "default_allowed_secret_templates")]
    pub allowed_secret_templates: Vec<String>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            sensitive_globs: default_sensitive_globs(),
            allow_unverified_sensitive: false,
            blocked_secret_globs: default_blocked_secret_globs(),
            allowed_secret_templates: default_allowed_secret_templates(),
        }
    }
}

fn default_sensitive_globs() -> Vec<String> {
    vec![
        "**/migrations/**".to_string(),
        "infra/**".to_string(),
        "**/auth/**".to_string(),
        "**/authz/**".to_string(),
        "contracts/**".to_string(),
        "**/spec/openapi*.yaml".to_string(),
        "dockside.toml".to_string(),
    ]
}

fn default_blocked_secret_globs() -> Vec<String> {
    vec![
        "**/.env".to_string(),
        "**/.env.*".to_string(),
        "**/.dev.vars".to_string(),
        "**/*.pem".to_string(),
        "**/*.key".to_string(),
        "**/id_rsa*".to_string(),
    ]
}

fn default_allowed_secret_templates() -> Vec<String> {
    vec![
        "**/.env.example".to_string(),
        "**/.env.*.example".to_string(),
        "**/.dev.vars.example".to_string(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotfilesConfig {
    #[serde(default = "default_migration_dirs")]
    pub migration_dirs: Vec<String>,
    #[serde(default = "default_sorted_merge_globs")]
    pub sorted_merge_globs: Vec<String>,
}

impl Default for HotfilesConfig {
    fn default() -> Self {
        Self {
            migration_dirs: default_migration_dirs(),
            sorted_merge_globs: default_sorted_merge_globs(),
        }
    }
}

fn default_migration_dirs() -> Vec<String> {
    vec!["services/api/src/db/migrations".to_string()]
}

fn default_sorted_merge_globs() -> Vec<String> {
    vec![
        "services/api/rust/crates/myapp-api/src/routes/mod.rs".to_string(),
        "apps/web/app/routes.ts".to_string(),
        "apps/dashboard/app/routes.ts".to_string(),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    RudderImage,
    CloudflareWorker,
    ExpoEas,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfig {
    pub name: String,
    pub path: String,
    pub kind: TargetKind,
    #[serde(default)]
    pub rudder_service: Option<String>,
    #[serde(default)]
    pub dockerfile: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub worker_name: Option<String>,
    #[serde(default)]
    pub watch_globs: Vec<String>,
    #[serde(default)]
    pub verify_commands: Vec<String>,
    #[serde(default)]
    pub pre_deploy_commands: Vec<String>,
    #[serde(default)]
    pub deploy_command: Option<String>,
    #[serde(default)]
    pub ready_url: Option<String>,
    /// Text the `ready_url` body must contain after deploy. `{rev}` expands to `rN`.
    /// Defaults: rudder-image => `r{rev}` (GIT_SHA build arg); others => HTTP 2xx only.
    #[serde(default)]
    pub ready_marker: Option<String>,
}

impl DocksideConfig {
    pub fn default_pilot() -> Self {
        Self {
            repo: RepoConfig::default(),
            policy: PolicyConfig::default(),
            hotfiles: HotfilesConfig::default(),
            targets: vec![
                TargetConfig {
                    name: "api".to_string(),
                    path: "services/api".to_string(),
                    kind: TargetKind::RudderImage,
                    rudder_service: Some("myproject/prod/api".to_string()),
                    dockerfile: Some("services/api/rust/Dockerfile".to_string()),
                    context: Some("services/api".to_string()),
                    worker_name: None,
                    watch_globs: vec![
                        "services/api/**".to_string(),
                        "contracts/api-v1/**".to_string(),
                        "contracts/api-v2/**".to_string(),
                    ],
                    verify_commands: vec![
                        "cargo fmt --manifest-path services/api/rust/Cargo.toml --all -- --check"
                            .to_string(),
                        "cargo clippy --manifest-path services/api/rust/Cargo.toml --workspace --all-targets --locked -- -D warnings"
                            .to_string(),
                        "cargo test --manifest-path services/api/rust/Cargo.toml --workspace --locked"
                            .to_string(),
                    ],
                    pre_deploy_commands: vec![],
                    deploy_command: None,
                    ready_url: Some("https://api.example.com/v1/ready".to_string()),
                    ready_marker: None,
                },
                TargetConfig {
                    name: "web".to_string(),
                    path: "apps/web".to_string(),
                    kind: TargetKind::CloudflareWorker,
                    rudder_service: None,
                    dockerfile: None,
                    context: None,
                    worker_name: Some("myapp-web".to_string()),
                    watch_globs: vec![
                        "apps/web/**".to_string(),
                        "contracts/api-v1/**".to_string(),
                        "contracts/api-v2/**".to_string(),
                    ],
                    verify_commands: vec![
                        "pnpm --dir apps/web install --frozen-lockfile".to_string(),
                        "pnpm --dir apps/web api:check".to_string(),
                        "pnpm --dir apps/web typecheck".to_string(),
                        "pnpm --dir apps/web test".to_string(),
                        "pnpm --dir apps/web format:check".to_string(),
                        "pnpm --dir apps/web build".to_string(),
                    ],
                    pre_deploy_commands: vec!["pnpm --dir apps/web api:check-domain".to_string()],
                    deploy_command: Some("pnpm --dir apps/web deploy".to_string()),
                    ready_url: Some("https://example.com".to_string()),
                    ready_marker: None,
                },
                TargetConfig {
                    name: "dashboard".to_string(),
                    path: "apps/dashboard".to_string(),
                    kind: TargetKind::CloudflareWorker,
                    rudder_service: None,
                    dockerfile: None,
                    context: None,
                    worker_name: Some("myapp-dashboard".to_string()),
                    watch_globs: vec![
                        "apps/dashboard/**".to_string(),
                        "contracts/api-v2/**".to_string(),
                        "contracts/cdn-admin/**".to_string(),
                    ],
                    verify_commands: vec![
                        "pnpm --dir apps/dashboard install --frozen-lockfile".to_string(),
                        "pnpm --dir apps/dashboard typecheck".to_string(),
                        "pnpm --dir apps/dashboard test".to_string(),
                        "pnpm --dir apps/dashboard format:check".to_string(),
                        "pnpm --dir apps/dashboard build".to_string(),
                    ],
                    pre_deploy_commands: vec![],
                    deploy_command: Some("pnpm --dir apps/dashboard deploy".to_string()),
                    ready_url: Some("https://dashboard.example.com".to_string()),
                    ready_marker: None,
                },
            ],
        }
    }

    pub fn from_toml_str(src: &str) -> CoreResult<Self> {
        toml::from_str(src).map_err(|e| CoreError::Config(e.to_string()))
    }

    pub fn from_file(path: &Path) -> CoreResult<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::from_toml_str(&content)
    }

    pub fn target_by_name(&self, name: &str) -> Option<&TargetConfig> {
        self.targets.iter().find(|t| t.name == name)
    }
}
