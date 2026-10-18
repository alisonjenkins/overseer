use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Permissions};

const DEFAULT_TIMEOUT_SECS: u64 = 1500;

const STARTER_TEMPLATE: &str = r#"# overseer.toml: where task worktrees come from and how tasks are run.
#
# A task runs on its profile, or on default_profile when it names none. An
# attempt that fails with quota-exhausted, auth-required or unreachable moves to
# the next profile in that profile's fallback list. Any other failure blocks the
# task.
#
# To use claude first, set default_profile = "claude". It then falls back to
# gemini, then local, whenever it cannot run.

# Repository that task worktrees are created from.
repo = {repo}

# Profile for tasks that do not name one with --profile.
default_profile = "local"

# A llama.cpp-style server on this machine. An openai backend returns text only
# and cannot edit files.
[[profiles]]
name = "local"
backend = "openai"
# Model name sent to the server.
model = "local"
# Base URL of any OpenAI-compatible server.
base_url = "http://127.0.0.1:8080/v1"
# Most this profile may be granted: read, write-workdir or write-shell.
permissions = "write-workdir"
# Seconds before an attempt gives up. For openai a timeout counts as unreachable,
# so the chain moves on; for command a timeout blocks the task.
timeout_secs = 1500
# System prompt sent before the task text.
role_prompt = "You are a careful senior engineer."
# Profiles to try, in order, when this one cannot run. Empty: local ends the chain.
fallback = []

# Gemini through the agy CLI. Needs `agy` on PATH and signed in.
[[profiles]]
name = "gemini"
# command backend: runs this argv in the task worktree with the prompt appended
# as the last argument.
backend = "command"
command = ["agy", "-p", "--model", "gemini-3.8-flash-medium"]
permissions = "write-workdir"
role_prompt = "You are a careful senior engineer."
fallback = ["local"]
# Extra arguments added for each permission level a run is granted. A level that
# is not listed adds nothing.
[profiles.permission_args]
read = []
write-workdir = ["--mode", "accept-edits"]
write-shell = ["--dangerously-skip-permissions"]

# Claude Code CLI. Metered: runs spend paid credits, so credits-low mode skips
# this profile, and a quota failure here turns credits-low on.
[[profiles]]
name = "claude"
backend = "command"
command = ["claude", "-p"]
metered = true
# Ceiling for this profile. Raise it to "write-shell" to let the write-shell
# argument below take effect.
permissions = "write-workdir"
role_prompt = "You are a careful senior engineer."
fallback = ["gemini", "local"]
[profiles.permission_args]
read = ["--permission-mode", "plan"]
write-workdir = ["--permission-mode", "acceptEdits"]
write-shell = ["--dangerously-skip-permissions"]

# Untested example of opencode's run command. The test suite never runs it and
# its flags are not checked against any opencode version; check them before use.
[[profiles]]
name = "opencode-local"
backend = "command"
command = ["opencode", "run"]
permissions = "write-workdir"
role_prompt = "You are a careful senior engineer."
fallback = []
"#;

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
    /// Flag placed immediately before the prompt argument for a `command`
    /// backend, such as `"-p"`. Appended together with the prompt as the
    /// final two arguments. Mutually exclusive with `prompt_stdin`.
    #[serde(default)]
    pub prompt_flag: Option<String>,
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
    /// True when runs on this profile spend paid credits. `credits_low` mode
    /// skips metered profiles when routing.
    #[serde(default)]
    pub metered: bool,
    /// Extra argv for a `command` backend, appended before the prompt for each
    /// effective permission level. A level with no entry adds nothing.
    #[serde(default)]
    pub permission_args: BTreeMap<Permissions, Vec<String>>,
}

impl Profile {
    /// The level a run gets when it asks for `requested`: the lower of the
    /// request and this profile's ceiling.
    pub fn effective_permissions(&self, requested: Permissions) -> Permissions {
        requested.min(self.permissions)
    }
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

    /// The `overseer.toml` that `overseer init` writes, with a comment for
    /// every field. It is a template rather than a serialised `Config` so the
    /// comments survive; the `{repo}` placeholder becomes the repository path.
    pub fn starter_toml(repo: &Path) -> String {
        let repo_text = toml::Value::String(repo.to_string_lossy().into_owned()).to_string();
        STARTER_TEMPLATE.replace("{repo}", &repo_text)
    }

    /// Serialise to the `overseer.toml` format.
    pub fn to_toml_string(&self) -> Result<String, CoreError> {
        toml::to_string(self).map_err(|err| CoreError::ConfigSerialize {
            message: err.to_string(),
        })
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
    if profile.prompt_stdin && profile.prompt_flag.is_some() {
        return Err(CoreError::PromptFlagWithStdin {
            profile: profile.name.clone(),
        });
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
        assert_eq!(profile.prompt_flag, None);
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
        assert_eq!(profile.prompt_flag, None);
        assert_eq!(profile.permissions, Permissions::WriteWorkdir);
        assert_eq!(profile.timeout_secs, 60);
    }

    #[test]
    fn command_profile_parses_prompt_flag() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = ["agy", "--model", "gemini-3.8-flash-medium"]
prompt_flag = "-p"
"#;
        let config = Config::from_toml_str(text).unwrap();
        let profile = config.profile("agent").unwrap();
        assert_eq!(profile.backend, BackendKind::Command);
        assert_eq!(profile.prompt_flag.as_deref(), Some("-p"));
        assert!(!profile.prompt_stdin);
    }

