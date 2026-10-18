use overseer_core::Verdict;

const MAX_DIFF_BYTES: usize = 60 * 1024;
const TRUNCATION_NOTE: &str = "\n\n[Diff truncated to 60 KB; remaining lines omitted]";

/// Outcome of aggregating reviewer verdicts for a task round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// All reviewers passed; task remains in `review` and is marked approved.
    Approved,
    /// At least one reviewer asked for changes and round < max_reworks.
    Rework,
    /// A reviewer escalated or max_reworks was reached without approval; moves to `needs-human`.
    NeedsHuman,
}

/// The overall verdict for a round across all reviewers.
/// Any ESCALATE gives ESCALATE; else any CHANGES gives CHANGES; else PASS.
pub fn overall_verdict(verdicts: &[Verdict]) -> Verdict {
    if verdicts.contains(&Verdict::Escalate) {
        Verdict::Escalate
    } else if verdicts.contains(&Verdict::Changes) {
        Verdict::Changes
    } else {
        Verdict::Pass
    }
}

/// Aggregate reviewer verdicts into a `ReviewOutcome`.
/// - Any ESCALATE -> `NeedsHuman`
/// - Else any CHANGES -> `Rework` if round < max_reworks else `NeedsHuman`
/// - Else (all PASS) -> `Approved`
pub fn aggregate_verdicts(verdicts: &[Verdict], round: u8, max_reworks: u8) -> ReviewOutcome {
    if verdicts.contains(&Verdict::Escalate) {
        ReviewOutcome::NeedsHuman
    } else if verdicts.contains(&Verdict::Changes) {
        if round < max_reworks {
            ReviewOutcome::Rework
        } else {
            ReviewOutcome::NeedsHuman
        }
    } else {
        ReviewOutcome::Approved
    }
}

/// Truncate diff to 60 KB if longer, appending an explanatory note.
fn truncate_diff(diff: &str) -> (String, bool) {
    if diff.len() <= MAX_DIFF_BYTES {
        return (diff.to_owned(), false);
    }
    let mut boundary = MAX_DIFF_BYTES;
    while boundary > 0 && !diff.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    let truncated = diff.get(..boundary).unwrap_or("");
    let mut out = String::with_capacity(truncated.len().saturating_add(TRUNCATION_NOTE.len()));
    out.push_str(truncated);
    out.push_str(TRUNCATION_NOTE);
    (out, true)
}

