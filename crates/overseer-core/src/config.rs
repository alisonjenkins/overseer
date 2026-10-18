use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Permissions};

const DEFAULT_TIMEOUT_SECS: u64 = 1500;
const DEFAULT_MAX_STEPS: u32 = 100;

const STARTER_TEMPLATE: &str = r#"# overseer.toml: where task worktrees come from and how tasks are run.
#
# A task runs on its profile, or on default_profile when it names none. An
# attempt that fails with quota-exhausted, auth-required or unreachable moves to
# the next profile in that profile's fallback list. Any other failure blocks the
# task.
#
# To use claude first, set default_profile = "claude". It then falls back to
# gemini, then local-agent, whenever it cannot run.

# Repository that task worktrees are created from.
repo = {repo}

# Profile for tasks that do not name one with --profile.
default_profile = "local"

# Optional review and rework loop. When profiles is non-empty, every task
# that finishes running is reviewed by each listed profile concurrently.
# If any reviewer asks for changes, the task is reworked up to max_reworks
# times. If any reviewer escalates, or the rework limit is reached without
# approval, the task moves to needs-human.
#
# [review]
# profiles = ["local"]
# max_reworks = 2
# rework_profile = "local-edit"  # defaults to the task's own profile

# Hard escalation rules. Tasks whose diff touches any of these patterns will
# pause at needs-human before review, requiring human approval.
#
# [escalation]
# enabled = true
# paths = [
#     ".github/**",
#     ".gitlab-ci.yml",
#     "**/migrations/**",
#     "**/auth/**",
#     "**/*.tf",
#     "Dockerfile*",
#     "**/Dockerfile*",
#     "deploy/**",
#     "Cargo.toml",
#     "**/Cargo.toml",
#     "package.json",
#     "**/package.json",
#     "flake.nix",
#     ".env*",
#     "**/secrets/**",
# ]

# Automated landing: push a reviewed task branch to a pull request on the
# remote, running test_command first, and optionally fast-forward merging if gates pass.
#
# [landing]
# remote = "origin"
# base = "main"
# auto_merge = false
# test_command = "cargo test"

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

# A local model running with the agent tool loop so it can inspect and edit
# files in the task worktree. It needs a llama-server started with tool calling (`--jinja`)
# and a model with a tool-capable chat template.
[[profiles]]
name = "local-edit"
backend = "openai"
model = "local"
base_url = "http://127.0.0.1:8080/v1"
permissions = "write-workdir"
timeout_secs = 1500
role_prompt = "You are a careful senior engineer."
agent_tools = true
max_steps = 100
fallback = []

# Gemini through the agy CLI. Needs `agy` on PATH and signed in.
# agy must be signed in (run `agy` once interactively).
[[profiles]]
name = "gemini"
# command backend: runs this argv in the task worktree with the prompt appended
# as the last argument.
backend = "command"
command = ["agy", "--model", "gemini-3.8-flash-medium"]
prompt_flag = "-p"
# headless agy needs write-shell to edit files at all (accept-edits is auto-denied a command permission); safe because runs happen in a throwaway task worktree
permissions = "write-shell"
role_prompt = "You are a careful senior engineer."
fallback = ["local-agent"]
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
fallback = ["gemini", "local-agent"]
[profiles.permission_args]
read = ["--permission-mode", "plan"]
write-workdir = ["--permission-mode", "acceptEdits"]
write-shell = ["--dangerously-skip-permissions"]

# runs the active local llama.cpp profile as an opencode agent that edits the task worktree; load a profile first with switch-local-profile.sh; no fallback
[[profiles]]
name = "local-agent"
backend = "command"
command = ["~/.claude/skills/delegation/scripts/delegate-to-local-agent.sh", "{workdir}"]
env = { LOCAL_LLM_AGENT_EDIT = "1" }
permissions = "write-workdir"
timeout_secs = 1800
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
    /// Environment variables set on the spawned process for a `command`
    /// backend. Values are plain strings, not secrets.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Run an agent tool loop instead of a single request (openai backend only).
    #[serde(default)]
    pub agent_tools: bool,
    /// Maximum number of tool iterations before the run fails.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// Skill names to inject into the system prompt.
    #[serde(default)]
    pub skills: Vec<String>,
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

fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

const DEFAULT_MAX_REWORKS: u8 = 2;

fn default_max_reworks() -> u8 {
    DEFAULT_MAX_REWORKS
}