    #[test]
    fn prompt_flag_together_with_prompt_stdin_is_rejected() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = ["tool"]
prompt_flag = "-p"
prompt_stdin = true
"#;
        assert_eq!(
            parse_err(text),
            CoreError::PromptFlagWithStdin {
                profile: "agent".to_owned()
            }
        );
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
    fn starter_config_is_valid_and_routes_claude_to_gemini_then_local() {
        let config =
            Config::from_toml_str(&Config::starter_toml(Path::new("/work/project"))).unwrap();
        assert_eq!(config.default_profile, "local");
        let names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["local", "gemini", "claude", "opencode-local"]);

        let local = config.profile("local").unwrap();
        assert_eq!(local.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert!(local.fallback.is_empty());
        assert!(!local.metered);

        let gemini = config.profile("gemini").unwrap();
        assert_eq!(gemini.fallback, ["local"]);
        assert_eq!(
            gemini.permission_args.get(&Permissions::Read),
            Some(&Vec::new())
        );

        let claude = config.profile("claude").unwrap();
        assert_eq!(claude.fallback, ["gemini", "local"]);
        assert!(claude.metered);
        assert_eq!(
            claude.permission_args.get(&Permissions::Read),
            Some(&vec!["--permission-mode".to_owned(), "plan".to_owned()])
        );
    }

    #[test]
    fn starter_config_quotes_the_repo_path_and_parses_back() {
        let repo = PathBuf::from("/work/my \"project\"");
        let config = Config::from_toml_str(&Config::starter_toml(&repo)).unwrap();
        assert_eq!(config.repo, repo);
    }

    #[test]
    fn effective_permissions_is_the_lower_of_request_and_ceiling() {
        let mut profile = Config::from_toml_str(MINIMAL)
            .unwrap()
            .profile("local")
            .unwrap()
            .clone();
        let cases = [
            (Permissions::Read, Permissions::Read, Permissions::Read),
            (
                Permissions::WriteShell,
                Permissions::Read,
                Permissions::Read,
            ),
            (
                Permissions::WriteShell,
                Permissions::WriteWorkdir,
                Permissions::WriteWorkdir,
            ),
            (
                Permissions::WriteWorkdir,
                Permissions::WriteShell,
                Permissions::WriteWorkdir,
            ),
            (
                Permissions::WriteShell,
                Permissions::WriteShell,
                Permissions::WriteShell,
            ),
        ];
        for (requested, ceiling, expected) in cases {
            profile.permissions = ceiling;
            assert_eq!(
                profile.effective_permissions(requested),
                expected,
                "requested {requested} under ceiling {ceiling}"
            );
        }
    }

    #[test]
    fn old_configs_default_to_unmetered_with_no_permission_args() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        let profile = config.profile("local").unwrap();
        assert!(!profile.metered);
        assert!(profile.permission_args.is_empty());
    }

    #[test]
    fn metered_and_permission_args_parse_per_level() {
        let text = r#"
repo = "/work/project"
default_profile = "claude"

[[profiles]]
name = "claude"
backend = "command"
command = ["claude", "-p"]
metered = true
permissions = "write-shell"

[profiles.permission_args]
read = ["--permission-mode", "plan"]
write-workdir = ["--permission-mode", "acceptEdits"]
"#;
        let config = Config::from_toml_str(text).unwrap();
        let profile = config.profile("claude").unwrap();
        assert!(profile.metered);
        assert_eq!(
            profile.permission_args.get(&Permissions::Read),
            Some(&vec!["--permission-mode".to_owned(), "plan".to_owned()])
        );
        assert_eq!(profile.permission_args.get(&Permissions::WriteShell), None);
    }

    #[test]
    fn unknown_permission_arg_level_is_rejected() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = ["true"]

[profiles.permission_args]
write_shell = ["--yes"]
"#;
        assert!(matches!(parse_err(text), CoreError::ConfigParse { .. }));
    }

    #[test]
    fn cycles_and_unknown_names_are_still_rejected_with_new_fields() {
        let cyclic = r#"
repo = "/work/project"
default_profile = "a"

[[profiles]]
name = "a"
backend = "command"
command = ["true"]
metered = true
fallback = ["b"]

[[profiles]]
name = "b"
backend = "command"
command = ["true"]
fallback = ["a"]
"#;
        assert!(matches!(parse_err(cyclic), CoreError::FallbackCycle { .. }));
        let dangling = cyclic.replace("fallback = [\"a\"]", "fallback = [\"zz\"]");
        assert!(matches!(
            parse_err(&dangling),
            CoreError::UnknownFallback { .. }
        ));
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        let text = toml::to_string(&config).unwrap();
        assert_eq!(Config::from_toml_str(&text).unwrap(), config);
    }
}
