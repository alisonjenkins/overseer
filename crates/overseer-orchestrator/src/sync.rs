use std::collections::HashSet;

use overseer_core::{NewTask, TaskSource, TaskStatus};
use overseer_store::{Store, StoreError};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub added: usize,
    pub updated: usize,
    pub cancelled: usize,
    pub revived: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
}

pub fn sync_sources(
    store: &mut Store,
    sources: &[Box<dyn TaskSource>],
) -> Result<SyncReport, SyncError> {
    let mut report = SyncReport::default();

    for source in sources {
        let source_name = source.name();
        let scanned = match source.scan() {
            Ok(tasks) => tasks,
            Err(err) => {
                report
                    .warnings
                    .push(format!("failed to scan source {source_name}: {err}"));
                continue;
            }
        };

        let mut seen_external_ids = HashSet::new();

        for task in scanned {
            seen_external_ids.insert(task.external_id.clone());

            let existing = store.find_task_by_external_id(source_name, &task.external_id)?;
            match existing {
                None => {
                    let created = store.add_task(NewTask {
                        title: task.title,
                        body: task.body,
                        priority: 0,
                        profile: task.profile,
                        depends_on: Vec::new(),
                        ready: task.ready,
                        source: Some(source_name.to_string()),
                        external_id: Some(task.external_id),
                        parent_task_id: None,
                    })?;
                    if task.done {
                        store.set_status(created.id, TaskStatus::Done)?;
                    }
                    report.added = report.added.saturating_add(1);
                }
                Some(existing) => {
                    if task.done {
                        match existing.status {
                            TaskStatus::Backlog | TaskStatus::Ready | TaskStatus::Cancelled => {
                                store.set_status(existing.id, TaskStatus::Done)?;
                                if existing.title != task.title || existing.body != task.body {
                                    store.set_task_title_and_body(
                                        existing.id,
                                        &task.title,
                                        &task.body,
                                    )?;
                                }
                                report.updated = report.updated.saturating_add(1);
                            }
                            _ => {}
                        }
                    } else {
                        match existing.status {
                            TaskStatus::Cancelled => {
                                let new_status = if task.ready {
                                    TaskStatus::Ready
                                } else {
                                    TaskStatus::Backlog
                                };
                                store.set_status(existing.id, new_status)?;
                                store.set_task_title_and_body(
                                    existing.id,
                                    &task.title,
                                    &task.body,
                                )?;
                                report.revived = report.revived.saturating_add(1);
                            }
                            TaskStatus::Backlog | TaskStatus::Ready
                                if existing.title != task.title || existing.body != task.body =>
                            {
                                store.set_task_title_and_body(
                                    existing.id,
                                    &task.title,
                                    &task.body,
                                )?;
                                report.updated = report.updated.saturating_add(1);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let existing_source_tasks = store.list_tasks_by_source(source_name)?;
        for existing in existing_source_tasks {
            let is_present = existing
                .external_id
                .as_ref()
                .is_some_and(|id| seen_external_ids.contains(id));

            if !is_present {
                match existing.status {
                    TaskStatus::Backlog | TaskStatus::Ready => {
                        store.set_status(existing.id, TaskStatus::Cancelled)?;
                        report.cancelled = report.cancelled.saturating_add(1);
                    }
                    TaskStatus::Cancelled | TaskStatus::Done => {}
                    other => {
                        report.warnings.push(format!(
                            "task {} has status {other}; cannot cancel",
                            existing.id
                        ));
                    }
                }
            }
        }
    }

    Ok(report)
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
    use crate::sources::SpecKitSource;
    use overseer_core::SourceConfig;
    use std::path::Path;

    fn setup(content: &str) -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        write_tasks(root.path(), content);
        (root, Store::open_in_memory().unwrap())
    }

    fn write_tasks(root: &Path, content: &str) {
        let spec_dir = root.join("specs").join("001-spec");
        std::fs::create_dir_all(&spec_dir).unwrap();
        std::fs::write(spec_dir.join("tasks.md"), content).unwrap();
    }

    fn make_source(root: &Path, ready: bool) -> Box<dyn TaskSource> {
        Box::new(SpecKitSource::new(&SourceConfig {
            kind: "speckit".to_owned(),
            path: root.join("specs"),
            profile: None,
            ready,
        }))
    }

    #[test]
    fn sync_adds_new_tasks_and_is_idempotent() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [ ] T001 Task 1\n- [ ] T002 Task 2\n");
        let sources = [make_source(root.path(), false)];

        let report = sync_sources(&mut store, &sources).unwrap();
        assert_eq!(
            (
                report.added,
                report.updated,
                report.cancelled,
                report.revived
            ),
            (2, 0, 0, 0)
        );
        assert!(report.warnings.is_empty());
        assert_eq!(store.list_tasks_by_source("speckit").unwrap().len(), 2);

        let report2 = sync_sources(&mut store, &sources).unwrap();
        assert_eq!(
            (
                report2.added,
                report2.updated,
                report2.cancelled,
                report2.revived
            ),
            (0, 0, 0, 0)
        );
        assert!(report2.warnings.is_empty());
    }

    #[test]
    fn sync_updates_changed_description_on_backlog_task() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [ ] T001 Task 1\n");
        let sources = [make_source(root.path(), false)];
        sync_sources(&mut store, &sources).unwrap();

        write_tasks(
            root.path(),
            "## Phase 1: Setup\n- [ ] T001 Task 1 Updated\n",
        );
        let report = sync_sources(&mut store, &sources).unwrap();
        assert_eq!((report.added, report.updated), (0, 1));
        let task = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(task.title, "Task 1 Updated");
    }

    #[test]
    fn removing_line_cancels_backlog_task_but_not_task_in_review() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [ ] T001 Task 1\n- [ ] T002 Task 2\n");
        let sources = [make_source(root.path(), false)];
        sync_sources(&mut store, &sources).unwrap();

        let t2 = store
            .find_task_by_external_id("speckit", "001-spec/T002")
            .unwrap()
            .unwrap();
        store.set_status(t2.id, TaskStatus::Review).unwrap();

        write_tasks(root.path(), "## Phase 1: Setup\n");
        let report = sync_sources(&mut store, &sources).unwrap();
        assert_eq!(report.cancelled, 1);
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].contains(&t2.id.to_string())
                && report.warnings[0].contains("review")
        );

        let t1 = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(t1.status, TaskStatus::Cancelled);
        let t2_after = store
            .find_task_by_external_id("speckit", "001-spec/T002")
            .unwrap()
            .unwrap();
        assert_eq!(t2_after.status, TaskStatus::Review);
    }

    #[test]
    fn failed_scan_cancels_nothing() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [ ] T001 Task 1\n");
        let sources = [make_source(root.path(), false)];
        sync_sources(&mut store, &sources).unwrap();

        let bad_source: Box<dyn TaskSource> = Box::new(SpecKitSource::new(&SourceConfig {
            kind: "speckit".to_owned(),
            path: root.path().join("missing"),
            profile: None,
            ready: false,
        }));
        let report = sync_sources(&mut store, &[bad_source]).unwrap();
        assert_eq!(report.cancelled, 0);
        assert_eq!(report.warnings.len(), 1);
        let t1 = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(t1.status, TaskStatus::Backlog);
    }

    #[test]
    fn re_adding_cancelled_line_revives_it() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [ ] T001 Task 1\n");
        let sources = [make_source(root.path(), false)];
        sync_sources(&mut store, &sources).unwrap();

        write_tasks(root.path(), "## Phase 1: Setup\n");
        assert_eq!(sync_sources(&mut store, &sources).unwrap().cancelled, 1);

        write_tasks(
            root.path(),
            "## Phase 1: Setup\n- [ ] T001 Task 1 Revived\n",
        );
        let report = sync_sources(&mut store, &sources).unwrap();
        assert_eq!(report.revived, 1);
        let t1 = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(t1.status, TaskStatus::Backlog);
        assert_eq!(t1.title, "Task 1 Revived");
    }

    #[test]
    fn new_checked_line_is_created_as_done_and_not_runnable() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [x] T001 Task 1\n");
        let report = sync_sources(&mut store, &[make_source(root.path(), true)]).unwrap();
        assert_eq!(report.added, 1);
        let task = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Done);
        assert_eq!(store.pick_next_ready_task().unwrap(), None);
    }

    #[test]
    fn backlog_task_becomes_done_when_line_is_checked() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [ ] T001 Task 1\n");
        let sources = [make_source(root.path(), false)];
        sync_sources(&mut store, &sources).unwrap();

        write_tasks(root.path(), "## Phase 1: Setup\n- [x] T001 Task 1\n");
        let report = sync_sources(&mut store, &sources).unwrap();
        assert_eq!(report.updated, 1);
        let task = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Done);
    }

    #[test]
    fn review_task_stays_review_when_line_is_checked() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [ ] T001 Task 1\n");
        let sources = [make_source(root.path(), false)];
        sync_sources(&mut store, &sources).unwrap();

        let task = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        store.set_status(task.id, TaskStatus::Review).unwrap();

        write_tasks(root.path(), "## Phase 1: Setup\n- [x] T001 Task 1\n");
        let report = sync_sources(&mut store, &sources).unwrap();
        assert_eq!(report.updated, 0);
        let task_after = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(task_after.status, TaskStatus::Review);
    }

    #[test]
    fn unchecked_line_for_done_task_stays_done() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [x] T001 Task 1\n");
        let sources = [make_source(root.path(), false)];
        sync_sources(&mut store, &sources).unwrap();

        let task = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Done);

        write_tasks(root.path(), "## Phase 1: Setup\n- [ ] T001 Task 1\n");
        let report = sync_sources(&mut store, &sources).unwrap();
        assert_eq!((report.updated, report.revived), (0, 0));
        let task_after = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(task_after.status, TaskStatus::Done);
    }

    #[test]
    fn removed_done_line_is_silent() {
        let (root, mut store) = setup("## Phase 1: Setup\n- [x] T001 Task 1\n");
        let sources = [make_source(root.path(), false)];
        sync_sources(&mut store, &sources).unwrap();

        let task = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(task.status, TaskStatus::Done);

        write_tasks(root.path(), "## Phase 1: Setup\n");
        let report = sync_sources(&mut store, &sources).unwrap();
        assert!(report.warnings.is_empty());
        assert_eq!(report.cancelled, 0);
        let task_after = store
            .find_task_by_external_id("speckit", "001-spec/T001")
            .unwrap()
            .unwrap();
        assert_eq!(task_after.status, TaskStatus::Done);
    }
}