/// Configuration for the automated review and rework loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    /// Profiles to run concurrently as reviewers. Empty means review is disabled.
    #[serde(default)]
    pub profiles: Vec<String>,
    /// Maximum number of rework cycles before escalating to needs-human.
    #[serde(default = "default_max_reworks")]
    pub max_reworks: u8,
    /// Profile to run rework attempts with. Defaults to None (the task's own profile).
    #[serde(default)]
    pub rework_profile: Option<String>,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            max_reworks: DEFAULT_MAX_REWORKS,
            rework_profile: None,
        }
    }
}

/// Configuration for task retrospectives and skill proposals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningConfig {
    /// Profile used to run retrospectives. None means learning is disabled.
    #[serde(default)]
    pub profile: Option<String>,
    /// The skill proposals are written for. Defaults to "general".
    #[serde(default = "default_learning_skill")]
    pub skill: Option<String>,
    /// If true, only run retrospectives on tasks that needed rework, were blocked, or escalated.
    #[serde(default = "default_min_signal")]
    pub min_signal: bool,
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            profile: None,
            skill: default_learning_skill(),
            min_signal: default_min_signal(),
        }
    }
}

fn default_learning_skill() -> Option<String> {
    Some("general".to_owned())
}

fn default_min_signal() -> bool {
    true
}

/// Check if a skill name consists of non-empty ASCII lowercase letters, digits, and hyphens.
pub fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub const DEFAULT_ESCALATION_PATHS: &[&str] = &[
    ".github/**",
    ".gitlab-ci.yml",
    "**/migrations/**",
    "**/auth/**",
    "**/*.tf",
    "Dockerfile*",
    "**/Dockerfile*",
    "deploy/**",
    "Cargo.toml",
    "**/Cargo.toml",
    "package.json",
    "**/package.json",
    "flake.nix",
    ".env*",
    "**/secrets/**",
];

fn default_escalation_enabled() -> bool {
    true
}

fn default_escalation_paths() -> Vec<String> {
    DEFAULT_ESCALATION_PATHS
        .iter()
        .map(|&s| s.to_owned())
        .collect()
}

/// Configuration for hard escalation on sensitive changed paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscalationConfig {
    #[serde(default = "default_escalation_enabled")]
    pub enabled: bool,
    #[serde(default = "default_escalation_paths")]
    pub paths: Vec<String>,
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            enabled: default_escalation_enabled(),
            paths: default_escalation_paths(),
        }
    }
}

/// Configuration for automated landing and PR creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandingConfig {
    #[serde(default = "default_landing_remote")]
    pub remote: String,
    #[serde(default = "default_landing_base")]
    pub base: String,
    #[serde(default)]
    pub auto_merge: bool,
    #[serde(default, deserialize_with = "deserialize_empty_as_none")]
    pub test_command: Option<String>,
}

fn default_landing_remote() -> String {
    "origin".to_owned()
}
fn default_landing_base() -> String {
    "main".to_owned()
}

impl Default for LandingConfig {
    fn default() -> Self {
        Self {
            remote: default_landing_remote(),
            base: default_landing_base(),
            auto_merge: false,
            test_command: None,
        }
    }
}

fn deserialize_empty_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.and_then(|s| (!s.is_empty()).then_some(s)))
}

/// The whole `overseer.toml` document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repo: PathBuf,
    pub default_profile: String,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub escalation: EscalationConfig,
    #[serde(default)]
    pub landing: LandingConfig,
    #[serde(default)]
    pub profiles: Vec<Profile>,
    #[serde(default)]
    pub learning: LearningConfig,
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
            check_env_keys(profile)?;
            for skill in &profile.skills {
                if !is_valid_skill_name(skill) {
                    return Err(CoreError::InvalidSkillName {
                        profile: profile.name.clone(),
                        skill: skill.clone(),
                    });
                }
            }
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

        for reviewer in &self.review.profiles {
            if !by_name.contains_key(reviewer.as_str()) {
                return Err(CoreError::UnknownReviewProfile {
                    profile: reviewer.clone(),
                });
            }
        }

        if let Some(rework_profile) = &self.review.rework_profile
            && !by_name.contains_key(rework_profile.as_str())
        {
            return Err(CoreError::UnknownReviewProfile {
                profile: rework_profile.clone(),
            });
        }

        let mut state: HashMap<&str, Visit> = HashMap::new();
        for profile in &self.profiles {
            check_fallback_cycles(profile.name.as_str(), &by_name, &mut state)?;
        }

        if let Some(skill) = &self.learning.skill
            && !is_valid_skill_name(skill)
        {
            return Err(CoreError::InvalidLearningSkill {
                skill: skill.clone(),
            });
        }

        if let Some(profile_name) = &self.learning.profile
            && !by_name.contains_key(profile_name.as_str())
        {
            return Err(CoreError::UnknownLearningProfile {
                profile: profile_name.clone(),
            });
        }

        for pattern in &self.escalation.paths {
            if let Err(err) = globset::Glob::new(pattern) {
                return Err(CoreError::InvalidEscalationGlob {
                    glob: pattern.clone(),
                    message: err.to_string(),
                });
            }
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
    if profile.max_steps < 1 {
        return Err(CoreError::InvalidMaxSteps {
            profile: profile.name.clone(),
            max_steps: profile.max_steps,
        });
    }
    Ok(())
}

