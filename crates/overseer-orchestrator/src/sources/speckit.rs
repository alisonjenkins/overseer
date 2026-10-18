use std::fs;
use std::path::PathBuf;

use overseer_core::{SourceConfig, SourceError, SourcedTask, TaskSource};

pub struct SpecKitSource {
    specs_dir: PathBuf,
    profile: Option<String>,
    ready: bool,
}

impl SpecKitSource {
    pub fn new(entry: &SourceConfig) -> Self {
        Self {
            specs_dir: entry.path.clone(),
            profile: entry.profile.clone(),
            ready: entry.ready,
        }
    }
}

impl TaskSource for SpecKitSource {
    fn name(&self) -> &str {
        "speckit"
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.specs_dir.clone()]
    }

    fn scan(&self) -> Result<Vec<SourcedTask>, SourceError> {
        let err = |source| SourceError::Io {
            path: self.specs_dir.clone(),
            source,
        };
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.specs_dir).map_err(err)? {
            let path = entry.map_err(err)?.path();
            if path.is_dir() {
                entries.push(path);
            }
        }
        entries.sort();

        let mut tasks = Vec::new();
        for dir in entries {
            let tasks_path = dir.join("tasks.md");
            if !tasks_path.is_file() {
                continue;
            }
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let content = fs::read_to_string(&tasks_path).map_err(|source| SourceError::Io {
                path: tasks_path.clone(),
                source,
            })?;
            parse_tasks_md(
                &content,
                name,
                self.profile.as_deref(),
                self.ready,
                &mut tasks,
            );
        }
        Ok(tasks)
    }
}

struct InProgressTask {
    task_id: String,
    title: String,
    story: Option<String>,
    full_desc: String,
    continuations: Vec<String>,
    phase: Option<String>,
    done: bool,
}

fn finish_task(
    spec_dir: &str,
    profile: Option<&str>,
    ready: bool,
    task: InProgressTask,
    out: &mut Vec<SourcedTask>,
) {
    let mut body = format!("Spec: {spec_dir}\n");
    if let Some(ref phase) = task.phase {
        body.push_str(&format!("Phase: {phase}\n"));
    }
    if let Some(ref story) = task.story {
        body.push_str(&format!("Story: {story}\n"));
    }
    body.push('\n');
    body.push_str(&task.full_desc);
    for cont in &task.continuations {
        body.push('\n');
        body.push_str(cont);
    }
    out.push(SourcedTask {
        external_id: format!("{spec_dir}/{}", task.task_id),
        title: task.title,
        body,
        profile: profile.map(str::to_owned),
        ready,
        done: task.done,
    });
}

fn parse_description(raw: &str) -> (Option<String>, String, String) {
    let mut story = None;
    let mut words = Vec::new();
    for word in raw.split_whitespace() {
        if word.eq_ignore_ascii_case("[P]") {
            continue;
        }
        if word.starts_with("[US") && word.ends_with(']') {
            if story.is_none() {
                story = Some(word.to_string());
            }
            continue;
        }
        words.push(word);
    }
    let full_desc = words.join(" ");
    let title = if full_desc.chars().count() > 80 {
        full_desc.chars().take(80).collect()
    } else {
        full_desc.clone()
    };
    (story, full_desc, title)
}

