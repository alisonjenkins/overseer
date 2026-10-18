use globset::GlobSetBuilder;
use overseer_core::EscalationConfig;

/// Return all paths from `changed_paths` that match any pattern in `rules`.
pub fn escalation_hits(changed_paths: &[String], rules: &EscalationConfig) -> Vec<String> {
    if !rules.enabled || rules.paths.is_empty() || changed_paths.is_empty() {
        return Vec::new();
    }

    let mut builder = GlobSetBuilder::new();
    for pattern in &rules.paths {
        if let Ok(glob) = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
        {
            builder.add(glob);
        }
    }
    let globset = match builder.build() {
        Ok(set) => set,
        Err(_) => return Vec::new(),
    };

    let mut hits = Vec::new();
    for path in changed_paths {
        if globset.is_match(path) {
            hits.push(path.clone());
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn test_nested_paths() {
        let rules = EscalationConfig::default();
        let changed = to_strings(&[
            "Cargo.toml",
            "crates/overseer-core/Cargo.toml",
            "auth/login.rs",
            "src/auth/service.rs",
            "migrations/0001_init.sql",
            "db/migrations/0001_init.sql",
            "infra/aws/main.tf",
            "Dockerfile",
            "services/worker/Dockerfile.prod",
            "package.json",
            "packages/frontend/package.json",
            "secrets/token",
            "internal/secrets/keys.txt",
            "deploy/k8s.yaml",
        ]);
        let hits = escalation_hits(&changed, &rules);
        assert_eq!(hits, changed);
    }

    #[test]
    fn test_dotfiles() {
        let rules = EscalationConfig::default();
        let changed = to_strings(&[
            ".github/workflows/ci.yml",
            ".gitlab-ci.yml",
            ".env",
            ".env.production",
        ]);
        let hits = escalation_hits(&changed, &rules);
        assert_eq!(hits, changed);
    }

    #[test]
    fn test_non_matches() {
        let rules = EscalationConfig::default();
        let changed = to_strings(&[
            "src/main.rs",
            "README.md",
            "docs/spec.md",
            "tests/common/mod.rs",
        ]);
        let hits = escalation_hits(&changed, &rules);
        assert!(hits.is_empty(), "expected no hits, got {hits:?}");
    }

    #[test]
    fn test_disabled() {
        let rules = EscalationConfig {
            enabled: false,
            ..Default::default()
        };
        let changed = to_strings(&[
            "Cargo.toml",
            ".github/workflows/ci.yml",
            "src/auth/login.rs",
        ]);
        let hits = escalation_hits(&changed, &rules);
        assert!(
            hits.is_empty(),
            "expected empty when disabled, got {hits:?}"
        );
    }

    #[test]
    fn test_empty() {
        let rules = EscalationConfig::default();
        assert!(escalation_hits(&[], &rules).is_empty());

        let empty_rules = EscalationConfig {
            enabled: true,
            paths: Vec::new(),
        };
        let changed = to_strings(&["Cargo.toml", "flake.nix"]);
        assert!(escalation_hits(&changed, &empty_rules).is_empty());
    }
}