fn check_env_keys(profile: &Profile) -> Result<(), CoreError> {
    for key in profile.env.keys() {
        if key.is_empty() || key.contains('=') {
            return Err(CoreError::InvalidEnvKey {
                profile: profile.name.clone(),
                key: key.clone(),
            });
        }
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
        assert!(!profile.agent_tools);
        assert_eq!(profile.max_steps, 100);
        assert_eq!(config.repo, PathBuf::from("/work/project"));
    }

    #[test]
    fn agent_tools_and_max_steps_parse_and_default() {
        let text = r#"
repo = "/work/project"
default_profile = "local"

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
agent_tools = true
max_steps = 12
"#;
        let config = Config::from_toml_str(text).unwrap();
        let profile = config.profile("local").unwrap();
        assert!(profile.agent_tools);
        assert_eq!(profile.max_steps, 12);
    }

    #[test]
    fn zero_max_steps_is_rejected() {
        let text = r#"
repo = "/work/project"
default_profile = "local"

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
max_steps = 0
"#;
        assert_eq!(
            parse_err(text),
            CoreError::InvalidMaxSteps {
                profile: "local".to_owned(),
                max_steps: 0,
            }
        );
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
        assert_eq!(
            names,
            ["local", "local-edit", "gemini", "claude", "local-agent"]
        );

        let local = config.profile("local").unwrap();
        assert_eq!(local.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert!(local.fallback.is_empty());
        assert!(!local.metered);

        let local_edit = config.profile("local-edit").unwrap();
        assert_eq!(local_edit.backend, BackendKind::Openai);
        assert_eq!(local_edit.model.as_deref(), Some("local"));
        assert_eq!(
            local_edit.base_url.as_deref(),
            Some("http://127.0.0.1:8080/v1")
        );
        assert!(local_edit.agent_tools);
        assert_eq!(local_edit.max_steps, 100);
        assert_eq!(local_edit.permissions, Permissions::WriteWorkdir);
        assert!(local_edit.fallback.is_empty());

        let starter = Config::starter_toml(Path::new("/work/project"));
        assert!(
            starter.contains("needs a llama-server started with tool calling (`--jinja`)"),
            "{starter}"
        );
        assert!(
            starter.contains("agy must be signed in (run `agy` once interactively)"),
            "{starter}"
        );
        assert!(
            starter.contains(
                "headless agy needs write-shell to edit files at all (accept-edits is auto-denied a command permission); safe because runs happen in a throwaway task worktree"
            ),
            "{starter}"
        );
        assert!(
            starter.contains(
                "runs the active local llama.cpp profile as an opencode agent that edits the task worktree; load a profile first with switch-local-profile.sh; no fallback"
            ),
            "{starter}"
        );

        let gemini = config.profile("gemini").unwrap();
        assert_eq!(gemini.permissions, Permissions::WriteShell);
        assert_eq!(
            gemini.command.as_deref(),
            Some(
                &[
                    "agy".to_owned(),
                    "--model".to_owned(),
                    "gemini-3.8-flash-medium".to_owned()
                ][..]
            )
        );
        assert_eq!(gemini.prompt_flag.as_deref(), Some("-p"));
        assert_eq!(gemini.fallback, ["local-agent"]);
        assert_eq!(
            gemini.permission_args.get(&Permissions::Read),
            Some(&Vec::new())
        );
        assert_eq!(
            gemini.permission_args.get(&Permissions::WriteWorkdir),
            Some(&vec!["--mode".to_owned(), "accept-edits".to_owned()])
        );
        assert_eq!(
            gemini.permission_args.get(&Permissions::WriteShell),
            Some(&vec!["--dangerously-skip-permissions".to_owned()])
        );

        let claude = config.profile("claude").unwrap();
        assert_eq!(claude.fallback, ["gemini", "local-agent"]);
        assert!(claude.metered);
        assert_eq!(
            claude.permission_args.get(&Permissions::Read),
            Some(&vec!["--permission-mode".to_owned(), "plan".to_owned()])
        );

        let local_agent = config.profile("local-agent").unwrap();
        assert_eq!(local_agent.backend, BackendKind::Command);
        assert_eq!(
            local_agent.command.as_deref(),
            Some(
                &[
                    "~/.claude/skills/delegation/scripts/delegate-to-local-agent.sh".to_owned(),
                    "{workdir}".to_owned()
                ][..]
            )
        );
        assert_eq!(
            local_agent
                .env
                .get("LOCAL_LLM_AGENT_EDIT")
                .map(String::as_str),
            Some("1")
        );
        assert_eq!(local_agent.permissions, Permissions::WriteWorkdir);
        assert_eq!(local_agent.timeout_secs, 1800);
        assert_eq!(local_agent.role_prompt, None);
        assert!(local_agent.fallback.is_empty());
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
        assert!(profile.env.is_empty());
    }

    #[test]
    fn command_profile_parses_env() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = ["tool"]
env = { FOO = "bar", BAZ = "qux" }
"#;
        let config = Config::from_toml_str(text).unwrap();
        let profile = config.profile("agent").unwrap();
        assert_eq!(profile.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(profile.env.get("BAZ").map(String::as_str), Some("qux"));
    }

    #[test]
    fn empty_env_key_is_rejected() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = ["tool"]
env = { "" = "val" }
"#;
        assert_eq!(
            parse_err(text),
            CoreError::InvalidEnvKey {
                profile: "agent".to_owned(),
                key: "".to_owned(),
            }
        );
    }

    #[test]
    fn env_key_with_equals_is_rejected() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = ["tool"]
env = { "FOO=BAR" = "val" }
"#;
        assert_eq!(
            parse_err(text),
            CoreError::InvalidEnvKey {
                profile: "agent".to_owned(),
                key: "FOO=BAR".to_owned(),
            }
        );
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

    #[test]
    fn review_config_defaults_when_omitted() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        assert_eq!(config.review.profiles, Vec::<String>::new());
        assert_eq!(config.review.max_reworks, 2);
        assert_eq!(config.review.rework_profile, None);
    }

    #[test]
    fn review_config_parses_custom_values() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[review]