/// Build the reviewer prompt containing the task title, body, and the unified diff.
/// Truncates the diff to 60 KB with an explanatory note if longer.
/// Explicitly instructs the model:
/// - Reply with the verdict line first (`VERDICT: PASS`, `VERDICT: CHANGES`, or `VERDICT: ESCALATE`)
/// - Follow with a short bullet list of problems (CHANGES) or reasons (ESCALATE)
/// - Do NOT use tools or run commands
/// - Do NOT edit files
pub fn build_reviewer_prompt(title: &str, body: &str, diff: &str) -> String {
    let (diff_content, _) = truncate_diff(diff);
    let mut prompt = String::new();
    prompt.push_str("Task Title: ");
    prompt.push_str(title);
    prompt.push_str("\n\nTask Description:\n");
    if body.trim().is_empty() {
        prompt.push_str("(No description provided)\n");
    } else {
        prompt.push_str(body);
        prompt.push('\n');
    }
    prompt.push_str("\nDiff against base branch:\n```diff\n");
    prompt.push_str(&diff_content);
    prompt.push_str("\n```\n\n");
    prompt.push_str(
        "Instructions for review:\n\
         1. You are reviewing the changes above for the task described.\n\
         2. Reply with the verdict as the FIRST non-empty line of your response:\n\
            - `VERDICT: PASS` if the changes correctly address the task without issues.\n\
            - `VERDICT: CHANGES` if modifications are required before this can be accepted.\n\
            - `VERDICT: ESCALATE` if human intervention is required.\n\
         3. If your verdict is CHANGES, follow it with a short bullet list of concrete problems.\n\
         4. If your verdict is ESCALATE, follow it with a short bullet list of reasons.\n\
         5. Do NOT use any tools or run commands because all necessary context and diff are provided above.\n\
         6. Do NOT edit files.\n",
    );
    prompt
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn build_reviewer_prompt_contains_all_required_elements() {
        let title = "Fix parser null pointer";
        let body = "Ensure tokenizer returns empty token stream on empty input.";
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n+fn new() {}\n";

        let prompt = build_reviewer_prompt(title, body, diff);

        assert!(prompt.contains("Task Title: Fix parser null pointer"));
        assert!(prompt.contains("Ensure tokenizer returns empty token stream on empty input."));
        assert!(prompt.contains("diff --git a/src/lib.rs b/src/lib.rs"));
        assert!(prompt.contains("VERDICT: PASS"));
        assert!(prompt.contains("VERDICT: CHANGES"));
        assert!(prompt.contains("VERDICT: ESCALATE"));
        assert!(prompt.contains("Do NOT use any tools or run commands"));
        assert!(prompt.contains("Do NOT edit files"));
        assert!(!prompt.contains("Diff truncated"));
    }

    #[test]
    fn build_reviewer_prompt_truncates_large_diff() {
        let title = "Large refactor";
        let body = "Lots of lines changed.";
        // Create a diff larger than 60 KB (65_000 bytes)
        let large_diff = "a".repeat(65_000);

        let prompt = build_reviewer_prompt(title, body, &large_diff);

        assert!(prompt.contains("Diff truncated to 60 KB"));
        let (truncated, is_truncated) = truncate_diff(&large_diff);
        assert!(is_truncated);
        assert!(truncated.contains("Diff truncated to 60 KB"));
        // Truncated content length should not exceed 60 KB + note
        assert!(truncated.len() <= MAX_DIFF_BYTES + TRUNCATION_NOTE.len());
    }

    #[test]
    fn truncate_diff_handles_multibyte_utf8() {
        // Multi-byte character '€' (3 bytes in UTF-8)
        let euro = "€".repeat(25_000); // 75_000 bytes
        let (truncated, is_truncated) = truncate_diff(&euro);
        assert!(is_truncated);
        assert!(truncated.contains("Diff truncated to 60 KB"));
    }

    #[test]
    fn overall_verdict_rules() {
        assert_eq!(overall_verdict(&[]), Verdict::Pass);
        assert_eq!(overall_verdict(&[Verdict::Pass]), Verdict::Pass);
        assert_eq!(
            overall_verdict(&[Verdict::Pass, Verdict::Pass]),
            Verdict::Pass
        );
        assert_eq!(
            overall_verdict(&[Verdict::Pass, Verdict::Changes]),
            Verdict::Changes
        );
        assert_eq!(
            overall_verdict(&[Verdict::Changes, Verdict::Pass]),
            Verdict::Changes
        );
        assert_eq!(
            overall_verdict(&[Verdict::Pass, Verdict::Escalate]),
            Verdict::Escalate
        );
        assert_eq!(
            overall_verdict(&[Verdict::Changes, Verdict::Escalate]),
            Verdict::Escalate
        );
    }

    #[test]
    fn aggregate_verdicts_approved_when_all_pass() {
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass], 0, 2),
            ReviewOutcome::Approved
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass, Verdict::Pass], 1, 2),
            ReviewOutcome::Approved
        );
        assert_eq!(aggregate_verdicts(&[], 0, 2), ReviewOutcome::Approved);
    }

    #[test]
    fn aggregate_verdicts_rework_when_changes_and_under_limit() {
        // round 0 < max_reworks 2
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes], 0, 2),
            ReviewOutcome::Rework
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass, Verdict::Changes], 0, 2),
            ReviewOutcome::Rework
        );
        // round 1 < max_reworks 2
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes], 1, 2),
            ReviewOutcome::Rework
        );
    }

    #[test]
    fn aggregate_verdicts_needs_human_when_changes_and_limit_reached() {
        // round 2 == max_reworks 2
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes], 2, 2),
            ReviewOutcome::NeedsHuman
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass, Verdict::Changes], 2, 2),
            ReviewOutcome::NeedsHuman
        );
        // round 3 > max_reworks 2
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes], 3, 2),
            ReviewOutcome::NeedsHuman
        );
    }

    #[test]
    fn aggregate_verdicts_needs_human_when_any_escalate() {
        assert_eq!(
            aggregate_verdicts(&[Verdict::Escalate], 0, 2),
            ReviewOutcome::NeedsHuman
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Pass, Verdict::Escalate], 0, 2),
            ReviewOutcome::NeedsHuman
        );
        assert_eq!(
            aggregate_verdicts(&[Verdict::Changes, Verdict::Escalate], 0, 2),
            ReviewOutcome::NeedsHuman
        );
    }
}
