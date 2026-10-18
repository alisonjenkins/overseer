use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::CoreError;

/// What a backend may do inside its working directory.
///
/// Variants are ordered from least to most capable, so a profile's permission
/// ceiling can be compared with a requested level using `<=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permissions {
    Read,
    WriteWorkdir,
    WriteShell,
}

impl Permissions {
    pub const ALL: [Self; 3] = [Self::Read, Self::WriteWorkdir, Self::WriteShell];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::WriteWorkdir => "write-workdir",
            Self::WriteShell => "write-shell",
        }
    }
}

impl fmt::Display for Permissions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Permissions {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|perm| perm.as_str() == s)
            .ok_or_else(|| CoreError::UnknownPermissions {
                value: s.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn display_and_from_str_use_kebab_case() {
        let expected = [
            (Permissions::Read, "read"),
            (Permissions::WriteWorkdir, "write-workdir"),
            (Permissions::WriteShell, "write-shell"),
        ];
        for (perm, text) in expected {
            assert_eq!(perm.to_string(), text);
            assert_eq!(text.parse::<Permissions>().unwrap(), perm);
        }
    }

    #[test]
    fn unknown_permission_is_rejected() {
        assert_eq!(
            "write_workdir".parse::<Permissions>(),
            Err(CoreError::UnknownPermissions {
                value: "write_workdir".to_owned()
            })
        );
    }

    #[test]
    fn ordering_is_least_to_most_capable() {
        assert!(Permissions::Read < Permissions::WriteWorkdir);
        assert!(Permissions::WriteWorkdir < Permissions::WriteShell);
    }

    #[test]
    fn serde_uses_kebab_case() {
        #[derive(Deserialize)]
        struct Wrap {
            level: Permissions,
        }
        let wrap: Wrap = toml::from_str("level = \"write-shell\"").unwrap();
        assert_eq!(wrap.level, Permissions::WriteShell);
    }
}
