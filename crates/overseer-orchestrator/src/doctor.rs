use std::collections::HashMap;
use std::fmt;

use overseer_backends::Health;
use overseer_core::{BackendKind, Config};

use crate::router::plan_attempts;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub name: String,
    pub health: Health,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileReport {
    pub name: String,
    pub backend: BackendKind,
    pub health: Health,
    pub metered: bool,
    pub plan: Vec<PlanEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Usable,
    NothingUsable,
}

impl Verdict {
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Usable)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usable => "usable",
            Self::NothingUsable => "NOTHING USABLE",
        }
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq<str> for Verdict {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Verdict {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub profiles: Vec<ProfileReport>,
    pub default_profile: String,
    pub default_plan: Vec<PlanEntry>,
    pub credits_low: bool,
    pub verdict: Verdict,
}

/// Build the doctor report for a configuration, given the current credits-low setting
/// and per-profile health check results.
pub fn doctor_report(
    config: &Config,
    credits_low: bool,
    checks: &HashMap<String, Health>,
) -> Report {
    let mut profile_reports = Vec::new();

    for p in &config.profiles {
        let health = checks.get(&p.name).cloned().unwrap_or(Health {
            ok: false,
            detail: "not checked".to_owned(),
        });
        let plan = match plan_attempts(config, p, credits_low) {
            Ok(chain) => chain
                .into_iter()
                .map(|item| PlanEntry {
                    name: item.name.clone(),
                    health: checks.get(&item.name).cloned().unwrap_or(Health {
                        ok: false,
                        detail: "not checked".to_owned(),
                    }),
                })
                .collect(),
            Err(_) => Vec::new(),
        };

        profile_reports.push(ProfileReport {
            name: p.name.clone(),
            backend: p.backend,
            health,
            metered: p.metered,
            plan,
        });
    }

    let default_plan = match config.profile(&config.default_profile) {
        Some(def) => match plan_attempts(config, def, credits_low) {
            Ok(chain) => chain
                .into_iter()
                .map(|item| PlanEntry {
                    name: item.name.clone(),
                    health: checks.get(&item.name).cloned().unwrap_or(Health {
                        ok: false,
                        detail: "not checked".to_owned(),
                    }),
                })
                .collect(),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };

    let verdict = if default_plan.iter().any(|entry| entry.health.ok) {
        Verdict::Usable
    } else {
        Verdict::NothingUsable
    };

    Report {
        profiles: profile_reports,
        default_profile: config.default_profile.clone(),
        default_plan,
        credits_low,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use std::collections::HashMap;

    use overseer_backends::Health;
    use overseer_core::{BackendKind, Config, Permissions, Profile};

    use super::*;

    fn profile(name: &str, backend: BackendKind, fallback: &[&str], metered: bool) -> Profile {
        Profile {
            name: name.to_owned(),
            backend,
            model: if backend == BackendKind::Openai {
                Some("local".to_owned())
            } else {
                None
            },
            base_url: if backend == BackendKind::Openai {
                Some("http://127.0.0.1:8080/v1".to_owned())
            } else {
                None
            },
            api_key_env: None,
            command: if backend == BackendKind::Command {
                Some(vec!["true".to_owned()])
            } else {
                None
            },
            prompt_flag: None,
            prompt_stdin: false,
            role_prompt: None,
            fallback: fallback.iter().map(|s| (*s).to_owned()).collect(),
            permissions: Permissions::WriteWorkdir,
            timeout_secs: 5,
            metered,
            permission_args: Default::default(),
            env: Default::default(),
            agent_tools: false,
            max_steps: 40,
        }
    }

    fn config(default_profile: &str, profiles: Vec<Profile>) -> Config {
        Config {
            repo: "/work/project".into(),
            default_profile: default_profile.to_owned(),
            profiles,
        }
    }

    #[test]
    fn single_healthy_profile_is_usable() {
        let cfg = config(
            "local",
            vec![profile("local", BackendKind::Openai, &[], false)],
        );
        let mut checks = HashMap::new();
        checks.insert(
            "local".to_owned(),
            Health {
                ok: true,
                detail: "llama-3".to_owned(),
            },
        );

        let report = doctor_report(&cfg, false, &checks);
        assert_eq!(report.verdict, Verdict::Usable);
        assert_eq!(report.verdict, "usable");
        assert_eq!(report.default_profile, "local");
        assert_eq!(
            report.default_plan,
            vec![PlanEntry {
                name: "local".to_owned(),
                health: Health {
                    ok: true,
                    detail: "llama-3".to_owned()
                }
            }]
        );
        assert_eq!(report.profiles.len(), 1);
        assert_eq!(report.profiles[0].name, "local");
        assert_eq!(report.profiles[0].backend, BackendKind::Openai);
        assert!(!report.profiles[0].metered);
    }

    #[test]
    fn single_unhealthy_profile_is_nothing_usable() {
        let cfg = config(
            "local",
            vec![profile("local", BackendKind::Openai, &[], false)],
        );
        let mut checks = HashMap::new();
        checks.insert(
            "local".to_owned(),
            Health {
                ok: false,
                detail: "unreachable".to_owned(),
            },
        );

        let report = doctor_report(&cfg, false, &checks);
        assert_eq!(report.verdict, Verdict::NothingUsable);
        assert_eq!(report.verdict, "NOTHING USABLE");
    }

    #[test]
    fn fallback_chain_first_unhealthy_second_healthy_is_usable() {
        let cfg = config(
            "claude",
            vec![
                profile("claude", BackendKind::Command, &["local"], true),
                profile("local", BackendKind::Openai, &[], false),
            ],
        );
        let mut checks = HashMap::new();
        checks.insert(
            "claude".to_owned(),
            Health {
                ok: false,
                detail: "auth required".to_owned(),
            },
        );
        checks.insert(
            "local".to_owned(),
            Health {
                ok: true,
                detail: "local".to_owned(),
            },
        );

        let report = doctor_report(&cfg, false, &checks);
        assert_eq!(report.verdict, Verdict::Usable);
        assert_eq!(
            report.default_plan,
            vec![
                PlanEntry {
                    name: "claude".to_owned(),
                    health: Health {
                        ok: false,
                        detail: "auth required".to_owned()
                    }
                },
                PlanEntry {
                    name: "local".to_owned(),
                    health: Health {
                        ok: true,
                        detail: "local".to_owned()
                    }
                }
            ]
        );
    }

    #[test]
    fn fallback_chain_all_unhealthy_is_nothing_usable() {
        let cfg = config(
            "claude",
            vec![
                profile("claude", BackendKind::Command, &["local"], true),
                profile("local", BackendKind::Openai, &[], false),
            ],
        );
        let mut checks = HashMap::new();
        checks.insert(
            "claude".to_owned(),
            Health {
                ok: false,
                detail: "quota-exhausted".to_owned(),
            },
        );
        checks.insert(
            "local".to_owned(),
            Health {
                ok: false,
                detail: "unreachable".to_owned(),
            },
        );

        let report = doctor_report(&cfg, false, &checks);
        assert_eq!(report.verdict, Verdict::NothingUsable);
    }

    #[test]
    fn credits_low_drops_metered_and_finds_unmetered_healthy_is_usable() {
        let cfg = config(
            "claude",
            vec![
                profile("claude", BackendKind::Command, &["local"], true),
                profile("local", BackendKind::Openai, &[], false),
            ],
        );
        let mut checks = HashMap::new();
        // Even if claude was healthy, in credits_low mode it is dropped from plan
        checks.insert(
            "claude".to_owned(),
            Health {
                ok: true,
                detail: "claude".to_owned(),
            },
        );
        checks.insert(
            "local".to_owned(),
            Health {
                ok: true,
                detail: "local".to_owned(),
            },
        );

        let report = doctor_report(&cfg, true, &checks);
        assert_eq!(report.verdict, Verdict::Usable);
        assert_eq!(
            report.default_plan,
            vec![PlanEntry {
                name: "local".to_owned(),
                health: Health {
                    ok: true,
                    detail: "local".to_owned()
                }
            }]
        );
    }

    #[test]
    fn credits_low_when_only_metered_profiles_exist_is_nothing_usable() {
        let cfg = config(
            "claude",
            vec![profile("claude", BackendKind::Command, &[], true)],
        );
        let mut checks = HashMap::new();
        checks.insert(
            "claude".to_owned(),
            Health {
                ok: true,
                detail: "claude".to_owned(),
            },
        );

        let report = doctor_report(&cfg, true, &checks);
        assert_eq!(report.verdict, Verdict::NothingUsable);
        assert!(report.default_plan.is_empty());
    }

    #[test]
    fn report_annotates_every_profile_plan() {
        let cfg = config(
            "local",
            vec![
                profile("local", BackendKind::Openai, &[], false),
                profile("gemini", BackendKind::Command, &["local"], false),
            ],
        );
        let mut checks = HashMap::new();
        checks.insert(
            "local".to_owned(),
            Health {
                ok: true,
                detail: "local".to_owned(),
            },
        );
        checks.insert(
            "gemini".to_owned(),
            Health {
                ok: false,
                detail: "not found: agy".to_owned(),
            },
        );

        let report = doctor_report(&cfg, false, &checks);
        assert_eq!(report.profiles.len(), 2);

        let gemini_rep = report.profiles.iter().find(|p| p.name == "gemini").unwrap();
        assert_eq!(
            gemini_rep.plan,
            vec![
                PlanEntry {
                    name: "gemini".to_owned(),
                    health: Health {
                        ok: false,
                        detail: "not found: agy".to_owned()
                    }
                },
                PlanEntry {
                    name: "local".to_owned(),
                    health: Health {
                        ok: true,
                        detail: "local".to_owned()
                    }
                }
            ]
        );
    }
}
