use std::fs;
use std::path::Path;

use overseer_core::is_valid_skill_name;

use crate::StoreError;

/// Overview of a skill in the state directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub current_version: u32,
    pub size_bytes: u64,
}

/// List all skills in `<skills_dir>`, returning summaries sorted by skill name.
pub fn list_skills(skills_dir: &Path) -> Result<Vec<SkillSummary>, StoreError> {
    if !skills_dir.exists() {
        return Ok(Vec::new());
    }
    let read_dir = fs::read_dir(skills_dir).map_err(|source| StoreError::SkillIo {
        action: "reading skills directory",
        path: skills_dir.to_path_buf(),
        source,
    })?;

    let mut summaries = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|source| StoreError::SkillIo {
            action: "reading entry in skills directory",
            path: skills_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !is_valid_skill_name(file_name) {
                continue;
            }
            if let Some(curr_ver) = current_version(skills_dir, file_name)? {
                let ver_path = path.join(format!("v{curr_ver}.md"));
                let size_bytes = match fs::metadata(&ver_path) {
                    Ok(meta) => meta.len(),
                    Err(_) => 0,
                };
                summaries.push(SkillSummary {
                    name: file_name.to_owned(),
                    current_version: curr_ver,
                    size_bytes,
                });
            }
        }
    }
    summaries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(summaries)
}

/// Read the current version number for a skill, if the skill exists and has a valid `current` file.
pub fn current_version(skills_dir: &Path, name: &str) -> Result<Option<u32>, StoreError> {
    if !is_valid_skill_name(name) {
        return Err(StoreError::InvalidSkillName {
            name: name.to_owned(),
        });
    }
    let current_path = skills_dir.join(name).join("current");
    if !current_path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&current_path).map_err(|source| StoreError::SkillIo {
        action: "reading current version file",
        path: current_path.clone(),
        source,
    })?;
    let ver = text
        .trim()
        .parse::<u32>()
        .map_err(|_| StoreError::InvalidSkillVersionFile { path: current_path })?;
    Ok(Some(ver))
}

/// Read the content of a specific version of a skill.
pub fn read_skill_version(
    skills_dir: &Path,
    name: &str,
    version: u32,
) -> Result<String, StoreError> {
    if !is_valid_skill_name(name) {
        return Err(StoreError::InvalidSkillName {
            name: name.to_owned(),
        });
    }
    let file_path = skills_dir.join(name).join(format!("v{version}.md"));
    if !file_path.exists() {
        return Err(StoreError::SkillVersionNotFound {
            name: name.to_owned(),
            version,
        });
    }
    fs::read_to_string(&file_path).map_err(|source| StoreError::SkillIo {
        action: "reading skill version file",
        path: file_path,
        source,
    })
}

/// Read the content of the current version of the skill. Returns None if skill or version doesn't exist.
pub fn read_current_skill(
    skills_dir: &Path,
    name: &str,
) -> Result<Option<(u32, String)>, StoreError> {
    let Some(version) = current_version(skills_dir, name)? else {
        return Ok(None);
    };
    let content = read_skill_version(skills_dir, name, version)?;
    Ok(Some((version, content)))
}

/// Create the next version of a skill with the given text.
/// If the skill doesn't exist, creates v1.md and writes "1" to `current`.
/// If it exists, finds the highest existing version N, writes v<N+1>.md, and updates `current` to N+1.
/// Returns the new version number.
pub fn create_next_version(
    skills_dir: &Path,
    name: &str,
    content: &str,
) -> Result<u32, StoreError> {
    if !is_valid_skill_name(name) {
        return Err(StoreError::InvalidSkillName {
            name: name.to_owned(),
        });
    }
    let skill_dir = skills_dir.join(name);
    if !skill_dir.exists() {
        fs::create_dir_all(&skill_dir).map_err(|source| StoreError::SkillIo {
            action: "creating skill directory",
            path: skill_dir.clone(),
            source,
        })?;
    }

    let mut highest_ver = 0u32;
    if let Ok(entries) = fs::read_dir(&skill_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(file_name) = path.file_name().and_then(|s| s.to_str())
                && let Some(num_str) = file_name
                    .strip_prefix('v')
                    .and_then(|s| s.strip_suffix(".md"))
                && let Ok(n) = num_str.parse::<u32>()
                && n > highest_ver
            {
                highest_ver = n;
            }
        }
    }
    if let Ok(Some(curr)) = current_version(skills_dir, name)
        && curr > highest_ver
    {
        highest_ver = curr;
    }

    let next_version = highest_ver.saturating_add(1);
    let ver_file = skill_dir.join(format!("v{next_version}.md"));
    fs::write(&ver_file, content).map_err(|source| StoreError::SkillIo {
        action: "writing next skill version file",
        path: ver_file,
        source,
    })?;

    let current_file = skill_dir.join("current");
    fs::write(&current_file, format!("{next_version}\n")).map_err(|source| {
        StoreError::SkillIo {
            action: "writing current version file",
            path: current_file,
            source,
        }
    })?;

    Ok(next_version)
}