profiles = ["agent"]
max_reworks = 4
rework_profile = "agent"

[[profiles]]
name = "agent"
backend = "command"
command = ["true"]
"#;
        let config = Config::from_toml_str(text).unwrap();
        assert_eq!(config.review.profiles, vec!["agent"]);
        assert_eq!(config.review.max_reworks, 4);
        assert_eq!(config.review.rework_profile.as_deref(), Some("agent"));
    }

    #[test]
    fn review_config_rejects_unknown_reviewer_profile() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[review]
profiles = ["ghost"]

[[profiles]]
name = "agent"
backend = "command"
command = ["true"]
"#;
        assert_eq!(
            parse_err(text),
            CoreError::UnknownReviewProfile {
                profile: "ghost".to_owned()
            }
        );
    }

    #[test]
    fn review_config_rejects_unknown_rework_profile() {
        let text = r#"
repo = "/work/project"
default_profile = "agent"

[review]
profiles = ["agent"]
rework_profile = "ghost"

[[profiles]]
name = "agent"
backend = "command"
command = ["true"]
"#;
        assert_eq!(
            parse_err(text),
            CoreError::UnknownReviewProfile {
                profile: "ghost".to_owned()
            }
        );
    }

    #[test]
    fn skills_parse_and_default_empty() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        let profile = config.profile("local").unwrap();
        assert!(profile.skills.is_empty());

        let text = r#"
