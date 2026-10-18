mod speckit;

pub use speckit::SpecKitSource;

use overseer_core::{Config, TaskSource};

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceBuildError {
    #[error("unknown source kind: {kind}")]
    UnknownKind { kind: String },
}

pub fn build_sources(config: &Config) -> Result<Vec<Box<dyn TaskSource>>, SourceBuildError> {
    let mut sources: Vec<Box<dyn TaskSource>> = Vec::new();
    for entry in &config.sources {
        match entry.kind.as_str() {
            "speckit" => sources.push(Box::new(SpecKitSource::new(entry))),
            _ => {
                return Err(SourceBuildError::UnknownKind {
                    kind: entry.kind.clone(),
                });
            }
        }
    }
    Ok(sources)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]
    use super::*;
    use overseer_core::SourceConfig;
    use std::path::PathBuf;

    fn sample_config(sources: Vec<SourceConfig>) -> Config {
        Config {
            repo: PathBuf::from("/repo"),
            default_profile: "local".to_owned(),
            review: Default::default(),
            escalation: Default::default(),
            landing: Default::default(),
            profiles: Vec::new(),
            learning: Default::default(),
            sources,
            gpu: Default::default(),
        }
    }

    #[test]
    fn build_sources_returns_speckit_source_and_errors_on_unknown_kind() {
        let config = sample_config(vec![SourceConfig {
            kind: "speckit".to_owned(),
            path: PathBuf::from("specs"),
            profile: None,
            ready: false,
        }]);

        let sources = match build_sources(&config) {
            Ok(sources) => sources,
            Err(err) => panic!("build_sources should succeed for a speckit entry: {err}"),
        };
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name(), "speckit");

        let bad_config = sample_config(vec![SourceConfig {
            kind: "unknown".to_owned(),
            path: PathBuf::from("other"),
            profile: None,
            ready: false,
        }]);

        assert!(matches!(
            build_sources(&bad_config),
            Err(SourceBuildError::UnknownKind { .. })
        ));
    }
}