/// Change the active version in `current` to `version`. Verifies that `v<version>.md` exists first.
pub fn set_current_version(skills_dir: &Path, name: &str, version: u32) -> Result<(), StoreError> {
    if !is_valid_skill_name(name) {
        return Err(StoreError::InvalidSkillName {
            name: name.to_owned(),
        });
    }
    let ver_file = skills_dir.join(name).join(format!("v{version}.md"));
    if !ver_file.exists() {
        return Err(StoreError::SkillVersionNotFound {
            name: name.to_owned(),
            version,
        });
    }
    let current_file = skills_dir.join(name).join("current");
    fs::write(&current_file, format!("{version}\n")).map_err(|source| StoreError::SkillIo {
        action: "writing current version file",
        path: current_file,
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn create_next_version_starts_at_v1_and_sets_current() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");

        let v = create_next_version(&skills_dir, "rust", "# Rust Skill").unwrap();
        assert_eq!(v, 1);

        assert_eq!(current_version(&skills_dir, "rust").unwrap(), Some(1));
        assert_eq!(
            read_skill_version(&skills_dir, "rust", 1).unwrap(),
            "# Rust Skill"
        );
        assert_eq!(
            read_current_skill(&skills_dir, "rust").unwrap(),
            Some((1, "# Rust Skill".to_owned()))
        );
    }

    #[test]
    fn create_next_version_increments_and_updates_current() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");

        let v1 = create_next_version(&skills_dir, "rust", "v1 content").unwrap();
        assert_eq!(v1, 1);
        let v2 = create_next_version(&skills_dir, "rust", "v2 content").unwrap();
        assert_eq!(v2, 2);

        assert_eq!(current_version(&skills_dir, "rust").unwrap(), Some(2));
        assert_eq!(
            read_skill_version(&skills_dir, "rust", 1).unwrap(),
            "v1 content"
        );
        assert_eq!(
            read_skill_version(&skills_dir, "rust", 2).unwrap(),
            "v2 content"
        );
        assert_eq!(
            read_current_skill(&skills_dir, "rust").unwrap(),
            Some((2, "v2 content".to_owned()))
        );
    }

    #[test]
    fn current_version_returns_none_when_skill_missing() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");

        assert_eq!(current_version(&skills_dir, "nonexistent").unwrap(), None);
        assert_eq!(
            read_current_skill(&skills_dir, "nonexistent").unwrap(),
            None
        );
    }

    #[test]
    fn set_current_version_updates_current_and_rejects_missing() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");

        create_next_version(&skills_dir, "rust", "v1").unwrap();
        create_next_version(&skills_dir, "rust", "v2").unwrap();
        assert_eq!(current_version(&skills_dir, "rust").unwrap(), Some(2));

        set_current_version(&skills_dir, "rust", 1).unwrap();
        assert_eq!(current_version(&skills_dir, "rust").unwrap(), Some(1));
        assert_eq!(
            read_current_skill(&skills_dir, "rust").unwrap(),
            Some((1, "v1".to_owned()))
        );

        let err = set_current_version(&skills_dir, "rust", 99).unwrap_err();
        assert!(matches!(err, StoreError::SkillVersionNotFound { .. }));
    }

    #[test]
    fn list_skills_returns_sorted_summaries_with_size() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");

        create_next_version(&skills_dir, "testing", "content for testing").unwrap();
        create_next_version(&skills_dir, "rust", "short").unwrap();

        let list = list_skills(&skills_dir).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "rust");
        assert_eq!(list[0].current_version, 1);
        assert_eq!(list[0].size_bytes, 5);
        assert_eq!(list[1].name, "testing");
        assert_eq!(list[1].current_version, 1);
        assert_eq!(list[1].size_bytes, 19);
    }

    #[test]
    fn invalid_skill_name_rejected() {
        let temp = TempDir::new().unwrap();
        let skills_dir = temp.path().join("skills");

        assert!(matches!(
            create_next_version(&skills_dir, "Bad_Skill", "test").unwrap_err(),
            StoreError::InvalidSkillName { .. }
        ));
        assert!(matches!(
            current_version(&skills_dir, "").unwrap_err(),
            StoreError::InvalidSkillName { .. }
        ));
    }
}