fn parse_tasks_md(
    content: &str,
    spec_dir: &str,
    profile: Option<&str>,
    ready: bool,
    out: &mut Vec<SourcedTask>,
) {
    let mut current_phase: Option<String> = None;
    let mut in_progress: Option<InProgressTask> = None;
    let mut flush = |in_prog: &mut Option<InProgressTask>| {
        if let Some(task) = in_prog.take() {
            finish_task(spec_dir, profile, ready, task, out);
        }
    };

    for line in content.lines() {
        let trimmed_start = line.trim_start();
        if trimmed_start.starts_with("## ") {
            flush(&mut in_progress);
            let phase = trimmed_start
                .strip_prefix("##")
                .unwrap_or(trimmed_start)
                .trim();
            current_phase = Some(phase.to_string());
            continue;
        }
        let task_box = trimmed_start
            .strip_prefix("- [ ] ")
            .map(|after| (after, false))
            .or_else(|| {
                trimmed_start
                    .strip_prefix("- [x] ")
                    .or_else(|| trimmed_start.strip_prefix("- [X] "))
                    .map(|after| (after, true))
            });

        if let Some((after_box, done)) = task_box {
            flush(&mut in_progress);
            let mut parts = after_box.splitn(2, char::is_whitespace);
            let id = parts.next().unwrap_or_default();
            if id.starts_with('T') && id.len() > 1 {
                let remainder = parts.next().unwrap_or_default().trim();
                let (story, full_desc, title) = parse_description(remainder);
                in_progress = Some(InProgressTask {
                    task_id: id.to_string(),
                    title,
                    story,
                    full_desc,
                    continuations: Vec::new(),
                    phase: current_phase.clone(),
                    done,
                });
            }
            continue;
        }
        if let Some(ref mut task) = in_progress {
            if line.starts_with(' ') || line.starts_with('\t') {
                task.continuations.push(line.trim().to_string());
            } else if !line.trim().is_empty() {
                flush(&mut in_progress);
            }
        }
    }
    flush(&mut in_progress);
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

    #[test]
    fn parse_fixture_returns_tasks_with_done_flag_and_stripped_markers() {
        let root = tempfile::tempdir().unwrap();
        let spec_dir = root.path().join("001-virtual-output-projection");
        std::fs::create_dir_all(&spec_dir).unwrap();
        let tasks_content = "\
# Spec: Virtual Output Projection\n\n\
## Phase 1: Setup\n\n\
  - [ ] T001 Description text here\n    Continuation line for T001\n\
  - [x] T002 Already done line\n    Continuation for done\n\
  - [ ] T003 [P] Description with a parallel marker\n\
  - [X] T004 [P] [US1] Description with a story marker\n";
        std::fs::write(spec_dir.join("tasks.md"), tasks_content).unwrap();

        let config = SourceConfig {
            kind: "speckit".to_owned(),
            path: root.path().to_path_buf(),
            profile: Some("worker".to_owned()),
            ready: true,
        };
        let source = SpecKitSource::new(&config);
        assert_eq!(source.name(), "speckit");
        assert_eq!(source.watch_paths(), vec![root.path().to_path_buf()]);

        let tasks = source.scan().unwrap();
        assert_eq!(tasks.len(), 4);
        assert_eq!(tasks[0].external_id, "001-virtual-output-projection/T001");
        assert_eq!(tasks[0].title, "Description text here");
        assert!(
            tasks[0]
                .body
                .contains("Spec: 001-virtual-output-projection")
        );
        assert!(tasks[0].body.contains("Phase: Phase 1: Setup"));
        assert!(tasks[0].body.contains("Continuation line for T001"));
        assert_eq!(tasks[0].profile.as_deref(), Some("worker"));
        assert!(tasks[0].ready);
        assert!(!tasks[0].done);

        assert_eq!(tasks[1].external_id, "001-virtual-output-projection/T002");
        assert_eq!(tasks[1].title, "Already done line");
        assert!(tasks[1].body.contains("Continuation for done"));
        assert!(tasks[1].done);

        assert_eq!(tasks[2].external_id, "001-virtual-output-projection/T003");
        assert_eq!(tasks[2].title, "Description with a parallel marker");
        assert!(!tasks[2].title.contains("[P]"));
        assert!(!tasks[2].done);

        assert_eq!(tasks[3].external_id, "001-virtual-output-projection/T004");
        assert_eq!(tasks[3].title, "Description with a story marker");
        assert!(!tasks[3].title.contains("[P]"));
        assert!(!tasks[3].title.contains("[US1]"));
        assert!(tasks[3].body.contains("Story: [US1]"));
        assert!(tasks[3].done);
    }

    #[test]
    fn missing_specs_dir_returns_error() {
        let root = tempfile::tempdir().unwrap();
        let config = SourceConfig {
            kind: "speckit".to_owned(),
            path: root.path().join("does-not-exist"),
            profile: None,
            ready: false,
        };
        assert!(matches!(
            SpecKitSource::new(&config).scan().unwrap_err(),
            SourceError::Io { .. }
        ));
    }

    #[test]
    fn dir_without_tasks_md_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("002-no-tasks")).unwrap();
        let config = SourceConfig {
            kind: "speckit".to_owned(),
            path: root.path().to_path_buf(),
            profile: None,
            ready: false,
        };
        assert!(SpecKitSource::new(&config).scan().unwrap().is_empty());
    }
}
