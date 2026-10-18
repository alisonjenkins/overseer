//! Chooses the ordered list of profiles one task may be attempted on.

use std::collections::HashSet;

use overseer_core::{Config, Profile};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouterError {
    #[error(
        "no profile is available to run a task that starts on {start:?}: every profile in its fallback chain is metered and credits are low"
    )]
    NoAvailableProfile { start: String },

    #[error("fallback chain names profile {name:?}, which is not in the config")]
    UnknownProfile { name: String },
}

/// The profiles to try, in order, for a task whose profile is `start`: the
/// start profile, then its fallbacks depth-first, each profile once. With
/// `credits_low`, metered profiles are dropped from the plan.
pub fn plan_attempts<'a>(
    config: &'a Config,
    start: &'a Profile,
    credits_low: bool,
) -> Result<Vec<&'a Profile>, RouterError> {
    let mut chain = Vec::new();
    visit(config, start, &mut HashSet::new(), &mut chain)?;
    if credits_low {
        chain.retain(|profile| !profile.metered);
    }
    if chain.is_empty() {
        return Err(RouterError::NoAvailableProfile {
            start: start.name.clone(),
        });
    }
    Ok(chain)
}

fn visit<'a>(
    config: &'a Config,
    profile: &'a Profile,
    seen: &mut HashSet<&'a str>,
    chain: &mut Vec<&'a Profile>,
) -> Result<(), RouterError> {
    if !seen.insert(profile.name.as_str()) {
        return Ok(());
    }
    chain.push(profile);
    for name in &profile.fallback {
        let next = config
            .profile(name)
            .ok_or_else(|| RouterError::UnknownProfile { name: name.clone() })?;
        visit(config, next, seen, chain)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use overseer_core::{BackendKind, Config, Permissions, Profile};

    use super::*;

    fn profile(name: &str, fallback: &[&str], metered: bool) -> Profile {
        Profile {
            name: name.to_owned(),
            backend: BackendKind::Command,
            model: None,
            base_url: None,
            api_key_env: None,
            command: Some(vec!["true".to_owned()]),
            prompt_flag: None,
            prompt_stdin: false,
            role_prompt: None,
            fallback: fallback.iter().map(|name| (*name).to_owned()).collect(),
            permissions: Permissions::WriteWorkdir,
            timeout_secs: 10,
            metered,
            permission_args: Default::default(),
            env: Default::default(),
            agent_tools: false,
            max_steps: 40,
            skills: Vec::new(),
        }
    }

    fn config(profiles: Vec<Profile>) -> Config {
        Config {
            repo: "/work/project".into(),
            default_profile: profiles[0].name.clone(),
            review: overseer_core::ReviewConfig::default(),
            escalation: Default::default(),
            landing: Default::default(),
            sources: Default::default(),
            profiles,
            learning: Default::default(),
        }
    }

    fn names(plan: &[&Profile]) -> Vec<String> {
        plan.iter().map(|profile| profile.name.clone()).collect()
    }

    #[test]
    fn profile_without_fallbacks_is_its_own_plan() {
        let config = config(vec![profile("local", &[], false)]);
        let start = config.profile("local").unwrap();
        let plan = plan_attempts(&config, start, false).unwrap();
        assert_eq!(names(&plan), ["local"]);
    }

    #[test]
    fn fallback_order_is_kept() {
        let config = config(vec![
            profile("a", &["b", "c"], false),
            profile("b", &[], false),
            profile("c", &[], false),
        ]);
        let start = config.profile("a").unwrap();
        let plan = plan_attempts(&config, start, false).unwrap();
        assert_eq!(names(&plan), ["a", "b", "c"]);
    }

    #[test]
    fn fallbacks_of_fallbacks_are_visited_depth_first() {
        let config = config(vec![
            profile("a", &["b", "c"], false),
            profile("b", &["d"], false),
            profile("c", &[], false),
            profile("d", &[], false),
        ]);
        let start = config.profile("a").unwrap();
        let plan = plan_attempts(&config, start, false).unwrap();
        assert_eq!(names(&plan), ["a", "b", "d", "c"]);
    }

    #[test]
    fn shared_fallbacks_appear_once_at_first_position() {
        let config = config(vec![
            profile("a", &["b", "c"], false),
            profile("b", &["c"], false),
            profile("c", &[], false),
        ]);
        let start = config.profile("a").unwrap();
        let plan = plan_attempts(&config, start, false).unwrap();
        assert_eq!(names(&plan), ["a", "b", "c"]);
    }

    #[test]
    fn credits_low_drops_metered_profiles_anywhere_in_the_plan() {
        let config = config(vec![
            profile("a", &["b", "c"], false),
            profile("b", &[], true),
            profile("c", &[], false),
        ]);
        let start = config.profile("a").unwrap();
        let plan = plan_attempts(&config, start, true).unwrap();
        assert_eq!(names(&plan), ["a", "c"]);
    }

    #[test]
    fn credits_low_starts_at_first_unmetered_when_start_is_metered() {
        let config = config(vec![
            profile("claude", &["gemini", "local"], true),
            profile("gemini", &["local"], false),
            profile("local", &[], false),
        ]);
        let start = config.profile("claude").unwrap();
        let plan = plan_attempts(&config, start, true).unwrap();
        assert_eq!(names(&plan), ["gemini", "local"]);
    }

    #[test]
    fn metered_start_is_kept_when_credits_are_not_low() {
        let config = config(vec![
            profile("claude", &["local"], true),
            profile("local", &[], false),
        ]);
        let start = config.profile("claude").unwrap();
        let plan = plan_attempts(&config, start, false).unwrap();
        assert_eq!(names(&plan), ["claude", "local"]);
    }

    #[test]
    fn credits_low_with_only_metered_profiles_is_no_available_profile() {
        let config = config(vec![
            profile("claude", &["other"], true),
            profile("other", &[], true),
        ]);
        let start = config.profile("claude").unwrap();
        let err = plan_attempts(&config, start, true).unwrap_err();
        assert_eq!(
            err,
            RouterError::NoAvailableProfile {
                start: "claude".to_owned()
            }
        );
    }

    #[test]
    fn fallback_missing_from_the_config_is_reported() {
        let config = config(vec![profile("a", &["ghost"], false)]);
        let start = config.profile("a").unwrap();
        let err = plan_attempts(&config, start, false).unwrap_err();
        assert_eq!(
            err,
            RouterError::UnknownProfile {
                name: "ghost".to_owned()
            }
        );
    }
}
