use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CoreError, Permissions};

const DEFAULT_TIMEOUT_SECS: u64 = 1500;

/// Which backend implementation a profile selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    Openai,
    Command,
}

/// A named model configuration, read from `overseer.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub backend: BackendKind,
    /// Model name sent to an `openai` server.
    #[serde(default)]
    pub model: Option<String>,
    /// Base URL of an `openai` server, e.g. `http://127.0.0.1:8080/v1`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the environment variable holding the API key. The key itself
    /// is never stored in the config file.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// argv for a `command` backend. The prompt is appended as the last
    /// argument unless `prompt_stdin` is set.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub prompt_stdin: bool,
    #[serde(default)]
    pub role_prompt: Option<String>,
    /// Profile names to try, in order, when this one fails with a routable
    /// failure. Used by the orchestrator from M3 on.
    #[serde(default)]
    pub fallback: Vec<String>,
    /// The most this profile may be granted, not what it is always granted.
    #[serde(default = "default_permissions")]
    pub permissions: Permissions,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_permissions() -> Permissions {
    Permissions::Read
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// The whole `overseer.toml` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repo: PathBuf,
    pub default_profile: String,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

impl Config {
    /// Parse and validate a configuration document.
    pub fn from_toml_str(text: &str) -> Result<Self, CoreError> {
        let config: Self = toml::from_str(text).map_err(|err| CoreError::ConfigParse {
            message: err.to_string(),
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    /// Check names, per-backend required fields, fallback references and
    /// fallback cycles.
    pub fn validate(&self) -> Result<(), CoreError> {
        let mut by_name: HashMap<&str, &Profile> = HashMap::new();
        for profile in &self.profiles {
            if by_name.insert(profile.name.as_str(), profile).is_some() {
                return Err(CoreError::DuplicateProfile {
                    name: profile.name.clone(),
                });
            }
            check_required_fields(profile)?;
        }

        if !by_name.contains_key(self.default_profile.as_str()) {
            return Err(CoreError::UnknownDefaultProfile {
                name: self.default_profile.clone(),
            });
        }

        for profile in &self.profiles {
            for fallback in &profile.fallback {
                if !by_name.contains_key(fallback.as_str()) {
                    return Err(CoreError::UnknownFallback {
                        profile: profile.name.clone(),
                        fallback: fallback.clone(),
                    });
                }
            }
        }

        let mut state: HashMap<&str, Visit> = HashMap::new();
        for profile in &self.profiles {
            check_fallback_cycles(profile.name.as_str(), &by_name, &mut state)?;
        }
        Ok(())
    }
}

fn check_required_fields(profile: &Profile) -> Result<(), CoreError> {
    let missing = |field: &'static str| CoreError::MissingProfileField {
        profile: profile.name.clone(),
        field,
    };
    match profile.backend {
        BackendKind::Openai => {
            if profile.base_url.is_none() {
                return Err(missing("base_url"));
            }
            if profile.model.is_none() {
                return Err(missing("model"));
            }
        }
        BackendKind::Command => match &profile.command {
            Some(argv) if !argv.is_empty() => {}
            _ => return Err(missing("command")),
        },
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    InProgress,
    Done,
}

fn check_fallback_cycles<'a>(
    name: &'a str,
    by_name: &HashMap<&'a str, &'a Profile>,
    state: &mut HashMap<&'a str, Visit>,
) -> Result<(), CoreError> {
    match state.get(name) {
        Some(Visit::Done) => return Ok(()),
        Some(Visit::InProgress) => {
            return Err(CoreError::FallbackCycle {
                profile: name.to_owned(),
            });
        }
        None => {}
    }
    state.insert(name, Visit::InProgress);
    if let Some(profile) = by_name.get(name) {
        for fallback in &profile.fallback {
            check_fallback_cycles(fallback.as_str(), by_name, state)?;
        }
    }
    state.insert(name, Visit::Done);
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const MINIMAL: &str = r#"
repo = "/work/project"
default_profile = "local"

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
"#;

    fn parse_err(text: &str) -> CoreError {
        Config::from_toml_str(text).unwrap_err()
    }

    #[test]
    fn minimal_openai_profile_gets_defaults() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        let profile = config.profile("local").unwrap();
        assert_eq!(profile.backend, BackendKind::Openai);
        assert_eq!(profile.permissions, Permissions::Read);
        assert_eq!(profile.timeout_secs, 1500);
        assert!(profile.fallback.is_empty());
        assert!(!profile.prompt_stdin);
        assert_eq!(profile.api_key_env, None);
        assert_eq!(config.repo, PathBuf::from("/work/project"));
    }

    #[test]
    fn command_profile_parses_argv_and_stdin_flag() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = ["opencode", "run"]
prompt_stdin = true
permissions = "write-workdir"
timeout_secs = 60
"#;
        let config = Config::from_toml_str(text).unwrap();
        let profile = config.profile("agent").unwrap();
        assert_eq!(profile.backend, BackendKind::Command);
        assert_eq!(
            profile.command.as_deref(),
            Some(&["opencode".to_owned(), "run".to_owned()][..])
        );
        assert!(profile.prompt_stdin);
        assert_eq!(profile.permissions, Permissions::WriteWorkdir);
        assert_eq!(profile.timeout_secs, 60);
    }

    #[test]
    fn fallback_chain_to_existing_profiles_is_accepted() {
        let text = format!(
            "{MINIMAL}\n[[profiles]]\nname = \"agy\"\nbackend = \"command\"\ncommand = [\"agy\", \"-p\"]\nfallback = [\"local\"]\n"
        );
        let config = Config::from_toml_str(&text).unwrap();
        assert_eq!(config.profile("agy").unwrap().fallback, vec!["local"]);
    }

    #[test]
    fn unknown_default_profile_is_rejected() {
        let text = MINIMAL.replace("default_profile = \"local\"", "default_profile = \"nope\"");
        assert_eq!(
            parse_err(&text),
            CoreError::UnknownDefaultProfile {
                name: "nope".to_owned()
            }
        );
    }

    #[test]
    fn unknown_fallback_is_rejected() {
        let text = MINIMAL.replace(
            "model = \"local\"",
            "model = \"local\"\nfallback = [\"ghost\"]",
        );
        assert_eq!(
            parse_err(&text),
            CoreError::UnknownFallback {
                profile: "local".to_owned(),
                fallback: "ghost".to_owned()
            }
        );
    }

    #[test]
    fn fallback_cycle_is_rejected() {
        let text = r#"
repo = "/work/project"
default_profile = "a"

[[profiles]]
name = "a"
backend = "command"
command = ["true"]
fallback = ["b"]

[[profiles]]
name = "b"
backend = "command"
command = ["true"]
fallback = ["a"]
"#;
        assert!(matches!(parse_err(text), CoreError::FallbackCycle { .. }));
    }

    #[test]
    fn self_fallback_is_a_cycle() {
        let text = MINIMAL.replace(
            "model = \"local\"",
            "model = \"local\"\nfallback = [\"local\"]",
        );
        assert_eq!(
            parse_err(&text),
            CoreError::FallbackCycle {
                profile: "local".to_owned()
            }
        );
    }

    #[test]
    fn duplicate_profile_names_are_rejected() {
        let text = format!(
            "{MINIMAL}\n[[profiles]]\nname = \"local\"\nbackend = \"command\"\ncommand = [\"true\"]\n"
        );
        assert_eq!(
            parse_err(&text),
            CoreError::DuplicateProfile {
                name: "local".to_owned()
            }
        );
    }

    #[test]
    fn openai_profile_requires_base_url_and_model() {
        let text = MINIMAL.replace("base_url = \"http://127.0.0.1:8080/v1\"\n", "");
        assert_eq!(
            parse_err(&text),
            CoreError::MissingProfileField {
                profile: "local".to_owned(),
                field: "base_url"
            }
        );
    }

    #[test]
    fn command_profile_requires_non_empty_argv() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = []
"#;
        assert_eq!(
            parse_err(text),
            CoreError::MissingProfileField {
                profile: "agent".to_owned(),
                field: "command"
            }
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = MINIMAL.replace("model = \"local\"", "model = \"local\"\ntemprature = 0.2");
        assert!(matches!(parse_err(&text), CoreError::ConfigParse { .. }));
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        let text = toml::to_string(&config).unwrap();
        assert_eq!(Config::from_toml_str(&text).unwrap(), config);
    }
}