repo = "/work/project"
default_profile = "local"

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
skills = ["rust", "test-driven-dev", "clean-code1"]
"#;
        let config = Config::from_toml_str(text).unwrap();
        let profile = config.profile("local").unwrap();
        assert_eq!(
            profile.skills,
            vec!["rust", "test-driven-dev", "clean-code1"]
        );
    }

    #[test]
    fn invalid_skill_name_is_rejected() {
        let text = r#"
repo = "/work/project"
default_profile = "local"

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
skills = ["Invalid_Skill"]
"#;
        assert_eq!(
            parse_err(text),
            CoreError::InvalidSkillName {
                profile: "local".to_owned(),
                skill: "Invalid_Skill".to_owned(),
            }
        );
    }

    #[test]
    fn learning_config_defaults() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        assert_eq!(
            config.learning,
            LearningConfig {
                profile: None,
                skill: Some("general".to_owned()),
                min_signal: true,
            }
        );
    }

    #[test]
    fn learning_config_custom_values() {
        let text = r#"
repo = "/work/project"
default_profile = "local"

[learning]
profile = "local"
skill = "rust-expert"
min_signal = false

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
"#;
        let config = Config::from_toml_str(text).unwrap();
        assert_eq!(
            config.learning,
            LearningConfig {
                profile: Some("local".to_owned()),
                skill: Some("rust-expert".to_owned()),
                min_signal: false,
            }
        );
    }

    #[test]
    fn learning_config_rejects_unknown_profile() {
        let text = r#"
repo = "/work/project"
default_profile = "local"

[learning]
profile = "nonexistent"

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
"#;
        assert_eq!(
            parse_err(text),
            CoreError::UnknownLearningProfile {
                profile: "nonexistent".to_owned(),
            }
        );
    }

    #[test]
    fn learning_config_rejects_invalid_skill_name() {
        let text = r#"
repo = "/work/project"
default_profile = "local"

[learning]
skill = "Bad Skill!"

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
"#;
        assert_eq!(
            parse_err(text),
            CoreError::InvalidLearningSkill {
                skill: "Bad Skill!".to_owned(),
            }
        );
    }

    #[test]
    fn escalation_config_defaults_when_omitted() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        assert!(config.escalation.enabled);
        assert_eq!(
            config.escalation.paths,
            vec![
                ".github/**",
                ".gitlab-ci.yml",
                "**/migrations/**",
                "**/auth/**",
                "**/*.tf",
                "Dockerfile*",
                "**/Dockerfile*",
                "deploy/**",
                "Cargo.toml",
                "**/Cargo.toml",
                "package.json",
                "**/package.json",
                "flake.nix",
                ".env*",
                "**/secrets/**",
            ]
        );
    }

    #[test]
    fn escalation_config_parses_custom_values() {
        let text = r#"
repo = "/work/project"
default_profile = "local"

[escalation]
enabled = false
paths = ["custom/**", "secret.key"]

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
"#;
        let config = Config::from_toml_str(text).unwrap();
        assert!(!config.escalation.enabled);
        assert_eq!(config.escalation.paths, vec!["custom/**", "secret.key"]);
    }

    #[test]
    fn escalation_config_rejects_invalid_glob() {
        let text = r#"
repo = "/work/project"
default_profile = "local"

[escalation]
paths = ["[a-"]

[[profiles]]
name = "local"
backend = "openai"
base_url = "http://127.0.0.1:8080/v1"
model = "local"
"#;
        assert!(matches!(
            parse_err(text),
            CoreError::InvalidEscalationGlob { ref glob, .. } if glob == "[a-"
        ));
    }

    #[test]
    fn landing_config_parses_defaults_and_custom() {
        let config = Config::from_toml_str(MINIMAL).unwrap();
        assert_eq!(config.landing.remote, "origin");
        assert_eq!(config.landing.base, "main");
        assert!(!config.landing.auto_merge);
        assert_eq!(config.landing.test_command, None);

        let text = format!(
            "{MINIMAL}\n[landing]\nremote = \"upstream\"\nbase = \"trunk\"\nauto_merge = true\ntest_command = \"cargo test --all\"\n"
        );
        let custom = Config::from_toml_str(&text).unwrap();
        assert_eq!(custom.landing.remote, "upstream");
        assert_eq!(custom.landing.base, "trunk");
        assert!(custom.landing.auto_merge);
        assert_eq!(
            custom.landing.test_command.as_deref(),
            Some("cargo test --all")
        );
    }

    #[test]
    fn landing_config_zero_length_test_command_is_treated_as_unset() {
        let text = format!("{MINIMAL}\n[landing]\ntest_command = \"\"\n");
        assert_eq!(
            Config::from_toml_str(&text).unwrap().landing.test_command,
            None
        );
    }
}
