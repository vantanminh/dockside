use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::{DocksideConfig, TargetConfig};
use crate::error::{CoreError, CoreResult};

#[derive(Debug, Clone)]
pub struct PolicyEngine {
    sensitive: GlobSet,
    allow_unverified_sensitive: bool,
    blocked_secrets: GlobSet,
    allowed_templates: GlobSet,
    sorted_merge: GlobSet,
    targets: Vec<(TargetConfig, GlobSet)>,
}

fn compile_globs(patterns: &[String]) -> CoreResult<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob =
            Glob::new(pat).map_err(|e| CoreError::Config(format!("invalid glob '{pat}': {e}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|e| CoreError::Config(format!("failed to compile globset: {e}")))
}

impl PolicyEngine {
    pub fn from_config(cfg: &DocksideConfig) -> CoreResult<Self> {
        let sensitive = compile_globs(&cfg.policy.sensitive_globs)?;
        let blocked_secrets = compile_globs(&cfg.policy.blocked_secret_globs)?;
        let allowed_templates = compile_globs(&cfg.policy.allowed_secret_templates)?;
        let sorted_merge = compile_globs(&cfg.hotfiles.sorted_merge_globs)?;

        let mut targets = Vec::with_capacity(cfg.targets.len());
        for t in &cfg.targets {
            let gs = compile_globs(&t.watch_globs)?;
            targets.push((t.clone(), gs));
        }

        Ok(Self {
            sensitive,
            allow_unverified_sensitive: cfg.policy.allow_unverified_sensitive,
            blocked_secrets,
            allowed_templates,
            sorted_merge,
            targets,
        })
    }

    /// Returns true if a file path is blocked as a secret (e.g. `.env`, `.dev.vars`, `*.pem`).
    pub fn is_secret_blocked(&self, path: &str) -> bool {
        if self.allowed_templates.is_match(path) {
            return false;
        }
        self.blocked_secrets.is_match(path)
    }

    /// Validate that none of the provided paths violate secret blocking rules.
    pub fn ensure_no_secrets<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> CoreResult<()> {
        for p in paths {
            if self.is_secret_blocked(p) {
                return Err(CoreError::SecretBlocked {
                    path: p.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Whether a sensitive changeset may land without an executed verify gate.
    pub fn allow_unverified_sensitive(&self) -> bool {
        self.allow_unverified_sensitive
    }

    /// Return sorted list of touched paths that are sensitive (migrations, auth, infra, contracts...).
    /// These never wait for a human; they get stricter automated checks instead.
    pub fn sensitive_paths(&self, paths: &[String]) -> Vec<String> {
        let mut triggers: Vec<String> = paths
            .iter()
            .filter(|p| self.sensitive.is_match(p.as_str()))
            .cloned()
            .collect();
        triggers.sort();
        triggers.dedup();
        triggers
    }

    /// Determine which deployment/verify targets are impacted by a set of touched paths.
    pub fn affected_targets(&self, paths: &[String]) -> Vec<TargetConfig> {
        let mut out = Vec::new();
        for (target, gs) in &self.targets {
            if paths.iter().any(|p| gs.is_match(p.as_str())) {
                out.push(target.clone());
            }
        }
        out
    }

    /// Returns true if this file allows safe sorted-line auto-merging for independent additions.
    pub fn is_sorted_merge_candidate(&self, path: &str) -> bool {
        self.sorted_merge.is_match(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_blocking_allows_templates_and_blocks_real_env() -> CoreResult<()> {
        let engine = PolicyEngine::from_config(&DocksideConfig::default_pilot())?;
        assert!(engine.is_secret_blocked("services/api/.env"));
        assert!(engine.is_secret_blocked("apps/web/.dev.vars"));
        assert!(engine.is_secret_blocked("services/api/prod.pem"));
        assert!(!engine.is_secret_blocked("services/api/.env.example"));
        assert!(!engine.is_secret_blocked("apps/web/.dev.vars.example"));
        assert!(!engine.is_secret_blocked("apps/web/app/root.tsx"));
        Ok(())
    }

    #[test]
    fn sensitive_paths_are_flagged_normal_code_is_not() -> CoreResult<()> {
        let engine = PolicyEngine::from_config(&DocksideConfig::default_pilot())?;
        let normal = vec![
            "apps/web/app/routes/home.tsx".to_string(),
            "services/api/rust/crates/myapp-api/src/modules/notes/mod.rs".to_string(),
        ];
        assert!(engine.sensitive_paths(&normal).is_empty());

        let sensitive = vec![
            "services/api/src/db/migrations/pending_add_tags.sql".to_string(),
            "services/api/rust/crates/myapp-api/src/modules/auth/login.rs".to_string(),
            "apps/web/app/routes/home.tsx".to_string(),
        ];
        let triggers = engine.sensitive_paths(&sensitive);
        assert_eq!(triggers.len(), 2);
        Ok(())
    }

    #[test]
    fn contracts_change_impacts_api_web_and_dashboard() -> CoreResult<()> {
        let engine = PolicyEngine::from_config(&DocksideConfig::default_pilot())?;
        let touched = vec!["contracts/api-v2/paths/surveys.yaml".to_string()];
        let affected: Vec<String> = engine
            .affected_targets(&touched)
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(affected, vec!["api", "web", "dashboard"]);
        Ok(())
    }
}
